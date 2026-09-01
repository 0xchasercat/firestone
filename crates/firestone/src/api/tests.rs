use std::{
    collections::BTreeSet,
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Method, Request, Response, StatusCode, header},
};
use firestone_core::{
    Action, DispatchFuture, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig,
    ImageRef, LogSource, MachineSpec, MachineSpecPatch, StepId,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

use super::{
    DEFAULT_LOG_LINES, EVENT_BUFFER_CAPACITY, MAX_JSON_BODY_BYTES, MAX_LOG_LINES,
    NDJSON_CONTENT_TYPE, REST_ROUTES, TEXT_CONTENT_TYPE, router,
};
use crate::render::{RenderOptions, Renderer};

#[allow(clippy::expect_used, clippy::unwrap_used)]
type TestResult = Result<(), Box<dyn Error>>;

struct RecordingDispatcher {
    actions: Mutex<Vec<Action>>,
    events: Vec<Event>,
    error: Option<ErrorKind>,
}

impl RecordingDispatcher {
    fn success(result_action: &str, payload: Value, mut events: Vec<Event>) -> Self {
        events.push(Event::Result {
            action: result_action.to_owned(),
            payload,
        });
        Self {
            actions: Mutex::new(Vec::new()),
            events,
            error: None,
        }
    }

    fn failure(kind: ErrorKind, events: Vec<Event>) -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
            events,
            error: Some(kind),
        }
    }

    fn exact_events(events: Vec<Event>) -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
            events,
            error: None,
        }
    }

    fn actions(&self) -> Result<Vec<Action>, FirestoneError> {
        self.actions
            .lock()
            .map(|actions| actions.clone())
            .map_err(|_| FirestoneError::new(ErrorKind::Generic, "mock action lock is poisoned"))
    }
}

impl Dispatcher for RecordingDispatcher {
    fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            self.actions
                .lock()
                .map_err(|_| {
                    FirestoneError::new(ErrorKind::Generic, "mock action lock is poisoned")
                })?
                .push(action);
            for event in self.events.clone() {
                events.emit(event)?;
            }
            match self.error {
                Some(kind) => {
                    Err(FirestoneError::new(kind, "planned failure").with_hint("planned hint"))
                }
                None => Ok(()),
            }
        })
    }
}

fn app<D>(dispatcher: Arc<D>) -> Router
where
    D: Dispatcher + 'static,
{
    router(dispatcher, &GlobalConfig::default())
}

fn request(method: Method, uri: &str, body: Body) -> Result<Request<Body>, axum::http::Error> {
    Request::builder().method(method).uri(uri).body(body)
}

fn json_request(method: Method, uri: &str, body: &Value) -> Result<Request<Body>, Box<dyn Error>> {
    Ok(Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body)?))?)
}

async fn send(app: &Router, request: Request<Body>) -> Result<Response<Body>, Box<dyn Error>> {
    Ok(app.clone().oneshot(request).await?)
}

async fn response_bytes(response: Response<Body>) -> Result<Bytes, Box<dyn Error>> {
    Ok(response.into_body().collect().await?.to_bytes())
}

fn content_type(response: &Response<Body>) -> Option<&str> {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
}

fn step_event(id: &str) -> Event {
    Event::StepStart {
        id: StepId::from(id),
        label: format!("{id} step"),
    }
}

fn expected_ndjson(events: &[Event]) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}
#[tokio::test]
async fn openapi_routes_and_methods_match_the_axum_contract() -> TestResult {
    const OPERATION_KEYS: [&str; 8] = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];

    let document: Value = serde_json::from_str(include_str!("../../../../docs/openapi.json"))?;
    assert_eq!(document["openapi"], "3.1.0");
    let path_items = document["paths"]
        .as_object()
        .ok_or("OpenAPI paths must be an object")?;
    let documented_paths = path_items.keys().cloned().collect::<BTreeSet<_>>();
    let mut documented_operations = BTreeSet::new();
    for (path, value) in path_items {
        let path_item = value
            .as_object()
            .ok_or_else(|| format!("OpenAPI path item {path} must be an object"))?;
        for (method, operation) in path_item {
            if !OPERATION_KEYS.contains(&method.as_str()) {
                return Err(
                    format!("OpenAPI path item {path} has unsupported key {method}").into(),
                );
            }
            if !operation.is_object() {
                return Err(format!("OpenAPI operation {method} {path} must be an object").into());
            }
            documented_operations.insert((path.clone(), method.to_ascii_uppercase()));
        }
    }

    let configured_paths = REST_ROUTES
        .iter()
        .map(|route| route.path.to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        configured_paths.len(),
        REST_ROUTES.len(),
        "REST route table contains a duplicate path"
    );
    assert_eq!(documented_paths, configured_paths);

    let configured_operations = REST_ROUTES
        .iter()
        .flat_map(|route| {
            route
                .authored_methods
                .iter()
                .map(|method| (route.path.to_owned(), (*method).to_owned()))
        })
        .collect::<BTreeSet<_>>();

    let dispatcher = Arc::new(RecordingDispatcher::success(
        "version",
        json!({"unused":true}),
        Vec::new(),
    ));
    let app = app(Arc::clone(&dispatcher));
    let probe = Method::from_bytes(b"FIRESTONE-OPENAPI-PROBE")?;
    for route in REST_ROUTES {
        let uri = route
            .path
            .replace("{name}", "contract")
            .replace("{id}", "contract");
        let response = send(&app, request(probe.clone(), &uri, Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");

        let mut methods = BTreeSet::new();
        for value in response.headers().get_all(header::ALLOW) {
            for method in value.to_str()?.split(',') {
                methods.insert(method.trim().to_owned());
            }
        }
        if methods.is_empty() {
            return Err(format!("registered route {uri} did not advertise Allow").into());
        }

        let mut expected_methods = route
            .authored_methods
            .iter()
            .map(|method| (*method).to_owned())
            .collect::<BTreeSet<_>>();
        if expected_methods.contains("GET") {
            expected_methods.insert("HEAD".to_owned());
        }
        assert_eq!(methods, expected_methods, "{uri}");
    }
    assert_eq!(dispatcher.actions()?, Vec::<Action>::new());
    assert_eq!(documented_operations, configured_operations);
    Ok(())
}
#[tokio::test]
async fn non_stream_routes_project_exact_actions_statuses_and_payloads() -> TestResult {
    struct Case {
        method: Method,
        uri: &'static str,
        body: Option<Value>,
        result_action: &'static str,
        expected_action: Action,
        status: StatusCode,
    }

    let create_spec = MachineSpec {
        cpus: 4,
        ..MachineSpec::default()
    };
    let put_spec = MachineSpec::default();
    let patch = MachineSpecPatch {
        cpus: Some(6),
        ..MachineSpecPatch::default()
    };
    let cases = vec![
        Case {
            method: Method::GET,
            uri: "/v1/version",
            body: None,
            result_action: "version",
            expected_action: Action::Version,
            status: StatusCode::OK,
        },
        Case {
            method: Method::GET,
            uri: "/v1/doctor",
            body: None,
            result_action: "doctor",
            expected_action: Action::Doctor {
                fix: false,
                elevation_confirmed: false,
            },
            status: StatusCode::OK,
        },
        Case {
            method: Method::GET,
            uri: "/v1/machines",
            body: None,
            result_action: "list",
            expected_action: Action::List,
            status: StatusCode::OK,
        },
        Case {
            method: Method::POST,
            uri: "/v1/machines",
            body: Some(json!({"name":"created","spec":{"cpus":4}})),
            result_action: "create",
            expected_action: Action::Create {
                name: "created".to_owned(),
                spec: create_spec,
            },
            status: StatusCode::CREATED,
        },
        Case {
            method: Method::GET,
            uri: "/v1/machines/dev%20one",
            body: None,
            result_action: "show",
            expected_action: Action::Show {
                name: "dev one".to_owned(),
                vmconfig: false,
            },
            status: StatusCode::OK,
        },
        Case {
            method: Method::PUT,
            uri: "/v1/machines/put",
            body: Some(serde_json::to_value(&put_spec)?),
            result_action: "edit",
            expected_action: Action::SetSpec {
                name: "put".to_owned(),
                spec: put_spec,
            },
            status: StatusCode::OK,
        },
        Case {
            method: Method::PATCH,
            uri: "/v1/machines/patch",
            body: Some(serde_json::to_value(&patch)?),
            result_action: "edit",
            expected_action: Action::PatchSpec {
                name: "patch".to_owned(),
                patch,
            },
            status: StatusCode::OK,
        },
        Case {
            method: Method::GET,
            uri: "/v1/machines/vm/vmconfig",
            body: None,
            result_action: "show-vmconfig",
            expected_action: Action::Show {
                name: "vm".to_owned(),
                vmconfig: true,
            },
            status: StatusCode::OK,
        },
        Case {
            method: Method::GET,
            uri: "/v1/catalog",
            body: None,
            result_action: "catalog",
            expected_action: Action::CatalogList,
            status: StatusCode::OK,
        },
        Case {
            method: Method::GET,
            uri: "/v1/images",
            body: None,
            result_action: "images-ls",
            expected_action: Action::ImageList,
            status: StatusCode::OK,
        },
        Case {
            method: Method::POST,
            uri: "/v1/images/prune",
            body: None,
            result_action: "images-prune",
            expected_action: Action::ImagePrune,
            status: StatusCode::OK,
        },
    ];

    for case in cases {
        let payload = json!({"route":case.result_action});
        let dispatcher = Arc::new(RecordingDispatcher::success(
            case.result_action,
            payload.clone(),
            Vec::new(),
        ));
        let app = app(Arc::clone(&dispatcher));
        let request = match case.body {
            Some(body) => json_request(case.method, case.uri, &body)?,
            None => request(case.method, case.uri, Body::empty())?,
        };
        let response = send(&app, request).await?;
        assert_eq!(response.status(), case.status, "{}", case.uri);
        assert_eq!(content_type(&response), Some("application/json"));
        assert_eq!(
            response_bytes(response).await?,
            serde_json::to_vec(&payload)?
        );
        assert_eq!(dispatcher.actions()?, vec![case.expected_action]);
    }
    Ok(())
}

#[tokio::test]
async fn streaming_routes_project_exact_actions_and_ndjson() -> TestResult {
    struct Case {
        uri: &'static str,
        body: Option<Value>,
        result_action: &'static str,
        expected_action: Action,
    }

    let cases = vec![
        Case {
            uri: "/v1/machines/alpha/start",
            body: None,
            result_action: "start",
            expected_action: Action::Start {
                name: "alpha".to_owned(),
                wait: true,
                timeout: Duration::from_secs(60),
            },
        },
        Case {
            uri: "/v1/machines/beta/stop",
            body: Some(json!({"timeout_s":17,"force":true})),
            result_action: "stop",
            expected_action: Action::Stop {
                name: "beta".to_owned(),
                timeout: Duration::from_secs(17),
                force: true,
            },
        },
        Case {
            uri: "/v1/machines/gamma/restart",
            body: None,
            result_action: "restart",
            expected_action: Action::Restart {
                name: "gamma".to_owned(),
                timeout: Duration::from_secs(60),
            },
        },
        Case {
            uri: "/v1/images/pull",
            body: Some(json!({
                "ref":"ubuntu:24.04",
                "sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            })),
            result_action: "images-pull",
            expected_action: Action::ImagePull {
                r#ref: ImageRef::from("ubuntu:24.04"),
                sha256: Some(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                ),
            },
        },
    ];

    for case in cases {
        let progress = step_event("work");
        let result = Event::Result {
            action: case.result_action.to_owned(),
            payload: json!({"ok":true}),
        };
        let dispatcher = Arc::new(RecordingDispatcher::exact_events(vec![
            progress.clone(),
            result.clone(),
        ]));
        let app = app(Arc::clone(&dispatcher));
        let request = match case.body {
            Some(body) => json_request(Method::POST, case.uri, &body)?,
            None => request(Method::POST, case.uri, Body::empty())?,
        };
        let response = send(&app, request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type(&response), Some(NDJSON_CONTENT_TYPE));
        assert_eq!(
            response_bytes(response).await?,
            expected_ndjson(&[progress, result])?
        );
        assert_eq!(dispatcher.actions()?, vec![case.expected_action]);
    }
    Ok(())
}

#[tokio::test]
async fn delete_routes_return_204_and_machine_stop_feedback_streams() -> TestResult {
    let machine = Arc::new(RecordingDispatcher::success(
        "rm",
        json!({"removed":["dev"]}),
        Vec::new(),
    ));
    let response = send(
        &app(Arc::clone(&machine)),
        request(Method::DELETE, "/v1/machines/dev?force=true", Body::empty())?,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(content_type(&response), None);
    assert!(response_bytes(response).await?.is_empty());
    assert_eq!(
        machine.actions()?,
        vec![Action::Remove {
            names: vec!["dev".to_owned()],
            force: true,
        }]
    );

    let image = Arc::new(RecordingDispatcher::success(
        "images-rm",
        json!({"id":"base"}),
        Vec::new(),
    ));
    let response = send(
        &app(Arc::clone(&image)),
        request(Method::DELETE, "/v1/images/base%20id", Body::empty())?,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response_bytes(response).await?.is_empty());
    assert_eq!(
        image.actions()?,
        vec![Action::ImageRemove {
            id: "base id".to_owned(),
            force: false,
        }]
    );

    let progress = step_event("stop");
    let result = Event::Result {
        action: "rm".to_owned(),
        payload: json!({"removed":["running"]}),
    };
    let running = Arc::new(RecordingDispatcher::exact_events(vec![
        progress.clone(),
        result.clone(),
    ]));
    let response = send(
        &app(Arc::clone(&running)),
        request(Method::DELETE, "/v1/machines/running", Body::empty())?,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), Some(NDJSON_CONTENT_TYPE));
    assert_eq!(
        response_bytes(response).await?,
        expected_ndjson(&[progress, result])?
    );
    Ok(())
}

#[tokio::test]
async fn logs_route_projects_query_and_sanitizes_text_chunks() -> TestResult {
    let dispatcher = Arc::new(RecordingDispatcher::success(
        "logs",
        json!({"lines":7}),
        vec![
            Event::Output {
                data: "ok\n\u{1b}[31m\tred\u{1b}[0m\n".to_owned(),
            },
            // A colour sequence split between two Output events is still one
            // sequence, and an OSC is destroyed whichever event carries it.
            Event::Output {
                data: "\u{1b}[3".to_owned(),
            },
            Event::Output {
                data: "2mgreen\u{1b}[0m\u{1b}]0;title\u{7}end".to_owned(),
            },
        ],
    ));
    let response = send(
        &app(Arc::clone(&dispatcher)),
        request(
            Method::GET,
            "/v1/machines/dev/logs?source=virtiofsd%2D2&follow=true&lines=7",
            Body::empty(),
        )?,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), Some(TEXT_CONTENT_TYPE));
    let body = response_bytes(response).await?;
    assert_eq!(
        body.as_ref(),
        "ok\n\u{1b}[31m\tred\u{1b}[0m\n\u{1b}[32mgreen\u{1b}[0m\u{fffd}end".as_bytes()
    );
    // SGR survives; the OSC introducer and its body do not.
    assert!(!body.contains(&b']'));
    assert_eq!(
        dispatcher.actions()?,
        vec![Action::Logs {
            name: "dev".to_owned(),
            source: LogSource::Virtiofsd(2),
            lines: 7,
            follow: true,
        }]
    );
    Ok(())
}

#[tokio::test]
async fn json_accept_aggregates_events_and_result_without_changing_result_bytes() -> TestResult {
    let progress = step_event("boot");
    let result = Event::Result {
        action: "start".to_owned(),
        payload: json!({"name":"dev","status":"running"}),
    };
    let dispatcher = Arc::new(RecordingDispatcher::exact_events(vec![
        progress.clone(),
        result.clone(),
    ]));
    let aggregate_request = Request::builder()
        .method(Method::POST)
        .uri("/v1/machines/dev/start")
        .header(header::ACCEPT, "application/json")
        .body(Body::empty())?;
    let response = send(&app(dispatcher), aggregate_request).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), Some("application/json"));
    assert_eq!(
        serde_json::from_slice::<Value>(&response_bytes(response).await?)?,
        json!({"events":[progress],"result":result})
    );

    let result = Event::Result {
        action: "start".to_owned(),
        payload: json!({"name":"dev","status":"running"}),
    };
    let dispatcher = Arc::new(RecordingDispatcher::exact_events(vec![result.clone()]));
    let response = send(
        &app(dispatcher),
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    let rest = response_bytes(response).await?;
    let mut renderer = Renderer::new(Vec::new(), Vec::new(), RenderOptions::json());
    renderer.emit(result)?;
    let (cli, stderr) = renderer.into_writers();
    assert!(stderr.is_empty());
    assert_eq!(rest.as_ref(), cli.as_slice());
    Ok(())
}

struct RuntimeOwningDispatcher;

impl Dispatcher for RuntimeOwningDispatcher {
    fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .map_err(|source| {
                    FirestoneError::new(ErrorKind::Generic, "cannot build nested runtime probe")
                        .with_source(source)
                })?;
            drop(runtime);
            events.emit(Event::Result {
                action: "start".to_owned(),
                payload: json!({"ok":true}),
            })
        })
    }
}

#[tokio::test]
async fn dispatcher_worker_does_not_nest_blocking_transports_in_tokio() -> TestResult {
    let response = send(
        &app(Arc::new(RuntimeOwningDispatcher)),
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_bytes(response).await?;
    assert!(body.starts_with(br#"{"type":"Result""#));
    Ok(())
}

struct DelayedDispatcher;

impl Dispatcher for DelayedDispatcher {
    fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            events.emit(step_event("first"))?;
            std::thread::sleep(Duration::from_millis(80));
            events.emit(Event::StepDone {
                id: StepId::from("second"),
                detail: None,
                elapsed_ms: 80,
            })?;
            events.emit(Event::Result {
                action: "start".to_owned(),
                payload: json!({"ok":true}),
            })
        })
    }
}

#[tokio::test]
async fn ndjson_body_releases_each_delayed_event_as_one_frame() -> TestResult {
    let response = send(
        &app(Arc::new(DelayedDispatcher)),
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    let mut body = response.into_body();
    let first = body.frame().await.ok_or("missing first frame")??;
    assert_eq!(
        first.into_data().map_err(|_| "first frame is not data")?,
        expected_ndjson(&[step_event("first")])?
    );
    let waiting = Instant::now();
    let second = body.frame().await.ok_or("missing second frame")??;
    assert!(waiting.elapsed() >= Duration::from_millis(40));
    let second = second.into_data().map_err(|_| "second frame is not data")?;
    let expected = Event::StepDone {
        id: StepId::from("second"),
        detail: None,
        elapsed_ms: 80,
    };
    assert_eq!(second.as_ref(), expected_ndjson(&[expected])?);
    let terminal = body.frame().await.ok_or("missing Result frame")??;
    let terminal = terminal
        .into_data()
        .map_err(|_| "terminal frame is not data")?;
    assert!(terminal.starts_with(br#"{"type":"Result""#));
    assert!(body.frame().await.is_none());
    Ok(())
}

struct BurstDispatcher {
    emitted: AtomicUsize,
    completed: AtomicBool,
}

impl Dispatcher for BurstDispatcher {
    fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            for index in 0..1_000_u64 {
                events.emit(Event::Progress {
                    id: StepId::from("burst"),
                    done: index,
                    total: Some(1_000),
                    unit: firestone_core::Unit::Bytes,
                })?;
                self.emitted.fetch_add(1, Ordering::SeqCst);
            }
            events.emit(Event::Result {
                action: "start".to_owned(),
                payload: json!({"ok":true}),
            })?;
            self.completed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn event_channel_applies_bounded_backpressure_and_disconnect_unblocks_it() -> TestResult {
    let dispatcher = Arc::new(BurstDispatcher {
        emitted: AtomicUsize::new(0),
        completed: AtomicBool::new(false),
    });
    let response = send(
        &app(Arc::clone(&dispatcher)),
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        dispatcher.emitted.load(Ordering::SeqCst) <= EVENT_BUFFER_CAPACITY + 1,
        "producer escaped the bounded channel"
    );
    drop(response);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dispatcher.completed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(dispatcher.emitted.load(Ordering::SeqCst), 1_000);
    Ok(())
}

struct MutationDispatcher {
    mutated: AtomicBool,
    completed: AtomicBool,
}

impl Dispatcher for MutationDispatcher {
    fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            events.emit(step_event("prepare"))?;
            std::thread::sleep(Duration::from_millis(50));
            self.mutated.store(true, Ordering::SeqCst);
            events.emit(Event::StepDone {
                id: StepId::from("mutate"),
                detail: None,
                elapsed_ms: 50,
            })?;
            events.emit(Event::Result {
                action: "start".to_owned(),
                payload: json!({"ok":true}),
            })?;
            self.completed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn action_disconnect_does_not_fail_or_cancel_safe_mutation() -> TestResult {
    let dispatcher = Arc::new(MutationDispatcher {
        mutated: AtomicBool::new(false),
        completed: AtomicBool::new(false),
    });
    let response = send(
        &app(Arc::clone(&dispatcher)),
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    drop(response);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dispatcher.completed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert!(dispatcher.mutated.load(Ordering::SeqCst));
    Ok(())
}

struct FollowDispatcher {
    cancelled: AtomicBool,
}

impl Dispatcher for FollowDispatcher {
    fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            events.emit(Event::Output {
                data: "ready\n".to_owned(),
            })?;
            while !events.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn logs_follow_disconnect_cancels_open_ended_read() -> TestResult {
    let dispatcher = Arc::new(FollowDispatcher {
        cancelled: AtomicBool::new(false),
    });
    let response = send(
        &app(Arc::clone(&dispatcher)),
        request(
            Method::GET,
            "/v1/machines/dev/logs?follow=true",
            Body::empty(),
        )?,
    )
    .await?;
    drop(response);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dispatcher.cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    Ok(())
}

struct ConflictDispatcher {
    active: AtomicBool,
    release: Notify,
}

impl Dispatcher for ConflictDispatcher {
    fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            if self.active.swap(true, Ordering::SeqCst) {
                return Err(
                    FirestoneError::new(ErrorKind::Conflict, "another action is active")
                        .with_hint("wait for the active action to finish"),
                );
            }
            events.emit(step_event("active"))?;
            self.release.notified().await;
            events.emit(Event::Result {
                action: "start".to_owned(),
                payload: json!({"ok":true}),
            })
        })
    }
}

#[tokio::test]
async fn concurrent_conflicting_actions_return_shared_409_error() -> TestResult {
    let dispatcher = Arc::new(ConflictDispatcher {
        active: AtomicBool::new(false),
        release: Notify::new(),
    });
    let app = app(Arc::clone(&dispatcher));
    let first_request = request(Method::POST, "/v1/machines/dev/start", Body::empty())?;
    let first = tokio::spawn(app.clone().oneshot(first_request));
    let first_response = first.await??;
    let second = send(
        &app,
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(content_type(&second), Some(NDJSON_CONTENT_TYPE));
    assert_eq!(
        serde_json::from_slice::<Value>(&response_bytes(second).await?)?,
        json!({"error":{
            "kind":"conflict",
            "message":"another action is active",
            "hint":"wait for the active action to finish"
        }})
    );
    dispatcher.release.notify_one();
    let body = response_bytes(first_response).await?;
    assert!(
        body.windows(b"\"type\":\"Result\"".len())
            .any(|window| window == b"\"type\":\"Result\"")
    );
    Ok(())
}

#[tokio::test]
async fn every_shared_error_kind_maps_to_exact_http_status_and_json() -> TestResult {
    let cases = [
        (ErrorKind::Usage, StatusCode::BAD_REQUEST),
        (ErrorKind::InvalidSpec, StatusCode::BAD_REQUEST),
        (ErrorKind::NotFound, StatusCode::NOT_FOUND),
        (ErrorKind::Conflict, StatusCode::CONFLICT),
        (ErrorKind::AlreadyRunning, StatusCode::CONFLICT),
        (ErrorKind::Busy, StatusCode::CONFLICT),
        (ErrorKind::Timeout, StatusCode::GATEWAY_TIMEOUT),
        (ErrorKind::Dependency, StatusCode::SERVICE_UNAVAILABLE),
        (ErrorKind::Checksum, StatusCode::BAD_GATEWAY),
        (ErrorKind::Generic, StatusCode::INTERNAL_SERVER_ERROR),
        (ErrorKind::NotRunning, StatusCode::INTERNAL_SERVER_ERROR),
        (ErrorKind::AlreadyExists, StatusCode::INTERNAL_SERVER_ERROR),
        (ErrorKind::Interrupted, StatusCode::INTERNAL_SERVER_ERROR),
    ];
    for (kind, status) in cases {
        let dispatcher = Arc::new(RecordingDispatcher::failure(kind, Vec::new()));
        let response = send(
            &app(dispatcher),
            request(Method::GET, "/v1/version", Body::empty())?,
        )
        .await?;
        assert_eq!(response.status(), status, "{kind}");
        assert_eq!(content_type(&response), Some("application/json"));
        assert_eq!(
            serde_json::from_slice::<Value>(&response_bytes(response).await?)?,
            json!({"error":{
                "kind":kind.as_str(),
                "message":"planned failure",
                "hint":"planned hint"
            }})
        );
    }
    Ok(())
}

#[tokio::test]
async fn malformed_route_query_body_and_boundaries_return_stable_errors_without_dispatch()
-> TestResult {
    let dispatcher = Arc::new(RecordingDispatcher::success(
        "version",
        json!({"ok":true}),
        Vec::new(),
    ));
    let app = app(Arc::clone(&dispatcher));

    let cases = vec![
        json_request(Method::POST, "/v1/machines", &json!({"secret":"KEY-BYTES"}))?,
        Request::builder()
            .method(Method::POST)
            .uri("/v1/machines")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{\"name\":"))?,
        Request::builder()
            .method(Method::POST)
            .uri("/v1/machines")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("{\"name\":\"dev\",\"spec\":{}}"))?,
        request(
            Method::DELETE,
            "/v1/machines/dev?force=maybe",
            Body::empty(),
        )?,
        request(
            Method::GET,
            &format!("/v1/machines/dev/logs?lines={}", MAX_LOG_LINES + 1),
            Body::empty(),
        )?,
        json_request(
            Method::POST,
            "/v1/machines/dev/start",
            &json!({"timeout_s":0}),
        )?,
        json_request(Method::POST, "/v1/machines/dev/restart", &json!({}))?,
        json_request(Method::PATCH, "/v1/machines/dev", &json!({"unknown":true}))?,
    ];
    for request in cases {
        let response = send(&app, request).await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(content_type(&response), Some("application/json"));
        let body = response_bytes(response).await?;
        assert!(
            !body
                .windows("KEY-BYTES".len())
                .any(|window| window == b"KEY-BYTES")
        );
    }

    let oversized = Request::builder()
        .method(Method::POST)
        .uri("/v1/machines")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(vec![b'a'; MAX_JSON_BODY_BYTES + 1]))?;
    let response = send(&app, oversized).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    for uri in ["/v1/absent", "/v1/version"] {
        let method = if uri.ends_with("version") {
            Method::POST
        } else {
            Method::GET
        };
        let response = send(&app, request(method, uri, Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(content_type(&response), Some("application/json"));
    }

    let invalid_percent = request(Method::GET, "/v1/machines/%GG", Body::empty())?;
    let response = send(&app, invalid_percent).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(dispatcher.actions()?, Vec::<Action>::new());
    Ok(())
}

#[tokio::test]
async fn ndjson_escapes_control_bytes_and_terminal_error_is_last() -> TestResult {
    let progress = Event::Log {
        level: firestone_core::Level::Warn,
        message: "bad\u{1b}\nvalue".to_owned(),
    };
    let dispatcher = Arc::new(RecordingDispatcher::failure(
        ErrorKind::Dependency,
        vec![progress.clone()],
    ));
    let response = send(
        &app(dispatcher),
        request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_bytes(response).await?;
    assert!(!body.contains(&0x1b));
    let records = body.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    assert_eq!(serde_json::from_slice::<Event>(records[0])?, progress);
    assert_eq!(
        serde_json::from_slice::<Value>(records[1])?,
        json!({"error":{
            "kind":"dependency",
            "message":"planned failure",
            "hint":"planned hint"
        }})
    );
    assert!(records[2].is_empty());
    Ok(())
}

#[tokio::test]
async fn duplicate_or_missing_result_becomes_one_terminal_shared_error() -> TestResult {
    let result = Event::Result {
        action: "start".to_owned(),
        payload: json!({"ok":true}),
    };
    for events in [Vec::new(), vec![result.clone(), result.clone()]] {
        let dispatcher = Arc::new(RecordingDispatcher::exact_events(events));
        let response = send(
            &app(dispatcher),
            request(Method::POST, "/v1/machines/dev/start", Body::empty())?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type(&response), Some(NDJSON_CONTENT_TYPE));
        let body = response_bytes(response).await?;
        assert_eq!(body.iter().filter(|byte| **byte == b'\n').count(), 1);
        let terminal = serde_json::from_slice::<Value>(&body[..body.len() - 1])?;
        assert!(terminal.get("type").is_none());
        assert_eq!(terminal["error"]["kind"], "generic");
    }
    Ok(())
}

#[test]
fn documented_defaults_remain_stable() {
    assert_eq!(DEFAULT_LOG_LINES, 200);
}
