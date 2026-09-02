//! Request handlers for the embedded web UI.
//!
//! Every handler is a read of a shared action result rendered into HTML. The
//! one exception is machine creation, which posts here rather than straight to
//! `/v1/machines` so a rejected field can be answered next to that field
//! instead of thrown as a notification.
//!
//! Lifecycle mutations are not routed here at all: start, stop, restart,
//! delete and image pull go from the browser to the real `/v1` endpoints and
//! render their NDJSON progress as it arrives. There is exactly one mutation
//! surface, and it is the documented one.

use std::collections::BTreeMap;

use axum::{
    Form,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::Response,
};
use firestone_core::{
    Action, CatalogEntrySummary, ErrorKind, FirestoneError, ImageRef, LogSource, MachineSpec,
    MachineSpecPatch, MachineSummary, MachineView, NetMode, SnapshotListResult, VersionResult,
};
use minijinja::{Value, context};
use serde::Deserialize;

use crate::{
    api,
    ui::{
        UiState,
        render::{render, urlencode},
        view::{
            CachedImage, CatalogCard, CheckInfo, HostInfo, MachineDetail, MachineRow,
            OverviewMachine, VersionInfo, format_bytes, image_rows, net_mode_token,
            overview_machines as overview_machine_rows, snapshot_rows, stats,
        },
    },
};

/// Cap on how much log text one non-following read renders.
const LOG_LINES: u32 = 400;

/// Log sources the UI offers, with the labels an operator recognises.
const LOG_SOURCES: &[(&str, &str)] = &[
    ("console", "console"),
    ("vmm", "cloud-hypervisor"),
    ("shim", "shim"),
    ("passt", "passt"),
];

// ------------------------------------------------------------------ pages --

pub async fn overview(State(state): State<UiState>, headers: HeaderMap) -> Response {
    match overview_body(&state).await {
        Ok(body) => page(&state, headers, "overview", "Host overview", body).await,
        Err(error) => failure(&state, headers, error).await,
    }
}

async fn overview_body(state: &UiState) -> Result<String, FirestoneError> {
    let machines = list_machines(state).await?;
    let images = list_images(state).await?;
    let report = state.doctor().await?;
    let version = version_info(state).await?;

    let checks: Vec<CheckInfo> = report.checks.iter().map(CheckInfo::from).collect();
    let attention: Vec<&CheckInfo> = checks.iter().filter(|check| check.status != "ok").collect();
    let overview_rows: Vec<OverviewMachine> = overview_machine_rows(&machines);

    render(
        "ui/overview.html",
        context! {
            subtitle => subtitle(&version),
            host => HostInfo::from(&report),
            checks => checks,
            attention => attention,
            stats => stats(&machines, &images),
            machines => overview_rows,
            images => cached_image_rows(&images),
            version => version,
        },
    )
}

pub async fn machines(
    State(state): State<UiState>,
    headers: HeaderMap,
    Query(query): Query<FilterQuery>,
) -> Response {
    match machines_body(&state, query.q.as_deref().unwrap_or_default()).await {
        Ok(body) => page(&state, headers, "machines", "Machines", body).await,
        Err(error) => failure(&state, headers, error).await,
    }
}

async fn machines_body(state: &UiState, query: &str) -> Result<String, FirestoneError> {
    let all = list_machines(state).await?;
    let rows = filtered_rows(&all, query);
    let running = all
        .iter()
        .filter(|machine| machine.status == "running")
        .count();

    render(
        "ui/machines.html",
        context! {
            query => query,
            machines => rows,
            machine_count => all.len(),
            subtitle => format!(
                "{} {} · {running} running",
                all.len(),
                if all.len() == 1 { "machine" } else { "machines" },
            ),
        },
    )
}

pub async fn detail(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<TabQuery>,
) -> Response {
    match detail_body(&state, &name, &query).await {
        Ok(body) => page(&state, headers, "machines", &name, body).await,
        Err(error) => failure(&state, headers, error).await,
    }
}

async fn detail_body(
    state: &UiState,
    name: &str,
    query: &TabQuery,
) -> Result<String, FirestoneError> {
    let view = show_machine(state, name).await?;
    let machine = MachineDetail::new(name, &view, &host_arch(state).await);
    let tab = query.tab.as_deref().unwrap_or("spec");
    let source = query.source.as_deref().unwrap_or("console");
    let console = machine.is_running;

    let mut ctx = tab_context(state, name, &machine, tab, source, query.follow).await?;
    ctx.insert("machine".to_owned(), Value::from_serialize(&machine));
    ctx.insert(
        "tabs".to_owned(),
        Value::from_serialize(tabs(name, tab, console)),
    );
    ctx.insert(
        "tab_template".to_owned(),
        Value::from(tab_template(tab, console).to_owned()),
    );

    render("ui/detail.html", Value::from_object(ctx))
}

pub async fn catalog(State(state): State<UiState>, headers: HeaderMap) -> Response {
    match catalog_body(&state).await {
        Ok(body) => page(&state, headers, "catalog", "Image catalog", body).await,
        Err(error) => failure(&state, headers, error).await,
    }
}

async fn catalog_body(state: &UiState) -> Result<String, FirestoneError> {
    render(
        "ui/catalog.html",
        context! {
            entries => catalog_cards_data(state).await?,
            images => image_rows(&list_images(state).await?),
        },
    )
}

/// `GET /machines/{name}/terminal` — the full-window browser terminal.
///
/// Rendering it is a read like every other screen; the terminal itself is the
/// `/v1` WebSocket transports of §16.3, opened by `term.js` after the document
/// loads. The page is served whatever the machine's state is, because a
/// stopped machine's terminal must explain itself rather than 404: the
/// overlay names the reason and the Reconnect button is the retry.
///
/// This is the only response in the application that carries a wasm-capable
/// Content-Security-Policy, and it asks for it by marking itself — see
/// [`crate::ui::auth::WasmPolicy`].
///
/// `?embed=1` renders the same terminal without the page chrome, for the
/// Console tab on the machine detail screen to frame. That variant is the one
/// response whose `frame-ancestors` is `'self'` rather than `'none'`; the
/// standalone page stays unframeable.
pub async fn terminal(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<TabQuery>,
) -> Response {
    let embedded = query.is_embed();
    match terminal_body(&state, &name, query.tab.as_deref(), embedded).await {
        Ok(body) => {
            let mut response = html(StatusCode::OK, body);
            response.extensions_mut().insert(if embedded {
                crate::ui::auth::WasmPolicy::Embedded
            } else {
                crate::ui::auth::WasmPolicy::Standalone
            });
            response
        }
        Err(error) => failure(&state, headers, error).await,
    }
}

async fn terminal_body(
    state: &UiState,
    name: &str,
    tab: Option<&str>,
    embed: bool,
) -> Result<String, FirestoneError> {
    let view = show_machine(state, name).await?;
    let detail = MachineDetail::new(name, &view, "");
    render(
        "ui/terminal.html",
        context! {
            build => state.build.as_str(),
            terminal => TerminalPage::new(&detail, tab, embed),
        },
    )
}

/// Everything the terminal page needs beyond the machine itself.
///
/// `slug` is the percent-encoded machine name the template composes every URL
/// from — the same encoding the routes themselves use, so a name with a slash
/// or a space links to exactly the route that will match it. The template
/// joins the fixed path segments, as every other template here does, which
/// keeps the interpolated value free of characters HTML escaping would
/// rewrite.
#[derive(Debug, serde::Serialize)]
struct TerminalPage {
    name: String,
    slug: String,
    status: String,
    tab: &'static str,
    note: &'static str,
    /// Rendered inside the detail page's Console tab: no back link, and an
    /// expand control that hands the session to the full page.
    embed: bool,
}

impl TerminalPage {
    fn new(machine: &MachineDetail, tab: Option<&str>, embed: bool) -> Self {
        // The console is the default because it is the one transport that
        // works before a machine has a network, a user, or an sshd.
        let tab = if tab == Some("shell") {
            "shell"
        } else {
            "console"
        };
        Self {
            name: machine.name.clone(),
            slug: urlencode(&machine.name),
            status: machine.status.clone(),
            tab,
            note: match tab {
                "shell" => "SSH over vsock on a host pseudo-terminal · resize is honoured",
                _ => "serial console · single client · resize is the guest's to decide",
            },
            embed,
        }
    }
}

// -------------------------------------------------------------- fragments --

pub async fn host_pill(State(state): State<UiState>) -> Response {
    match state.doctor().await {
        Ok(report) => fragment(render(
            "ui/_hostpill.html",
            context! { host => HostInfo::from(&report) },
        )),
        Err(error) => fragment_error(error),
    }
}

pub async fn overview_stats(State(state): State<UiState>) -> Response {
    let result = async {
        let machines = list_machines(&state).await?;
        let images = list_images(&state).await?;
        render(
            "ui/_stats.html",
            context! { stats => stats(&machines, &images) },
        )
    }
    .await;
    fragment(result)
}

pub async fn overview_machines(State(state): State<UiState>) -> Response {
    let result = async {
        let machines = list_machines(&state).await?;
        let rows: Vec<OverviewMachine> = overview_machine_rows(&machines);
        render("ui/_overview_machines.html", context! { machines => rows })
    }
    .await;
    fragment(result)
}

pub async fn machine_rows(
    State(state): State<UiState>,
    Query(query): Query<FilterQuery>,
) -> Response {
    let result = async {
        let all = list_machines(&state).await?;
        let filter = query.q.as_deref().unwrap_or_default();
        render(
            "ui/_machine_rows.html",
            context! {
                machines => filtered_rows(&all, filter),
                machine_count => all.len(),
                query => filter,
            },
        )
    }
    .await;
    fragment(result)
}

pub async fn machine_head(State(state): State<UiState>, Path(name): Path<String>) -> Response {
    let result = async {
        let view = show_machine(&state, &name).await?;
        // The head renders no spec rows, so it needs no host architecture and
        // does not spend a version read on a five-second poll.
        render(
            "ui/_detail_head.html",
            context! { machine => MachineDetail::new(&name, &view, "") },
        )
    }
    .await;
    fragment(result)
}

pub async fn machine_tab(
    State(state): State<UiState>,
    Path((name, tab)): Path<(String, String)>,
    Query(query): Query<TabQuery>,
) -> Response {
    let result = async {
        let view = show_machine(&state, &name).await?;
        let machine = MachineDetail::new(&name, &view, &host_arch(&state).await);
        let source = query.source.as_deref().unwrap_or("console");
        let console = machine.is_running;
        let mut ctx = tab_context(&state, &name, &machine, &tab, source, query.follow).await?;
        ctx.insert("machine".to_owned(), Value::from_serialize(&machine));
        render(tab_template(&tab, console), Value::from_object(ctx))
    }
    .await;
    fragment(result)
}

pub async fn catalog_cards(State(state): State<UiState>) -> Response {
    let result = async {
        render(
            "ui/_catalog_cards.html",
            context! { entries => catalog_cards_data(&state).await? },
        )
    }
    .await;
    fragment(result)
}

/// The cached-images table on its own, re-read after a delete or a prune.
///
/// A read like every other `/ui` fragment: the deletions themselves go from
/// the browser to `DELETE /v1/images/{id}` and `POST /v1/images/prune`.
pub async fn catalog_images(State(state): State<UiState>) -> Response {
    let result = async {
        render(
            "ui/_catalog_images.html",
            context! { images => image_rows(&list_images(&state).await?) },
        )
    }
    .await;
    fragment(result)
}

pub async fn palette(State(state): State<UiState>, Query(query): Query<FilterQuery>) -> Response {
    let result = async {
        let needle = query.q.as_deref().unwrap_or_default().trim().to_lowercase();
        let machines = list_machines(&state).await?;
        let entries = list_catalog(&state).await?;
        let images = list_images(&state).await?;

        let matched_machines: Vec<OverviewMachine> = machines
            .iter()
            .filter(|machine| {
                needle.is_empty()
                    || machine.name.to_lowercase().contains(&needle)
                    || machine.image.to_lowercase().contains(&needle)
            })
            .take(6)
            .map(OverviewMachine::from)
            .collect();

        let cached = cached_by_reference(&images);
        let matched_images: Vec<PaletteImage> = entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.reference.to_lowercase().contains(&needle))
            .take(6)
            .map(|entry| PaletteImage {
                reference: entry.reference.clone(),
                cached: cached.contains_key(&entry.reference),
                note: cached
                    .get(&entry.reference)
                    .map_or_else(|| "not cached".to_owned(), |size| format_bytes(*size)),
            })
            .collect();

        render(
            "ui/palette.html",
            context! {
                query => query.q.as_deref().unwrap_or_default(),
                machines => matched_machines,
                images => matched_images,
                actions => palette_actions(&needle, &machines),
            },
        )
    }
    .await;
    fragment(result)
}

#[derive(Debug, serde::Serialize)]
struct PaletteImage {
    reference: String,
    cached: bool,
    note: String,
}

/// One command the palette can start.
///
/// `machine` is empty for a host-wide command. The palette itself only opens
/// the dialog named by `kind`; every write still goes to `/v1`.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct PaletteAction {
    kind: &'static str,
    label: String,
    note: &'static str,
    machine: String,
    /// Whether the named machine is running, so the snapshot dialog can state
    /// the tier it is about to take without a second read.
    running: bool,
}

/// How many machines a verb-matched palette query offers a command for.
const PALETTE_ACTION_MACHINES: usize = 5;

/// Builds the palette's Actions group.
///
/// The three host-wide commands always show, because they are what an operator
/// reaches the palette for when nothing in particular is named, and each of
/// them opens a dialog rather than writing. The machine-scoped commands are
/// **verb-first**: they appear only when the query is reaching for one, so an
/// empty palette is a list of machines rather than four commands per machine.
///
/// The whole set is exactly the set the screens themselves offer, and every
/// entry opens the same dialog or the same page the screen's own control opens
/// (SPEC §16.5). The palette adds no capability, and in particular offers no
/// lifecycle command: start, stop, restart and delete render a transition on
/// the button that dispatched them, and a palette entry has no button.
fn palette_actions(needle: &str, machines: &[MachineSummary]) -> Vec<PaletteAction> {
    let matches = |keywords: &[&str]| {
        needle.is_empty() || keywords.iter().any(|keyword| keyword.starts_with(needle))
    };

    let mut actions = Vec::new();
    if matches(&["new", "machine", "create"]) {
        actions.push(PaletteAction {
            kind: "new-machine",
            label: "New machine".to_owned(),
            note: "opens the create dialog",
            machine: String::new(),
            running: false,
        });
    }
    if matches(&["prune", "images", "cache"]) {
        actions.push(PaletteAction {
            kind: "prune-images",
            label: "Prune unused images".to_owned(),
            note: "POST /v1/images/prune",
            machine: String::new(),
            running: false,
        });
    }
    if matches(&["prune", "disk", "space", "free", "reclaim"]) {
        actions.push(PaletteAction {
            kind: "prune-system",
            label: "Free disk space".to_owned(),
            note: "previews before it removes",
            machine: String::new(),
            running: false,
        });
    }

    // Verb-first, and never on an empty query: "snap" and "clone" are what an
    // operator types, and a machine name alone is a request to open it.
    for (verb, kind, note) in [
        ("snapshot", "snapshot", "POST …/snapshots"),
        ("clone", "clone", "POST …/clone"),
        ("edit", "edit", "opens the edit dialog"),
        ("terminal", "terminal", "console and shell"),
    ] {
        if needle.is_empty() || !verb.starts_with(needle) {
            continue;
        }
        for machine in machines.iter() {
            // A clone of a running machine is refused before it starts
            // (§24.2), so the palette does not offer one at all: an entry that
            // can only fail is worse than an entry that is absent. Both
            // terminal transports need a live machine for the same reason.
            if kind == "clone" && !crate::ui::view::is_clonable(&machine.status) {
                continue;
            }
            if kind == "terminal" && machine.status != "running" {
                continue;
            }
            if actions.iter().filter(|entry| entry.kind == kind).count() >= PALETTE_ACTION_MACHINES
            {
                break;
            }
            actions.push(PaletteAction {
                kind,
                label: format!("{verb} {}", machine.name),
                note,
                machine: machine.name.clone(),
                running: machine.status == "running",
            });
        }
    }
    actions
}

// ----------------------------------------------------------------- create --

pub async fn create_form(State(state): State<UiState>) -> Response {
    let result = async {
        let defaults = MachineSpec::from_layers(
            &state.create_defaults,
            &MachineSpecPatch::default(),
            &MachineSpecPatch::default(),
        )?;
        let form = CreateForm::from_defaults(&defaults);
        let images = picker_images(&state).await?;
        render(
            "ui/create.html",
            create_context(&form, &BTreeMap::new(), images),
        )
    }
    .await;
    fragment(result)
}

/// The image picker on its own, re-read after a pull finishes inside the
/// dialog. A read, like every other `/ui` fragment: the field the form
/// actually submits lives outside it and is never rewritten here.
pub async fn create_form_images(State(state): State<UiState>) -> Response {
    let result = async {
        render(
            "ui/_image_picker.html",
            context! {
                images => picker_images(&state).await?,
                selected => "",
                custom => "",
            },
        )
    }
    .await;
    fragment(result)
}

pub async fn create_machine(
    State(state): State<UiState>,
    Form(form): Form<CreateForm>,
) -> Response {
    let patch = match form.to_patch() {
        Ok(patch) => patch,
        Err(errors) => return create_rejected(&state, &form, errors).await,
    };

    let spec = match MachineSpec::from_layers(
        &state.create_defaults,
        &MachineSpecPatch::default(),
        &patch,
    ) {
        Ok(spec) => spec,
        Err(error) => return create_rejected(&state, &form, field_errors(&error)).await,
    };

    let name = form.name.trim().to_owned();
    match api::dispatch_payload(
        &state.dispatcher,
        Action::Create {
            name: name.clone(),
            spec,
        },
        "create",
    )
    .await
    {
        Ok(_) => created(&name),
        Err(error) => create_rejected(&state, &form, field_errors(&error)).await,
    }
}

/// Re-renders the dialog with the message attached to the offending field.
/// A 400 or a 409 here is ordinary, expected input handling, not an incident.
async fn create_rejected(
    state: &UiState,
    form: &CreateForm,
    errors: BTreeMap<String, String>,
) -> Response {
    let images = picker_images(state).await.unwrap_or_default();
    fragment(render(
        "ui/create.html",
        create_context(form, &errors, images),
    ))
}

/// One context builder for both renders of the dialog, so a field that is
/// offered on the first attempt is still offered after a rejection.
fn create_context(
    form: &CreateForm,
    errors: &BTreeMap<String, String>,
    images: Vec<PickerImage>,
) -> Value {
    // The free-text row holds whatever the catalog list cannot: a URL, a path,
    // or a reference this host does not know about.
    let custom_image = if images
        .iter()
        .any(|image| image.reference == form.image.trim())
    {
        String::new()
    } else {
        form.image.trim().to_owned()
    };

    context! {
        form => form,
        errors => errors,
        images => images,
        custom_image => custom_image,
        memory => SizeField::split(&form.memory),
        disk => SizeField::split(&form.disk),
        net_modes => ["passt", "tap", "none"],
        // The password itself never reaches a template — `CreateForm` does not
        // serialize it — so the dialog says it was dropped rather than letting
        // the user submit a machine with a credential they thought they typed.
        password_cleared => !form.password.is_empty(),
    }
}

/// One catalog entry as the create dialog's picker renders it.
#[derive(Debug, Clone, serde::Serialize)]
struct PickerImage {
    reference: String,
    aliases: String,
    cached: bool,
    cached_id: String,
    size: String,
}

/// The same assembly the catalog cards use: catalog entries joined to the
/// image store so an entry that is already on disk says so, with its size,
/// rather than offering a pull that would do nothing.
async fn picker_images(state: &UiState) -> Result<Vec<PickerImage>, FirestoneError> {
    let entries = list_catalog(state).await?;
    let images = list_images(state).await?;
    let cached = cached_by_reference(&images);
    let ids = cached_ids(&images);

    Ok(entries
        .iter()
        .map(|entry| {
            let size = cached.get(&entry.reference).copied();
            PickerImage {
                reference: entry.reference.clone(),
                aliases: entry.aliases.join(" · "),
                cached: size.is_some(),
                cached_id: ids.get(&entry.reference).cloned().unwrap_or_default(),
                size: size.map(format_bytes).unwrap_or_default(),
            }
        })
        .collect())
}

/// A size split into the number and unit the dialog edits, beside the exact
/// string the handler will parse.
///
/// `raw` is what gets submitted when nothing is touched, so a value the server
/// could not parse comes back verbatim rather than being silently rewritten
/// into something that would fail differently.
#[derive(Debug, Clone, serde::Serialize)]
struct SizeField {
    raw: String,
    amount: String,
    unit: &'static str,
}

impl SizeField {
    fn split(raw: &str) -> Self {
        let trimmed = raw.trim();
        // ByteSize's grammar: a trailing G is GiB, a trailing M is MiB, and a
        // bare integer is MiB. The unit select must round-trip that exactly.
        let (digits, unit) = match trimmed.as_bytes().last() {
            Some(b'G' | b'g') => (&trimmed[..trimmed.len() - 1], "G"),
            Some(b'M' | b'm') => (&trimmed[..trimmed.len() - 1], "M"),
            Some(_) => (trimmed, "M"),
            None => ("", "G"),
        };
        let amount = if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            digits.to_owned()
        } else {
            String::new()
        };
        Self {
            raw: trimmed.to_owned(),
            amount,
            unit,
        }
    }
}

fn created(name: &str) -> Response {
    let mut response = Response::new(Body::empty());
    let headers = response.headers_mut();
    // Close the dialog, announce the result and land on the new machine, all
    // through htmx's own response protocol rather than a client-side guess.
    let trigger = serde_json::json!({
        "fs:created": { "name": name, "sub": "POST /v1/machines 201" }
    });
    if let Ok(value) = HeaderValue::from_str(&ascii_json(&trigger)) {
        headers.insert("HX-Trigger", value);
    }
    let location = serde_json::json!({
        "path": format!("/machines/{}", urlencode(name)),
        "target": "#fs-main-content",
    });
    if let Ok(value) = HeaderValue::from_str(&ascii_json(&location)) {
        headers.insert("HX-Location", value);
    }
    response
}

/// Serializes to JSON with every non-ASCII scalar escaped as `\uXXXX`.
///
/// A header value may only carry visible ASCII. A machine name is arbitrary
/// UTF-8, so serializing it directly would make `HeaderValue::from_str` fail
/// and silently drop the completion the operator is waiting for.
fn ascii_json(value: &serde_json::Value) -> String {
    let raw = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned());
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_ascii_graphic() || character == ' ' {
            escaped.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0u16; 2]) {
                escaped.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    escaped
}

/// Attaches an error to the field it is about, falling back to a form-level
/// notice when the message names nothing recognisable.
fn field_errors(error: &FirestoneError) -> BTreeMap<String, String> {
    let info = error.info();
    let mut errors = BTreeMap::new();
    let lowered = info.message.to_lowercase();

    let field = if lowered.contains("name") || matches!(error.kind(), ErrorKind::AlreadyExists) {
        "name"
    } else if lowered.contains("image") {
        "image"
    } else if lowered.contains("cpus") || lowered.contains("vcpu") {
        "cpus"
    } else if lowered.contains("memory") {
        "memory"
    } else if lowered.contains("disk") {
        "disk"
    } else if lowered.contains("forward") || lowered.contains("port") {
        "forward"
    // The cloud-init branches sit ahead of the plain "user" branch on purpose:
    // "cloud_init.user_data_inline …" contains "user", and would otherwise be
    // answered beside the guest-user field.
    } else if lowered.contains("user_data") || lowered.contains("user-data") {
        "user_data_inline"
    } else if lowered.contains("ssh_authorized_keys") || lowered.contains("public key") {
        "ssh_authorized_keys"
    } else if lowered.contains("password") {
        "password"
    } else if lowered.contains("provisioning") {
        "provisioning"
    } else if lowered.contains("user") {
        "user"
    } else if lowered.contains("mount") || lowered.contains("share") {
        "mounts"
    } else if lowered.contains("tap") {
        "tap"
    } else if lowered.contains("mac address") {
        "mac"
    } else {
        "form"
    };

    errors.insert(field.to_owned(), info.message.clone());
    if let Some(hint) = info.hint.clone() {
        errors.insert("hint".to_owned(), hint);
    }
    errors
}

/// The create dialog's fields, in both directions: rendered into the form and
/// parsed back out of it.
///
/// `Debug` is written by hand rather than derived: this struct carries a guest
/// password and inline cloud-init, and SPEC §10.5 forbids either reaching a log
/// line, a trace, or a panic message.
#[derive(Default, Clone, Deserialize, serde::Serialize)]
pub struct CreateForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub cpus: String,
    #[serde(default)]
    pub memory: String,
    #[serde(default)]
    pub disk: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub net_mode: String,
    #[serde(default)]
    pub forward: String,
    #[serde(default)]
    pub tap: String,
    #[serde(default)]
    pub mac: String,
    /// Newline-separated `HOST:GUEST[:ro]`, the grammar `--mount` already uses.
    #[serde(default)]
    pub mounts: String,
    /// Inline cloud-init user-data, verbatim except for CRLF normalisation.
    #[serde(default)]
    pub user_data_inline: String,
    /// One OpenSSH public key per line.
    #[serde(default)]
    pub ssh_authorized_keys: String,
    /// The guest password, write-only.
    ///
    /// `skip_serializing` is the mechanism, not a convention: the template
    /// context is built from this struct, so a field that cannot serialize
    /// cannot be echoed back into the dialog by any template, present or
    /// future. A rejected submission says the password was cleared instead.
    #[serde(default, skip_serializing)]
    pub password: String,
    /// `on` when the checkbox was ticked, empty otherwise.
    #[serde(default)]
    pub ssh_pwauth: String,
    /// `on` when the checkbox was ticked, empty otherwise.
    #[serde(default)]
    pub provisioning: String,
    /// Set by the hidden marker the provisioning section carries.
    ///
    /// An unticked checkbox submits nothing, so without this a submission from
    /// a form that never offered the section is indistinguishable from one
    /// where both boxes were cleared, and `provisioning` would silently flip to
    /// false. The marker makes the two checkbox fields meaningful only when the
    /// section that owns them was actually rendered.
    #[serde(default)]
    pub provisioning_section: String,
}

/// Redacts the same two leaves [`firestone_core::CloudInitSpec`] redacts.
impl std::fmt::Debug for CreateForm {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreateForm")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("cpus", &self.cpus)
            .field("memory", &self.memory)
            .field("disk", &self.disk)
            .field("user", &self.user)
            .field("net_mode", &self.net_mode)
            .field("forward", &self.forward)
            .field("tap", &self.tap)
            .field("mac", &self.mac)
            .field("mounts", &self.mounts)
            .field("user_data_inline", &redacted_len(&self.user_data_inline))
            .field("ssh_authorized_keys", &self.ssh_authorized_keys)
            .field("password", &redacted_len(&self.password))
            .field("ssh_pwauth", &self.ssh_pwauth)
            .field("provisioning", &self.provisioning)
            .field("provisioning_section", &self.provisioning_section)
            .finish()
    }
}

/// A secret's shape without its bytes: empty, or a byte count.
fn redacted_len(value: &str) -> String {
    if value.is_empty() {
        "\"\"".to_owned()
    } else {
        format!("<redacted, {} bytes>", value.len())
    }
}

impl CreateForm {
    fn from_defaults(spec: &MachineSpec) -> Self {
        Self {
            name: String::new(),
            image: spec.image.as_str().to_owned(),
            cpus: spec.cpus.to_string(),
            memory: spec.memory.to_string(),
            disk: spec.disk.to_string(),
            user: spec.user.clone(),
            net_mode: net_mode_token(spec.network.mode).to_owned(),
            forward: spec
                .network
                .forward
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            tap: spec.network.tap.clone().unwrap_or_default(),
            mac: spec
                .network
                .mac
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            mounts: spec
                .mounts
                .iter()
                .map(mount_line)
                .collect::<Vec<_>>()
                .join("\n"),
            // Configured inline user-data is offered back for editing: it is
            // the user's own configuration being shown back to them, which
            // §10.5 allows. The password is not, and is never rendered.
            user_data_inline: spec.cloud_init.user_data_inline.clone().unwrap_or_default(),
            ssh_authorized_keys: spec.cloud_init.ssh_authorized_keys.join("\n"),
            password: String::new(),
            ssh_pwauth: checkbox(spec.cloud_init.ssh_pwauth),
            provisioning: checkbox(spec.cloud_init.provisioning),
            provisioning_section: "1".to_owned(),
        }
    }

    /// Turns the submitted text into a sparse patch, collecting every field
    /// problem rather than stopping at the first: a form that reports one
    /// error at a time makes the user pay a round trip per mistake.
    fn to_patch(&self) -> Result<MachineSpecPatch, BTreeMap<String, String>> {
        let mut errors = BTreeMap::new();
        let mut patch = MachineSpecPatch::default();

        if self.name.trim().is_empty() {
            errors.insert("name".to_owned(), "a machine needs a name".to_owned());
        }

        match self.image.trim() {
            "" => {
                errors.insert(
                    "image".to_owned(),
                    "an image reference is required".to_owned(),
                );
            }
            // ImageRef accepts any non-empty string here; catalog, URL and
            // path forms are resolved and rejected by shared validation, so
            // the UI does not grow a second, divergent parser.
            value => patch.image = Some(ImageRef::new(value)),
        }

        if !self.cpus.trim().is_empty() {
            match self.cpus.trim().parse::<u8>() {
                Ok(cpus) if cpus >= 1 => patch.cpus = Some(cpus),
                _ => {
                    errors.insert("cpus".to_owned(), "cpus must be 1 through 255".to_owned());
                }
            }
        }

        parse_into(&self.memory, "memory", &mut errors, |value| {
            patch.memory = Some(value);
        });
        parse_into(&self.disk, "disk", &mut errors, |value| {
            patch.disk = Some(value);
        });

        if !self.user.trim().is_empty() {
            patch.user = Some(self.user.trim().to_owned());
        }

        let mut network = firestone_core::NetworkSpecPatch::default();
        match self.net_mode.trim() {
            "" => {}
            "passt" => network.mode = Some(NetMode::Passt),
            "tap" => network.mode = Some(NetMode::Tap),
            "none" => network.mode = Some(NetMode::None),
            other => {
                errors.insert(
                    "net_mode".to_owned(),
                    format!("{other} is not a network mode; use passt, tap or none"),
                );
            }
        }

        let forwards: Vec<&str> = self
            .forward
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect();
        if !forwards.is_empty() {
            let mut parsed = Vec::with_capacity(forwards.len());
            for value in forwards {
                match value.parse() {
                    Ok(forward) => parsed.push(forward),
                    Err(error) => {
                        errors.insert("forward".to_owned(), format!("{value}: {error}"));
                    }
                }
            }
            network.forward = Some(parsed);
        }

        if !self.tap.trim().is_empty() {
            network.tap = Some(self.tap.trim().to_owned());
        }
        if !self.mac.trim().is_empty() {
            match self.mac.trim().parse() {
                Ok(mac) => network.mac = Some(mac),
                Err(error) => {
                    errors.insert("mac".to_owned(), format!("{}: {error}", self.mac.trim()));
                }
            }
        }
        patch.network = Some(network);

        // Every bad row is reported, not just the first: a form that answers
        // one mount at a time costs a round trip per typo.
        let mut mounts = Vec::new();
        let mut mount_errors = Vec::new();
        let mut row = 0usize;
        for line in self.mounts.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            row += 1;
            match crate::cli::parse_mount(line) {
                Ok(mount) => mounts.push(mount),
                Err(error) => mount_errors.push(format!("row {row}: {error}")),
            }
        }
        if mount_errors.is_empty() {
            if !mounts.is_empty() {
                patch.mounts = Some(mounts);
            }
        } else {
            errors.insert("mounts".to_owned(), mount_errors.join("; "));
        }

        patch.cloud_init = self.cloud_init_patch();

        if errors.is_empty() {
            Ok(patch)
        } else {
            Err(errors)
        }
    }

    /// The provisioning section as a sparse cloud-init patch, or `None` when
    /// the section contributed nothing.
    ///
    /// Nothing here validates. The 32 KiB inline cap, the OpenSSH key grammar
    /// and the password rules all live in shared validation, which the CLI and
    /// REST already answer to; a second parser here would be a second contract.
    /// The dialog's byte counter is a courtesy, not a check.
    fn cloud_init_patch(&self) -> Option<firestone_core::CloudInitSpecPatch> {
        let mut cloud_init = firestone_core::CloudInitSpecPatch::default();

        // A browser submits a textarea with CRLF line endings. Cloud-init
        // user-data is YAML that will be written to a file and read by the
        // guest, so the carriage returns are normalised out rather than
        // shipped into the seed image.
        let user_data = self.user_data_inline.replace("\r\n", "\n");
        if !user_data.trim().is_empty() {
            cloud_init.user_data_inline = Some(user_data);
        }

        let keys: Vec<String> = self
            .ssh_authorized_keys
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !keys.is_empty() {
            cloud_init.ssh_authorized_keys = Some(keys);
        }

        // Not trimmed: a password is bytes the user chose, and trimming one
        // would set a credential the guest will never accept.
        if !self.password.is_empty() {
            cloud_init.password = Some(self.password.clone());
        }

        // The two checkboxes speak only when the section that owns them was
        // rendered; see `provisioning_section`.
        if !self.provisioning_section.trim().is_empty() {
            cloud_init.ssh_pwauth = Some(is_checked(&self.ssh_pwauth));
            cloud_init.provisioning = Some(is_checked(&self.provisioning));
        }

        if cloud_init == firestone_core::CloudInitSpecPatch::default() {
            None
        } else {
            Some(cloud_init)
        }
    }
}

/// Renders a boolean as the value a checked checkbox submits, so the form
/// round-trips a spec through the same string the browser would have sent.
fn checkbox(value: bool) -> String {
    if value {
        "on".to_owned()
    } else {
        String::new()
    }
}

/// A checkbox submits its `value` when ticked and nothing at all when not.
fn is_checked(value: &str) -> bool {
    !value.trim().is_empty()
}

/// Renders one mount back into the `HOST:GUEST[:ro]` grammar it was parsed
/// from, so the form round-trips through the same string the CLI accepts.
fn mount_line(mount: &firestone_core::MountSpec) -> String {
    format!(
        "{}:{}{}",
        mount.host.display(),
        mount.guest.display(),
        if mount.readonly { ":ro" } else { "" }
    )
}

fn parse_into<T, F>(raw: &str, field: &str, errors: &mut BTreeMap<String, String>, mut apply: F)
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
    F: FnMut(T),
{
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    match trimmed.parse::<T>() {
        Ok(value) => apply(value),
        Err(error) => {
            errors.insert(field.to_owned(), error.to_string());
        }
    }
}

// ------------------------------------------------------------------- edit --

/// The machine edit dialog, prefilled from the machine's own spec.
///
/// A read, like every other `/ui` fragment. The dialog itself writes through
/// `PATCH /v1/machines/{name}` and `POST /v1/machines/{name}/resize` from the
/// browser, so the UI still offers exactly one mutation surface (§16.5).
///
/// The prefill is deliberately the same [`CreateForm`] projection the create
/// dialog renders: identical field names, identical grammars, one place where
/// a `MachineSpec` becomes editable text. `original` carries that same
/// projection as JSON so the browser can send a *sparse* patch — only what the
/// operator actually changed — rather than a full spec rewrite.
pub async fn edit_form(State(state): State<UiState>, Path(name): Path<String>) -> Response {
    let result = async {
        let view = show_machine(&state, &name).await?;
        let machine = MachineDetail::new(&name, &view, "");
        let form = CreateForm::from_defaults(&view.spec);
        let original = serde_json::to_string(&form).unwrap_or_else(|_| "{}".to_owned());
        render(
            "ui/edit.html",
            context! {
                machine => machine,
                form => form,
                original => original,
                errors => BTreeMap::<String, String>::new(),
                memory => SizeField::split(&form.memory),
                disk => SizeField::split(&form.disk),
                net_modes => ["passt", "tap", "none"],
                mount_tags => view.spec.mounts.iter().any(|mount| mount.tag.is_some()),
            },
        )
    }
    .await;
    fragment(result)
}

// ------------------------------------------------------------------- tabs --

/// Resolves a requested tab name to the one tab that will actually render.
///
/// One function decides this, so the strip and the panel can never disagree:
/// an unknown name is the spec tab in both, which is what keeps exactly one
/// tab marked active for any input a URL can carry.
///
/// `console` is whether this machine offers the Console tab at all. Both
/// terminal transports need a running machine, so a stopped one carries no
/// Console tab and `?tab=console` resolves to the spec tab rather than to a
/// panel that could only apologise.
fn resolve_tab(tab: &str, console: bool) -> &'static str {
    match tab {
        "console" if console => "console",
        "logs" => "logs",
        "vmconfig" => "vmconfig",
        "snapshots" => "snapshots",
        _ => "spec",
    }
}

fn tab_template(tab: &str, console: bool) -> &'static str {
    match resolve_tab(tab, console) {
        "console" => "ui/tab_console.html",
        "logs" => "ui/tab_logs.html",
        "vmconfig" => "ui/tab_vmconfig.html",
        "snapshots" => "ui/tab_snapshots.html",
        _ => "ui/tab_spec.html",
    }
}

#[derive(Debug, serde::Serialize)]
struct Tab {
    label: &'static str,
    active: bool,
    href: String,
    fragment: String,
}

fn tabs(name: &str, active: &str, console: bool) -> Vec<Tab> {
    let selected = resolve_tab(active, console);
    [
        ("spec", "Spec"),
        ("console", "Console"),
        ("logs", "Logs"),
        ("snapshots", "Snapshots"),
        ("vmconfig", "VM Config"),
    ]
    .into_iter()
    .filter(|(id, _)| *id != "console" || console)
    .map(|(id, label)| Tab {
        label,
        active: id == selected,
        href: format!("/machines/{}?tab={id}", urlencode(name)),
        fragment: format!("/ui/machines/{}/tab/{id}", urlencode(name)),
    })
    .collect()
}

#[derive(Debug, serde::Serialize)]
struct SourceChip {
    label: &'static str,
    active: bool,
    href: String,
    fragment: String,
}

async fn tab_context(
    state: &UiState,
    name: &str,
    machine: &MachineDetail,
    tab: &str,
    source: &str,
    follow: Option<bool>,
) -> Result<BTreeMap<String, Value>, FirestoneError> {
    let mut ctx = BTreeMap::new();
    match tab {
        "logs" => {
            let parsed = source.parse::<LogSource>().map_err(|error| {
                FirestoneError::new(ErrorKind::Usage, error.to_string())
                    .with_hint("choose console, vmm, shim, or passt")
            })?;
            // Following only makes sense while something is writing. Default
            // it on for a running machine and off otherwise, so opening the
            // tab on a stopped machine does not leave a dead indicator
            // pulsing.
            let follow = follow.unwrap_or(machine.is_running) && machine.is_running;
            // Opening the tab on a machine that has never started must not
            // raise anything. A read that comes back empty and a read the
            // dispatcher refuses render the same quiet empty state, so this
            // path never produces a status a browser would toast (SPEC §16.5).
            let text = read_logs(state, name, parsed).await.unwrap_or_default();

            ctx.insert("logs".to_owned(), Value::from(text));
            ctx.insert("source".to_owned(), Value::from(source.to_owned()));
            ctx.insert("follow".to_owned(), Value::from(follow));
            ctx.insert("can_follow".to_owned(), Value::from(machine.is_running));
            ctx.insert(
                "sources".to_owned(),
                Value::from_serialize(source_chips(name, source)),
            );
        }
        "vmconfig" => {
            ctx.insert(
                "vmconfig".to_owned(),
                Value::from(read_vmconfig(state, name).await),
            );
        }
        // A read like every other tab: `Action::SnapshotList` is the same
        // action `GET /v1/machines/{name}/snapshots` dispatches, and every
        // write the tab offers goes from the browser to those `/v1` routes.
        "snapshots" => {
            let list = list_snapshots(state, name).await?;
            ctx.insert(
                "snapshots".to_owned(),
                Value::from_serialize(snapshot_rows(&list.snapshots)),
            );
            // §23 tiers: what a create takes here depends on the machine's own
            // state, and the button says which one rather than implying both.
            ctx.insert(
                "snapshot_kind".to_owned(),
                Value::from(if machine.is_running { "warm" } else { "cold" }),
            );
            // A machine that is starting, stopping or failed has no coherent
            // disk to copy and is refused with `conflict` (§23), so the button
            // is withheld rather than offered as a request that cannot work.
            ctx.insert(
                "can_snapshot".to_owned(),
                Value::from(matches!(
                    machine.status.as_str(),
                    "running" | "stopped" | "created"
                )),
            );
        }
        _ => {}
    }
    Ok(ctx)
}

fn source_chips(name: &str, active: &str) -> Vec<SourceChip> {
    LOG_SOURCES
        .iter()
        .map(|(id, label)| SourceChip {
            label,
            active: *id == active,
            href: format!("/machines/{}?tab=logs&source={id}", urlencode(name)),
            fragment: format!("/ui/machines/{}/tab/logs?source={id}", urlencode(name)),
        })
        .collect()
}

// ------------------------------------------------------------ action reads --

async fn list_machines(state: &UiState) -> Result<Vec<MachineSummary>, FirestoneError> {
    decode(
        api::dispatch_payload(&state.dispatcher, Action::List, "list").await?,
        "machine list",
    )
}

async fn list_images(state: &UiState) -> Result<Vec<CachedImage>, FirestoneError> {
    decode(
        api::dispatch_payload(&state.dispatcher, Action::ImageList, "images-ls").await?,
        "image list",
    )
}

/// The same read `GET /v1/machines/{name}/snapshots` performs (§23.1).
async fn list_snapshots(state: &UiState, name: &str) -> Result<SnapshotListResult, FirestoneError> {
    decode(
        api::dispatch_payload(
            &state.dispatcher,
            Action::SnapshotList {
                name: name.to_owned(),
            },
            "snapshot-list",
        )
        .await?,
        "snapshot list",
    )
}

async fn list_catalog(state: &UiState) -> Result<Vec<CatalogEntrySummary>, FirestoneError> {
    decode(
        api::dispatch_payload(&state.dispatcher, Action::CatalogList, "catalog").await?,
        "catalog",
    )
}

async fn show_machine(state: &UiState, name: &str) -> Result<MachineView, FirestoneError> {
    decode(
        api::dispatch_payload(
            &state.dispatcher,
            Action::Show {
                name: name.to_owned(),
                vmconfig: false,
            },
            "show",
        )
        .await?,
        "machine",
    )
}

/// This host's architecture, for the spec tab's `arch` row.
///
/// A failed version read is not a reason to fail a machine page, so the answer
/// degrades to an empty string and the row says only `host default`.
async fn host_arch(state: &UiState) -> String {
    version_info(state)
        .await
        .map(|version| version.architecture)
        .unwrap_or_default()
}

async fn version_info(state: &UiState) -> Result<VersionInfo, FirestoneError> {
    let result: VersionResult = decode(
        api::dispatch_payload(&state.dispatcher, Action::Version, "version").await?,
        "version",
    )?;
    Ok(VersionInfo::from(&result))
}

async fn read_logs(
    state: &UiState,
    name: &str,
    source: LogSource,
) -> Result<String, FirestoneError> {
    let text = api::dispatch_output(
        &state.dispatcher,
        Action::Logs {
            name: name.to_owned(),
            source,
            lines: LOG_LINES,
            follow: false,
        },
        "logs",
    )
    .await?;
    Ok(crate::api::sanitized_output(&text))
}

/// A machine that has never started has no published config. That is an
/// ordinary state, not an error, so the tab renders its empty state and the
/// refusal is dropped: the message names a path inside the machine directory,
/// which tells the reader nothing they can act on and reads as a fault.
async fn read_vmconfig(state: &UiState, name: &str) -> String {
    api::dispatch_payload(
        &state.dispatcher,
        Action::Show {
            name: name.to_owned(),
            vmconfig: true,
        },
        "show-vmconfig",
    )
    .await
    .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
    .unwrap_or_default()
}

async fn catalog_cards_data(state: &UiState) -> Result<Vec<CatalogCard>, FirestoneError> {
    let entries = list_catalog(state).await?;
    let images = list_images(state).await?;
    let cached = cached_by_reference(&images);
    let ids = cached_ids(&images);

    Ok(entries
        .iter()
        .map(|entry| {
            let size = cached.get(&entry.reference).copied();
            CatalogCard {
                reference: entry.reference.clone(),
                cached: size.is_some(),
                cached_id: ids.get(&entry.reference).cloned().unwrap_or_default(),
                size: size.map(format_bytes).unwrap_or_default(),
                chips: chips(entry),
            }
        })
        .collect())
}

fn chips(entry: &CatalogEntrySummary) -> Vec<String> {
    let mut chips: Vec<String> = entry
        .aliases
        .iter()
        .map(|alias| format!("alias: {alias}"))
        .collect();
    for architecture in &entry.architectures {
        chips.push(format!(
            "{} · {}",
            architecture.architecture,
            serde_json::to_value(architecture.firmware)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "edk2".to_owned())
        ));
    }
    chips
}

fn cached_by_reference(images: &[CachedImage]) -> BTreeMap<String, u64> {
    let mut sizes = BTreeMap::new();
    for image in images {
        let entry = sizes
            .entry(image.metadata.source_ref.clone())
            .or_insert(0u64);
        *entry = (*entry).max(image.metadata.size);
    }
    sizes
}

fn cached_ids(images: &[CachedImage]) -> BTreeMap<String, String> {
    images
        .iter()
        .map(|image| (image.metadata.source_ref.clone(), image.metadata.id.clone()))
        .collect()
}

fn cached_image_rows(images: &[CachedImage]) -> Vec<CachedImageRow> {
    images
        .iter()
        .take(6)
        .map(|image| CachedImageRow {
            reference: image.metadata.source_ref.clone(),
            id: image.metadata.id.clone(),
            size: format_bytes(image.metadata.size),
        })
        .collect()
}

#[derive(Debug, serde::Serialize)]
struct CachedImageRow {
    reference: String,
    id: String,
    size: String,
}

fn filtered_rows(machines: &[MachineSummary], query: &str) -> Vec<MachineRow> {
    let needle = query.trim().to_lowercase();
    machines
        .iter()
        .filter(|machine| {
            needle.is_empty()
                || machine.name.to_lowercase().contains(&needle)
                || machine.image.to_lowercase().contains(&needle)
                || machine.status.to_lowercase().contains(&needle)
        })
        .map(MachineRow::from)
        .collect()
}

fn decode<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    what: &'static str,
) -> Result<T, FirestoneError> {
    serde_json::from_value(value).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("the {what} result did not match the shared contract: {error}"),
        )
        .with_hint("this is a Firestone defect; report it with the version string")
    })
}

// ------------------------------------------------------------- responses --

/// Wraps a rendered body in the full document, unless htmx asked for the
/// fragment alone. One handler serves both, so every screen is a real URL:
/// deep links, reload and the back button all work without a second route.
async fn page(
    state: &UiState,
    headers: HeaderMap,
    nav: &str,
    title: &str,
    body: String,
) -> Response {
    if is_htmx_navigation(&headers) {
        return html(StatusCode::OK, body);
    }

    // The frame degrades rather than fails. If version, doctor or the machine
    // list is unavailable — a misconfigured host, a doctor that cannot run —
    // the reader still gets navigable chrome around whatever did render,
    // instead of an unstyled error with no way forward.
    let version = version_info(state)
        .await
        .unwrap_or_else(|_| VersionInfo::unknown());
    let host = state
        .doctor()
        .await
        .map_or_else(|_| HostInfo::unknown(), |report| HostInfo::from(&report));
    let machine_count = list_machines(state)
        .await
        .map_or(0, |machines| machines.len());

    let shell = render(
        "ui/shell.html",
        context! {
            title => title,
            build => state.build.as_str(),
            nav => nav,
            version => version,
            host => host,
            machine_count => machine_count,
            listener => state.listener_label.as_str(),
            content => Value::from_safe_string(body),
        },
    );

    match shell {
        Ok(document) => html(StatusCode::OK, document),
        Err(error) => html(
            status_for(&error),
            format!(
                "<!doctype html><meta charset=\"utf-8\"><title>Firestone</title><pre>{}</pre>",
                html_escape(&error.info().message)
            ),
        ),
    }
}

/// htmx sets HX-Request on every ajax navigation, but also replays history
/// entries with HX-History-Restore-Request, which needs the whole document.
fn is_htmx_navigation(headers: &HeaderMap) -> bool {
    headers.get("HX-Request").is_some() && headers.get("HX-History-Restore-Request").is_none()
}

async fn failure(state: &UiState, headers: HeaderMap, error: FirestoneError) -> Response {
    let info = error.info();
    let status = status_for(&error);
    let body = render(
        "ui/error.html",
        context! {
            title => match status {
                StatusCode::NOT_FOUND => "Not found",
                _ => "Something went wrong",
            },
            message => info.message.clone(),
            hint => info.hint.clone(),
        },
    );

    match body {
        Ok(body) => {
            let mut response = page(state, headers, "overview", "Error", body).await;
            *response.status_mut() = status;
            response
        }
        Err(_) => html(
            status,
            "<p>Firestone could not render this page.</p>".to_owned(),
        ),
    }
}

/// The UI's own 404, rendered in the shell so the reader can navigate out.
pub(crate) async fn render_not_found(state: &UiState, headers: HeaderMap) -> Response {
    failure(
        state,
        headers,
        FirestoneError::new(ErrorKind::NotFound, "that page does not exist")
            .with_hint("use the sidebar to reach Overview, Machines or Catalog"),
    )
    .await
}

fn fragment(result: Result<String, FirestoneError>) -> Response {
    match result {
        Ok(body) => html(StatusCode::OK, body),
        Err(error) => fragment_error(error),
    }
}

/// A failed fragment answers with its real status and a compact notice. The
/// browser's htmx:responseError handler turns that into a toast, so a
/// background poll that starts failing is visible rather than silent.
fn fragment_error(error: FirestoneError) -> Response {
    let info = error.info();
    let status = status_for(&error);
    let mut body = format!(
        "<div class=\"fs-inline-notice fs-inline-notice--fail\" role=\"alert\"><span>{}</span>",
        html_escape(&info.message)
    );
    if let Some(hint) = &info.hint {
        body.push_str(&format!(
            "<span class=\"fs-inline-notice__hint\">{}</span>",
            html_escape(hint)
        ));
    }
    body.push_str("</div>");
    html(status, body)
}

fn html(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

fn status_for(error: &FirestoneError) -> StatusCode {
    match error.kind() {
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::Usage | ErrorKind::InvalidSpec => StatusCode::BAD_REQUEST,
        ErrorKind::Conflict | ErrorKind::AlreadyRunning | ErrorKind::Busy => StatusCode::CONFLICT,
        ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorKind::Dependency => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn subtitle(version: &VersionInfo) -> String {
    format!("Daemonless · {}", version.identity)
}

// ---------------------------------------------------------------- queries --

#[derive(Debug, Default, Deserialize)]
pub struct FilterQuery {
    pub q: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TabQuery {
    pub tab: Option<String>,
    pub source: Option<String>,
    pub follow: Option<bool>,
    /// `embed=1` on the terminal page. Read as a string and compared to one
    /// value, so every other spelling is the standalone page: the flag decides
    /// which Content-Security-Policy the response carries, and a lenient parse
    /// there is a way to reach the framable variant by accident.
    pub embed: Option<String>,
}

impl TabQuery {
    fn is_embed(&self) -> bool {
        self.embed.as_deref() == Some("1")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use firestone_core::{ErrorKind, FirestoneError};

    use super::{CreateForm, field_errors, html_escape, tab_template, tabs, urlencode};

    #[test]
    fn urlencode_escapes_every_reserved_path_byte() {
        assert_eq!(urlencode("web"), "web");
        assert_eq!(urlencode("staging-db"), "staging-db");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("../etc"), "..%2Fetc");
    }

    #[test]
    fn html_escape_neutralizes_markup() {
        assert_eq!(
            html_escape("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"
        );
        assert_eq!(html_escape("a & b"), "a &amp; b");
    }

    #[test]
    fn tab_template_falls_back_to_spec_for_unknown_names() {
        assert_eq!(tab_template("logs", true), "ui/tab_logs.html");
        assert_eq!(tab_template("vmconfig", true), "ui/tab_vmconfig.html");
        assert_eq!(tab_template("spec", true), "ui/tab_spec.html");
        assert_eq!(tab_template("../../etc/passwd", true), "ui/tab_spec.html");
        assert_eq!(tab_template("console", true), "ui/tab_console.html");
        // Both transports need a running machine, so a stopped one has no
        // console panel to resolve to.
        assert_eq!(tab_template("console", false), "ui/tab_spec.html");
    }

    #[test]
    fn tabs_mark_exactly_one_active_for_any_input() {
        for console in [true, false] {
            for requested in ["spec", "console", "logs", "vmconfig", "nonsense"] {
                let active = tabs("web", requested, console)
                    .iter()
                    .filter(|tab| tab.active)
                    .count();
                assert_eq!(
                    active, 1,
                    "tab {requested} did not select exactly one (console: {console})"
                );
            }
        }
        // The Console tab is offered only where it can attach.
        assert!(
            tabs("web", "spec", true)
                .iter()
                .any(|tab| tab.label == "Console")
        );
        assert!(
            !tabs("web", "spec", false)
                .iter()
                .any(|tab| tab.label == "Console")
        );
    }

    #[test]
    fn create_form_collects_every_field_problem_at_once() {
        let form = CreateForm {
            name: String::new(),
            image: String::new(),
            cpus: "0".to_owned(),
            memory: "not-a-size".to_owned(),
            disk: "20G".to_owned(),
            user: "ubuntu".to_owned(),
            net_mode: "passt".to_owned(),
            forward: "nonsense".to_owned(),
            tap: String::new(),
            mac: "not-a-mac".to_owned(),
            mounts: "/srv:/work\nbroken\nalso-broken".to_owned(),
            ..CreateForm::default()
        };
        let errors = form.to_patch().expect_err("the form must be rejected");
        for field in [
            "name", "image", "cpus", "memory", "forward", "mac", "mounts",
        ] {
            assert!(errors.contains_key(field), "missing error for {field}");
        }
        assert!(!errors.contains_key("disk"), "disk was valid");
        let mounts = errors.get("mounts").expect("a mount error");
        assert!(
            mounts.contains("row 2") && mounts.contains("row 3"),
            "every bad mount row must be reported, got {mounts}"
        );
    }

    #[test]
    fn create_form_accepts_a_complete_valid_submission() {
        let form = CreateForm {
            name: "web".to_owned(),
            image: "ubuntu:24.04".to_owned(),
            cpus: "4".to_owned(),
            memory: "8G".to_owned(),
            disk: "40G".to_owned(),
            user: "ubuntu".to_owned(),
            net_mode: "tap".to_owned(),
            forward: "127.0.0.1:8080:80, udp:5353:5353".to_owned(),
            tap: "tap0".to_owned(),
            mac: "52:54:00:9a:1f:c3".to_owned(),
            mounts: "/srv/project:/work:ro\n\n~/code:/code\n".to_owned(),
            ..CreateForm::default()
        };
        let patch = form.to_patch().expect("the form must be accepted");
        assert_eq!(patch.cpus, Some(4));
        let network = patch.network.as_ref().expect("a network patch");
        assert_eq!(network.forward.as_ref().map(Vec::len), Some(2));
        assert_eq!(network.tap.as_deref(), Some("tap0"));
        assert_eq!(
            network.mac.map(|mac| mac.to_string()).as_deref(),
            Some("52:54:00:9a:1f:c3")
        );

        let mounts = patch.mounts.as_ref().expect("a mount patch");
        assert_eq!(mounts.len(), 2, "a blank line is not a mount");
        assert_eq!(mounts[0].host, std::path::PathBuf::from("/srv/project"));
        assert_eq!(mounts[0].guest, std::path::PathBuf::from("/work"));
        assert!(mounts[0].readonly);
        assert!(mounts[0].tag.is_none());
        assert!(!mounts[1].readonly);
    }

    #[test]
    fn create_form_round_trips_composed_fields_through_the_same_grammar() {
        // What the dialog composes must parse back into what it composed from,
        // or the friendly controls and the field the server reads have drifted.
        let form = CreateForm {
            name: "web".to_owned(),
            image: "ubuntu:24.04".to_owned(),
            forward: "8080:80, udp:127.0.0.1:5353:5353, [::1]:9090:90".to_owned(),
            mounts: "/srv:/work:ro\n/tmp/x:/x".to_owned(),
            memory: "1536M".to_owned(),
            disk: "40G".to_owned(),
            ..CreateForm::default()
        };
        let patch = form.to_patch().expect("the form must be accepted");

        let forwards = patch
            .network
            .as_ref()
            .and_then(|network| network.forward.clone())
            .expect("a forward patch");
        assert_eq!(
            forwards
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            "8080:80, udp:127.0.0.1:5353:5353, [::1]:9090:90"
        );

        let mounts = patch.mounts.as_ref().expect("a mount patch");
        assert_eq!(
            mounts.iter().map(super::mount_line).collect::<Vec<_>>(),
            ["/srv:/work:ro", "/tmp/x:/x"]
        );

        assert_eq!(
            patch.memory.map(|size| size.to_string()).as_deref(),
            Some("1536M")
        );
        assert_eq!(
            patch.disk.map(|size| size.to_string()).as_deref(),
            Some("40G")
        );
    }

    #[test]
    fn size_field_splits_a_canonical_size_into_an_amount_and_a_unit() {
        for (raw, amount, unit) in [
            ("8G", "8", "G"),
            ("512M", "512", "M"),
            (" 2G ", "2", "G"),
            // A bare integer is MiB in the ByteSize grammar; the select must
            // say so rather than quietly promote it to GiB.
            ("2048", "2048", "M"),
            ("", "", "G"),
            // An unparseable value is handed back verbatim, with no amount to
            // recompose it from.
            ("nonsense", "", "M"),
        ] {
            let field = super::SizeField::split(raw);
            assert_eq!(field.amount, amount, "{raw} amount");
            assert_eq!(field.unit, unit, "{raw} unit");
            assert_eq!(field.raw, raw.trim(), "{raw} raw");
        }
    }

    #[test]
    fn create_form_leaves_blank_optional_fields_unset() {
        let form = CreateForm {
            name: "web".to_owned(),
            image: "ubuntu:24.04".to_owned(),
            ..CreateForm::default()
        };
        let patch = form.to_patch().expect("blanks inherit configured defaults");
        assert_eq!(patch.cpus, None);
        assert_eq!(patch.memory, None);
        assert_eq!(patch.disk, None);
        assert_eq!(patch.user, None);
    }

    #[test]
    fn field_errors_route_a_message_to_the_field_it_names() {
        let cases: [(ErrorKind, &str, &str); 4] = [
            (
                ErrorKind::AlreadyExists,
                "machine web already exists",
                "name",
            ),
            (
                ErrorKind::InvalidSpec,
                "image reference is not valid",
                "image",
            ),
            (
                ErrorKind::InvalidSpec,
                "port forward is malformed",
                "forward",
            ),
            (ErrorKind::Generic, "the host is on fire", "form"),
        ];
        for (kind, message, expected) in cases {
            let errors: BTreeMap<String, String> =
                field_errors(&FirestoneError::new(kind, message));
            assert!(
                errors.contains_key(expected),
                "{message:?} should attach to {expected}"
            );
        }
    }

    #[test]
    fn create_form_round_trips_the_provisioning_section_into_the_patch() {
        let form = CreateForm {
            name: "web".to_owned(),
            image: "ubuntu:24.04".to_owned(),
            // A browser posts a textarea with CRLF endings; the seed must not
            // inherit them.
            user_data_inline: "#cloud-config\r\npackages:\r\n  - jq\r\n".to_owned(),
            ssh_authorized_keys: "  ssh-ed25519 AAAAC3 one@host  \n\n\tssh-rsa AAAAB3 two@host\n"
                .to_owned(),
            password: "  hunter2  ".to_owned(),
            ssh_pwauth: "on".to_owned(),
            provisioning: "on".to_owned(),
            provisioning_section: "1".to_owned(),
            ..CreateForm::default()
        };

        let patch = form.to_patch().expect("the form must be accepted");
        let cloud_init = patch.cloud_init.as_ref().expect("a cloud-init patch");

        assert_eq!(
            cloud_init.user_data_inline.as_deref(),
            Some("#cloud-config\npackages:\n  - jq\n"),
            "carriage returns must not reach the seed"
        );
        let keys = cloud_init
            .ssh_authorized_keys
            .as_ref()
            .expect("an authorized-key patch");
        assert_eq!(
            keys,
            &["ssh-ed25519 AAAAC3 one@host", "ssh-rsa AAAAB3 two@host"],
            "one key per line, trimmed, blank lines dropped"
        );
        // A password is bytes the user chose: trimming one would set a
        // credential the guest will never accept.
        assert_eq!(cloud_init.password.as_deref(), Some("  hunter2  "));
        assert_eq!(cloud_init.ssh_pwauth, Some(true));
        assert_eq!(cloud_init.provisioning, Some(true));
    }

    #[test]
    fn create_form_cleared_checkboxes_turn_provisioning_off() {
        // An unticked checkbox submits nothing at all, so the section marker is
        // what makes the absence mean false rather than "not offered".
        let form = CreateForm {
            name: "web".to_owned(),
            image: "ubuntu:24.04".to_owned(),
            provisioning_section: "1".to_owned(),
            ..CreateForm::default()
        };
        let patch = form.to_patch().expect("the form must be accepted");
        let cloud_init = patch.cloud_init.as_ref().expect("a cloud-init patch");
        assert_eq!(cloud_init.provisioning, Some(false));
        assert_eq!(cloud_init.ssh_pwauth, Some(false));
        assert!(cloud_init.user_data_inline.is_none());
        assert!(cloud_init.password.is_none());
    }

    #[test]
    fn create_form_without_the_provisioning_section_leaves_cloud_init_untouched() {
        // A submission from a form that never rendered the section must not
        // flip provisioning off behind the user's back.
        let form = CreateForm {
            name: "web".to_owned(),
            image: "ubuntu:24.04".to_owned(),
            ..CreateForm::default()
        };
        let patch = form.to_patch().expect("the form must be accepted");
        assert!(
            patch.cloud_init.is_none(),
            "an unrendered section must contribute no patch"
        );
    }

    #[test]
    fn create_form_debug_never_prints_a_password_or_inline_user_data() {
        let form = CreateForm {
            user_data_inline: "#cloud-config\nruncmd: [id]".to_owned(),
            password: "hunter2".to_owned(),
            ..CreateForm::default()
        };
        let rendered = format!("{form:?}");
        assert!(!rendered.contains("hunter2"), "a password was formatted");
        assert!(
            !rendered.contains("runcmd"),
            "inline user-data was formatted"
        );
        assert!(rendered.contains("<redacted, 7 bytes>"));
    }

    #[test]
    fn field_errors_route_a_cloud_init_message_to_its_own_field() {
        let cases: [(&str, &str); 4] = [
            (
                "cloud_init.user_data_inline is 40000 bytes and exceeds 32 KiB",
                "user_data_inline",
            ),
            (
                "cloud_init.ssh_authorized_keys[1] is not an OpenSSH public key",
                "ssh_authorized_keys",
            ),
            ("cloud_init.password must not be empty", "password"),
            ("provisioning is disabled for this machine", "provisioning"),
        ];
        for (message, expected) in cases {
            let errors: BTreeMap<String, String> =
                field_errors(&FirestoneError::new(ErrorKind::InvalidSpec, message));
            assert!(
                errors.contains_key(expected),
                "{message:?} should attach to {expected}, got {errors:?}"
            );
            // "user_data" contains "user": the guest-user field must not
            // swallow a cloud-init message.
            assert!(
                !errors.contains_key("user") || expected == "user",
                "{message:?} leaked onto the guest-user field"
            );
        }
    }
}
