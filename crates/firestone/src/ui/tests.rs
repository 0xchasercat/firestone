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
    /// The `snapshot-list` payload the snapshots tab reads (M6-26).
    snapshots: Value,
    /// The `images-ls` payload, so the cached-images table can be driven with
    /// more than the one entry the picker needs.
    images: Value,
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
            snapshots: json!({ "snapshots": [] }),
            images: json!([stored_image(
                "sha256-abc",
                "ubuntu:24.04",
                642_000_000,
                None
            )]),
            fail: None,
        }
    }
}

/// One `images-ls` entry, shaped exactly like the stored sidecar the REST
/// route serializes. `kind` is omitted for a disk image, which is what the
/// real sidecar does (§8.5), so an absent field is exercised rather than
/// assumed.
fn stored_image(id: &str, reference: &str, size: u64, kind: Option<&str>) -> Value {
    let mut image = json!({
        "metadata": {
            "version": 1,
            "id": id,
            "generation": 1,
            "source_ref": reference,
            "source_url": null,
            "source_sha256": "a",
            "stored_sha256": "b",
            "architecture": "x86_64",
            "firmware": "edk2",
            "source_format": "qcow2",
            "stored_format": "qcow2",
            "verification_algorithm": null,
            "verification_digest": null,
            "size": size,
            "pulled_at": "2026-09-01T05:00:00Z"
        },
        "path": format!("/data/images/{id}.qcow2")
    });
    if let Some(kind) = kind {
        image["metadata"]["kind"] = json!(kind);
    }
    image
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
                Action::ImageList => ("images-ls", self.images.clone()),
                Action::SnapshotList { .. } => ("snapshot-list", self.snapshots.clone()),
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
    // picker is a read and must declare no write at all, the edit dialog writes
    // through PATCH /v1 and POST /v1/…/resize from app.js, and the in-dialog
    // pull goes to /v1/images/pull like every other pull.
    for uri in [
        "/ui/machines/new",
        "/ui/machines/new/images",
        "/ui/machines/web/edit",
    ] {
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

// ------------------------------------------------------- provisioning (M6-27) --

/// The same view with cloud-init actually configured, so the spec tab can be
/// held to the redaction rule rather than to an empty section.
fn machine_view_with_cloud_init(name: &str, status: &str) -> Value {
    let mut view = machine_view(name, status);
    view["spec"]["cloud_init"] = json!({
        "user_data": null,
        "user_data_inline": "#cloud-config\nruncmd:\n  - [touch, /tmp/SECRET-MARKER]\n",
        "network_config": null,
        "ssh_keys": [],
        "ssh_authorized_keys": [
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 one@host",
            "ssh-rsa AAAAB3NzaC1yc2E two@host"
        ],
        "password": "hunter2-SECRET",
        "ssh_pwauth": true,
        "provisioning": true
    });
    view
}

#[tokio::test]
async fn the_provisioning_section_offers_every_credential_field() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/new").await?;
    assert_eq!(status, StatusCode::OK);

    // The section is collapsible and carries the marker that makes its
    // checkboxes meaningful.
    assert!(body.contains("data-fs-provisioning"));
    assert!(body.contains(">Provisioning</summary>"));
    assert!(body.contains(r#"name="provisioning_section" value="1""#));

    for field in [
        r#"name="user_data_inline""#,
        r#"name="ssh_authorized_keys""#,
        r#"name="password""#,
        r#"name="ssh_pwauth""#,
        r#"name="provisioning""#,
    ] {
        assert!(body.contains(field), "{field} is missing");
    }

    // A password field is a password field: it must never render as text, and
    // must never be pre-filled.
    assert!(body.contains(r#"type="password" id="fs-create-password""#));
    assert!(body.contains(r#"name="password""#) && body.contains(r#"value="""#));

    // Provisioning defaults on, and the consequences of turning it off are
    // stated rather than implied.
    assert!(body.contains(r#"data-fs-provisioning-toggle checked"#));
    assert!(body.contains("the console is the only way in"));
    // Help text explains the password/pwauth split.
    assert!(body.contains("enables console login"));
    assert!(body.contains("SSH password authentication"));
    // The soft cap is a courtesy warning, hidden until it applies.
    assert!(body.contains("data-fs-userdata-over role=\"note\" hidden"));
    assert!(body.contains("32 KiB"));
    // Visibility is the hidden attribute, never a style; the CSP forbids one.
    assert!(!body.contains(" style=\""), "the dialog emitted a style");
    Ok(())
}

#[tokio::test]
async fn a_rejected_create_blanks_the_password_and_keeps_every_other_field() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/ui/machines")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "name=&image=&cpus=0&memory=nope&forward=bad\
                 &user_data_inline=%23cloud-config%0Aruncmd%3A+%5Bid%5D\
                 &ssh_authorized_keys=ssh-ed25519+AAAAC3+me%40host\
                 &password=hunter2&ssh_pwauth=on&provisioning=on&provisioning_section=1",
            ))?,
    )
    .await?;

    assert_eq!(status, StatusCode::OK, "the dialog is re-rendered");

    // Every field problem is still collected in one pass while the new fields
    // ride along.
    for field in ["name", "image"] {
        assert!(
            body.contains(&format!(r#"id="fs-create-{field}-error""#)),
            "{field} lost its error"
        );
    }
    assert!(body.contains("cpus must be 1 through 255"));

    // The password is gone from the response entirely, and the dialog says so
    // rather than letting the user submit a machine without it.
    assert!(!body.contains("hunter2"), "the password was echoed back");
    assert!(body.contains("data-fs-password-cleared"));
    assert!(body.contains("password cleared, re-enter it"));
    assert!(!body.contains("fs:toast"), "a field problem is not a toast");

    // Everything that is not a credential comes back verbatim.
    assert!(body.contains("runcmd: [id]"), "inline user-data was lost");
    assert!(
        body.contains("ssh-ed25519 AAAAC3 me@host"),
        "keys were lost"
    );
    assert!(body.contains(r#"name="ssh_pwauth" value="on" checked"#));
    Ok(())
}

#[tokio::test]
async fn a_submitted_provisioning_section_reaches_the_create_action() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/ui/machines")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "name=web&image=ubuntu%3A24.04\
                 &user_data_inline=%23cloud-config%0D%0Apackages%3A+%5Bjq%5D\
                 &ssh_authorized_keys=ssh-ed25519+AAAAC3+me%40host%0A%0A\
                 &password=hunter2&ssh_pwauth=on&provisioning=on&provisioning_section=1",
            ))?,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty(), "a valid submission renders no dialog");
    Ok(())
}

#[tokio::test]
async fn the_detail_spec_reports_cloud_init_by_shape_and_never_by_value() -> TestResult {
    let router = app(FakeDispatcher {
        machine: machine_view_with_cloud_init("web", "running"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, spec) = get_fragment(&router, "/ui/machines/web/tab/spec").await?;

    // Inline user-data is reported by size. Its content — and the password —
    // must not appear anywhere in the rendered fragment.
    assert!(
        !spec.contains("SECRET-MARKER"),
        "inline user-data content reached the page"
    );
    assert!(
        !spec.contains("hunter2"),
        "the guest password reached the page"
    );
    assert!(spec.contains("54 bytes"), "no byte count was reported");

    // Keys are a count, not a list, and the password is a state, not a value.
    assert!(spec.contains("2 keys"), "the key count is missing");
    assert!(
        !spec.contains("AAAAC3NzaC1lZDI1NTE5"),
        "an authorized key was listed"
    );
    assert!(spec.contains("password"));
    assert!(spec.contains("set"));
    assert!(spec.contains("ssh_pwauth"));

    // An unconfigured machine reports the same rows, emptily rather than not
    // at all.
    let bare = app(FakeDispatcher::default())?;
    let (_, _, empty) = get_fragment(&bare, "/ui/machines/web/tab/spec").await?;
    assert!(empty.contains("user_data_inline") && empty.contains("null"));
    assert!(empty.contains("unset"), "an unset password must say so");
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

/// The embedded runtime has to at least be parseable.
///
/// A merge once spliced a new section into the middle of `applySgr`, leaving
/// three braces unclosed. `app.js` stopped parsing, every line of UI
/// JavaScript stopped running, and nothing in the suite noticed — the Rust
/// side only ever asserted that the bytes were served. This is not a
/// JavaScript parser: it skips comments and string literals and counts braces,
/// which is exactly the damage a bad merge does.
///
/// One caveat, deliberately loud rather than silent: a regular-expression
/// literal containing a brace or a quote would trip this. Write such a pattern
/// with a character class, or build it with `new RegExp`.
#[test]
fn the_embedded_runtime_script_closes_every_block() {
    const APP_JS: &str = include_str!("../../assets/ui/app.js");

    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut in_block_comment = false;
    let bytes: Vec<char> = APP_JS.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        let current = bytes[index];
        let next = bytes.get(index + 1).copied();
        if in_block_comment {
            if current == '*' && next == Some('/') {
                in_block_comment = false;
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if let Some(delimiter) = quote {
            if current == '\\' {
                index += 2;
                continue;
            }
            if current == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        match (current, next) {
            ('/', Some('*')) => {
                in_block_comment = true;
                index += 2;
                continue;
            }
            ('/', Some('/')) => {
                while index < bytes.len() && bytes[index] != '\n' {
                    index += 1;
                }
                continue;
            }
            _ => {}
        }
        match current {
            '"' | '\'' | '`' => quote = Some(current),
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        assert!(depth >= 0, "app.js closes a block it never opened");
        index += 1;
    }

    assert_eq!(depth, 0, "app.js does not close every block it opens");
}

/// The same view with observable drift: the dispatcher, which owns the catalog,
/// reports both the image reference and the forwards as pending. Both flags are
/// server-computed and false for a machine that is not running, so the fixture
/// mirrors that rather than inventing a state the dispatcher cannot produce.
fn machine_view_with_drift(name: &str, status: &str) -> Value {
    let mut view = machine_view(name, status);
    view["spec"]["image"] = json!("ubuntu:24.10");
    view["forwards_pending"] = json!(status == "running");
    view["image_pending"] = json!(status == "running");
    view
}

/// A running machine created from a catalog alias or default: the spec keeps
/// what was typed, the state keeps the canonical reference the pull resolved,
/// and the dispatcher therefore reports no image drift at all.
fn machine_view_with_catalog_alias(name: &str) -> Value {
    let mut view = machine_view(name, "running");
    view["spec"]["image"] = json!("ubuntu");
    view["state"]["image"]["ref"] = json!("ubuntu:24.04");
    view
}

/// Reads one attribute value out of rendered HTML and undoes the escaping
/// minijinja applied, so a JSON payload carried in an attribute can be parsed.
fn attribute(body: &str, name: &str) -> String {
    let needle = format!("{name}=\"");
    let Some(start) = body.find(&needle).map(|at| at + needle.len()) else {
        return String::new();
    };
    let end = body[start..].find('"').map_or(body.len(), |at| start + at);
    body[start..end]
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#x2f;", "/")
        .replace("&#x2F;", "/")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[tokio::test]
async fn the_edit_dialog_prefills_every_field_group_from_the_machine_spec() -> TestResult {
    let router = app(FakeDispatcher {
        machine: machine_view_with_mounts("web", "running"),
        ..FakeDispatcher::default()
    })?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/web/edit").await?;
    assert_eq!(status, StatusCode::OK);

    // Scalars come back as the exact strings the CLI and REST accept.
    assert!(body.contains(r#"name="cpus""#) && body.contains(r#"value="4""#));
    assert!(body.contains(r#"name="memory" value="8G""#));
    assert!(body.contains(r#"name="disk" value="40G""#));
    assert!(body.contains(r#"name="user""#) && body.contains(r#"value="ubuntu""#));
    assert!(body.contains(r#"name="image""#) && body.contains(r#"value="ubuntu:24.04""#));

    // Network, forwards and mounts render the same macros the create dialog
    // uses, against the same names.
    assert!(body.contains(r#"<option value="passt" selected>"#));
    assert!(body.contains(r#"name="tap""#) && body.contains(r#"name="mac""#));
    assert!(body.contains(r#"name="forward""#));
    assert!(
        body.contains("127.0.0.1:8080:80"),
        "forwards were not prefilled"
    );
    assert!(body.contains("data-fs-forward-template"));
    assert!(body.contains("data-fs-mount-template"));
    assert!(body.contains("work:ro"), "the mount row was not prefilled");

    // The dialog opens itself and identifies the machine it edits.
    assert!(body.contains("data-fs-autoopen"));
    assert!(body.contains(r#"data-fs-machine="web""#));
    // The CSP forbids inline styles, here as everywhere else.
    assert!(!body.contains(" style=\""), "the dialog emitted a style");
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
async fn the_edit_dialog_original_matches_every_rendered_control() -> TestResult {
    // patchFromForm sends only what changed, by comparing each control against
    // this projection. If the two ever disagree the dialog would report every
    // field as edited, so the equality is asserted here rather than in a
    // browser.
    let router = app(FakeDispatcher {
        machine: machine_view_with_mounts("web", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/web/edit").await?;
    let original: Value = serde_json::from_str(&attribute(&body, "data-fs-original"))?;

    assert_eq!(original["image"], json!("ubuntu:24.04"));
    assert_eq!(original["cpus"], json!("4"));
    assert_eq!(original["memory"], json!("8G"));
    assert_eq!(original["disk"], json!("40G"));
    assert_eq!(original["user"], json!("ubuntu"));
    assert_eq!(original["net_mode"], json!("passt"));
    assert_eq!(original["forward"], json!("127.0.0.1:8080:80"));
    assert_eq!(original["tap"], json!(""));
    assert_eq!(original["mac"], json!(""));
    assert_eq!(original["mounts"], json!("/srv/project:/work:ro"));

    // Every one of those is also what the control itself carries.
    for value in ["8G", "40G", "ubuntu", "127.0.0.1:8080:80"] {
        assert!(body.contains(value), "{value} is not in the rendered form");
    }
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
async fn a_running_edit_dialog_separates_live_fields_from_restart_fields() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/web/edit").await?;

    assert!(body.contains(r#"data-fs-running="true""#));
    // vCPUs and memory are the only two that change a live VM (§9.5).
    assert!(body.contains(r#"data-fs-applies="live""#));
    assert!(body.contains("/v1/machines/web/resize"));
    // The disk grows at the next start; everything else waits for a restart.
    assert!(body.contains(r#"data-fs-applies="next-start""#));
    assert!(body.contains(r#"data-fs-applies="restart""#));
    assert!(body.contains("applies after restart"));
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

// ---------------------------------------------------------------- terminal --

/// The strict policy, spelled out rather than imported.
///
/// A test that reads the constant it is checking can only prove the constant
/// equals itself. These two literals are the contract SPEC §16.5 states, so a
/// policy edit has to be made here as well, deliberately, in a diff a reviewer
/// reads as a security change.
const STRICT_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; font-src 'self'; connect-src 'self'; \
base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// The terminal page's policy: the same, plus `'wasm-unsafe-eval'`.
const TERMINAL_CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// The router as it is actually served: wrapped in the shared security
/// headers, with no loopback gate (the Unix-socket transport).
fn secured_app(dispatcher: FakeDispatcher) -> Result<Router, FirestoneError> {
    Ok(super::auth::secured(app(dispatcher)?, None))
}

async fn csp_of(router: &Router, uri: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty())?)
        .await?;
    Ok(response
        .headers()
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned))
}

#[tokio::test]
async fn the_terminal_page_renders_its_own_full_window_document() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, content_type, body) = get(&router, "/machines/web/terminal").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(body.starts_with("<!DOCTYPE html>"));
    assert!(body.trim_end().ends_with("</html>"));

    // Its own layout: none of the sidebar shell's chrome is on this page.
    for absent in ["fs-sidebar", "id=\"fs-main-content\"", "hx-ext=\"morph\""] {
        assert!(!body.contains(absent), "the terminal page carries {absent}");
    }
    // And none of the shell's scripts: htmx is not on a page with no swaps.
    assert!(!body.contains("htmx.min.js"));
    assert!(body.contains("/ui/static/term.js?v="));
    assert!(body.contains("/ui/static/theme.js?v="));

    // Both transports, the machine's own state read, and the emulator, are
    // resolved by the server and handed to the page as data.
    for expected in [
        "data-fs-console-url=\"/v1/machines/web/console/ws\"",
        "data-fs-shell-url=\"/v1/machines/web/shell/ws\"",
        "data-fs-state-url=\"/v1/machines/web\"",
        "data-fs-module-url=\"/ui/static/ghostty-web.js?v=",
        "data-fs-wasm-url=\"/ui/static/ghostty-vt.wasm?v=",
        "data-fs-term-tab=\"console\"",
        "data-fs-term-tab=\"shell\"",
        "id=\"fs-term-overlay\"",
        "id=\"fs-term-reconnect\"",
        "href=\"/machines/web\"",
    ] {
        assert!(
            body.contains(expected),
            "the terminal page lacks {expected}"
        );
    }

    // The CSP forbids an inline script and an inline style on this page too.
    assert!(!body.contains("<script>"));
    assert!(!body.contains(" style=\""));
    Ok(())
}

#[tokio::test]
async fn the_terminal_page_opens_the_tab_the_url_asks_for() -> TestResult {
    let router = app(FakeDispatcher::default())?;

    let (_, _, console) = get(&router, "/machines/web/terminal").await?;
    assert!(console.contains("data-fs-tab=\"console\""));

    let (_, _, shell) = get(&router, "/machines/web/terminal?tab=shell").await?;
    assert!(shell.contains("data-fs-tab=\"shell\""));

    // An unknown tab is the console rather than an error: the console is the
    // transport that works before a machine has a network or an sshd.
    let (status, _, unknown) = get(&router, "/machines/web/terminal?tab=nonsense").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(unknown.contains("data-fs-tab=\"console\""));
    Ok(())
}

#[tokio::test]
async fn a_stopped_edit_dialog_promises_nothing_live() -> TestResult {
    let router = app(FakeDispatcher {
        machine: machine_view("web", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, body) = get_fragment(&router, "/ui/machines/web/edit").await?;

    assert!(body.contains(r#"data-fs-running="false""#));
    assert!(
        !body.contains("data-fs-applies"),
        "a stopped machine must not badge a field as live or deferred"
    );
    assert!(body.contains("data-fs-edit-note"));
    assert!(body.contains("the next time it starts"));
    Ok(())
}

#[tokio::test]
async fn the_detail_head_offers_the_edit_dialog_for_every_machine() -> TestResult {
    for status in ["running", "stopped", "starting"] {
        let router = app(FakeDispatcher {
            machine: machine_view("web", status),
            ..FakeDispatcher::default()
        })?;
        let (_, _, head) = get_fragment(&router, "/ui/machines/web/head").await?;
        assert!(
            head.contains(r#"hx-get="/ui/machines/web/edit""#),
            "a {status} machine is not offered an edit dialog"
        );
        assert!(head.contains(r##"hx-target="#fs-dialog-slot""##));
    }
    Ok(())
}

#[tokio::test]
async fn a_stopped_machine_still_renders_a_terminal_page_that_can_explain_itself() -> TestResult {
    let router = app(FakeDispatcher {
        machine: machine_view("staging-db", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (status, _, body) = get(&router, "/machines/staging-db/terminal").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data-status=\"stopped\""));
    assert!(body.contains("id=\"fs-term-overlay\""));
    Ok(())
}

#[tokio::test]
async fn a_missing_machine_has_no_terminal_page() -> TestResult {
    let router = app(FakeDispatcher {
        fail: Some(ErrorKind::NotFound),
        ..FakeDispatcher::default()
    })?;
    let (status, _, body) = get(&router, "/machines/ghost/terminal").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The failure renders in the navigable shell, not in the terminal layout.
    assert!(body.contains("id=\"fs-main-content\""));
    Ok(())
}

#[tokio::test]
async fn only_the_terminal_page_relaxes_the_content_security_policy() -> TestResult {
    let router = secured_app(FakeDispatcher::default())?;

    assert_eq!(
        csp_of(&router, "/machines/web/terminal").await?.as_deref(),
        Some(TERMINAL_CSP),
        "the terminal page must carry exactly the wasm-capable policy"
    );
    assert_eq!(
        csp_of(&router, "/machines/web/terminal?tab=shell")
            .await?
            .as_deref(),
        Some(TERMINAL_CSP)
    );

    // Every other surface keeps the strict policy byte for byte: the screens,
    // a fragment, the assets the terminal itself loads, and the 404 for a
    // path that merely looks like the terminal's.
    for uri in [
        "/",
        "/machines",
        "/machines/web",
        "/catalog",
        "/ui/machines/rows",
        "/ui/machines/web/head",
        "/ui/static/app.css",
        "/ui/static/term.js",
        "/ui/static/ghostty-web.js",
        "/ui/static/ghostty-vt.wasm",
        "/machines/web/terminals",
        "/machines/web/terminal/extra",
    ] {
        assert_eq!(
            csp_of(&router, uri).await?.as_deref(),
            Some(STRICT_CSP),
            "{uri} does not carry the strict policy"
        );
    }

    // The relaxation is one token, and it is not 'unsafe-eval'.
    assert!(!TERMINAL_CSP.contains("'unsafe-eval';"));
    assert!(!TERMINAL_CSP.contains("unsafe-inline"));
    assert_eq!(
        TERMINAL_CSP.replace(" 'wasm-unsafe-eval'", ""),
        STRICT_CSP,
        "the two policies differ by more than the wasm token"
    );
    Ok(())
}

#[tokio::test]
async fn the_detail_head_reports_observable_drift_only_while_running() -> TestResult {
    let router = app(FakeDispatcher {
        machine: machine_view_with_drift("web", "running"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, head) = get_fragment(&router, "/ui/machines/web/head").await?;
    assert!(head.contains(r#"data-fs-drift="server""#), "no drift pill");
    assert!(head.contains("spec drift · image"));
    assert!(head.contains("port forwards"));

    // A machine that is not running has nothing applied to disagree with.
    let stopped = app(FakeDispatcher {
        machine: machine_view_with_drift("web", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, quiet) = get_fragment(&stopped, "/ui/machines/web/head").await?;
    assert!(
        !quiet.contains("data-fs-drift"),
        "a stopped machine cannot have drifted from a running instance"
    );

    // And a running machine whose spec matches what it booted says nothing.
    let matched = app(FakeDispatcher::default())?;
    let (_, _, silent) = get_fragment(&matched, "/ui/machines/web/head").await?;
    assert!(!silent.contains("data-fs-drift"));
    Ok(())
}

#[tokio::test]
async fn a_failed_terminal_page_falls_back_to_the_strict_policy() -> TestResult {
    // The marker is attached to the rendered page, never to the error path,
    // so a machine that cannot be read cannot hand out the weaker policy.
    let router = secured_app(FakeDispatcher {
        fail: Some(ErrorKind::NotFound),
        ..FakeDispatcher::default()
    })?;
    assert_eq!(
        csp_of(&router, "/machines/ghost/terminal")
            .await?
            .as_deref(),
        Some(STRICT_CSP)
    );
    Ok(())
}

#[tokio::test]
async fn the_terminal_link_is_offered_only_while_a_machine_is_running() -> TestResult {
    let running = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&running, "/machines/web").await?;
    assert!(
        body.contains("href=\"/machines/web/terminal\""),
        "a running machine offers no terminal link"
    );
    // The same head rendered as a fragment must agree with the page.
    let (_, _, head) = get_fragment(&running, "/ui/machines/web/head").await?;
    assert!(head.contains("href=\"/machines/web/terminal\""));

    let stopped = app(FakeDispatcher {
        machine: machine_view("staging-db", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, body) = get(&stopped, "/machines/staging-db").await?;
    assert!(
        !body.contains("/terminal\""),
        "a stopped machine offers a terminal that cannot connect"
    );
    Ok(())
}

#[tokio::test]
async fn the_edit_dialog_warns_when_a_shared_folder_sets_a_tag() -> TestResult {
    // HOST:GUEST[:ro] cannot carry a MountSpec tag, so a list rebuild would
    // drop one from rows nobody touched. Untagged mounts say nothing.
    let quiet = app(FakeDispatcher {
        machine: machine_view_with_mounts("web", "running"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, body) = get_fragment(&quiet, "/ui/machines/web/edit").await?;
    assert!(!body.contains("data-fs-mount-tags"));

    let mut tagged = machine_view_with_mounts("web", "running");
    tagged["spec"]["mount"][0]["tag"] = json!("project");
    let router = app(FakeDispatcher {
        machine: tagged,
        ..FakeDispatcher::default()
    })?;
    let (_, _, warned) = get_fragment(&router, "/ui/machines/web/edit").await?;
    assert!(warned.contains("data-fs-mount-tags"), "no tag warning");
    assert!(warned.contains("firestone.toml"));
    Ok(())
}

#[tokio::test]
async fn the_terminal_assets_are_served_with_their_own_media_types() -> TestResult {
    let router = app(FakeDispatcher::default())?;

    let wasm = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ui/static/ghostty-vt.wasm")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(wasm.status(), StatusCode::OK);
    assert_eq!(
        wasm.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/wasm"),
        "a wasm module served as anything else is refused by the browser"
    );
    let bytes = to_bytes(wasm.into_body(), 8 * 1024 * 1024).await?;
    assert_eq!(
        &bytes[..4],
        b"\0asm",
        "the wasm body was not served verbatim"
    );

    for name in [
        "term.js",
        "ghostty-web.js",
        "__vite-browser-external-2447137e.js",
    ] {
        let (status, content_type, _) = get(&router, &format!("/ui/static/{name}")).await?;
        assert_eq!(status, StatusCode::OK, "{name} is not served");
        assert_eq!(content_type, "text/javascript; charset=utf-8", "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn the_terminal_page_offers_no_mutation_surface_of_its_own() -> TestResult {
    // The page is a read. Its only outbound traffic is the two documented /v1
    // WebSocket transports and one GET of the machine it is attached to.
    let router = app(FakeDispatcher::default())?;
    for uri in ["/machines/web/terminal", "/machines/web/terminal?tab=shell"] {
        let (_, _, body) = get(&router, uri).await?;
        for forbidden in [
            "hx-post=",
            "hx-delete=",
            "hx-put=",
            "hx-patch=",
            "<form",
            "/v1/machines/web/start",
            "/v1/machines/web/stop",
        ] {
            assert!(
                !body.contains(forbidden),
                "{uri} declares {forbidden}, forking the mutation contract"
            );
        }
    }

    // term.js reaches for the same three URLs and nothing else under /v1.
    let source = include_str!("../../assets/ui/term.js");
    for forbidden in ["method: \"POST\"", "method: \"DELETE\"", "method: \"PUT\""] {
        assert!(!source.contains(forbidden), "term.js issues a {forbidden}");
    }
    assert_eq!(
        source.matches("fetch(").count(),
        1,
        "term.js makes a request beyond the one machine read"
    );
    Ok(())
}

// ================= snapshots, clone, images, prune surfaces (M6-26) =========

/// Undoes minijinja's HTML escaping across a whole rendered body.
///
/// The escaper rewrites `/` as `&#x2f;`, so every URL in the markup — a chip's
/// `http://…` href, a tab's fragment path — is escaped by the time it reaches
/// a test. A browser reads those entities back; so does this, rather than
/// asserting against a spelling no operator will ever see.
fn decoded(body: &str) -> String {
    body.replace("&#x2f;", "/")
        .replace("&#x2F;", "/")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

/// A snapshot list with both tiers, so the tab can be held to §23's semantics
/// rather than to one row.
fn snapshot_list() -> Value {
    json!({
        "snapshots": [
            {
                "snapshot": "before-upgrade",
                "kind": "cold",
                "created_at": "2026-09-01T01:00:00Z",
                "image_id": "sha256-abc",
                "disk_bytes": 1_500_000_000u64
            },
            {
                "snapshot": "snap-20260902-020000",
                "kind": "warm",
                "created_at": "2026-09-02T02:00:00Z",
                "image_id": "sha256-abc",
                "disk_bytes": 642_000_000u64,
                "memory_bytes": 8_589_934_592u64
            }
        ]
    })
}

/// A fleet whose forwards exercise every chip branch at once: a plain TCP
/// forward, one with a bind address, a UDP forward, a port range, and a
/// running machine whose spec has forwards it has not applied.
fn forward_fleet() -> Value {
    json!([
        {
            "name": "web",
            "status": "running",
            "image": "ubuntu:24.04",
            "cpus": 4,
            "memory": "8G",
            "uptime": "2h 14m",
            "forwards": [
                "8080:80",
                "192.168.1.5:9090:90",
                "udp:5353:5353",
                "8000-8010:80-90"
            ],
            "forwards_pending": true
        },
        {
            "name": "staging-db",
            "status": "stopped",
            "image": "debian:12",
            "cpus": 2,
            "memory": "4G",
            "uptime": null,
            "forwards": ["5432:5432"],
            "forwards_pending": false
        }
    ])
}

#[tokio::test]
async fn the_snapshots_tab_lists_every_snapshot_with_its_tier_and_actions() -> TestResult {
    let router = app(FakeDispatcher {
        snapshots: snapshot_list(),
        ..FakeDispatcher::default()
    })?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/web/tab/snapshots").await?;
    assert_eq!(status, StatusCode::OK);

    // Both rows, both tiers, and the figures the list result carries.
    assert!(body.contains(r#"data-fs-snapshot-row="before-upgrade""#));
    assert!(body.contains(r#"data-fs-snapshot-row="snap-20260902-020000""#));
    assert!(body.contains(r#"data-fs-tier="cold""#));
    assert!(body.contains(r#"data-fs-tier="warm""#));
    assert!(body.contains("1.5 GB"), "the disk size is missing");
    assert!(body.contains("8.5 GB"), "the warm memory size is missing");
    // A cold snapshot captured no memory; it did not capture zero bytes.
    assert!(
        body.contains("—"),
        "a cold memory figure must be an em dash"
    );

    // Every row offers both writes, each behind a confirm.
    assert!(body.contains(r#"data-fs-snapshot-action="restore""#));
    assert!(body.contains(r#"data-fs-snapshot-action="delete""#));
    assert!(body.contains(r#"data-fs-confirm="restore""#));
    assert!(body.contains(r#"data-fs-confirm="delete""#));
    assert!(body.contains(r#"data-fs-snapshot="before-upgrade""#));
    // The restore button carries the machine's own state, because that is what
    // decides whether force is even offered (§23.5).
    assert!(body.contains(r#"data-fs-running="true""#));
    // Colour is never emitted; the tier is a token resolved in CSS.
    assert!(!body.contains(" style=\""), "the tab emitted a style");
    Ok(())
}

#[tokio::test]
async fn the_snapshots_tab_explains_itself_when_a_machine_has_none() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (status, _, body) = get_fragment(&router, "/ui/machines/web/tab/snapshots").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data-fs-snapshot-empty"), "no empty state");
    assert!(body.contains("No snapshots yet"));
    // The empty state still states what a restore does, because that is the
    // fact worth knowing before the first snapshot exists.
    assert!(body.contains("Restoring one replaces all three"));
    assert!(!body.contains("data-fs-snapshot-row"));
    Ok(())
}

#[tokio::test]
async fn the_snapshot_button_names_the_pause_a_running_machine_will_take() -> TestResult {
    // §23: a snapshot of a running machine is warm, and warm means the guest
    // is paused. Discovering that from a stall is not acceptable.
    let running = app(FakeDispatcher::default())?;
    let (_, _, body) = get_fragment(&running, "/ui/machines/web/tab/snapshots").await?;
    assert!(body.contains("Snapshot (brief pause)"));
    assert!(body.contains(r#"data-fs-snapshot-kind="warm""#));
    assert!(body.contains(r#"data-fs-snapshot-action="create""#));

    let stopped = app(FakeDispatcher {
        machine: machine_view("web", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, cold) = get_fragment(&stopped, "/ui/machines/web/tab/snapshots").await?;
    assert!(cold.contains("Take snapshot"));
    assert!(cold.contains(r#"data-fs-snapshot-kind="cold""#));
    assert!(!cold.contains("brief pause"));

    // A machine mid-transition has no coherent disk to copy, so the button is
    // withheld rather than offered as a request that can only be refused.
    for status in ["starting", "stopping", "failed"] {
        let router = app(FakeDispatcher {
            machine: machine_view("web", status),
            ..FakeDispatcher::default()
        })?;
        let (_, _, body) = get_fragment(&router, "/ui/machines/web/tab/snapshots").await?;
        assert!(
            body.contains("data-fs-snapshot-blocked"),
            "a {status} machine was offered a snapshot"
        );
        assert!(!body.contains(r#"data-fs-snapshot-action="create""#));
    }
    Ok(())
}

#[tokio::test]
async fn the_tab_strip_offers_snapshots_and_still_marks_exactly_one_tab() -> TestResult {
    let router = app(FakeDispatcher {
        snapshots: snapshot_list(),
        ..FakeDispatcher::default()
    })?;
    let (status, _, raw) = get(&router, "/machines/web?tab=snapshots").await?;
    let page = decoded(&raw);
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("/ui/machines/web/tab/snapshots"));
    assert!(page.contains(">Snapshots</a>"));
    assert!(
        page.contains("data-fs-snapshot-row"),
        "the tab did not open"
    );
    assert_eq!(
        page.matches(r#"aria-selected="true""#).count(),
        1,
        "exactly one tab may be selected"
    );

    // An unknown tab is still the spec tab, and still exactly one selection.
    let (_, _, unknown) = get(&router, "/machines/web?tab=nonsense").await?;
    assert_eq!(unknown.matches(r#"aria-selected="true""#).count(), 1);
    assert!(!unknown.contains("data-fs-snapshot-row"));
    Ok(())
}

#[tokio::test]
async fn clone_is_offered_from_the_detail_head_and_the_row_menu() -> TestResult {
    let stopped = app(FakeDispatcher {
        machines: forward_fleet(),
        machine: machine_view("staging-db", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, head) = get_fragment(&stopped, "/ui/machines/staging-db/head").await?;
    assert!(head.contains(r#"data-fs-clone="staging-db""#));
    assert!(
        !head.contains("stop staging-db first"),
        "a stopped machine's clone button must not be refused in advance"
    );

    let (_, _, raw) = get_fragment(&stopped, "/ui/machines/rows").await?;
    let rows = decoded(&raw);
    assert!(rows.contains(r#"data-fs-row-menu="staging-db""#));
    assert!(rows.contains(r#"data-fs-clone="staging-db""#));
    // The menu also reaches the snapshots tab for that machine.
    assert!(rows.contains("/machines/staging-db?tab=snapshots"));

    // §24.2: a running source is refused before the lock is taken, so the
    // control says so rather than spending a round trip to be told.
    assert!(
        rows.contains(r#"data-fs-clone="web""#) && rows.contains("stop web first"),
        "a running machine must be offered a disabled clone with a reason"
    );

    // The clone control is never a lifecycle button: `data-fs-machine` means
    // "this dispatches start/stop/restart/delete" and nothing else, so a
    // transitioning machine still carries a clone entry and no lifecycle one.
    let default = app(FakeDispatcher::default())?;
    let (_, _, transitioning) = get_fragment(&default, "/ui/machines/rows").await?;
    assert!(transitioning.contains(r#"data-fs-clone="mid-flight""#));
    assert!(!transitioning.contains(r#"data-fs-machine="mid-flight""#));
    Ok(())
}

#[tokio::test]
async fn the_clone_dialog_takes_a_name_and_an_empty_disk_choice() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/machines/web").await?;

    assert!(body.contains(r#"id="fs-clone""#), "no clone dialog");
    assert!(body.contains(r#"id="fs-clone-name""#));
    assert!(body.contains(r#"id="fs-clone-fresh""#));
    assert!(body.contains("Empty disk"));
    assert!(body.contains("data-fs-clone-source"));
    // The known limitation §24.4 names is stated where the choice is made.
    assert!(decoded(&body).contains("/etc/machine-id"));
    assert!(body.contains("The base image"));
    Ok(())
}

#[tokio::test]
async fn the_catalog_lists_cached_images_with_an_oci_badge_and_a_delete() -> TestResult {
    let router = app(FakeDispatcher {
        images: json!([
            stored_image("sha256-abc", "ubuntu:24.04", 642_000_000, None),
            stored_image(
                "sha256-0123456789abcdef",
                "docker.io/library/alpine:3.20",
                1_500_000_000,
                Some("oci"),
            ),
        ]),
        ..FakeDispatcher::default()
    })?;

    for uri in ["/catalog", "/ui/catalog/images"] {
        let (status, _, body) = get_fragment(&router, uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri} did not render");
        assert!(
            body.contains(r#"data-fs-image-row="sha256-abc""#),
            "{uri} lost a cached image"
        );
        assert!(body.contains("642 MB") && body.contains("1.5 GB"), "{uri}");
        assert!(
            body.contains("2026-09-01T05:00:00Z"),
            "{uri} does not report when the image was pulled"
        );
        // An OCI-derived image is badged; a disk image carries no badge at all.
        assert!(
            body.contains(r#"data-fs-tier="oci""#),
            "{uri} does not badge the OCI image"
        );
        assert_eq!(
            body.matches(r#"data-fs-tier="oci""#).count(),
            1,
            "{uri} badged an image that is not OCI"
        );
        // Every row offers a delete that carries the id the route takes and
        // the reference a human reads in the confirm.
        assert!(body.contains(r#"data-fs-image-action="delete""#), "{uri}");
        assert!(body.contains(r#"data-fs-image="sha256-abc""#), "{uri}");
        assert!(
            body.contains(r#"data-fs-image-ref="ubuntu:24.04""#),
            "{uri}"
        );
        // The id is shortened for reading and kept whole in the title.
        assert!(body.contains("sha256-0123456789ab…"), "{uri}");
        assert!(body.contains(r#"title="sha256-0123456789abcdef""#), "{uri}");
    }

    // An empty store explains itself rather than rendering an empty table.
    let bare = app(FakeDispatcher {
        images: json!([]),
        ..FakeDispatcher::default()
    })?;
    let (_, _, empty) = get_fragment(&bare, "/ui/catalog/images").await?;
    assert!(empty.contains("data-fs-images-empty"));
    assert!(empty.contains("No images cached"));
    Ok(())
}

#[tokio::test]
async fn the_image_delete_confirm_states_what_would_refuse_it() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/catalog").await?;

    assert!(body.contains(r#"id="fs-image-delete""#));
    assert!(body.contains("data-fs-imagedelete-ref"));
    // The refusal an operator will actually hit is named up front: an image a
    // machine or a snapshot pins is kept, and Firestone says what holds it.
    assert!(body.contains("A machine or a snapshot that pins it keeps"));
    assert!(body.contains("names what is holding it"));
    Ok(())
}

#[tokio::test]
async fn the_catalog_offers_both_prune_surfaces_narrow_one_first() -> TestResult {
    // Both live on /catalog because that is the screen that shows what is on
    // disk, and the bounded command is read before the broad one.
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/catalog").await?;

    let images = body
        .find(r#"data-fs-prune="images""#)
        .ok_or("no image prune button")?;
    let system = body
        .find(r#"data-fs-prune="system""#)
        .ok_or("no system prune button")?;
    assert!(images < system, "the broad command must not come first");
    assert!(body.contains("Prune unused images"));
    assert!(body.contains("Free disk space"));

    assert!(body.contains(r#"id="fs-image-prune""#));
    assert!(body.contains("no machine and no snapshot references"));
    Ok(())
}

#[tokio::test]
async fn the_system_prune_dialog_previews_before_it_removes() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, body) = get(&router, "/catalog").await?;

    assert!(body.contains(r#"id="fs-system-prune""#), "no prune dialog");
    // The two tiers, each mapped to the flag it actually sets.
    assert!(body.contains(r#"id="fs-prune-machines""#));
    assert!(body.contains(r#"id="fs-prune-images""#));
    assert!(body.contains("Also remove stopped machines"));
    assert!(body.contains("Also remove unused images"));
    assert!(body.matches("data-fs-prune-option").count() >= 2);

    // The preview region, and a confirm that starts disabled: there is nothing
    // to approve until the dry run has answered.
    assert!(body.contains("data-fs-prune-preview"));
    assert!(body.contains("data-fs-prune-rows"));
    assert!(body.contains("data-fs-prune-total"));
    assert!(body.contains("data-fs-prune-submit disabled"));
    assert!(body.contains("dry_run: true"));
    Ok(())
}

#[tokio::test]
async fn forward_chips_link_a_running_tcp_forward_and_nothing_else() -> TestResult {
    let router = app(FakeDispatcher {
        machines: forward_fleet(),
        ..FakeDispatcher::default()
    })?;
    let (_, _, raw) = get_fragment(&router, "/ui/machines/rows").await?;
    let rows = decoded(&raw);

    // A single TCP forward on a running machine is a link, at loopback when
    // nothing was bound and at the bind address when something was.
    assert!(
        rows.contains(r#"href="http://127.0.0.1:8080" target="_blank" rel="noopener""#),
        "an unbound tcp forward did not become a chip link"
    );
    assert!(rows.contains(r#"href="http://192.168.1.5:9090""#));

    // UDP is never linkified, and neither is a range: both render as chips
    // with no href at all.
    assert!(
        !rows.contains("http://127.0.0.1:5353"),
        "a udp forward was linkified"
    );
    assert!(rows.contains(r#"data-fs-forward="udp:5353:5353""#));
    assert!(
        !rows.contains("http://127.0.0.1:8000"),
        "a port range was linkified"
    );
    assert!(rows.contains(r#"data-fs-forward="8000-8010:80-90""#));

    // A stopped machine has applied nothing a browser can reach.
    assert!(rows.contains(r#"data-fs-forward="5432:5432""#));
    assert!(
        !rows.contains("http://127.0.0.1:5432"),
        "a stopped machine's forward was linkified"
    );
    Ok(())
}

#[tokio::test]
async fn a_pending_forward_set_is_marked_beside_the_chips_not_instead_of_them() -> TestResult {
    let router = app(FakeDispatcher {
        machines: forward_fleet(),
        ..FakeDispatcher::default()
    })?;
    let (_, _, raw) = get_fragment(&router, "/ui/machines/rows").await?;
    let rows = decoded(&raw);

    assert!(rows.contains("data-fs-forwards-pending"), "no marker");
    assert!(rows.contains("pending restart"));
    // Exactly one row is marked: the running one whose spec moved on.
    assert_eq!(rows.matches("data-fs-forwards-pending").count(), 1);
    // The applied set is still shown, because it is what a client can reach.
    assert!(rows.contains(r#"href="http://127.0.0.1:8080""#));
    Ok(())
}

#[tokio::test]
async fn the_detail_head_renders_the_applied_forwards_as_chips() -> TestResult {
    let router = app(FakeDispatcher::default())?;
    let (_, _, raw) = get_fragment(&router, "/ui/machines/web/head").await?;
    let head = decoded(&raw);
    assert!(head.contains(r#"data-fs-forward="127.0.0.1:8080:80""#));
    assert!(head.contains(r#"href="http://127.0.0.1:8080""#));

    // A stopped machine's head reports the same set, unlinked.
    let stopped = app(FakeDispatcher {
        machine: machine_view("web", "stopped"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, quiet_raw) = get_fragment(&stopped, "/ui/machines/web/head").await?;
    let quiet = decoded(&quiet_raw);
    assert!(quiet.contains(r#"data-fs-forward="127.0.0.1:8080:80""#));
    assert!(!quiet.contains("http://127.0.0.1:8080"));
    Ok(())
}

#[tokio::test]
async fn the_palette_offers_the_host_commands_and_verb_matched_machine_commands() -> TestResult {
    let router = app(FakeDispatcher {
        machines: forward_fleet(),
        ..FakeDispatcher::default()
    })?;

    // The two host-wide commands are there for an empty query, because they
    // are what the palette is opened for when nothing is named.
    let (_, _, all) = get_fragment(&router, "/ui/palette").await?;
    assert!(all.contains(r#"data-fs-palette-action="prune-images""#));
    assert!(all.contains(r#"data-fs-palette-action="prune-system""#));
    // Machine-scoped commands are verb-first, so an empty palette is a list of
    // machines rather than two commands for every one of them.
    assert!(!all.contains(r#"data-fs-palette-action="snapshot""#));
    assert!(!all.contains(r#"data-fs-palette-action="clone""#));

    let (_, _, snap) = get_fragment(&router, "/ui/palette?q=snap").await?;
    assert!(snap.contains(r#"data-fs-palette-action="snapshot""#));
    assert!(snap.contains("snapshot web"));
    // The entry carries the machine's state, so the dialog can name the tier
    // it is about to take without a second read.
    assert!(snap.contains(r#"data-fs-running="true""#));

    // A running machine cannot be cloned (§24.2), so it is not offered.
    let (_, _, clone) = get_fragment(&router, "/ui/palette?q=clone").await?;
    assert!(clone.contains("clone staging-db"));
    assert!(
        !clone.contains("clone web"),
        "a running machine was offered a clone that can only be refused"
    );

    let (_, _, prune) = get_fragment(&router, "/ui/palette?q=prune").await?;
    assert!(prune.contains(r#"data-fs-palette-action="prune-images""#));
    assert!(prune.contains(r#"data-fs-palette-action="prune-system""#));
    Ok(())
}

#[tokio::test]
async fn the_new_surfaces_offer_no_mutation_surface_of_their_own() -> TestResult {
    // Same rule as `the_ui_never_offers_a_second_mutation_surface`, applied to
    // every template M6-26 added: snapshots, clone, image delete, both prunes
    // and the palette commands all write to `/v1` from app.js, never to `/ui`.
    let router = app(FakeDispatcher {
        snapshots: snapshot_list(),
        ..FakeDispatcher::default()
    })?;

    for uri in [
        "/machines/web?tab=snapshots",
        "/ui/machines/web/tab/snapshots",
        "/ui/catalog/images",
        "/ui/palette?q=prune",
        "/ui/palette?q=snap",
    ] {
        let (_, _, body) = get_fragment(&router, uri).await?;
        for forbidden in ["hx-post=", "hx-delete=", "hx-put=", "hx-patch=", "<form"] {
            assert!(
                !body.contains(forbidden),
                "{uri} declares {forbidden}, forking the mutation contract"
            );
        }
        assert!(!body.contains(" style=\""), "{uri} emitted an inline style");
    }

    // The shell's dialogs are markup only: they carry `method="dialog"` forms,
    // which submit nowhere, and declare no htmx write at all.
    let (_, _, page) = get(&router, "/catalog").await?;
    for forbidden in [
        "hx-post=\"/ui/",
        "hx-delete=",
        "hx-put=",
        "hx-patch=",
        // A dialog form that named an action would submit somewhere; these
        // name none, so the dialog's only outcome is its returnValue.
        " action=\"",
    ] {
        assert!(
            !page.contains(forbidden),
            "/catalog declares {forbidden}, forking the mutation contract"
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_detail_head_reports_no_image_drift_for_a_catalog_alias() -> TestResult {
    // `firestone run ubuntu` stores `ubuntu` in the spec and the resolved
    // `ubuntu:24.04` in the state. The two strings differ and the machine has
    // not drifted, so the head must stay silent: this pill once accused every
    // default-reference machine of a drift no restart could clear.
    let router = app(FakeDispatcher {
        machine: machine_view_with_catalog_alias("web"),
        ..FakeDispatcher::default()
    })?;
    let (_, _, head) = get_fragment(&router, "/ui/machines/web/head").await?;
    assert!(
        !head.contains("data-fs-drift"),
        "a catalog alias or default reference is not image drift"
    );
    Ok(())
}
