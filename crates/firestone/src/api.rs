use std::{
    convert::Infallible,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{FromRequestParts, Path, State},
    http::{
        HeaderMap, HeaderValue, Request, StatusCode, Uri,
        header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE},
        request::Parts,
    },
    middleware::{self, Next},
    response::Response,
    routing::{MethodRouter, delete, get, post},
};
use firestone_core::{
    Action, ByteSize, Dispatcher, ErrorInfo, ErrorKind, Event, EventSink, FirestoneError,
    GlobalConfig, ImageRef, LogSource, MachineSpec, MachineSpecPatch, Paths,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};

use crate::render::{TerminalSanitizer, sanitize_terminal_output};

mod ws;

const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
const MAX_TIMEOUT_SECONDS: u64 = 60 * 60;
const MAX_LOG_LINES: u32 = 100_000;
const DEFAULT_LOG_LINES: u32 = 200;
const EVENT_BUFFER_CAPACITY: usize = 16;
const JSON_CONTENT_TYPE: &str = "application/json";
const NDJSON_CONTENT_TYPE: &str = "application/x-ndjson";
const TEXT_CONTENT_TYPE: &str = "text/plain";
const INTERNAL_ERROR_JSON: &[u8] = br#"{"error":{"kind":"generic","message":"the REST adapter could not serialize a response","hint":"retry the request; if it fails again, report a Firestone bug"}}"#;
const INTERNAL_ERROR_NDJSON: &[u8] = br#"{"error":{"kind":"generic","message":"the REST adapter could not serialize a response","hint":"retry the request; if it fails again, report a Firestone bug"}}
"#;

#[derive(Clone)]
pub struct RestConfig {
    create_defaults: MachineSpecPatch,
    start_timeout: Duration,
    stop_timeout: Duration,
}

impl From<&GlobalConfig> for RestConfig {
    fn from(config: &GlobalConfig) -> Self {
        Self {
            create_defaults: config.defaults.clone(),
            start_timeout: config.start.timeout.get(),
            stop_timeout: config.stop.timeout.get(),
        }
    }
}

#[derive(Clone)]
struct ApiState {
    dispatcher: Arc<dyn Dispatcher>,
    config: RestConfig,
    /// Resolved for the WebSocket terminal transports, which reach the
    /// machine's console socket and SSH identity directly rather than
    /// through an action.
    paths: Paths,
}

struct RestRoute {
    path: &'static str,
    #[cfg(test)]
    authored_methods: &'static [&'static str],
    method_router: fn() -> MethodRouter<ApiState>,
}

#[cfg(test)]
macro_rules! rest_method_name {
    (get) => {
        "GET"
    };
    (post) => {
        "POST"
    };
    (put) => {
        "PUT"
    };
    (patch) => {
        "PATCH"
    };
    (delete) => {
        "DELETE"
    };
    (head) => {
        "HEAD"
    };
    (options) => {
        "OPTIONS"
    };
    (trace) => {
        "TRACE"
    };
}

macro_rules! define_rest_routes {
    ($(
        $path:literal => $first_method:ident($first_handler:path)
            $(.$method:ident($handler:path))*;
    )+) => {
        const REST_ROUTES: &[RestRoute] = &[
            $(
                RestRoute {
                    path: $path,
                    #[cfg(test)]
                    authored_methods: &[
                        rest_method_name!($first_method)
                        $(, rest_method_name!($method))*
                    ],
                    method_router: || {
                        $first_method($first_handler)
                            $(.$method($handler))*
                    },
                },
            )+
        ];
    };
}

define_rest_routes! {
    "/v1/version" => get(version);
    "/v1/doctor" => get(doctor);
    "/v1/machines" => get(machines).post(create_machine);
    "/v1/catalog" => get(catalog);
    "/v1/machines/{name}" => get(machine)
        .put(set_machine_spec)
        .patch(patch_machine_spec)
        .delete(remove_machine);
    "/v1/machines/{name}/start" => post(start_machine);
    "/v1/machines/{name}/stop" => post(stop_machine);
    "/v1/machines/{name}/restart" => post(restart_machine);
    "/v1/machines/{name}/resize" => post(resize_machine);
    "/v1/machines/{name}/logs" => get(machine_logs);
    "/v1/machines/{name}/vmconfig" => get(machine_vmconfig);
    "/v1/machines/{name}/metrics" => get(machine_metrics);
    "/v1/machines/{name}/console/ws" => get(ws::console);
    "/v1/machines/{name}/shell/ws" => get(ws::shell);
    "/v1/images" => get(images);
    "/v1/images/pull" => post(pull_image);
    "/v1/images/prune" => post(prune_images);
    "/v1/images/{id}" => delete(remove_image);
    "/v1/machines/{name}/clone" => post(clone_machine);
    "/v1/machines/{name}/snapshots" => get(machine_snapshots).post(create_machine_snapshot);
    "/v1/machines/{name}/snapshots/{snapshot}/restore" => post(restore_machine_snapshot);
    "/v1/machines/{name}/snapshots/{snapshot}" => delete(remove_machine_snapshot);
    "/v1/system/prune" => post(prune_system);
}

/// Builds the complete v1 REST router over the shared action dispatcher.
pub fn router(dispatcher: Arc<dyn Dispatcher>, config: &GlobalConfig, paths: &Paths) -> Router {
    let state = ApiState {
        dispatcher,
        config: RestConfig::from(config),
        paths: paths.clone(),
    };

    REST_ROUTES
        .iter()
        .fold(Router::new(), |router, route| {
            router.route(route.path, (route.method_router)())
        })
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
        .layer(middleware::from_fn(validate_request_uri))
}

#[derive(Debug)]
struct ApiPath(String);

impl<S> FromRequestParts<S> for ApiPath
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<String>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| {
                error_response(
                    FirestoneError::new(ErrorKind::Usage, "the route path is not valid UTF-8")
                        .with_hint("percent-encode one UTF-8 machine name or image id"),
                )
            })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateMachineBody {
    name: String,
    spec: MachineSpecPatch,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StartBody {
    wait: Option<bool>,
    timeout_s: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StopBody {
    timeout_s: Option<u64>,
    force: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ResizeBody {
    cpus: Option<u8>,
    memory: Option<ByteSize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloneMachineBody {
    name: String,
    #[serde(default)]
    fresh_disk: Option<bool>,
}

/// Path parameters of the two snapshot item routes.
#[derive(Debug)]
struct ApiSnapshotPath {
    name: String,
    snapshot: String,
}

impl<S> FromRequestParts<S> for ApiSnapshotPath
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<(String, String)>::from_request_parts(parts, state)
            .await
            .map(|Path((name, snapshot))| Self { name, snapshot })
            .map_err(|_| {
                error_response(
                    FirestoneError::new(ErrorKind::Usage, "the route path is not valid UTF-8")
                        .with_hint("percent-encode one UTF-8 machine name and snapshot name"),
                )
            })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CreateSnapshotBody {
    snapshot: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RestoreSnapshotBody {
    force: Option<bool>,
    start: Option<bool>,
    timeout_s: Option<u64>,
}

/// Optional JSON body of `POST /v1/system/prune` (SPEC §26).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PruneSystemBody {
    machines: Option<bool>,
    images: Option<bool>,
    force: Option<bool>,
    dry_run: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullImageBody {
    #[serde(rename = "ref")]
    reference: ImageRef,
    sha256: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ForceQuery {
    force: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogsQuery {
    source: Option<LogSource>,
    follow: Option<bool>,
    lines: Option<u32>,
}

async fn version(State(state): State<ApiState>) -> Response {
    payload_response(&state, Action::Version, "version", StatusCode::OK).await
}

async fn doctor(State(state): State<ApiState>) -> Response {
    payload_response(
        &state,
        Action::Doctor {
            fix: false,
            elevation_confirmed: false,
        },
        "doctor",
        StatusCode::OK,
    )
    .await
}

async fn machines(State(state): State<ApiState>) -> Response {
    payload_response(&state, Action::List, "list", StatusCode::OK).await
}

async fn create_machine(State(state): State<ApiState>, request: Request<Body>) -> Response {
    let body = match parse_required_json::<CreateMachineBody>(request, "create machine").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    let spec = match MachineSpec::from_layers(
        &state.config.create_defaults,
        &MachineSpecPatch::default(),
        &body.spec,
    ) {
        Ok(spec) => spec,
        Err(error) => return error_response(error),
    };
    payload_response(
        &state,
        Action::Create {
            name: body.name,
            spec,
        },
        "create",
        StatusCode::CREATED,
    )
    .await
}

async fn machine(State(state): State<ApiState>, ApiPath(name): ApiPath) -> Response {
    payload_response(
        &state,
        Action::Show {
            name,
            vmconfig: false,
        },
        "show",
        StatusCode::OK,
    )
    .await
}

async fn set_machine_spec(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let spec = match parse_required_json::<MachineSpec>(request, "replace machine spec").await {
        Ok(spec) => spec,
        Err(error) => return error_response(error),
    };
    payload_response(
        &state,
        Action::SetSpec { name, spec },
        "edit",
        StatusCode::OK,
    )
    .await
}

async fn patch_machine_spec(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let patch = match parse_required_json::<MachineSpecPatch>(request, "patch machine spec").await {
        Ok(patch) => patch,
        Err(error) => return error_response(error),
    };
    payload_response(
        &state,
        Action::PatchSpec { name, patch },
        "edit",
        StatusCode::OK,
    )
    .await
}

async fn remove_machine(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let query = match parse_query::<ForceQuery>(&uri, "remove machine") {
        Ok(query) => query,
        Err(error) => return error_response(error),
    };
    conditional_delete_response(
        &state,
        Action::Remove {
            names: vec![name],
            force: query.force.unwrap_or(false),
        },
        "rm",
        accepts_json(&headers),
    )
    .await
}

async fn start_machine(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_optional_json::<StartBody>(request, "start machine").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    let timeout = match request_timeout(body.timeout_s, state.config.start_timeout) {
        Ok(timeout) => timeout,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::Start {
            name,
            wait: body.wait.unwrap_or(true),
            timeout,
        },
        "start",
        aggregate,
    )
    .await
}

async fn stop_machine(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_optional_json::<StopBody>(request, "stop machine").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    let timeout = match request_timeout(body.timeout_s, state.config.stop_timeout) {
        Ok(timeout) => timeout,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::Stop {
            name,
            timeout,
            force: body.force.unwrap_or(false),
        },
        "stop",
        aggregate,
    )
    .await
}

async fn restart_machine(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    if let Err(error) = require_empty_body(request, "restart machine").await {
        return error_response(error);
    }
    let timeout = match request_timeout(None, state.config.start_timeout) {
        Ok(timeout) => timeout,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::Restart { name, timeout },
        "restart",
        aggregate,
    )
    .await
}

async fn clone_machine(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_required_json::<CloneMachineBody>(request, "clone machine").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::Clone {
            source: name,
            dest: body.name,
            fresh_disk: body.fresh_disk.unwrap_or(false),
        },
        "clone",
        aggregate,
    )
    .await
}

async fn machine_snapshots(State(state): State<ApiState>, ApiPath(name): ApiPath) -> Response {
    payload_response(
        &state,
        Action::SnapshotList { name },
        "snapshot-list",
        StatusCode::OK,
    )
    .await
}

async fn create_machine_snapshot(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_optional_json::<CreateSnapshotBody>(request, "create snapshot").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::SnapshotCreate {
            name,
            snapshot: body.snapshot,
        },
        "snapshot-create",
        aggregate,
    )
    .await
}

async fn restore_machine_snapshot(
    State(state): State<ApiState>,
    ApiSnapshotPath { name, snapshot }: ApiSnapshotPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_optional_json::<RestoreSnapshotBody>(request, "restore snapshot").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    let timeout = match request_timeout(body.timeout_s, state.config.stop_timeout) {
        Ok(timeout) => timeout,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::SnapshotRestore {
            name,
            snapshot,
            force: body.force.unwrap_or(false),
            start: body.start.unwrap_or(false),
            timeout,
        },
        "snapshot-restore",
        aggregate,
    )
    .await
}

async fn remove_machine_snapshot(
    State(state): State<ApiState>,
    ApiSnapshotPath { name, snapshot }: ApiSnapshotPath,
) -> Response {
    match collect_action(
        &state,
        Action::SnapshotRemove { name, snapshot },
        "snapshot-rm",
    )
    .await
    {
        Ok(_) => empty_response(StatusCode::NO_CONTENT),
        Err(error) => error_response(error),
    }
}

async fn resize_machine(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    request: Request<Body>,
) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_required_json::<ResizeBody>(request, "resize machine").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::Resize {
            name,
            cpus: body.cpus,
            memory: body.memory,
        },
        "resize",
        aggregate,
    )
    .await
}

async fn machine_logs(State(state): State<ApiState>, ApiPath(name): ApiPath, uri: Uri) -> Response {
    let query = match parse_query::<LogsQuery>(&uri, "machine logs") {
        Ok(query) => query,
        Err(error) => return error_response(error),
    };
    let lines = query.lines.unwrap_or(DEFAULT_LOG_LINES);
    if lines > MAX_LOG_LINES {
        return error_response(
            FirestoneError::new(
                ErrorKind::Usage,
                format!("log line count exceeds the {MAX_LOG_LINES} line limit"),
            )
            .with_hint(format!("set lines to {MAX_LOG_LINES} or fewer")),
        );
    }
    logs_response(
        &state,
        Action::Logs {
            name,
            source: query.source.unwrap_or(LogSource::Console),
            lines,
            follow: query.follow.unwrap_or(false),
        },
    )
    .await
}

async fn machine_vmconfig(State(state): State<ApiState>, ApiPath(name): ApiPath) -> Response {
    payload_response(
        &state,
        Action::Show {
            name,
            vmconfig: true,
        },
        "show-vmconfig",
        StatusCode::OK,
    )
    .await
}

async fn machine_metrics(State(state): State<ApiState>, ApiPath(name): ApiPath) -> Response {
    payload_response(&state, Action::Metrics { name }, "metrics", StatusCode::OK).await
}

async fn catalog(State(state): State<ApiState>) -> Response {
    payload_response(&state, Action::CatalogList, "catalog", StatusCode::OK).await
}

async fn images(State(state): State<ApiState>) -> Response {
    payload_response(&state, Action::ImageList, "images-ls", StatusCode::OK).await
}

async fn pull_image(State(state): State<ApiState>, request: Request<Body>) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_required_json::<PullImageBody>(request, "pull image").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::ImagePull {
            r#ref: body.reference,
            sha256: body.sha256,
        },
        "images-pull",
        aggregate,
    )
    .await
}

async fn remove_image(State(state): State<ApiState>, ApiPath(id): ApiPath, uri: Uri) -> Response {
    let query = match parse_query::<ForceQuery>(&uri, "remove image") {
        Ok(query) => query,
        Err(error) => return error_response(error),
    };
    match collect_action(
        &state,
        Action::ImageRemove {
            id,
            force: query.force.unwrap_or(false),
        },
        "images-rm",
    )
    .await
    {
        Ok(_) => empty_response(StatusCode::NO_CONTENT),
        Err(error) => error_response(error),
    }
}

async fn prune_images(State(state): State<ApiState>, request: Request<Body>) -> Response {
    if let Err(error) = require_empty_body(request, "prune images").await {
        return error_response(error);
    }
    payload_response(&state, Action::ImagePrune, "images-prune", StatusCode::OK).await
}

async fn prune_system(State(state): State<ApiState>, request: Request<Body>) -> Response {
    let aggregate = accepts_json(request.headers());
    let body = match parse_optional_json::<PruneSystemBody>(request, "prune system").await {
        Ok(body) => body,
        Err(error) => return error_response(error),
    };
    action_response(
        &state,
        Action::SystemPrune {
            machines: body.machines.unwrap_or(false),
            images: body.images.unwrap_or(false),
            force: body.force.unwrap_or(false),
            dry_run: body.dry_run.unwrap_or(false),
        },
        "system-prune",
        aggregate,
    )
    .await
}

async fn payload_response(
    state: &ApiState,
    action: Action,
    expected_result: &'static str,
    status: StatusCode,
) -> Response {
    match collect_action(state, action, expected_result).await {
        Ok(output) => match output.result {
            Event::Result { payload, .. } => json_response(status, &payload),
            _ => contract_error_response(),
        },
        Err(error) => error_response(error),
    }
}

async fn action_response(
    state: &ApiState,
    action: Action,
    expected_result: &'static str,
    aggregate: bool,
) -> Response {
    if aggregate {
        return match collect_action(state, action, expected_result).await {
            Ok(output) => json_response(StatusCode::OK, &output),
            Err(error) => error_response(error),
        };
    }

    let mut receiver = dispatch_channel(state, action, expected_result);
    match receiver.recv().await {
        Some(DispatchMessage::Event(event)) => ndjson_stream_response(event, receiver),
        Some(DispatchMessage::Error(error)) => ndjson_error_response(error),
        None => contract_error_ndjson_response(),
    }
}

async fn conditional_delete_response(
    state: &ApiState,
    action: Action,
    expected_result: &'static str,
    aggregate: bool,
) -> Response {
    if aggregate {
        return match collect_action(state, action, expected_result).await {
            Ok(output) if output.events.is_empty() => empty_response(StatusCode::NO_CONTENT),
            Ok(output) => json_response(StatusCode::OK, &output),
            Err(error) => error_response(error),
        };
    }

    let mut receiver = dispatch_channel(state, action, expected_result);
    match receiver.recv().await {
        Some(DispatchMessage::Event(Event::Result { .. })) => {
            empty_response(StatusCode::NO_CONTENT)
        }
        Some(DispatchMessage::Event(event)) => ndjson_stream_response(event, receiver),
        Some(DispatchMessage::Error(error)) => ndjson_error_response(error),
        None => contract_error_ndjson_response(),
    }
}

async fn logs_response(state: &ApiState, action: Action) -> Response {
    let mut receiver = dispatch_channel(state, action, "logs");
    // One sanitiser per follow stream: a colour sequence split across two
    // Output events is still one sequence.
    let mut sanitizer = TerminalSanitizer::new();
    loop {
        match receiver.recv().await {
            Some(DispatchMessage::Event(Event::Output { data })) => {
                let first = Bytes::from(sanitizer.push(&data));
                return text_stream_response(first, receiver, sanitizer);
            }
            Some(DispatchMessage::Event(Event::Result { .. })) => {
                return text_response(StatusCode::OK, Bytes::new());
            }
            Some(DispatchMessage::Event(_)) => {}
            Some(DispatchMessage::Error(error)) => return error_response(error),
            None => return contract_error_response(),
        }
    }
}

#[derive(Debug, Serialize)]
struct DispatchOutput {
    events: Vec<Event>,
    result: Event,
}
enum DispatchMessage {
    Event(Event),
    Error(FirestoneError),
}

struct ChannelEventSink {
    sender: mpsc::Sender<DispatchMessage>,
    expected_result: &'static str,
    result: Option<Event>,
    contract_violation: bool,
    disconnected: bool,
}

impl ChannelEventSink {
    fn new(sender: mpsc::Sender<DispatchMessage>, expected_result: &'static str) -> Self {
        Self {
            sender,
            expected_result,
            result: None,
            contract_violation: false,
            disconnected: false,
        }
    }

    fn send(&mut self, message: DispatchMessage) {
        let mut message = message;
        loop {
            if self.disconnected || self.sender.is_closed() {
                self.disconnected = true;
                return;
            }
            match self.sender.try_send(message) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.disconnected = true;
                    return;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                    message = returned;
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    fn finish(&mut self, result: Result<(), FirestoneError>) {
        if self.disconnected || self.sender.is_closed() {
            return;
        }
        match result {
            Err(error) => self.send(DispatchMessage::Error(error)),
            Ok(()) if self.contract_violation => {
                self.send(DispatchMessage::Error(dispatch_contract_error(
                    "the dispatcher emitted an invalid terminal event sequence",
                )));
            }
            Ok(()) => match self.result.take() {
                Some(result) => self.send(DispatchMessage::Event(result)),
                None => self.send(DispatchMessage::Error(dispatch_contract_error(
                    "the dispatcher completed without a Result event",
                ))),
            },
        }
    }
}

impl EventSink for ChannelEventSink {
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
        if self.disconnected || self.sender.is_closed() {
            self.disconnected = true;
            return Ok(());
        }
        match &event {
            Event::Result { action, .. } => {
                if self.result.is_some() || action != self.expected_result {
                    self.contract_violation = true;
                } else {
                    self.result = Some(event);
                }
            }
            _ => {
                if self.result.is_some() {
                    self.contract_violation = true;
                }
                self.send(DispatchMessage::Event(event));
            }
        }
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        self.disconnected || self.sender.is_closed()
    }
}

fn dispatch_channel(
    state: &ApiState,
    action: Action,
    expected_result: &'static str,
) -> mpsc::Receiver<DispatchMessage> {
    spawn_dispatch(Arc::clone(&state.dispatcher), action, expected_result)
}

fn spawn_dispatch(
    dispatcher: Arc<dyn Dispatcher>,
    action: Action,
    expected_result: &'static str,
) -> mpsc::Receiver<DispatchMessage> {
    let (sender, receiver) = mpsc::channel(EVENT_BUFFER_CAPACITY);
    drop(tokio::task::spawn_blocking(move || {
        let mut events = ChannelEventSink::new(sender, expected_result);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            firestone_core::block_on(dispatcher.run(action, &mut events))
        }));
        match outcome {
            Ok(result) => events.finish(result),
            Err(_) => events.finish(Err(dispatch_contract_error(
                "the dispatcher panicked while handling the request",
            ))),
        }
    }));
    receiver
}

async fn collect_action(
    state: &ApiState,
    action: Action,
    expected_result: &'static str,
) -> Result<DispatchOutput, FirestoneError> {
    let mut receiver = dispatch_channel(state, action, expected_result);
    let mut events = Vec::new();
    loop {
        match receiver.recv().await {
            Some(DispatchMessage::Event(event @ Event::Result { .. })) => {
                return Ok(DispatchOutput {
                    events,
                    result: event,
                });
            }
            Some(DispatchMessage::Event(event)) => events.push(event),
            Some(DispatchMessage::Error(error)) => return Err(error),
            None => {
                return Err(dispatch_contract_error(
                    "the dispatcher ended without a terminal event",
                ));
            }
        }
    }
}

/// Runs one action to completion and returns its terminal `Result` payload.
///
/// The web UI renders the same shared results the REST routes serialize, so it
/// reuses this dispatch path rather than growing a second one: identical
/// blocking-worker isolation, identical panic containment, and the identical
/// "exactly one expected terminal Result" contract check.
pub(crate) async fn dispatch_payload(
    dispatcher: &Arc<dyn Dispatcher>,
    action: Action,
    expected_result: &'static str,
) -> Result<serde_json::Value, FirestoneError> {
    let mut receiver = spawn_dispatch(Arc::clone(dispatcher), action, expected_result);
    loop {
        match receiver.recv().await {
            Some(DispatchMessage::Event(Event::Result { payload, .. })) => return Ok(payload),
            Some(DispatchMessage::Event(_)) => {}
            Some(DispatchMessage::Error(error)) => return Err(error),
            None => {
                return Err(dispatch_contract_error(
                    "the dispatcher ended without a terminal event",
                ));
            }
        }
    }
}

/// Runs one action and returns everything it wrote as `Output`.
///
/// Used by the web UI's non-following log read, which needs the log text
/// rather than the terminal metadata payload.
pub(crate) async fn dispatch_output(
    dispatcher: &Arc<dyn Dispatcher>,
    action: Action,
    expected_result: &'static str,
) -> Result<String, FirestoneError> {
    let mut receiver = spawn_dispatch(Arc::clone(dispatcher), action, expected_result);
    let mut output = String::new();
    loop {
        match receiver.recv().await {
            Some(DispatchMessage::Event(Event::Output { data })) => output.push_str(&data),
            Some(DispatchMessage::Event(Event::Result { .. })) => return Ok(output),
            Some(DispatchMessage::Event(_)) => {}
            Some(DispatchMessage::Error(error)) => return Err(error),
            // A read that produced output and then ended without a terminal
            // record still has usable bytes; returning them beats discarding
            // the log the operator asked for.
            None if !output.is_empty() => return Ok(output),
            None => {
                return Err(dispatch_contract_error(
                    "the dispatcher ended without a terminal event",
                ));
            }
        }
    }
}

fn dispatch_contract_error(message: &'static str) -> FirestoneError {
    FirestoneError::new(ErrorKind::Generic, message)
        .with_hint("retry the request; if it fails again, report a Firestone bug")
}

fn request_timeout(
    requested_seconds: Option<u64>,
    default: Duration,
) -> Result<Duration, FirestoneError> {
    let seconds = requested_seconds.unwrap_or(default.as_secs());
    if seconds == 0 || seconds > MAX_TIMEOUT_SECONDS {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            format!("timeout_s must be between 1 and {MAX_TIMEOUT_SECONDS} seconds"),
        )
        .with_hint(format!(
            "set timeout_s to a whole number from 1 through {MAX_TIMEOUT_SECONDS}"
        )));
    }
    Ok(Duration::from_secs(seconds))
}

async fn parse_required_json<T>(
    request: Request<Body>,
    label: &'static str,
) -> Result<T, FirestoneError>
where
    T: DeserializeOwned,
{
    let bytes = request_body(request, label).await?;
    if bytes.is_empty() {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            format!("{label} requires a JSON request body"),
        )
        .with_hint("send one application/json object matching the route body"));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Usage,
            format!("{label} request body is not valid for this route"),
        )
        .with_hint("send one application/json object with only the documented fields")
    })
}

async fn parse_optional_json<T>(
    request: Request<Body>,
    label: &'static str,
) -> Result<T, FirestoneError>
where
    T: Default + DeserializeOwned,
{
    let bytes = request_body(request, label).await?;
    if bytes.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Usage,
            format!("{label} request body is not valid for this route"),
        )
        .with_hint("send one application/json object with only the documented fields")
    })
}

async fn require_empty_body(
    request: Request<Body>,
    label: &'static str,
) -> Result<(), FirestoneError> {
    let bytes = request_body_untyped(request, label).await?;
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Usage,
            format!("{label} does not accept a request body"),
        )
        .with_hint("send the request with an empty body"))
    }
}

async fn request_body(
    request: Request<Body>,
    label: &'static str,
) -> Result<Bytes, FirestoneError> {
    let is_json = is_json_content_type(request.headers());
    let bytes = request_body_untyped(request, label).await?;
    if !bytes.is_empty() && !is_json {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            format!("{label} request body must use application/json"),
        )
        .with_hint("set Content-Type: application/json"));
    }
    Ok(bytes)
}

async fn request_body_untyped(
    request: Request<Body>,
    label: &'static str,
) -> Result<Bytes, FirestoneError> {
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_JSON_BODY_BYTES as u64)
    {
        return Err(body_limit_error(label));
    }
    to_bytes(request.into_body(), MAX_JSON_BODY_BYTES)
        .await
        .map_err(|_| body_limit_error(label))
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(JSON_CONTENT_TYPE))
}

fn body_limit_error(label: &'static str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Usage,
        format!("{label} request body exceeds the {MAX_JSON_BODY_BYTES} byte limit"),
    )
    .with_hint("send a smaller JSON object")
}

fn parse_query<T>(uri: &Uri, label: &'static str) -> Result<T, FirestoneError>
where
    T: Default + DeserializeOwned,
{
    if uri.query().is_none_or(str::is_empty) {
        return Ok(T::default());
    }
    axum::extract::Query::<T>::try_from_uri(uri)
        .map(|axum::extract::Query(value)| value)
        .map_err(|_| {
            FirestoneError::new(
                ErrorKind::Usage,
                format!("{label} query is not valid for this route"),
            )
            .with_hint("use each documented query field once with its expected value type")
        })
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| value.split(';').next())
        .any(|value| value.trim().eq_ignore_ascii_case(JSON_CONTENT_TYPE))
}

async fn validate_request_uri(request: Request<Body>, next: Next) -> Response {
    let uri = request.uri();
    let valid =
        valid_percent_encoding(uri.path()) && uri.query().is_none_or(valid_percent_encoding);
    if valid {
        next.run(request).await
    } else {
        error_response(
            FirestoneError::new(
                ErrorKind::Usage,
                "the request URI has invalid percent encoding",
            )
            .with_hint("encode every percent sign as '%' followed by two hexadecimal digits"),
        )
    }
}

fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

async fn not_found() -> Response {
    error_response(
        FirestoneError::new(ErrorKind::NotFound, "no REST route matches this request")
            .with_hint("check the HTTP method and the /v1 route path"),
    )
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorInfo,
}

fn error_response(error: FirestoneError) -> Response {
    let status = status_for_error(error.kind());
    json_response(
        status,
        &ErrorEnvelope {
            error: error.info(),
        },
    )
}

fn status_for_error(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::Usage | ErrorKind::InvalidSpec => StatusCode::BAD_REQUEST,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::Conflict | ErrorKind::AlreadyRunning | ErrorKind::Busy => StatusCode::CONFLICT,
        ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorKind::Dependency => StatusCode::SERVICE_UNAVAILABLE,
        ErrorKind::Checksum => StatusCode::BAD_GATEWAY,
        ErrorKind::Generic
        | ErrorKind::NotRunning
        | ErrorKind::AlreadyExists
        | ErrorKind::Interrupted => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn json_response<T>(status: StatusCode, value: &T) -> Response
where
    T: Serialize + ?Sized,
{
    match serde_json::to_vec(value) {
        Ok(body) => response_with_content_type(status, JSON_CONTENT_TYPE, Body::from(body)),
        Err(_) => response_with_content_type(
            StatusCode::INTERNAL_SERVER_ERROR,
            JSON_CONTENT_TYPE,
            Body::from(INTERNAL_ERROR_JSON),
        ),
    }
}

fn ndjson_error_response(error: FirestoneError) -> Response {
    let status = status_for_error(error.kind());
    let body = encode_error_ndjson(&error);
    response_with_content_type(status, NDJSON_CONTENT_TYPE, Body::from(body))
}

fn ndjson_stream_response(first: Event, receiver: mpsc::Receiver<DispatchMessage>) -> Response {
    let first = tokio_stream::iter([Ok::<Bytes, Infallible>(encode_event_ndjson(&first))]);
    let rest = ReceiverStream::new(receiver).filter_map(|message| match message {
        DispatchMessage::Event(event) => Some(Ok(encode_event_ndjson(&event))),
        DispatchMessage::Error(error) => Some(Ok(encode_error_ndjson(&error))),
    });
    response_with_content_type(
        StatusCode::OK,
        NDJSON_CONTENT_TYPE,
        Body::from_stream(first.chain(rest)),
    )
}

/// Streams the rest of a log through the sanitiser that produced `first`.
///
/// The sanitiser travels with the stream rather than being applied per chunk,
/// so a sequence straddling two Output events parses once, and the dangling
/// tail of a truncated sequence is flushed when the stream ends.
fn text_stream_response(
    first: Bytes,
    receiver: mpsc::Receiver<DispatchMessage>,
    sanitizer: TerminalSanitizer,
) -> Response {
    let first = tokio_stream::iter([Ok::<Bytes, Infallible>(first)]);
    let rest = futures_util::stream::unfold(Some((receiver, sanitizer)), |carried| async move {
        let (mut receiver, mut sanitizer) = carried?;
        loop {
            match receiver.recv().await {
                Some(DispatchMessage::Event(Event::Output { data })) => {
                    let chunk = sanitizer.push(&data);
                    if chunk.is_empty() {
                        continue;
                    }
                    return Some((
                        Ok::<Bytes, Infallible>(Bytes::from(chunk)),
                        Some((receiver, sanitizer)),
                    ));
                }
                Some(DispatchMessage::Event(_) | DispatchMessage::Error(_)) => continue,
                None => {
                    let tail = sanitizer.finish();
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(tail)), None));
                }
            }
        }
    });
    response_with_content_type(
        StatusCode::OK,
        TEXT_CONTENT_TYPE,
        Body::from_stream(first.chain(rest)),
    )
}

fn encode_event_ndjson(event: &Event) -> Bytes {
    match serde_json::to_vec(event) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            Bytes::from(bytes)
        }
        Err(_) => Bytes::from_static(INTERNAL_ERROR_NDJSON),
    }
}

fn encode_error_ndjson(error: &FirestoneError) -> Bytes {
    match serde_json::to_vec(&ErrorEnvelope {
        error: error.info(),
    }) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            Bytes::from(bytes)
        }
        Err(_) => Bytes::from_static(INTERNAL_ERROR_NDJSON),
    }
}

/// Applies the same log sanitisation to a whole aggregated string.
///
/// The web UI renders log text into HTML rather than streaming it as
/// `text/plain`, and must not be the one surface where a raw control byte
/// survives. It sees exactly what the streaming path emits: SGR colour kept,
/// every other escape sequence replaced.
pub(crate) fn sanitized_output(data: &str) -> String {
    sanitize_terminal_output(data)
}

fn contract_error_response() -> Response {
    error_response(dispatch_contract_error(
        "the dispatcher returned an invalid terminal event",
    ))
}

fn contract_error_ndjson_response() -> Response {
    ndjson_error_response(dispatch_contract_error(
        "the dispatcher ended without a terminal event",
    ))
}

fn text_response(status: StatusCode, body: Bytes) -> Response {
    response_with_content_type(status, TEXT_CONTENT_TYPE, Body::from(body))
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn response_with_content_type(
    status: StatusCode,
    content_type: &'static str,
    body: Body,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[cfg(test)]
mod tests;
