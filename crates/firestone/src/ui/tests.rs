//! Router-level coverage for the embedded web UI.
//!
//! Every test drives the real router through a fake dispatcher that answers
//! with the shared result payloads, so a template that stops matching the
//! action contract fails here rather than in a browser.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CONTENT_TYPE},
};
use firestone_core::{
    Action, DispatchFuture, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig,
    PathInputs, Paths,
};
use serde_json::{Value, json};
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Answers every read action with a fixed, contract-shaped payload.
struct FakeDispatcher {
    machines: Value,
    machine: Value,
    catalog: Value,
    /// Injected to prove failures render as pages and fragments rather than
    /// panicking or leaking a blank response.
    fail: Option<ErrorKind>,
}

impl Default for FakeDispatcher {
    fn default() -> Self {
        Self {
            machines: json!([
                {
                    "name": "web",
                    "status": "running",
                    "image": "ubuntu:24.04",
                    "cpus": 4,
                    "memory": "8G",
                    "uptime": "2h 14m",
                    "forwards": ["127.0.0.1:8080:80"]
                },
                {
                    "name": "staging-db",
                    "status": "stopped",
                    "image": "debian:12",
                    "cpus": 2,
                    "memory": "4G",
                    "uptime": null,
                    "forwards": []
                },
                {
                    "name": "mid-flight",
                    "status": "starting",
                    "image": "fedora:44",
                    "cpus": 8,
                    "memory": "16G",
                    "uptime": null,
                    "forwards": []
                }
            ]),
            machine: machine_view("web", "running"),
            catalog: json!([{
                "reference": "ubuntu:24.04",
                "aliases": ["noble"],
                "architectures": [{ "architecture": "x86_64", "firmware": "edk2" }]
            }]),
            fail: None,
        }
    }
}

fn machine_view(name: &str, status: &str) -> Value {
    json!({
        "spec": {
            "image": "ubuntu:24.04",
            "arch": null,
            "cpus": 4,
            "memory": "8G",
            "disk": "40G",
            "user": "ubuntu",
            "network": { "mode": "passt", "forward": ["127.0.0.1:8080:80"], "tap": null, "mac": null },
            "mount": [],
            "cloud_init": {
                "user_data": null,
                "network_config": null,
                "ssh_keys": [],
                "provisioning": true
            },
            "vmm": { "binary": null, "firmware": "auto", "extra_args": [], "config_overlay": null }
        },
        "state": {
            "version": 1,
            "status": status,
            "image": { "ref": "ubuntu:24.04", "id": null, "sha256": null },
            "mac": "52:54:00:9a:1f:c3",
            "cid": 3,
            "instance_id": null,
            "shim_pid": 41282,
            "vmm_pid": 41283,
            "sidecar_pids": {},
            "runtime_dir": format!("/run/user/1000/firestone/{name}"),
            "started_at": "2026-09-01 05:57:12",
            "forwards": ["127.0.0.1:8080:80"],
            "degraded": [],
            "last_exit": null
        },
        "supervision": "supervised"
    })
}

/// The same view with one read-only shared folder, for the Mounts group.
fn machine_view_with_mounts(name: &str, status: &str) -> Value {
    let mut view = machine_view(name, status);
    view["spec"]["mount"] = json!([{
        "host": "/srv/project",
        "guest": "/work",
        "readonly": true,
        "tag": null
    }]);
    view
}

impl Dispatcher for FakeDispatcher {
    fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            if let Some(kind) = self.fail {
                return Err(FirestoneError::new(kind, "the fake dispatcher refused")
                    .with_hint("this is a test fixture"));
            }

            let (name, payload) = match action {
                Action::List => ("list", self.machines.clone()),
                Action::Show {
                    vmconfig: false, ..
                } => ("show", self.machine.clone()),
                Action::Show { vmconfig: true, .. } => (
                    "show-vmconfig",
                    json!({ "cpus": { "boot_vcpus": 4, "max_vcpus": 4 } }),
                ),
                Action::ImageList => (
                    "images-ls",
                    json!([{
                        "metadata": {
                            "version": 1,
                            "id": "sha256-abc",
                            "generation": 1,
                            "source_ref": "ubuntu:24.04",
                            "source_url": null,
                            "source_sha256": "a",
                            "stored_sha256": "b",
                            "architecture": "x86_64",
                            "firmware": "edk2",
                            "source_format": "qcow2",
                            "stored_format": "qcow2",
                            "verification_algorithm": null,
                            "verification_digest": null,
                            "size": 642_000_000u64,
                            "pulled_at": "2026-09-01T05:00:00Z"
                        },
                        "path": "/data/images/sha256-abc.qcow2"
                    }]),
                ),
                Action::CatalogList => ("catalog", self.catalog.clone()),
                Action::Doctor { .. } => (
                    "doctor",
                    json!({
                        "checks": [
                            { "id": "host_arch", "status": "ok", "reason": "x86_64" },
                            {
                                "id": "nested_virtualization",
                                "status": "warn",
                                "reason": "nested virtualization unavailable"
                            }
                        ]
                    }),
                ),
                Action::Version => (
                    "version",
                    json!({
                        "version": "0.1.3",
                        "identity": { "release": "0.1.3", "git_commit": null },
                        "architecture": "x86_64",
                        "dependencies": {},
                        "paths": {
                            "config": "/home/a/.config/firestone",
                            "data": "/home/a/.local/share/firestone",
                            "runtime": "/run/user/1000/firestone"
                        }
                    }),
                ),
                Action::Logs { .. } => {
                    // Deliberately includes a control character, markup, an SGR
                    // colour run and an OSC 52 clipboard write: the UI must
                    // sanitize the first, escape the second, keep the third
                    // verbatim and swallow the fourth.
                    events.emit(Event::Output {
                        data: "boot ok\u{7}\n<script>alert(1)</script>\n\u{1b}[32mok\u{1b}[0m\n\u{1b}]52;c;ZXZpbA==\u{7}after\n".to_owned(),
                    })?;
                    (
                        "logs",
                        json!({ "name": "web", "source": "console", "lines": 400, "follow": false }),
                    )
                }
                Action::Create { name, .. } => (
                    "create",
                    json!({ "name": name, "spec": self.machine["spec"], "state": self.machine["state"] }),
                ),
                _ => return Ok(()),
            };

            events.emit(Event::Result {
                action: name.to_owned(),
                payload,
            })
        })
    }
}

fn app(dispatcher: FakeDispatcher) -> Result<Router, FirestoneError> {
    let directory = tempfile::tempdir().map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot create a test home").with_source(source)
    })?;
    let inputs = PathInputs {
        firestone_home: Some(directory.path().to_path_buf()),
        ..PathInputs::capture()?
    };
    let paths = Paths::from_inputs(&inputs)?;
    // The temporary directory only has to outlive path resolution: no handler
    // touches the filesystem.
    Ok(super::router(
        Arc::new(dispatcher),
        &GlobalConfig::default(),
        &paths,
    ))
}

async fn get(
    router: &Router,
    uri: &str,
) -> Result<(StatusCode, String, String), Box<dyn std::error::Error>> {
    request(router, Request::builder().uri(uri).body(Body::empty())?).await
}

async fn get_fragment(
    router: &Router,
    uri: &str,
) -> Result<(StatusCode, String, String), Box<dyn std::error::Error>> {
    request(
        router,
        Request::builder()
            .uri(uri)
            .header("HX-Request", "true")
            .body(Body::empty())?,
    )
    .await
}

async fn request(
    router: &Router,
    request: Request<Body>,
) -> Result<(StatusCode, String, String), Box<dyn std::error::Error>> {
    let response = router.clone().oneshot(request).await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    Ok((status, content_type, String::from_utf8(body.to_vec())?))
}

#[tokio::test]
async fn every_screen_renders_a_complete_document() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    for uri in ["/", "/machines", "/machines/web", "/catalog"] {
        let (status, content_type, body) = get(&router, uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri} did not render");
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(
            body.starts_with("<!DOCTYPE html>"),
            "{uri} is not a document"
        );
        assert!(body.contains("id=\"fs-main-content\""), "{uri} has no main");
        assert!(
            body.contains("id=\"fs-toasts\""),
            "{uri} has no toast region"
        );
        assert!(body.trim_end().ends_with("</html>"), "{uri} is truncated");
    }
    Ok(())
}

#[tokio::test]
async fn an_htmx_navigation_returns_the_fragment_without_the_shell() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/machines").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("<!DOCTYPE html>"),
        "the shell was sent again"
    );
    assert!(
        body.contains("fs-table__head"),
        "the screen body is missing"
    );
    Ok(())
}

#[tokio::test]
async fn a_restored_history_entry_returns_the_whole_document() -> TestResult {
    // htmx replays history with HX-Request set, but it needs the full page.
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = request(
        &router,
        Request::builder()
            .uri("/machines")
            .header("HX-Request", "true")
            .header("HX-History-Restore-Request", "true")
            .body(Body::empty())?,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("<!DOCTYPE html>"));
    Ok(())
}

#[tokio::test]
async fn a_transitioning_machine_is_offered_no_lifecycle_button() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/rows").await?;

    assert!(body.contains(r#"data-fs-row="web""#));
    assert!(body.contains(r#"data-fs-action="stop""#));
    assert!(body.contains(r#"data-fs-machine="web""#));
    assert!(body.contains(r#"data-fs-action="start""#));
    assert!(body.contains(r#"data-fs-machine="staging-db""#));
    assert!(
        !body.contains(r#"data-fs-machine="mid-flight""#),
        "a starting machine must not offer a lifecycle action"
    );
    Ok(())
}

#[tokio::test]
async fn status_is_rendered_as_a_token_and_never_as_a_colour() -> TestResult {
    // The CSP forbids inline styles; every status must resolve through CSS.
    let router = app(FakeDispatcher::default())?;
    for uri in ["/", "/machines", "/machines/web"] {
        let (_, _, body) = get(&router, uri).await?;
        assert!(
            !body.contains(" style=\""),
            "{uri} emitted an inline style attribute"
        );
        assert!(!body.contains("var(--ok)"), "{uri} emitted a colour");
    }

    let (_, _, rows) = get_fragment(&router, "/ui/machines/rows").await?;
    assert!(rows.contains(r#"data-status="running""#));
    assert!(rows.contains(r#"data-status="starting""#));
    Ok(())
}

#[tokio::test]
async fn the_filter_narrows_rows_without_changing_the_sidebar_count() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/rows?q=staging").await?;

    assert!(body.contains(r#"data-fs-row="staging-db""#));
    assert!(!body.contains(r#"data-fs-row="web""#));
    // The out-of-band count reports the whole fleet, not the filtered view.
    assert!(body.contains(r#"id="fs-nav-count""#));
    assert!(body.contains(">3</span>"), "the count must stay unfiltered");
    Ok(())
}

#[tokio::test]
async fn the_poll_carries_the_active_filter() -> TestResult {
    // Without this the five-second refresh would silently clear whatever the
    // user typed, which is the kind of bug that only shows up while someone
    // is mid-sentence.
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/machines").await?;
    assert!(body.contains(r##"hx-include="#fs-machine-filter""##));
    assert!(body.contains(r#"id="fs-machine-filter""#));
    Ok(())
}

#[tokio::test]
async fn an_empty_filter_result_explains_itself() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/rows?q=zzz").await?;
    assert!(body.contains("fs-empty"), "no empty state was rendered");
    assert!(body.contains("zzz"), "the empty state must name the filter");
    Ok(())
}

#[tokio::test]
async fn log_text_is_sanitized_and_escaped() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/web/tab/logs").await?;

    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains('\u{7}'),
        "a control character reached the page"
    );
    assert!(
        body.contains('\u{fffd}'),
        "the control byte was not replaced"
    );
    assert!(
        !body.contains("<script>alert(1)</script>"),
        "log markup was not escaped"
    );
    assert!(body.contains("&lt;script&gt;"), "expected escaped markup");
    Ok(())
}

#[tokio::test]
async fn log_sgr_colour_survives_while_other_sequences_are_neutralized() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/web/tab/logs").await?;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("\u{1b}[32mok\u{1b}[0m"),
        "the SGR colour run did not survive the logs surface"
    );
    assert!(
        !body.contains("52;c;ZXZpbA=="),
        "an OSC 52 clipboard payload reached the page"
    );
    assert!(
        !body.contains("\u{1b}]"),
        "an OSC introducer reached the page"
    );
    assert!(
        body.contains("\u{fffd}after"),
        "the swallowed OSC must leave exactly one replacement character"
    );
    Ok(())
}

#[tokio::test]
async fn following_is_offered_only_while_a_machine_is_running() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, running) = get_fragment(&router, "/ui/machines/web/tab/logs").await?;
    assert!(running.contains(r#"data-fs-follow="true""#));
    assert!(!running.contains("disabled title="));

    let stopped = app(FakeDispatcher {
        machine: machine_view("web", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, body) = get_fragment(&stopped, "/ui/machines/web/tab/logs").await?;
    assert!(body.contains(r#"data-fs-follow="false""#));
    assert!(
        body.contains("disabled"),
        "a stopped machine must not offer a live follow"
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_log_source_is_rejected_rather_than_guessed() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) =
        get_fragment(&router, "/ui/machines/web/tab/logs?source=../etc").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("fs-inline-notice--fail"));
    Ok(())
}

#[tokio::test]
async fn the_detail_head_reports_supervision_and_spec_groups() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, head) = get_fragment(&router, "/ui/machines/web/head").await?;
    assert!(head.contains("supervised"));
    assert!(head.contains("52:54:00:9a:1f:c3"));

    let (_, _, spec) = get_fragment(&router, "/ui/machines/web/tab/spec").await?;
    for group in ["Resources", "Network", "Cloud-init", "VMM"] {
        assert!(spec.contains(group), "the {group} group is missing");
    }
    // Cloud-init contents are never read into the UI, only paths.
    assert!(spec.contains("provisioning"));
    Ok(())
}

#[tokio::test]
async fn a_machine_that_never_started_explains_its_missing_vmconfig() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/web/tab/vmconfig").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("boot_vcpus"));
    Ok(())
}

#[tokio::test]
async fn the_overview_surfaces_a_warning_check_and_its_reason() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/").await?;

    assert!(body.contains("nested_virtualization"));
    assert!(body.contains("nested virtualization unavailable"));
    assert!(body.contains(r#"data-status="warn""#));
    // One warning is not a failure, so no standing banner.
    assert!(
        !body.contains("fs-banner"),
        "a warning must not raise a banner"
    );
    assert!(body.contains("1 ok · 1 warn"));
    Ok(())
}

#[tokio::test]
async fn the_create_form_offers_catalog_references_and_configured_defaults() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/new").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("fs-image-options"));
    assert!(body.contains(r#"<option value="ubuntu:24.04">"#));
    assert!(body.contains("data-fs-autoopen"));
    Ok(())
}

#[tokio::test]
async fn a_rejected_create_answers_beside_the_offending_field() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/ui/machines")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("name=&image=&cpus=0&memory=nope&forward=bad"))?,
    )
    .await?;

    assert_eq!(
        status,
        StatusCode::OK,
        "the dialog is re-rendered, not errored"
    );
    assert!(body.contains(r#"id="fs-create-name-error""#));
    assert!(body.contains(r#"id="fs-create-image-error""#));
    assert!(body.contains(r#"aria-invalid="true""#));
    // Field problems are never announced as toasts.
    assert!(!body.contains("fs:toast"));
    Ok(())
}

#[tokio::test]
async fn a_successful_create_closes_the_dialog_and_navigates() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ui/machines")
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("name=web&image=ubuntu%3A24.04"))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let trigger = response
        .headers()
        .get("HX-Trigger")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        trigger.contains("fs:created"),
        "no completion was announced; headers were {:?}",
        response.headers()
    );
    let location = response
        .headers()
        .get("HX-Location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(location.contains("/machines/web"));
    Ok(())
}

#[tokio::test]
async fn the_image_picker_reports_cached_entries_and_offers_a_pull_for_the_rest() -> TestResult {
    // The fake store holds ubuntu:24.04 and nothing else, so one catalog entry
    // must read as cached with its size and the other must offer a pull.
    let router = app(FakeDispatcher {
        catalog: json!([
            {
                "reference": "ubuntu:24.04",
                "aliases": ["noble"],
                "architectures": [{ "architecture": "x86_64", "firmware": "edk2" }]
            },
            {
                "reference": "debian:12",
                "aliases": [],
                "architectures": [{ "architecture": "x86_64", "firmware": "edk2" }]
            }
        ]),
        ..FakeDispatcher::default()
    })?;

    for uri in ["/ui/machines/new", "/ui/machines/new/images"] {
        let (status, _, body) = get_fragment(&router, uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri} did not render");
        assert!(body.contains("fs-picker__list"), "{uri} has no picker");
        assert!(
            body.contains(r#"value="ubuntu:24.04""#),
            "{uri} lost the cached entry"
        );
        assert!(body.contains("noble"), "{uri} does not show the aliases");
        assert!(
            body.contains("642 MB") && body.contains("fs-cached"),
            "{uri} does not badge the cached entry with its size"
        );
        assert!(
            body.contains(r#"data-fs-pull-picker="debian:12""#),
            "{uri} does not offer a pull for the uncached entry"
        );
        assert!(
            !body.contains(r#"data-fs-pull-picker="ubuntu:24.04""#),
            "{uri} offers a pull for an image that is already cached"
        );
        // The free-text row takes a URL, a path, or anything else.
        assert!(
            body.contains("data-fs-picker-custom"),
            "{uri} has no free row"
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_create_form_composes_every_friendly_control_into_one_named_field() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/new").await?;

    // Sizes: a number and a unit over the hidden canonical string. The units
    // are labelled GiB and MiB because that is what the grammar means.
    assert!(body.contains(r#"id="fs-create-memory-amount""#));
    assert!(body.contains(">GiB</option>") && body.contains(">MiB</option>"));
    assert!(!body.contains(">GB<"), "a GiB unit must not be labelled GB");
    assert!(body.contains(r#"name="memory" value="2G""#));

    // The tap device is present but hidden until the mode selects it, and the
    // MAC lives behind Advanced. Both toggle by attribute, never by style.
    assert!(body.contains("data-fs-tap-field hidden"));
    assert!(body.contains(r#"name="tap""#) && body.contains(r#"name="mac""#));
    assert!(!body.contains(" style=\""), "the dialog emitted a style");

    // Repeatable rows over the canonical composed fields.
    for marker in [
        "data-fs-forward-template",
        "data-fs-forward-add",
        "data-fs-mount-template",
        "data-fs-mount-add",
        "data-fs-row-remove",
        "data-fs-raw-toggle",
    ] {
        assert!(body.contains(marker), "{marker} is missing");
    }
    assert!(body.contains(r#"name="forward""#));
    assert!(body.contains(r#"name="mounts""#));
    Ok(())
}

#[tokio::test]
async fn a_submitted_mount_list_reaches_the_spec_and_bad_rows_answer_inline() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/ui/machines")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "name=web&image=ubuntu%3A24.04&mounts=%2Fsrv%3A%2Fwork%3Aro%0Anonsense\
                 &forward=bad&mac=zz",
            ))?,
    )
    .await?;

    assert_eq!(status, StatusCode::OK, "the dialog is re-rendered");
    // Every new field collects its own problem in the same pass.
    assert!(body.contains(r#"id="fs-create-mounts-error""#));
    assert!(body.contains(r#"id="fs-create-forward-error""#));
    assert!(body.contains(r#"id="fs-create-mac-error""#));
    // The rejected values come back verbatim so nothing the user typed is lost
    // (paths are HTML-escaped, so this reads the slash-free tail).
    assert!(body.contains("work:ro"), "the mount rows were not returned");
    assert!(!body.contains("fs:toast"), "a field problem is not a toast");

    let ok = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/ui/machines")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "name=web&image=ubuntu%3A24.04&net_mode=tap&tap=tap0\
                 &mounts=%2Fsrv%3A%2Fwork%3Aro&forward=udp%3A5353%3A5353",
            ))?,
    )
    .await?;
    assert_eq!(ok.0, StatusCode::OK);
    assert!(ok.2.is_empty(), "a valid submission renders no dialog");
    Ok(())
}

#[tokio::test]
async fn the_detail_spec_renders_the_mounts_group() -> TestResult {
    let router = app(FakeDispatcher {
        machine: machine_view_with_mounts("web", "running"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, spec) = get_fragment(&router, "/ui/machines/web/tab/spec").await?;
    assert!(spec.contains("Mounts"), "the Mounts group is missing");
    // Paths are HTML-escaped, so the assertion reads the slash-free tail of
    // the same `HOST:GUEST[:ro]` string the CLI and the create form use.
    assert!(
        spec.contains("work:ro · tag share0"),
        "the mount row does not render its grammar and effective tag"
    );

    // A machine with no shared folders still gets the group, reported as empty
    // rather than silently absent.
    let bare = app(FakeDispatcher::default())?;
    let (_, _, empty) = get_fragment(&bare, "/ui/machines/web/tab/spec").await?;
    assert!(empty.contains("Mounts"));
    Ok(())
}

#[tokio::test]
async fn the_palette_matches_machines_and_catalog_entries() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, all) = get_fragment(&router, "/ui/palette").await?;
    assert!(all.contains("data-fs-palette-item"));

    let (_, _, filtered) = get_fragment(&router, "/ui/palette?q=staging").await?;
    assert!(filtered.contains("staging-db"));
    assert!(!filtered.contains(">web<"));

    let (_, _, empty) = get_fragment(&router, "/ui/palette?q=zzzz").await?;
    assert!(empty.contains("fs-palette__empty"));
    Ok(())
}

#[tokio::test]
async fn static_assets_are_served_from_a_closed_table() -> TestResult {
    let router = app(FakeDispatcher::default())?;

    let (status, content_type, _) = get(&router, "/ui/static/app.css").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/css; charset=utf-8");

    let (status, content_type, _) = get(&router, "/ui/static/htmx.min.js").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/javascript; charset=utf-8");

    let font = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/static/fonts/ibm-plex-sans-latin-400.woff2")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(font.status(), StatusCode::OK);
    assert_eq!(
        font.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("font/woff2")
    );
    let bytes = to_bytes(font.into_body(), 1024 * 1024).await?;
    assert_eq!(
        &bytes[..4],
        b"wOF2",
        "the font body was not served verbatim"
    );

    // The name is matched against a table, never joined onto a directory.
    for hostile in [
        "/ui/static/../../../etc/passwd",
        "/ui/static/..%2f..%2fetc%2fpasswd",
        "/ui/static/nothing.js",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(hostile).body(Body::empty())?)
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{hostile} was served"
        );
    }
    Ok(())
}

#[tokio::test]
async fn assets_are_cacheable_only_because_their_urls_carry_the_build() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/static/app.css")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );

    let (_, _, document) = get(&router, "/").await?;
    let build = env!("CARGO_PKG_VERSION");
    assert!(
        document.contains(&format!("/ui/static/app.css?v={build}")),
        "the stylesheet URL is not build-stamped"
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_ui_path_renders_a_navigable_page() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, content_type, body) = get(&router, "/nope").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(body.contains("Not found"));
    assert!(body.contains("Back to overview"));
    Ok(())
}

#[tokio::test]
async fn a_missing_machine_renders_the_error_page_with_its_status() -> TestResult {
    let router = app(FakeDispatcher {
        fail: Some(ErrorKind::NotFound),
        ..FakeDispatcher::default()
    })?;
    let (status, _, body) = get(&router, "/machines/ghost").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("the fake dispatcher refused"));
    Ok(())
}

#[tokio::test]
async fn a_failing_fragment_reports_its_real_status_for_the_client_to_surface() -> TestResult {
    let router = app(FakeDispatcher {
        fail: Some(ErrorKind::Dependency),
        ..FakeDispatcher::default()
    })?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/rows").await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("fs-inline-notice--fail"));
    assert!(
        body.contains("this is a test fixture"),
        "the hint is missing"
    );
    Ok(())
}

#[tokio::test]
async fn a_machine_name_is_escaped_everywhere_it_is_rendered() -> TestResult {
    let router = app(FakeDispatcher {
        machines: json!([{
            "name": "<img src=x onerror=alert(1)>",
            "status": "stopped",
            "image": "\"><script>alert(2)</script>",
            "cpus": 1,
            "memory": "1G",
            "uptime": null,
            "forwards": []
        }]),
        ..FakeDispatcher::default()
    })?;

    let (_, _, body) = get_fragment(&router, "/ui/machines/rows").await?;
    assert!(!body.contains("<img src=x"), "a name was not escaped");
    assert!(
        !body.contains("<script>alert(2)"),
        "an image was not escaped"
    );
    assert!(body.contains("&lt;img src=x"));
    // The link target is percent-encoded, not merely HTML-escaped.
    assert!(body.contains("/machines/%3Cimg%20src%3Dx"));
    Ok(())
}

#[tokio::test]
async fn a_failing_host_check_raises_a_standing_banner() -> TestResult {
    struct FailingHost;
    impl Dispatcher for FailingHost {
        fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            let fallback = FakeDispatcher::default();
            Box::pin(async move {
                if matches!(action, Action::Doctor { .. }) {
                    return events.emit(Event::Result {
                        action: "doctor".to_owned(),
                        payload: json!({
                            "checks": [{ "id": "kvm", "status": "fail", "reason": "/dev/kvm is absent" }]
                        }),
                    });
                }
                fallback.run(action, events).await
            })
        }
    }

    let directory = tempfile::tempdir()?;
    let inputs = PathInputs {
        firestone_home: Some(directory.path().to_path_buf()),
        ..PathInputs::capture()?
    };
    let paths = Paths::from_inputs(&inputs)?;
    let router = super::router(Arc::new(FailingHost), &GlobalConfig::default(), &paths);

    let (_, _, body) = get(&router, "/").await?;
    assert!(body.contains("fs-banner"), "a failing host needs a banner");
    assert!(body.contains("Host blocked"));
    assert!(body.contains(r#"role="alert""#));
    Ok(())
}

#[tokio::test]
async fn the_doctor_report_is_reused_within_its_cache_window() -> TestResult {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingDoctor {
        runs: AtomicUsize,
        inner: FakeDispatcher,
    }

    impl Dispatcher for CountingDoctor {
        fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                if matches!(action, Action::Doctor { .. }) {
                    self.runs.fetch_add(1, Ordering::Relaxed);
                }
                self.inner.run(action, events).await
            })
        }
    }

    let directory = tempfile::tempdir()?;
    let inputs = PathInputs {
        firestone_home: Some(directory.path().to_path_buf()),
        ..PathInputs::capture()?
    };
    let paths = Paths::from_inputs(&inputs)?;
    let dispatcher = Arc::new(CountingDoctor {
        runs: AtomicUsize::new(0),
        inner: FakeDispatcher::default(),
    });
    let router = super::router(
        Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
        &GlobalConfig::default(),
        &paths,
    );

    // Overview renders the check grid and the shell renders the pill; the
    // 30-second pill poll then asks again. Doctor probes the real host, so
    // all of that must come from one run.
    get(&router, "/").await?;
    get_fragment(&router, "/ui/host").await?;
    get_fragment(&router, "/ui/host").await?;

    assert_eq!(
        dispatcher.runs.load(Ordering::Relaxed),
        1,
        "doctor was re-run inside its cache window"
    );
    Ok(())
}

#[tokio::test]
async fn live_regions_swap_with_morph_so_polling_preserves_interaction() -> TestResult {
    let router = app(FakeDispatcher::default())?;

    let (_, _, machines) = get(&router, "/machines").await?;
    assert!(
        machines.contains(r#"hx-swap="morph:innerHTML""#),
        "the machines table must morph, not replace"
    );
    assert!(
        machines.contains(r#"hx-ext="morph""#),
        "the extension is not enabled"
    );

    let (_, _, detail) = get(&router, "/machines/web").await?;
    assert!(detail.contains(r#"hx-swap="morph:innerHTML""#));
    // The stream drawer must sit outside the polled region.
    let live_start = detail.find("id=\"fs-detail-live\"").unwrap_or_default();
    let drawer = detail.find("data-fs-stream-host").unwrap_or_default();
    assert!(
        drawer > live_start,
        "the drawer must follow the live region"
    );
    Ok(())
}

#[tokio::test]
async fn the_ui_never_offers_a_second_mutation_surface() -> TestResult {
    // Start, stop, restart, delete and pull go to the documented /v1 routes.
    // If a template ever grows its own POST for one of them, the contract has
    // been forked and this test says so.
    let router = app(FakeDispatcher::default())?;
    for uri in ["/", "/machines", "/machines/web", "/catalog"] {
        let (_, _, body) = get(&router, uri).await?;
        for forbidden in [
            "hx-post=\"/ui/machines/",
            "hx-delete=",
            "hx-put=",
            "hx-patch=",
        ] {
            assert!(
                !body.contains(forbidden),
                "{uri} declares {forbidden}, forking the mutation contract"
            );
        }
    }

    // The create dialog and its picker fragment are held to the same rule. The
    // dialog's own POST /ui/machines is the one documented exception; the
    // picker is a read and must declare no write at all, and the in-dialog
    // pull goes to /v1/images/pull through app.js like every other pull.
    for uri in ["/ui/machines/new", "/ui/machines/new/images"] {
        let (_, _, body) = get_fragment(&router, uri).await?;
        for forbidden in [
            "hx-post=\"/ui/machines/",
            "hx-delete=",
            "hx-put=",
            "hx-patch=",
        ] {
            assert!(
                !body.contains(forbidden),
                "{uri} declares {forbidden}, forking the mutation contract"
            );
        }
        assert_eq!(
            body.matches("hx-post=").count(),
            usize::from(uri == "/ui/machines/new"),
            "{uri} declares an unexpected write"
        );
    }
    Ok(())
}

// -------------------------------------------------------- live utilization --

#[tokio::test]
async fn the_metrics_strip_renders_only_for_a_running_machine() -> TestResult {
    let running = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&running, "/machines/web").await?;
    assert!(
        body.contains(r#"data-fs-metrics="web""#),
        "a running machine has no utilization strip"
    );

    for status in ["stopped", "created", "failed", "starting"] {
        let router = app(FakeDispatcher {
            machine: machine_view("web", status),
            ..FakeDispatcher::default()
        })?;
        let (_, _, page) = get(&router, "/machines/web").await?;
        assert!(
            !page.contains("data-fs-metrics="),
            "a {status} machine must not poll for metrics"
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_metrics_strip_ships_an_empty_state_and_no_fabricated_numbers() -> TestResult {
    // Counters are cumulative, so a rate needs two samples. Until the client
    // holds two, the server renders the frame and nothing that could be read
    // as a measurement.
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/machines/web").await?;

    assert!(body.contains("data-fs-collecting"), "no empty state");
    assert!(
        body.contains("collecting…"),
        "the empty state is unlabelled"
    );
    for tile in ["cpu", "memory", "guest", "disk"] {
        assert!(
            body.contains(&format!(r#"data-fs-tile="{tile}""#)),
            "the {tile} tile is missing"
        );
    }
    // Every sparkline starts empty and every meter at zero width: a point the
    // client has not derived is never drawn by the server.
    assert!(body.contains(r#"data-fs-spark="cpu" points="""#));
    assert!(body.contains(r#"data-fs-meter x="0" y="0" width="0""#));
    // Every figure is an em dash until the client derives one.
    assert_eq!(
        body.matches("data-fs-tile-value>—<").count(),
        4,
        "the server rendered a utilization figure it did not measure"
    );
    Ok(())
}

#[tokio::test]
async fn the_metrics_strip_sits_outside_the_polled_detail_region() -> TestResult {
    // `#fs-detail-live` is swapped every five seconds. The strip must not be
    // inside it, or each poll would throw away the client's ring buffer.
    let router = app(FakeDispatcher::default())?;
    let (_, _, detail) = get(&router, "/machines/web").await?;

    let strip = detail
        .find("data-fs-metrics=")
        .ok_or("the running machine rendered no utilization strip")?;
    let tabs = detail
        .find(r#"class="fs-tabs""#)
        .ok_or("the detail page rendered no tabs")?;
    let live = detail
        .find(r#"id="fs-detail-live""#)
        .ok_or("the detail page rendered no live region")?;
    assert!(strip > live, "the strip must follow the live region");
    assert!(strip < tabs, "the strip must precede the tabs");

    let (_, _, head) = get_fragment(&router, "/ui/machines/web/head").await?;
    assert!(
        !head.contains("data-fs-metrics="),
        "the polled head fragment must not carry the strip"
    );
    Ok(())
}

#[tokio::test]
async fn the_overview_polls_utilization_for_running_rows_only() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&router, "/ui/overview/machines").await?;

    assert!(body.contains(r#"data-fs-cpu="web""#), "no poll attribute");
    assert!(
        !body.contains(r#"data-fs-cpu="staging-db""#),
        "a stopped machine must not be polled"
    );
    assert!(
        !body.contains(r#"data-fs-cpu="mid-flight""#),
        "a starting machine has no counters to read"
    );
    Ok(())
}

#[tokio::test]
async fn the_overview_caps_how_many_rows_poll_for_metrics() -> TestResult {
    // Every polling row is one request every five seconds against the very
    // host the numbers describe, so the fan-out is bounded rather than
    // proportional to the fleet.
    let fleet: Vec<Value> = (0..12)
        .map(|index| {
            json!({
                "name": format!("node-{index:02}"),
                "status": "running",
                "image": "ubuntu:24.04",
                "cpus": 2,
                "memory": "2G",
                "uptime": "1m",
                "forwards": []
            })
        })
        .collect();
    let router = app(FakeDispatcher {
        machines: Value::Array(fleet),
        ..FakeDispatcher::default()
    })?;

    let (_, _, body) = get_fragment(&router, "/ui/overview/machines").await?;
    assert_eq!(
        body.matches("data-fs-cpu=").count(),
        super::view::OVERVIEW_METRICS_CAP,
        "the poll fan-out is not capped"
    );
    // The cap reads the list in order, so the same rows poll on every refresh.
    assert!(body.contains(r#"data-fs-cpu="node-00""#));
    assert!(body.contains(r#"data-fs-cpu="node-07""#));
    assert!(!body.contains(r#"data-fs-cpu="node-08""#));
    // Every machine still has a row; only the utilization figure is capped.
    assert!(body.contains("node-11"), "a machine was dropped by the cap");
    Ok(())
}
