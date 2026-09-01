//! Authentication and hardening for the browser-reachable transport.
//!
//! A Unix listener is authenticated by the socket's mode 0600: the kernel is
//! the boundary and no header is inspected. A loopback TCP listener has no such
//! boundary, so every request is gated here by a per-process session token plus
//! the browser-specific defenses (DNS rebinding, cross-origin mutation) that a
//! plaintext HTTP origin on `127.0.0.1` needs.

use std::{fs::File, hint::black_box, io::Read as _, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::Response,
};
use firestone_core::{ErrorInfo, ErrorKind, FirestoneError};
use serde::Serialize;

const TOKEN_BYTES: usize = 32;
const COOKIE_NAME: &str = "firestone_session";
const URANDOM: &str = "/dev/urandom";
const JSON_CONTENT_TYPE: &str = "application/json";

/// The cookie written by the `?token=` bootstrap.
///
/// `Secure` is deliberately absent: the UI is served over plaintext HTTP on
/// loopback, and a `Secure` cookie would never be stored by the browser.
/// `HttpOnly` keeps the token out of `document.cookie`, `SameSite=Strict`
/// stops any other origin from driving an authenticated navigation, and
/// `Max-Age` bounds the cookie to one day even though the token itself dies
/// with the process.
const COOKIE_ATTRIBUTES: &str = "; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400";

/// The response hardening applied to every response on every listener kind.
///
/// The policy has no `'unsafe-inline'` anywhere. Every script and stylesheet
/// must be a same-origin asset, which is what the shipped UI templates use.
const CONTENT_SECURITY_POLICY: &str = concat!(
    "default-src 'none'; script-src 'self'; style-src 'self'; ",
    "img-src 'self' data:; font-src 'self'; connect-src 'self'; ",
    "base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
);

/// A 32-byte session secret for the loopback transport.
///
/// The bytes are never printed by `Debug` and never compared with `==`.
#[derive(Clone)]
pub struct SessionToken([u8; TOKEN_BYTES]);

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionToken(<redacted>)")
    }
}

impl SessionToken {
    /// Draws one token from the kernel CSPRNG, refusing any short read.
    pub fn generate() -> Result<Self, FirestoneError> {
        let mut file = File::open(URANDOM).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot open '{URANDOM}' to generate a session token"),
            )
            .with_hint("check that /dev/urandom exists and is readable")
            .with_source(source)
        })?;
        let mut bytes = [0_u8; TOKEN_BYTES];
        file.read_exact(&mut bytes).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot read {TOKEN_BYTES} random bytes from '{URANDOM}'"),
            )
            .with_hint("check that /dev/urandom is a working character device")
            .with_source(source)
        })?;
        Ok(Self(bytes))
    }

    /// Renders the token as 64 lowercase hexadecimal characters.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut text = String::with_capacity(TOKEN_BYTES * 2);
        for byte in self.0 {
            let high = byte >> 4;
            let low = byte & 0x0f;
            text.push(hex_digit(high));
            text.push(hex_digit(low));
        }
        text
    }

    /// Parses 64 hexadecimal characters into a token.
    pub fn from_hex(value: &str) -> Result<Self, FirestoneError> {
        parse_hex(value).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Usage,
                format!(
                    "a session token must be exactly {} hexadecimal characters",
                    TOKEN_BYTES * 2
                ),
            )
            .with_hint("regenerate the token file, or delete it and let firestone create one")
        })
    }

    fn matches(&self, candidate: &Self) -> bool {
        constant_time_eq(&self.0, &candidate.0)
    }
}

const fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + value - 10) as char,
    }
}

fn parse_hex(value: &str) -> Option<SessionToken> {
    let bytes = value.as_bytes();
    if bytes.len() != TOKEN_BYTES * 2 {
        return None;
    }
    let mut token = [0_u8; TOKEN_BYTES];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        token[index] = (high << 4) | low;
    }
    Some(SessionToken(token))
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Compares two secrets without an early exit.
///
/// `unsafe_code` is forbidden workspace-wide, so this cannot use a volatile
/// read. Instead every byte pair is folded into one accumulator with no
/// branch, and `black_box` on the accumulator stops the optimizer from
/// rewriting the fold into a short-circuiting comparison. `#[inline(never)]`
/// keeps the fold from being inlined into a caller that could then specialize
/// it against a known-length literal.
#[inline(never)]
fn constant_time_eq(left: &[u8; TOKEN_BYTES], right: &[u8; TOKEN_BYTES]) -> bool {
    let mut difference = 0_u8;
    for index in 0..TOKEN_BYTES {
        difference |= left[index] ^ right[index];
    }
    black_box(difference) == 0
}

/// How the browser-reachable transport authenticates a request.
#[derive(Debug, Clone)]
pub struct UiAuth {
    token: Option<SessionToken>,
}

impl UiAuth {
    /// Authentication is the Unix socket's mode 0600; no header is inspected.
    #[must_use]
    pub const fn trusted() -> Self {
        Self { token: None }
    }

    /// Authentication is a session token carried by cookie, bearer, or query.
    #[must_use]
    pub const fn token(token: SessionToken) -> Self {
        Self { token: Some(token) }
    }

    /// Reports whether this transport carries a session token.
    #[must_use]
    pub const fn is_trusted(&self) -> bool {
        self.token.is_none()
    }

    /// Builds the request gate for a listener bound to `address`.
    ///
    /// Returns `None` for a trusted transport, where no gating happens at all.
    #[must_use]
    pub fn gate(&self, address: SocketAddr) -> Option<LoopbackGate> {
        self.token.clone().map(|token| LoopbackGate {
            token,
            port: address.port(),
        })
    }
}

/// The per-request gate for one bound loopback port.
#[derive(Debug, Clone)]
pub struct LoopbackGate {
    token: SessionToken,
    port: u16,
}

impl LoopbackGate {
    fn host_is_loopback(&self, headers: &HeaderMap) -> bool {
        let Some(host) = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        let port = self.port;
        host == format!("127.0.0.1:{port}")
            || host == format!("localhost:{port}")
            || host == format!("[::1]:{port}")
    }

    /// Kept in step with `host_is_loopback`: an origin the Host allowlist
    /// accepts for navigation must also be accepted for a mutation, or a
    /// viewer on `http://[::1]:PORT/` could load every page and start nothing.
    fn origin_is_loopback(&self, origin: &str) -> bool {
        let port = self.port;
        origin == format!("http://127.0.0.1:{port}")
            || origin == format!("http://localhost:{port}")
            || origin == format!("http://[::1]:{port}")
    }
}

/// Wraps `app` with the loopback gate (when one is supplied) and the shared
/// security headers.
///
/// The headers are the outermost layer so that they are also present on the
/// gate's own 401 and 403 responses.
pub fn secured(app: Router, gate: Option<LoopbackGate>) -> Router {
    let app = match gate {
        Some(gate) => app.layer(middleware::from_fn_with_state(Arc::new(gate), enforce)),
        None => app,
    };
    app.layer(middleware::from_fn(security_headers))
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        ),
        (
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ),
    ] {
        headers.insert(name, value);
    }
    response
}

async fn enforce(
    State(gate): State<Arc<LoopbackGate>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match decide(&gate, request.method(), request.uri(), request.headers()) {
        Decision::Allow => next.run(request).await,
        Decision::Bootstrap { location, token } => bootstrap_response(&location, &token),
        Decision::Reject(rejection) => rejection.into_response(),
    }
}

enum Decision {
    Allow,
    Bootstrap { location: String, token: String },
    Reject(Rejection),
}

struct Rejection {
    status: StatusCode,
    message: &'static str,
    hint: &'static str,
}

impl Rejection {
    /// Builds the same `ErrorEnvelope` body the REST adapter uses.
    ///
    /// The HTTP status is chosen by the transport gate rather than by the
    /// error kind, because 401 and 403 have no stable `ErrorKind` mapping.
    /// The body never echoes the supplied token and never reports how close a
    /// wrong token was. `WWW-Authenticate` is deliberately omitted so browsers
    /// do not raise a basic-auth dialog.
    fn into_response(self) -> Response {
        let envelope = ErrorEnvelope {
            error: ErrorInfo {
                kind: ErrorKind::Usage,
                message: self.message.to_owned(),
                hint: Some(self.hint.to_owned()),
                field: None,
            },
        };
        let body = serde_json::to_vec(&envelope).unwrap_or_else(|_| INTERNAL_ERROR_JSON.to_vec());
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(JSON_CONTENT_TYPE),
        );
        response
    }
}

const INTERNAL_ERROR_JSON: &[u8] = br#"{"error":{"kind":"generic","message":"the loopback gate could not serialize a response","hint":"retry the request; if it fails again, report a Firestone bug"}}"#;

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorInfo,
}

fn decide(gate: &LoopbackGate, method: &Method, uri: &Uri, headers: &HeaderMap) -> Decision {
    // 1. DNS-rebinding defense. A hostile page that resolves its own name to
    //    127.0.0.1 still sends its own Host header, so this runs before any
    //    token is looked at.
    if !gate.host_is_loopback(headers) {
        return Decision::Reject(Rejection {
            status: StatusCode::FORBIDDEN,
            message: "the request Host header is not the bound loopback address",
            hint: "open the printed http://127.0.0.1:PORT/ URL directly",
        });
    }

    // 2. Token acceptance. Both candidates are parsed and compared before any
    //    branch on the result, so a wrong token costs the same as a right one.
    let cookie_ok = cookie_token(headers).is_some_and(|token| gate.token.matches(&token));
    let bearer_ok = bearer_token(headers).is_some_and(|token| gate.token.matches(&token));
    let header_ok = cookie_ok | bearer_ok;

    // 3. Token-in-query bootstrap: move the token from the URL bar into an
    //    HttpOnly cookie on the very first navigation.
    if let Some(candidate) = query_token(uri) {
        let query_ok = parse_hex(&candidate).is_some_and(|token| gate.token.matches(&token));
        if !query_ok {
            return Decision::Reject(unauthorized());
        }
        if matches!(*method, Method::GET | Method::HEAD) {
            return Decision::Bootstrap {
                location: uri.path().to_owned(),
                token: candidate,
            };
        }
    } else if !header_ok {
        return Decision::Reject(unauthorized());
    }

    // 4. Cross-origin defense on mutations. Token-bearing non-browser clients
    //    send neither header and are already gated above.
    if !matches!(*method, Method::GET | Method::HEAD) {
        let site = headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok());
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let allowed = match site {
            Some(site) => site == "same-origin",
            None => origin.is_none_or(|origin| gate.origin_is_loopback(origin)),
        };
        if !allowed {
            return Decision::Reject(Rejection {
                status: StatusCode::FORBIDDEN,
                message: "the request was issued from another origin",
                hint: "drive mutations from the Firestone UI page itself",
            });
        }
    }

    Decision::Allow
}

const fn unauthorized() -> Rejection {
    Rejection {
        status: StatusCode::UNAUTHORIZED,
        message: "the request did not carry the Firestone session token",
        hint: "open the URL printed by `firestone ui`, or send Authorization: Bearer <token>",
    }
}

fn bootstrap_response(location: &str, token: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, value);
    }
    if let Ok(mut value) =
        HeaderValue::from_str(&format!("{COOKIE_NAME}={token}{COOKIE_ATTRIBUTES}"))
    {
        value.set_sensitive(true);
        headers.insert(header::SET_COOKIE, value);
    }
    response
}

fn cookie_token(headers: &HeaderMap) -> Option<SessionToken> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for pair in text.split(';') {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            if name.trim() == COOKIE_NAME {
                return parse_hex(value.trim());
            }
        }
    }
    None
}

fn bearer_token(headers: &HeaderMap) -> Option<SessionToken> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    parse_hex(credential.trim())
}

fn query_token(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            return Some(value.to_owned());
        }
    }
    None
}

/// Reads an existing token file, or creates one holding a fresh token.
///
/// An existing file must be a current-user regular file with mode 0600. The
/// token is never written to a log or an error message.
pub fn load_or_create_token_file(
    path: &std::path::Path,
    uid: u32,
) -> Result<SessionToken, FirestoneError> {
    use std::{
        fs::OpenOptions,
        io::Write as _,
        os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    };

    match OpenOptions::new().read(true).open(path) {
        Ok(mut file) => {
            let metadata = file.metadata().map_err(|source| {
                token_file_error("inspect", path, source).with_hint("check the token file's owner")
            })?;
            if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o600 {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "serve token file '{}' must be a current-user regular file with mode 0600",
                        path.display()
                    ),
                )
                .with_hint("run `chmod 600` on the token file, or delete it and retry"));
            }
            let mut text = String::new();
            file.read_to_string(&mut text).map_err(|source| {
                token_file_error("read", path, source)
                    .with_hint("store the token as 64 hexadecimal characters")
            })?;
            SessionToken::from_hex(text.trim())
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            let token = SessionToken::generate()?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|source| {
                    token_file_error("create", path, source)
                        .with_hint("choose a token path inside a private, writable directory")
                })?;
            file.write_all(token.to_hex().as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|source| {
                    token_file_error("write", path, source)
                        .with_hint("choose a token path inside a private, writable directory")
                })?;
            Ok(token)
        }
        Err(source) => Err(token_file_error("open", path, source)
            .with_hint("check the token file's owner and permissions")),
    }
}

fn token_file_error(
    operation: &str,
    path: &std::path::Path,
    source: std::io::Error,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot {operation} serve token file '{}'", path.display()),
    )
    .with_source(source)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::error::Error;

    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, HeaderValue, Request, StatusCode, header},
        routing::{get, post},
    };
    use tower::ServiceExt as _;

    use super::{
        LoopbackGate, SessionToken, UiAuth, constant_time_eq, load_or_create_token_file, secured,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    const PORT: u16 = 47_318;

    fn sample_token() -> SessionToken {
        SessionToken([7_u8; 32])
    }

    fn gate() -> LoopbackGate {
        LoopbackGate {
            token: sample_token(),
            port: PORT,
        }
    }

    fn app(gate: Option<LoopbackGate>) -> Router {
        secured(
            Router::new()
                .route("/", get(|| async { "ui" }))
                .route("/act", post(|| async { "done" })),
            gate,
        )
    }

    async fn send(app: &Router, request: Request<Body>) -> TestResult<axum::response::Response> {
        Ok(app.clone().oneshot(request).await?)
    }

    fn get_request(uri: &str, host: &str) -> TestResult<Request<Body>> {
        Ok(Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::HOST, host)
            .body(Body::empty())?)
    }

    #[test]
    fn session_token_hex_round_trip_preserves_every_byte() -> TestResult {
        let token = SessionToken::generate()?;
        let hex = token.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(hex.bytes().all(|byte| !byte.is_ascii_uppercase()));
        let parsed = SessionToken::from_hex(&hex)?;
        assert!(token.matches(&parsed));
        Ok(())
    }

    #[test]
    fn session_token_generate_twice_returns_distinct_secrets() -> TestResult {
        let first = SessionToken::generate()?;
        let second = SessionToken::generate()?;
        assert!(!first.matches(&second));
        Ok(())
    }

    #[test]
    fn session_token_from_hex_rejects_wrong_length_and_non_hex_input() {
        assert!(SessionToken::from_hex("").is_err());
        assert!(SessionToken::from_hex(&"a".repeat(63)).is_err());
        assert!(SessionToken::from_hex(&"a".repeat(65)).is_err());
        assert!(SessionToken::from_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn session_token_debug_never_prints_the_secret() {
        let token = SessionToken([0xab_u8; 32]);
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "SessionToken(<redacted>)");
        assert!(!rendered.contains("ab"));
    }

    #[test]
    fn constant_time_eq_matches_plain_equality_on_every_probe() {
        let base = [5_u8; 32];
        assert!(constant_time_eq(&base, &base));
        for index in 0..32 {
            let mut flipped = base;
            flipped[index] ^= 0x01;
            assert!(!constant_time_eq(&base, &flipped));
        }
    }

    #[tokio::test]
    async fn trusted_transport_performs_no_request_gating() -> TestResult {
        let app = app(None);
        let response = send(
            &app,
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[test]
    fn ui_auth_trusted_builds_no_gate_and_token_builds_one() {
        let address = "127.0.0.1:47318".parse().expect("loopback address");
        assert!(UiAuth::trusted().gate(address).is_none());
        assert!(UiAuth::trusted().is_trusted());
        let auth = UiAuth::token(sample_token());
        assert!(!auth.is_trusted());
        let gate = auth.gate(address).expect("token transport has a gate");
        assert_eq!(gate.port, PORT);
    }

    #[tokio::test]
    async fn gated_request_without_any_credential_is_unauthorized() -> TestResult {
        let app = app(Some(gate()));
        let response = send(&app, get_request("/", "127.0.0.1:47318")?).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"]["kind"], "usage");
        assert!(json["error"]["message"].is_string());
        assert!(json["error"]["hint"].is_string());
        Ok(())
    }

    #[tokio::test]
    async fn gated_request_with_hostile_host_header_is_forbidden() -> TestResult {
        let app = app(Some(gate()));
        for host in ["evil.example.com:47318", "firestone.attacker.test:47318"] {
            let request = Request::builder()
                .method("GET")
                .uri("/")
                .header(header::HOST, host)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", sample_token().to_hex()),
                )
                .body(Body::empty())?;
            let response = send(&app, request).await?;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
            let json: serde_json::Value = serde_json::from_slice(&body)?;
            assert_eq!(json["error"]["kind"], "usage");
        }
        Ok(())
    }

    #[tokio::test]
    async fn gated_request_missing_host_header_is_forbidden() -> TestResult {
        let app = app(Some(gate()));
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", sample_token().to_hex()),
            )
            .body(Body::empty())?;
        // http::Request::builder does not add a Host header on its own.
        let response = send(&app, request).await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        Ok(())
    }

    #[tokio::test]
    async fn every_accepted_host_is_also_an_accepted_mutation_origin() -> TestResult {
        // A viewer who can navigate must also be able to act. If the two
        // allowlists drift, pages load and every button silently 403s.
        let gate = gate();
        for host in [
            format!("127.0.0.1:{PORT}"),
            format!("localhost:{PORT}"),
            format!("[::1]:{PORT}"),
        ] {
            let headers = {
                let mut headers = HeaderMap::new();
                headers.insert(header::HOST, HeaderValue::from_str(&host)?);
                headers
            };
            assert!(gate.host_is_loopback(&headers), "host {host}");
            assert!(
                gate.origin_is_loopback(&format!("http://{host}")),
                "origin for {host}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn gated_request_accepts_every_documented_loopback_host() -> TestResult {
        let app = app(Some(gate()));
        for host in ["127.0.0.1:47318", "localhost:47318", "[::1]:47318"] {
            let request = Request::builder()
                .method("GET")
                .uri("/")
                .header(header::HOST, host)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", sample_token().to_hex()),
                )
                .body(Body::empty())?;
            assert_eq!(
                send(&app, request).await?.status(),
                StatusCode::OK,
                "{host}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn bearer_token_accepts_the_secret_and_refuses_a_single_flipped_bit() -> TestResult {
        let app = app(Some(gate()));
        let correct = sample_token().to_hex();
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::AUTHORIZATION, format!("Bearer {correct}"))
            .body(Body::empty())?;
        assert_eq!(send(&app, request).await?.status(), StatusCode::OK);

        let mut wrong = SessionToken([7_u8; 32]);
        wrong.0[31] ^= 0x01;
        let wrong = wrong.to_hex();
        assert_eq!(wrong.len(), correct.len());
        assert_ne!(wrong, correct);
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::AUTHORIZATION, format!("Bearer {wrong}"))
            .body(Body::empty())?;
        assert_eq!(
            send(&app, request).await?.status(),
            StatusCode::UNAUTHORIZED
        );

        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::AUTHORIZATION, "Bearer not-hexadecimal")
            .body(Body::empty())?;
        assert_eq!(
            send(&app, request).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        Ok(())
    }

    #[tokio::test]
    async fn cookie_token_is_accepted_even_when_it_is_not_the_first_cookie() -> TestResult {
        let app = app(Some(gate()));
        let hex = sample_token().to_hex();
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(header::HOST, "127.0.0.1:47318")
            .header(
                header::COOKIE,
                format!("theme=dark; consent=1; firestone_session={hex}; other=2"),
            )
            .body(Body::empty())?;
        assert_eq!(send(&app, request).await?.status(), StatusCode::OK);

        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::COOKIE, format!("firestone_session={hex}"))
            .body(Body::empty())?;
        assert_eq!(send(&app, request).await?.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn query_token_bootstrap_redirects_and_sets_the_session_cookie() -> TestResult {
        let app = app(Some(gate()));
        let hex = sample_token().to_hex();
        let response = send(
            &app,
            get_request(&format!("/?token={hex}"), "127.0.0.1:47318")?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some("/")
        );
        assert_eq!(
            response
                .headers()
                .get(header::SET_COOKIE)
                .and_then(|value| value.to_str().ok()),
            Some(
                format!(
                    "firestone_session={hex}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400"
                )
                .as_str()
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_token_bootstrap_strips_every_query_parameter() -> TestResult {
        let app = app(Some(gate()));
        let hex = sample_token().to_hex();
        let response = send(
            &app,
            get_request(&format!("/?token={hex}&next=1"), "127.0.0.1:47318")?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some("/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn wrong_query_token_is_unauthorized_without_a_cookie() -> TestResult {
        let app = app(Some(gate()));
        let response = send(
            &app,
            get_request(&format!("/?token={}", "0".repeat(64)), "127.0.0.1:47318")?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn cross_site_mutation_is_forbidden_while_same_origin_passes() -> TestResult {
        let app = app(Some(gate()));
        let hex = sample_token().to_hex();

        let cross = Request::builder()
            .method("POST")
            .uri("/act")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::COOKIE, format!("firestone_session={hex}"))
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())?;
        assert_eq!(send(&app, cross).await?.status(), StatusCode::FORBIDDEN);

        let same = Request::builder()
            .method("POST")
            .uri("/act")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::COOKIE, format!("firestone_session={hex}"))
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())?;
        assert_eq!(send(&app, same).await?.status(), StatusCode::OK);

        let foreign_origin = Request::builder()
            .method("POST")
            .uri("/act")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::COOKIE, format!("firestone_session={hex}"))
            .header(header::ORIGIN, "http://evil.example.com")
            .body(Body::empty())?;
        assert_eq!(
            send(&app, foreign_origin).await?.status(),
            StatusCode::FORBIDDEN
        );

        let loopback_origin = Request::builder()
            .method("POST")
            .uri("/act")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::COOKIE, format!("firestone_session={hex}"))
            .header(header::ORIGIN, "http://127.0.0.1:47318")
            .body(Body::empty())?;
        assert_eq!(send(&app, loopback_origin).await?.status(), StatusCode::OK);

        let headerless = Request::builder()
            .method("POST")
            .uri("/act")
            .header(header::HOST, "127.0.0.1:47318")
            .header(header::AUTHORIZATION, format!("Bearer {hex}"))
            .body(Body::empty())?;
        assert_eq!(send(&app, headerless).await?.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn security_headers_are_present_on_every_listener_kind() -> TestResult {
        for gate in [None, Some(gate())] {
            let gated = gate.is_some();
            let app = app(gate);
            let request = if gated {
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .header(header::HOST, "127.0.0.1:47318")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", sample_token().to_hex()),
                    )
                    .body(Body::empty())?
            } else {
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())?
            };
            let response = send(&app, request).await?;
            let headers = response.headers();
            assert_eq!(
                headers
                    .get(header::CONTENT_SECURITY_POLICY)
                    .and_then(|value| value.to_str().ok()),
                Some(super::CONTENT_SECURITY_POLICY)
            );
            assert!(
                !super::CONTENT_SECURITY_POLICY.contains("unsafe-inline"),
                "the policy must never allow inline scripts or styles"
            );
            assert_eq!(
                headers
                    .get(header::REFERRER_POLICY)
                    .and_then(|value| value.to_str().ok()),
                Some("no-referrer")
            );
            assert_eq!(
                headers
                    .get(header::X_CONTENT_TYPE_OPTIONS)
                    .and_then(|value| value.to_str().ok()),
                Some("nosniff")
            );
            assert_eq!(
                headers
                    .get("cross-origin-opener-policy")
                    .and_then(|value| value.to_str().ok()),
                Some("same-origin")
            );
            assert_eq!(
                headers
                    .get("cross-origin-resource-policy")
                    .and_then(|value| value.to_str().ok()),
                Some("same-origin")
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn security_headers_are_present_on_a_rejected_request() -> TestResult {
        let app = app(Some(gate()));
        let response = send(&app, get_request("/", "127.0.0.1:47318")?).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        Ok(())
    }

    #[test]
    fn token_file_is_created_private_then_reread_identically() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("token");
        let uid = nix::unistd::getuid().as_raw();
        let created = load_or_create_token_file(&path, uid)?;
        let metadata = std::fs::metadata(&path)?;
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o7777,
            0o600
        );
        let reread = load_or_create_token_file(&path, uid)?;
        assert!(created.matches(&reread));
        assert_eq!(std::fs::read_to_string(&path)?.trim().len(), 64);
        Ok(())
    }

    #[test]
    fn token_file_with_group_readable_mode_is_refused() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("token");
        let uid = nix::unistd::getuid().as_raw();
        load_or_create_token_file(&path, uid)?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o640))?;
        let error = load_or_create_token_file(&path, uid)
            .expect_err("a group-readable token file must be refused");
        assert_eq!(error.kind(), firestone_core::ErrorKind::Dependency);
        assert!(error.to_string().contains("mode 0600"));
        Ok(())
    }

    #[test]
    fn token_file_with_short_contents_is_refused_as_usage() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("token");
        std::fs::write(&path, b"deadbeef\n")?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let uid = nix::unistd::getuid().as_raw();
        let error =
            load_or_create_token_file(&path, uid).expect_err("a short token file must be refused");
        assert_eq!(error.kind(), firestone_core::ErrorKind::Usage);
        Ok(())
    }
}
