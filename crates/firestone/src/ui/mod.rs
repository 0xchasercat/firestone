//! The embedded web UI.
//!
//! Firestone ships one executable, so the UI ships inside it: templates,
//! stylesheet, scripts and fonts are all compiled in, and the router serves
//! them beside the REST API rather than from a second process.
//!
//! The division of labour is deliberate. This module renders *reads* — the
//! same shared action results the REST routes serialize, projected into HTML.
//! Every lifecycle mutation goes from the browser straight to the documented
//! `/v1` endpoints and renders their NDJSON progress live. There is one
//! mutation surface, one contract, and no second implementation of what it
//! means to start a machine.

mod assets;
pub mod auth;
mod render;
mod routes;
mod view;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Path, State},
    response::Response,
    routing::{get, post},
};
use firestone_core::{
    Action, Dispatcher, DoctorReport, ErrorKind, FirestoneError, GlobalConfig, MachineSpecPatch,
    Paths,
};

use crate::api;

/// How long one doctor report is reused.
///
/// Doctor runs real host probes, and both the top-bar pill and the Overview
/// panel want the same answer. One short-lived cache keeps a 30-second poll
/// from re-probing the host, without ever showing something stale enough to
/// mislead.
const DOCTOR_TTL: Duration = Duration::from_secs(15);

/// Shared state for every UI handler.
#[derive(Clone)]
pub(crate) struct UiState {
    dispatcher: Arc<dyn Dispatcher>,
    create_defaults: MachineSpecPatch,
    listener_label: String,
    build: String,
    doctor: Arc<Mutex<Option<(Instant, DoctorReport)>>>,
}

impl UiState {
    /// Returns a recent doctor report, running the checks only when the
    /// cached one has expired.
    async fn doctor(&self) -> Result<DoctorReport, FirestoneError> {
        if let Some(report) = self.cached_doctor() {
            return Ok(report);
        }

        let payload = api::dispatch_payload(
            &self.dispatcher,
            Action::Doctor {
                fix: false,
                elevation_confirmed: false,
            },
            "doctor",
        )
        .await?;
        let report: DoctorReport = serde_json::from_value(payload).map_err(|error| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("the doctor result did not match the shared contract: {error}"),
            )
            .with_hint("this is a Firestone defect; report it with the version string")
        })?;

        // A poisoned cache is not a reason to fail a page render: drop the
        // memo and serve the freshly computed report.
        if let Ok(mut cache) = self.doctor.lock() {
            *cache = Some((Instant::now(), report.clone()));
        }
        Ok(report)
    }

    fn cached_doctor(&self) -> Option<DoctorReport> {
        let cache = self.doctor.lock().ok()?;
        let (at, report) = cache.as_ref()?;
        (at.elapsed() < DOCTOR_TTL).then(|| report.clone())
    }
}

/// Builds the web UI router.
///
/// Screens are real URLs (`/`, `/machines`, `/machines/{name}`, `/catalog`):
/// one handler serves both the full document and, when htmx asks for it, the
/// fragment alone. Deep links, reload and the back button therefore work
/// without a parallel route table.
pub fn router(dispatcher: Arc<dyn Dispatcher>, config: &GlobalConfig, paths: &Paths) -> Router {
    let state = UiState {
        dispatcher,
        create_defaults: config.defaults.clone(),
        listener_label: paths.serve_socket().display().to_string(),
        build: env!("CARGO_PKG_VERSION").to_owned(),
        doctor: Arc::new(Mutex::new(None)),
    };

    Router::new()
        // Screens.
        .route("/", get(routes::overview))
        .route("/machines", get(routes::machines))
        .route("/machines/{name}", get(routes::detail))
        .route("/catalog", get(routes::catalog))
        // A read like every other screen: the page renders, and the byte
        // stream it then opens goes to the documented `/v1` WebSocket routes.
        .route("/machines/{name}/terminal", get(routes::terminal))
        // Live regions and partial swaps.
        .route("/ui/host", get(routes::host_pill))
        .route("/ui/overview/stats", get(routes::overview_stats))
        .route("/ui/overview/machines", get(routes::overview_machines))
        .route("/ui/machines/rows", get(routes::machine_rows))
        .route("/ui/machines/new", get(routes::create_form))
        .route("/ui/machines/new/images", get(routes::create_form_images))
        .route("/ui/machines", post(routes::create_machine))
        .route("/ui/machines/{name}/head", get(routes::machine_head))
        .route("/ui/machines/{name}/edit", get(routes::edit_form))
        .route("/ui/machines/{name}/tab/{tab}", get(routes::machine_tab))
        .route("/ui/catalog/cards", get(routes::catalog_cards))
        .route("/ui/palette", get(routes::palette))
        .route("/ui/static/{*path}", get(static_asset))
        // `/v1` keeps its stable JSON ErrorEnvelope 404; a mistyped UI URL
        // should land somewhere a person can navigate out of.
        .fallback(not_found)
        .with_state(state)
}

async fn static_asset(Path(path): Path<String>) -> Response {
    assets::serve(&path)
}

async fn not_found(State(state): State<UiState>, headers: axum::http::HeaderMap) -> Response {
    routes::render_not_found(&state, headers).await
}

#[cfg(test)]
mod tests;
