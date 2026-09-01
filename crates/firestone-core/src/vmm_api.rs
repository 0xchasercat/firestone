//! Bounded Cloud Hypervisor v53 HTTP client over a Unix socket.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::UnixStream,
    },
    path::Path,
    time::{Duration, Instant},
};

#[cfg(not(any(target_os = "linux", target_os = "android")))]
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{
        AddressFamily, SockFlag, SockProtocol, SockType, UnixAddr, connect, getsockopt, socket,
        sockopt::SocketError,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ErrorKind, FirestoneError, VmmPingProbe};

const MAX_CREATE_BODY_BYTES: usize = 51_200;
const MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;
const MAX_PING_BODY_BYTES: usize = 64 * 1024;
const MAX_INFO_BODY_BYTES: usize = 1024 * 1024;
const MAX_COUNTERS_BODY_BYTES: usize = 64 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const IO_CHUNK_BYTES: usize = 8 * 1024;

/// Cloud Hypervisor's v53 response to `GET /api/v1/vmm.ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmmPingResponse {
    pub build_version: String,
    pub version: String,
    pub pid: i64,
    pub features: Vec<String>,
}

/// Cloud Hypervisor v53 VM states returned by `GET /api/v1/vm.info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    Created,
    Running,
    Shutdown,
    Paused,
    BreakPoint,
}

/// The bounded subset of Cloud Hypervisor's v53 `VmInfoResponse` used by Firestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmInfo {
    pub config: Value,
    pub state: VmState,
    pub memory_actual_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_tree: Option<Value>,
}

/// Cloud Hypervisor's v53 `VmResize` body. Absent fields leave that resource
/// untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct VmResizeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_vcpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_ram: Option<u64>,
}

/// A one-request-per-connection Cloud Hypervisor v53 API client.
///
/// Each method uses one absolute deadline for connect, all partial writes, and
/// all response reads. Response headers are limited to 16 KiB. Ping and error
/// bodies are limited to 64 KiB, while `vm.info` is limited to 1 MiB. The
/// `vm.create` JSON request is limited to the pinned micro-http maximum of
/// 51,200 bytes.
#[derive(Debug, Clone, Copy)]
pub struct VmmApi<'a> {
    api_socket: &'a Path,
    timeout: Duration,
}

impl<'a> VmmApi<'a> {
    #[must_use]
    pub const fn new(api_socket: &'a Path, timeout: Duration) -> Self {
        Self {
            api_socket,
            timeout,
        }
    }

    /// Pings the VMM and decodes its v53 identity response.
    pub fn vmm_ping(&self) -> Result<VmmPingResponse, FirestoneError> {
        self.vmm_ping_inner()
            .map_err(|failure| self.firestone_error(Endpoint::VmmPing, failure))
    }

    /// Creates a VM from the exact persisted v53 `VmConfig` JSON bytes.
    ///
    /// The body is not parsed or reserialized here. Callers can therefore prove
    /// that `vm.create` received the byte sequence published as vmconfig.json.
    pub fn vm_create(&self, config: &[u8]) -> Result<(), FirestoneError> {
        let endpoint = Endpoint::VmCreate;
        if config.len() > MAX_CREATE_BODY_BYTES {
            return Err(self.firestone_error(endpoint, ClientFailure::RequestBodyTooLarge));
        }
        self.request_inner(endpoint, Some(config))
            .map(|_| ())
            .map_err(|failure| self.firestone_error(endpoint, failure))
    }

    /// Boots the VM previously created through `vm.create`.
    pub fn vm_boot(&self) -> Result<(), FirestoneError> {
        self.empty_request(Endpoint::VmBoot)
    }

    /// Returns the VM's v53 configuration and runtime state.
    pub fn vm_info(&self) -> Result<VmInfo, FirestoneError> {
        let endpoint = Endpoint::VmInfo;
        let response = self
            .request_inner(endpoint, None)
            .map_err(|failure| self.firestone_error(endpoint, failure))?;
        decode_json(response.body(), "vm.info JSON response")
            .map_err(|failure| self.firestone_error(endpoint, failure))
    }

    /// Returns the VM's cumulative per-device counters.
    ///
    /// Cloud Hypervisor v53 keys the outer map by device id (`_disk0`) and the
    /// inner map by counter name. Values are not interpreted here; callers
    /// project them and drop the saturating sentinels v53 uses for counters a
    /// device has never exercised.
    pub fn vm_counters(&self) -> Result<BTreeMap<String, BTreeMap<String, u64>>, FirestoneError> {
        let endpoint = Endpoint::VmCounters;
        let response = self
            .request_inner(endpoint, None)
            .map_err(|failure| self.firestone_error(endpoint, failure))?;
        decode_json(response.body(), "vm.counters JSON response")
            .map_err(|failure| self.firestone_error(endpoint, failure))
    }

    /// Resizes a booted VM's vCPU count and RAM in place.
    ///
    /// Cloud Hypervisor v53 answers `PUT /api/v1/vm.resize` with 204 and no
    /// body. `desired_ram` may only reach the boot size plus the `hotplug_size`
    /// declared in the boot configuration, and `desired_vcpus` may only reach
    /// `cpus.max_vcpus`; the caller checks that headroom before asking.
    pub fn vm_resize(
        &self,
        desired_vcpus: Option<u8>,
        desired_ram: Option<u64>,
    ) -> Result<(), FirestoneError> {
        let endpoint = Endpoint::VmResize;
        let request = VmResizeRequest {
            desired_vcpus: desired_vcpus.map(u32::from),
            desired_ram,
        };
        let body = serde_json::to_vec(&request).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                "cannot encode the VMM API vm.resize request body",
            )
            .with_hint("report this Firestone serialization bug")
            .with_source(source)
        })?;
        if body.len() > MAX_CREATE_BODY_BYTES {
            return Err(self.firestone_error(endpoint, ClientFailure::RequestBodyTooLarge));
        }
        self.request_inner(endpoint, Some(&body))
            .map(|_| ())
            .map_err(|failure| self.firestone_error(endpoint, failure))
    }

    /// Triggers the guest ACPI power button.
    pub fn vm_power_button(&self) -> Result<(), FirestoneError> {
        self.empty_request(Endpoint::VmPowerButton)
    }

    /// Tears down the VM while leaving the VMM process available.
    pub fn vm_shutdown(&self) -> Result<(), FirestoneError> {
        self.empty_request(Endpoint::VmShutdown)
    }

    /// Terminates the VMM process.
    ///
    /// Cloud Hypervisor v53 returns HTTP 200 with `Content-Length: 0` here,
    /// despite its OpenAPI document advertising 204.
    pub fn vmm_shutdown(&self) -> Result<(), FirestoneError> {
        self.empty_request(Endpoint::VmmShutdown)
    }

    fn empty_request(&self, endpoint: Endpoint) -> Result<(), FirestoneError> {
        self.request_inner(endpoint, None)
            .map(|_| ())
            .map_err(|failure| self.firestone_error(endpoint, failure))
    }

    fn vmm_ping_inner(&self) -> Result<VmmPingResponse, ClientFailure> {
        let response = self.request_inner(Endpoint::VmmPing, None)?;
        decode_json(response.body(), "vmm.ping JSON response")
    }

    fn vmm_ping_for_liveness(&self) -> Result<bool, FirestoneError> {
        match self.request_inner(Endpoint::VmmPing, None) {
            Ok(_) => Ok(true),
            Err(failure) if failure.is_liveness_negative() => Ok(false),

            Err(failure) => {
                let error = self.firestone_error(Endpoint::VmmPing, failure);
                if error.kind() == ErrorKind::NotRunning {
                    Err(FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("VMM liveness is ambiguous: {}", error.message()),
                    )
                    .with_hint("preserve runtime evidence and retry the liveness probe")
                    .with_source(error))
                } else {
                    Err(error)
                }
            }
        }
    }

    fn request_inner(
        &self,
        endpoint: Endpoint,
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, ClientFailure> {
        let started = Instant::now();
        let deadline = started
            .checked_add(self.timeout)
            .ok_or(ClientFailure::DeadlineOutOfRange)?;
        ensure_before_deadline(deadline, "starting request")?;

        let mut stream = connect_socket(self.api_socket, deadline)?;
        let request_header = request_header(endpoint, body.map_or(0, <[u8]>::len))?;
        write_until_deadline(
            &mut stream,
            request_header.as_bytes(),
            deadline,
            "writing request headers",
        )?;
        if let Some(body) = body {
            write_until_deadline(&mut stream, body, deadline, "writing request body")?;
        }

        read_response(&mut stream, endpoint, deadline)
    }

    fn firestone_error(&self, endpoint: Endpoint, failure: ClientFailure) -> FirestoneError {
        let request = format!("{} {}", endpoint.method(), endpoint.path());
        let socket = self.api_socket.display();
        match failure {
            ClientFailure::Deadline { phase } => FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "VMM API {request} timed out after {} ms while {phase} on socket {socket}",
                    self.timeout.as_millis()
                ),
            )
            .with_hint("check the cloud-hypervisor process and its API socket"),
            ClientFailure::DeadlineOutOfRange => FirestoneError::new(
                ErrorKind::Usage,
                format!("VMM API {request} deadline is out of range"),
            )
            .with_hint("use a finite VMM API timeout"),
            ClientFailure::Io { phase, source } => {
                let kind = if socket_error_is_unresponsive(source.kind()) {
                    ErrorKind::NotRunning
                } else {
                    ErrorKind::Generic
                };
                FirestoneError::new(
                    kind,
                    format!("cannot {phase} for VMM API {request} on socket {socket}"),
                )
                .with_hint("start the machine or inspect its cloud-hypervisor log")
                .with_source(source)
            }
            ClientFailure::Protocol(detail) => FirestoneError::new(
                ErrorKind::Generic,
                format!("invalid VMM API {request} response from socket {socket}: {detail}"),
            )
            .with_hint("the socket must speak the pinned Cloud Hypervisor v53.0 HTTP contract"),
            ClientFailure::HttpResponse {
                status,
                body_preview,
                detail,
                timed_out,
            } => {
                let detail = detail.map_or_else(String::new, |detail| format!("; {detail}"));
                let kind = if timed_out {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::Generic
                };
                FirestoneError::new(
                    kind,
                    format!(
                        "VMM API {request} returned HTTP status {status}; body: \"{body_preview}\"{detail}"
                    ),
                )
                .with_hint("inspect the cloud-hypervisor log for this machine")
            }
            ClientFailure::InvalidJson { label, source } => FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot decode {label} from VMM API {request} on socket {socket}"),
            )
            .with_hint("the response must match the pinned Cloud Hypervisor v53.0 JSON schema")
            .with_source(source),
            ClientFailure::RequestBodyTooLarge => FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!(
                    "VMM API {request} JSON body exceeds the {MAX_CREATE_BODY_BYTES}-byte micro-http limit"
                ),
            )
            .with_hint("reduce the VMM configuration or config overlay"),
        }
    }
}

/// Liveness adapter that uses the same bounded v53 client as lifecycle calls.
#[derive(Debug, Clone, Copy)]
pub struct VmmApiLivenessProbe {
    timeout: Duration,
}

impl VmmApiLivenessProbe {
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl VmmPingProbe for VmmApiLivenessProbe {
    fn ping(&self, api_socket: &Path) -> Result<bool, FirestoneError> {
        VmmApi::new(api_socket, self.timeout).vmm_ping_for_liveness()
    }
}

#[derive(Debug, Clone, Copy)]
enum Endpoint {
    VmmPing,
    VmCreate,
    VmBoot,
    VmInfo,
    VmCounters,
    VmPowerButton,
    VmShutdown,
    VmmShutdown,
    VmResize,
}

impl Endpoint {
    const fn method(self) -> &'static str {
        match self {
            Self::VmmPing | Self::VmInfo | Self::VmCounters => "GET",
            Self::VmCreate
            | Self::VmBoot
            | Self::VmPowerButton
            | Self::VmShutdown
            | Self::VmmShutdown
            | Self::VmResize => "PUT",
        }
    }

    const fn path(self) -> &'static str {
        match self {
            Self::VmmPing => "/api/v1/vmm.ping",
            Self::VmCreate => "/api/v1/vm.create",
            Self::VmBoot => "/api/v1/vm.boot",
            Self::VmInfo => "/api/v1/vm.info",
            Self::VmCounters => "/api/v1/vm.counters",
            Self::VmPowerButton => "/api/v1/vm.power-button",
            Self::VmShutdown => "/api/v1/vm.shutdown",
            Self::VmmShutdown => "/api/v1/vmm.shutdown",
            Self::VmResize => "/api/v1/vm.resize",
        }
    }

    const fn expected_status(self) -> u16 {
        match self {
            Self::VmmPing | Self::VmInfo | Self::VmCounters | Self::VmmShutdown => 200,
            Self::VmCreate
            | Self::VmBoot
            | Self::VmPowerButton
            | Self::VmShutdown
            | Self::VmResize => 204,
        }
    }

    const fn success_body_limit(self) -> usize {
        match self {
            Self::VmmPing => MAX_PING_BODY_BYTES,
            Self::VmInfo => MAX_INFO_BODY_BYTES,
            Self::VmCounters => MAX_COUNTERS_BODY_BYTES,
            Self::VmCreate
            | Self::VmBoot
            | Self::VmPowerButton
            | Self::VmShutdown
            | Self::VmmShutdown
            | Self::VmResize => 0,
        }
    }

    const fn has_request_body(self) -> bool {
        matches!(self, Self::VmCreate | Self::VmResize)
    }
}

#[derive(Debug)]
enum ClientFailure {
    Deadline {
        phase: &'static str,
    },
    DeadlineOutOfRange,
    Io {
        phase: &'static str,
        source: io::Error,
    },
    Protocol(String),
    HttpResponse {
        status: u16,
        body_preview: String,
        detail: Option<String>,
        timed_out: bool,
    },
    InvalidJson {
        label: &'static str,
        source: serde_json::Error,
    },
    RequestBodyTooLarge,
}

impl ClientFailure {
    fn is_liveness_negative(&self) -> bool {
        matches!(
            self,
            Self::Io { source, .. }
                if matches!(
                    source.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                )
        )
    }
}

struct HttpResponse {
    bytes: Vec<u8>,
    body_start: usize,
}

impl HttpResponse {
    fn body(&self) -> &[u8] {
        &self.bytes[self.body_start..]
    }
}

struct ParsedHead {
    status: u16,
    content_length: Option<usize>,
}

fn decode_json<T>(body: &[u8], label: &'static str) -> Result<T, ClientFailure>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(body).map_err(|source| ClientFailure::InvalidJson { label, source })
}

fn request_header(endpoint: Endpoint, body_len: usize) -> Result<String, ClientFailure> {
    let mut header = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n",
        endpoint.method(),
        endpoint.path()
    );
    if endpoint.has_request_body() {
        write!(
            header,
            "Content-Type: application/json\r\nContent-Length: {body_len}\r\n"
        )
        .map_err(|_| ClientFailure::Protocol("cannot build request headers".to_string()))?;
    } else if body_len != 0 {
        return Err(ClientFailure::Protocol(
            "body supplied for a bodyless VMM API endpoint".to_string(),
        ));
    }
    header.push_str("\r\n");
    Ok(header)
}

fn connect_socket(api_socket: &Path, deadline: Instant) -> Result<UnixStream, ClientFailure> {
    let address = UnixAddr::new(api_socket).map_err(|source| ClientFailure::Io {
        phase: "address socket",
        source: io::Error::from(source),
    })?;

    loop {
        ensure_before_deadline(deadline, "connecting")?;
        let flags = {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                SockFlag::empty()
            }
        };
        let descriptor = socket(
            AddressFamily::Unix,
            SockType::Stream,
            flags,
            None::<SockProtocol>,
        )
        .map_err(|source| ClientFailure::Io {
            phase: "create socket",
            source: io::Error::from(source),
        })?;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            fcntl(&descriptor, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(|source| {
                ClientFailure::Io {
                    phase: "set socket nonblocking",
                    source: io::Error::from(source),
                }
            })?;
            fcntl(&descriptor, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| {
                ClientFailure::Io {
                    phase: "set socket close-on-exec",
                    source: io::Error::from(source),
                }
            })?;
        }
        match connect(descriptor.as_raw_fd(), &address) {
            Ok(()) => return Ok(UnixStream::from(descriptor)),
            Err(Errno::EINPROGRESS | Errno::EALREADY) => {
                let stream = UnixStream::from(descriptor);
                wait_for_socket(&stream, PollFlags::POLLOUT, deadline, "connecting")?;
                let pending =
                    getsockopt(&stream, SocketError).map_err(|source| ClientFailure::Io {
                        phase: "inspect connection",
                        source: io::Error::from(source),
                    })?;
                if pending == 0 {
                    return Ok(stream);
                }
                return Err(ClientFailure::Io {
                    phase: "connect",
                    source: io::Error::from_raw_os_error(pending),
                });
            }
            Err(Errno::EAGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(ClientFailure::Deadline {
                        phase: "connecting",
                    });
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(source) => {
                return Err(ClientFailure::Io {
                    phase: "connect",
                    source: io::Error::from(source),
                });
            }
        }
    }
}

fn write_until_deadline(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
    phase: &'static str,
) -> Result<(), ClientFailure> {
    let mut written = 0;
    while written < bytes.len() {
        ensure_before_deadline(deadline, phase)?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(ClientFailure::Io {
                    phase,
                    source: io::Error::new(
                        io::ErrorKind::WriteZero,
                        "VMM API socket wrote zero bytes",
                    ),
                });
            }
            Ok(count) => written += count,
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                wait_for_socket(stream, PollFlags::POLLOUT, deadline, phase)?;
            }
            Err(source) if source.kind() == io::ErrorKind::TimedOut => {
                return Err(ClientFailure::Deadline { phase });
            }
            Err(source) => return Err(ClientFailure::Io { phase, source }),
        }
    }
    ensure_before_deadline(deadline, phase)
}

fn read_response(
    stream: &mut UnixStream,
    endpoint: Endpoint,
    deadline: Instant,
) -> Result<HttpResponse, ClientFailure> {
    let mut bytes = Vec::with_capacity(MAX_RESPONSE_HEADER_BYTES + 1);
    let body_start = loop {
        if let Some(offset) = find_header_terminator(&bytes) {
            let body_start = offset + 4;
            if body_start > MAX_RESPONSE_HEADER_BYTES {
                return Err(ClientFailure::Protocol(format!(
                    "response headers exceed {MAX_RESPONSE_HEADER_BYTES} bytes"
                )));
            }
            break body_start;
        }
        if bytes.len() > MAX_RESPONSE_HEADER_BYTES {
            return Err(ClientFailure::Protocol(format!(
                "response headers exceed {MAX_RESPONSE_HEADER_BYTES} bytes"
            )));
        }
        let remaining = MAX_RESPONSE_HEADER_BYTES + 1 - bytes.len();
        let mut chunk = [0_u8; IO_CHUNK_BYTES];
        let chunk_length = remaining.min(chunk.len());
        let read = read_until_deadline(
            stream,
            &mut chunk[..chunk_length],
            deadline,
            "reading response headers",
        )?;
        if read == 0 {
            return Err(ClientFailure::Protocol(
                "response ended before the header terminator".to_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    };

    let head = parse_response_head(&bytes[..body_start - 4])?;
    let buffered_body = bytes.len() - body_start;
    let body_length = if head.status == 204 {
        if head.content_length.is_some() {
            return Err(http_response_failure(
                head.status,
                &bytes[body_start..],
                Some("HTTP 204 response must not include Content-Length".to_owned()),
            ));
        }
        if buffered_body != 0 {
            return Err(http_response_failure(
                head.status,
                &bytes[body_start..],
                Some("HTTP 204 response includes unexpected body bytes".to_owned()),
            ));
        }
        0
    } else {
        match head.content_length {
            Some(length) => length,
            None => {
                return Err(http_response_failure(
                    head.status,
                    &bytes[body_start..],
                    Some(format!(
                        "HTTP {} response is missing Content-Length",
                        head.status
                    )),
                ));
            }
        }
    };

    let expected_status = head.status == endpoint.expected_status();
    let body_limit = if expected_status {
        endpoint.success_body_limit()
    } else {
        MAX_ERROR_BODY_BYTES
    };
    let body_too_large = body_length > body_limit;
    if body_too_large && expected_status {
        return Err(http_response_failure(
            head.status,
            &bytes[body_start..],
            Some(format!(
                "HTTP {} response body length {body_length} exceeds the {body_limit}-byte limit",
                head.status
            )),
        ));
    }
    let oversized_detail = body_too_large.then(|| {
        format!(
            "HTTP {} response body length {body_length} exceeds the {body_limit}-byte limit; body preview truncated",
            head.status
        )
    });
    if buffered_body > body_length {
        return Err(http_response_failure(
            head.status,
            &bytes[body_start..],
            Some(format!(
                "HTTP {} response includes {} extra body bytes beyond Content-Length",
                head.status,
                buffered_body - body_length
            )),
        ));
    }

    let read_length = body_length.min(body_limit);
    bytes.reserve(read_length.saturating_sub(buffered_body.min(read_length)));
    while bytes.len() - body_start < read_length {
        let remaining = read_length - (bytes.len() - body_start);
        let mut chunk = [0_u8; IO_CHUNK_BYTES];
        let chunk_length = remaining.min(chunk.len());
        let read = match read_until_deadline(
            stream,
            &mut chunk[..chunk_length],
            deadline,
            "reading response body",
        ) {
            Ok(read) => read,
            Err(ClientFailure::Deadline { phase }) => {
                return Err(http_response_timeout(
                    head.status,
                    &bytes[body_start..],
                    combine_response_detail(
                        oversized_detail.as_deref(),
                        format!("response body timed out while {phase}"),
                    ),
                ));
            }
            Err(ClientFailure::Io { phase, source }) => {
                return Err(http_response_failure(
                    head.status,
                    &bytes[body_start..],
                    Some(combine_response_detail(
                        oversized_detail.as_deref(),
                        format!("cannot continue {phase}: {source}"),
                    )),
                ));
            }
            Err(failure) => return Err(failure),
        };
        if read == 0 {
            return Err(http_response_failure(
                head.status,
                &bytes[body_start..],
                Some(combine_response_detail(
                    oversized_detail.as_deref(),
                    format!(
                        "HTTP {} response body ended before Content-Length {body_length}",
                        head.status
                    ),
                )),
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    match ensure_before_deadline(deadline, "reading response body") {
        Ok(()) => {}
        Err(ClientFailure::Deadline { phase }) => {
            return Err(http_response_timeout(
                head.status,
                &bytes[body_start..],
                combine_response_detail(
                    oversized_detail.as_deref(),
                    format!("response body timed out while {phase}"),
                ),
            ));
        }
        Err(failure) => return Err(failure),
    }
    if let Some(detail) = oversized_detail {
        return Err(http_response_failure(
            head.status,
            &bytes[body_start..],
            Some(detail),
        ));
    }
    if !expected_status {
        return Err(http_response_failure(
            head.status,
            &bytes[body_start..],
            None,
        ));
    }

    Ok(HttpResponse { bytes, body_start })
}

fn combine_response_detail(prefix: Option<&str>, detail: String) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}; {detail}"),
        None => detail,
    }
}

fn http_response_failure(status: u16, body: &[u8], detail: Option<String>) -> ClientFailure {
    ClientFailure::HttpResponse {
        status,
        body_preview: escaped_body_preview(body),
        detail,
        timed_out: false,
    }
}

fn http_response_timeout(status: u16, body: &[u8], detail: String) -> ClientFailure {
    ClientFailure::HttpResponse {
        status,
        body_preview: escaped_body_preview(body),
        detail: Some(detail),
        timed_out: true,
    }
}
fn escaped_body_preview(body: &[u8]) -> String {
    let bounded = &body[..body.len().min(MAX_ERROR_BODY_BYTES)];
    let mut preview: String = match std::str::from_utf8(bounded) {
        Ok(text) => text.chars().flat_map(char::escape_debug).collect(),
        Err(_) => bounded
            .iter()
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect(),
    };
    if body.len() > bounded.len() {
        preview.push_str("...[truncated]");
    }
    preview
}

fn parse_response_head(bytes: &[u8]) -> Result<ParsedHead, ClientFailure> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        ClientFailure::Protocol("response status or headers are not valid UTF-8".to_string())
    })?;
    let mut lines = text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| ClientFailure::Protocol("response status line is missing".to_string()))?;
    let status = parse_status_line(status_line)?;
    let mut content_length = None;

    for line in lines {
        if line.is_empty() {
            return Err(ClientFailure::Protocol(
                "response contains an empty line before the header terminator".to_string(),
            ));
        }
        if line.starts_with([' ', '\t']) {
            return Err(ClientFailure::Protocol(
                "folded response header lines are not supported".to_string(),
            ));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            ClientFailure::Protocol(format!("malformed response header line {line:?}"))
        })?;
        if name.is_empty() || !name.bytes().all(is_header_name_byte) {
            return Err(ClientFailure::Protocol(format!(
                "malformed response header name {name:?}"
            )));
        }
        let value = value.trim_matches([' ', '\t']);
        if value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(ClientFailure::Protocol(format!(
                "malformed response header value for {name:?}"
            )));
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ClientFailure::Protocol(
                "Transfer-Encoding responses are not supported".to_string(),
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ClientFailure::Protocol(format!(
                    "malformed Content-Length value {value:?}"
                )));
            }
            let parsed = value.parse::<usize>().map_err(|_| {
                ClientFailure::Protocol(format!("Content-Length value {value:?} is out of range"))
            })?;
            if let Some(previous) = content_length {
                let qualifier = if previous == parsed {
                    "duplicate"
                } else {
                    "conflicting"
                };
                return Err(ClientFailure::Protocol(format!(
                    "{qualifier} Content-Length headers ({previous} and {parsed})"
                )));
            }
            content_length = Some(parsed);
        }
    }

    Ok(ParsedHead {
        status,
        content_length,
    })
}

fn parse_status_line(line: &str) -> Result<u16, ClientFailure> {
    let mut fields = line.splitn(3, ' ');
    if fields.next() != Some("HTTP/1.1") {
        return Err(ClientFailure::Protocol(format!(
            "malformed HTTP/1.1 status line {line:?}"
        )));
    }
    let code = fields.next().ok_or_else(|| {
        ClientFailure::Protocol(format!("malformed HTTP/1.1 status line {line:?}"))
    })?;
    if fields.next().is_none() || code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ClientFailure::Protocol(format!(
            "malformed HTTP/1.1 status line {line:?}"
        )));
    }
    let status = code
        .parse::<u16>()
        .map_err(|_| ClientFailure::Protocol(format!("malformed HTTP/1.1 status code {code:?}")))?;
    if !(100..=599).contains(&status) {
        return Err(ClientFailure::Protocol(format!(
            "HTTP status code {status} is out of range"
        )));
    }
    Ok(status)
}

fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn find_header_terminator(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_until_deadline(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
    phase: &'static str,
) -> Result<usize, ClientFailure> {
    loop {
        ensure_before_deadline(deadline, phase)?;
        match stream.read(buffer) {
            Ok(read) => return Ok(read),
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                wait_for_socket(stream, PollFlags::POLLIN, deadline, phase)?;
            }
            Err(source) if source.kind() == io::ErrorKind::TimedOut => {
                return Err(ClientFailure::Deadline { phase });
            }
            Err(source) => return Err(ClientFailure::Io { phase, source }),
        }
    }
}

fn wait_for_socket(
    stream: &UnixStream,
    events: PollFlags,
    deadline: Instant,
    phase: &'static str,
) -> Result<(), ClientFailure> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientFailure::Deadline { phase });
        }
        let timeout = match PollTimeout::try_from(remaining) {
            Ok(timeout) => timeout,
            Err(_) => PollTimeout::MAX,
        };
        let mut descriptors = [PollFd::new(stream.as_fd(), events)];
        match poll(&mut descriptors, timeout) {
            Ok(0) => return Err(ClientFailure::Deadline { phase }),
            Ok(_) => return ensure_before_deadline(deadline, phase),
            Err(Errno::EINTR) => {}
            Err(source) => {
                return Err(ClientFailure::Io {
                    phase,
                    source: io::Error::from(source),
                });
            }
        }
    }
}

fn ensure_before_deadline(deadline: Instant, phase: &'static str) -> Result<(), ClientFailure> {
    if Instant::now() >= deadline {
        Err(ClientFailure::Deadline { phase })
    } else {
        Ok(())
    }
}

fn socket_error_is_unresponsive(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Read, Write},
        os::unix::net::{UnixListener, UnixStream},
        path::{Path, PathBuf},
        sync::mpsc::{self, Sender},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::{
        MAX_CREATE_BODY_BYTES, MAX_ERROR_BODY_BYTES, MAX_INFO_BODY_BYTES, MAX_PING_BODY_BYTES,
        VmState, VmmApi, VmmApiLivenessProbe, VmmPingResponse, find_header_terminator,
    };
    use crate::{ErrorKind, VmmPingProbe};

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    struct FakeServer {
        _directory: TempDir,
        socket: PathBuf,
        release: Option<Sender<()>>,
        thread: JoinHandle<io::Result<Vec<u8>>>,
    }

    impl FakeServer {
        fn spawn(response_chunks: Vec<Vec<u8>>, hold_open: bool) -> io::Result<Self> {
            Self::spawn_with_delay(response_chunks, hold_open, Duration::ZERO)
        }

        fn spawn_with_delay(
            response_chunks: Vec<Vec<u8>>,
            hold_open: bool,
            delay: Duration,
        ) -> io::Result<Self> {
            let directory = TempDir::new()?;
            let socket = directory.path().join("api.sock");
            let listener = UnixListener::bind(&socket)?;
            let (release, wait_for_release) = if hold_open {
                let (sender, receiver) = mpsc::channel();
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
            let thread = thread::spawn(move || {
                let (mut stream, _) = listener.accept()?;
                let request = read_request(&mut stream)?;
                for chunk in response_chunks {
                    if !delay.is_zero() {
                        thread::sleep(delay);
                    }
                    if stream.write_all(&chunk).is_err() {
                        return Ok(request);
                    }
                }
                if let Some(receiver) = wait_for_release {
                    let _ = receiver.recv_timeout(Duration::from_secs(2));
                }
                Ok(request)
            });
            Ok(Self {
                _directory: directory,
                socket,
                release,
                thread,
            })
        }

        fn path(&self) -> &Path {
            &self.socket
        }

        fn finish(mut self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            self.thread
                .join()
                .map_err(|_| io::Error::other("fake VMM server panicked"))?
                .map_err(Into::into)
        }
    }

    fn read_request(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
        let mut request = Vec::new();
        let body_start = loop {
            if let Some(offset) = find_header_terminator(&request) {
                break offset + 4;
            }
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "request ended before headers",
                ));
            }
            request.extend_from_slice(&chunk[..read]);
        };
        let headers = std::str::from_utf8(&request[..body_start])
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
        let content_length = headers
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?
            .unwrap_or(0);
        while request.len() - body_start < content_length {
            let mut chunk = [0_u8; 4096];
            let remaining = content_length - (request.len() - body_start);
            let chunk_length = remaining.min(chunk.len());
            let read = stream.read(&mut chunk[..chunk_length])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "request body was truncated",
                ));
            }
            request.extend_from_slice(&chunk[..read]);
        }
        Ok(request)
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nServer: Cloud Hypervisor API\r\nConnection: keep-alive\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn no_content_response() -> Vec<u8> {
        b"HTTP/1.1 204 No Content\r\nServer: Cloud Hypervisor API\r\nConnection: keep-alive\r\nContent-Type: application/json\r\n\r\n"
            .to_vec()
    }

    fn ping_body() -> &'static [u8] {
        br#"{"build_version":"v53.0","version":"53.0.0","pid":42,"features":["kvm"]}"#
    }
    fn require_error<T, E>(result: Result<T, E>, message: &'static str) -> Result<E, io::Error> {
        match result {
            Ok(_) => Err(io::Error::other(message)),
            Err(error) => Ok(error),
        }
    }
    fn assert_protocol_error(response: Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
        let server = FakeServer::spawn(vec![response], false)?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vmm_ping(),
            "malformed response must fail",
        )?;
        let _ = server.finish()?;
        assert_eq!(error.kind(), ErrorKind::Generic);
        let message = error.message();
        assert!(message.contains("VMM API GET /api/v1/vmm.ping"));
        assert!(message.contains("invalid VMM API") || message.contains("returned HTTP status"));
        Ok(())
    }

    #[test]
    fn methods_expected_statuses_emit_exact_requests() -> Result<(), Box<dyn std::error::Error>> {
        let ping_server = FakeServer::spawn(vec![response("200 OK", ping_body())], false)?;
        let ping = VmmApi::new(ping_server.path(), TEST_TIMEOUT).vmm_ping()?;
        assert_eq!(
            ping,
            VmmPingResponse {
                build_version: "v53.0".to_string(),
                version: "53.0.0".to_string(),
                pid: 42,
                features: vec!["kvm".to_string()],
            }
        );
        assert_eq!(
            ping_server.finish()?,
            b"GET /api/v1/vmm.ping HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );

        let create_body = br#"{"answer":42}"#;
        let create_server = FakeServer::spawn(vec![no_content_response()], false)?;
        VmmApi::new(create_server.path(), TEST_TIMEOUT).vm_create(create_body)?;
        let mut create_request = format!(
            "PUT /api/v1/vm.create HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            create_body.len()
        )
        .into_bytes();
        create_request.extend_from_slice(create_body);
        assert_eq!(create_server.finish()?, create_request);

        let boot_server = FakeServer::spawn(vec![no_content_response()], false)?;
        VmmApi::new(boot_server.path(), TEST_TIMEOUT).vm_boot()?;
        assert_eq!(
            boot_server.finish()?,
            b"PUT /api/v1/vm.boot HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );

        let info_body = br#"{"config":{"payload":{"kernel":"fw"}},"state":"Running","memory_actual_size":2147483648,"device_tree":null}"#;
        let info_server = FakeServer::spawn(vec![response("200 OK", info_body)], false)?;
        let info = VmmApi::new(info_server.path(), TEST_TIMEOUT).vm_info()?;
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.memory_actual_size, 2_147_483_648);
        assert_eq!(
            info_server.finish()?,
            b"GET /api/v1/vm.info HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );

        let power_server = FakeServer::spawn(vec![no_content_response()], false)?;
        VmmApi::new(power_server.path(), TEST_TIMEOUT).vm_power_button()?;
        assert_eq!(
            power_server.finish()?,
            b"PUT /api/v1/vm.power-button HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );

        let vm_shutdown_server = FakeServer::spawn(vec![no_content_response()], false)?;
        VmmApi::new(vm_shutdown_server.path(), TEST_TIMEOUT).vm_shutdown()?;
        assert_eq!(
            vm_shutdown_server.finish()?,
            b"PUT /api/v1/vm.shutdown HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );

        let vmm_shutdown_server = FakeServer::spawn(
            vec![
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n".to_vec(),
            ],
            false,
        )?;
        VmmApi::new(vmm_shutdown_server.path(), TEST_TIMEOUT).vmm_shutdown()?;
        assert_eq!(
            vmm_shutdown_server.finish()?,
            b"PUT /api/v1/vmm.shutdown HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );
        Ok(())
    }

    #[test]
    fn vm_resize_omits_absent_fields_and_expects_204() -> Result<(), Box<dyn std::error::Error>> {
        for (cpus, memory, body) in [
            (
                Some(4_u8),
                Some(4_294_967_296_u64),
                br#"{"desired_vcpus":4,"desired_ram":4294967296}"#.as_slice(),
            ),
            (Some(4), None, br#"{"desired_vcpus":4}"#.as_slice()),
            (
                None,
                Some(4_294_967_296),
                br#"{"desired_ram":4294967296}"#.as_slice(),
            ),
        ] {
            let server = FakeServer::spawn(vec![no_content_response()], false)?;
            VmmApi::new(server.path(), TEST_TIMEOUT).vm_resize(cpus, memory)?;
            let mut expected = format!(
                "PUT /api/v1/vm.resize HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            expected.extend_from_slice(body);
            assert_eq!(server.finish()?, expected);
        }
        Ok(())
    }

    #[test]
    fn vm_resize_non_204_status_surfaces_the_body_preview() -> Result<(), Box<dyn std::error::Error>>
    {
        let server = FakeServer::spawn(
            vec![response(
                "500 Internal Server Error",
                b"no hotplug headroom",
            )],
            false,
        )?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vm_resize(Some(8), None),
            "a non-204 resize must fail",
        )?;
        let _ = server.finish()?;
        assert_eq!(error.kind(), ErrorKind::Generic);
        assert!(
            error
                .message()
                .contains("VMM API PUT /api/v1/vm.resize returned HTTP status 500"),
            "{}",
            error.message()
        );
        assert!(error.message().contains("no hotplug headroom"));
        Ok(())
    }

    #[test]
    fn response_fragmented_and_keep_alive_completes_at_content_length()
    -> Result<(), Box<dyn std::error::Error>> {
        let full = response("200 OK", ping_body());
        let chunks = full.chunks(3).map(<[u8]>::to_vec).collect();
        let server = FakeServer::spawn_with_delay(chunks, true, Duration::from_millis(1))?;
        let ping = VmmApi::new(server.path(), TEST_TIMEOUT).vmm_ping()?;
        assert_eq!(ping.version, "53.0.0");
        let _ = server.finish()?;
        Ok(())
    }

    #[test]
    fn vm_create_body_limit_accepts_51200_and_rejects_51201_before_connect()
    -> Result<(), Box<dyn std::error::Error>> {
        let accepted_body = vec![b'a'; MAX_CREATE_BODY_BYTES];
        let accepted_server = FakeServer::spawn(vec![no_content_response()], false)?;
        VmmApi::new(accepted_server.path(), TEST_TIMEOUT).vm_create(&accepted_body)?;
        let accepted_request = accepted_server.finish()?;
        let body_start = find_header_terminator(&accepted_request)
            .ok_or_else(|| io::Error::other("request header terminator missing"))?
            + 4;
        assert_eq!(accepted_request.len() - body_start, MAX_CREATE_BODY_BYTES);
        assert_eq!(&accepted_request[body_start..], accepted_body);
        let expected_length = b"Content-Length: 51200\x0d\x0a";
        assert!(
            accepted_request[..body_start]
                .windows(expected_length.len())
                .any(|window| window == expected_length)
        );

        let directory = TempDir::new()?;
        let socket = directory.path().join("api.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let rejected_body = vec![b'a'; MAX_CREATE_BODY_BYTES + 1];
        let error = require_error(
            VmmApi::new(&socket, TEST_TIMEOUT).vm_create(&rejected_body),
            "oversized create body must fail locally",
        )?;
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("51200-byte"));
        let accept_error = require_error(listener.accept(), "client must not connect")?;
        assert_eq!(accept_error.kind(), io::ErrorKind::WouldBlock);
        Ok(())
    }

    #[test]
    fn response_body_limits_reject_oversized_success_and_error_bodies()
    -> Result<(), Box<dyn std::error::Error>> {
        let ping_server = FakeServer::spawn(
            vec![
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    MAX_PING_BODY_BYTES + 1
                )
                .into_bytes(),
            ],
            false,
        )?;
        let ping_error = require_error(
            VmmApi::new(ping_server.path(), TEST_TIMEOUT).vmm_ping(),
            "oversized ping response must fail",
        )?;
        let _ = ping_server.finish()?;
        assert!(ping_error.message().contains("65536-byte limit"));

        let info_server = FakeServer::spawn(
            vec![
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    MAX_INFO_BODY_BYTES + 1
                )
                .into_bytes(),
            ],
            false,
        )?;
        let info_error = require_error(
            VmmApi::new(info_server.path(), TEST_TIMEOUT).vm_info(),
            "oversized info response must fail",
        )?;
        let _ = info_server.finish()?;
        assert!(info_error.message().contains("1048576-byte limit"));

        let mut oversized_error = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n",
            MAX_ERROR_BODY_BYTES + 1
        )
        .into_bytes();
        oversized_error.extend_from_slice(b"diagnostic-prefix");
        let error_server = FakeServer::spawn(vec![oversized_error], false)?;
        let server_error = require_error(
            VmmApi::new(error_server.path(), TEST_TIMEOUT).vmm_ping(),
            "oversized error response must fail",
        )?;
        let _ = error_server.finish()?;
        assert!(server_error.message().contains("HTTP status 500"));
        assert!(server_error.message().contains("diagnostic-prefix"));
        assert!(server_error.message().contains("65536-byte limit"));
        Ok(())
    }

    #[test]
    fn unexpected_status_preserves_bounded_utf8_diagnostic()
    -> Result<(), Box<dyn std::error::Error>> {
        let diagnostic = br#"["VM is not created"]"#;
        let server = FakeServer::spawn(
            vec![response("500 Internal Server Error", diagnostic)],
            true,
        )?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vm_boot(),
            "unexpected status must fail",
        )?;
        let _ = server.finish()?;
        assert_eq!(error.kind(), ErrorKind::Generic);
        assert!(error.message().contains("PUT /api/v1/vm.boot"));
        assert!(error.message().contains("HTTP status 500"));
        assert!(error.message().contains("VM is not created"));
        Ok(())
    }

    #[test]
    fn response_framing_malformed_variants_are_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        for malformed in [
            b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nMalformed\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n{}".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}x".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n".to_vec(),
        ] {
            assert_protocol_error(malformed)?;
        }

        assert_protocol_error(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}".to_vec())?;
        assert_protocol_error(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n".to_vec())?;
        Ok(())
    }

    #[test]
    fn response_header_limit_and_unexpected_empty_body_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nX-Fill: {}\r\nContent-Length: 0\r\n\r\n",
            "a".repeat(16 * 1024)
        )
        .into_bytes();
        assert_protocol_error(oversized)?;

        let server = FakeServer::spawn(
            vec![b"HTTP/1.1 204 No Content\r\nContent-Length: 1\r\n\r\nx".to_vec()],
            false,
        )?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vm_boot(),
            "204 body must fail",
        )?;
        let _ = server.finish()?;
        assert!(error.message().contains("must not include Content-Length"));
        Ok(())
    }

    #[test]
    fn error_diagnostic_non_utf8_preserves_status_and_escaped_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = FakeServer::spawn(
            vec![response("500 Internal Server Error", &[0xff, b'\n', 0x1b])],
            false,
        )?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vmm_ping(),
            "non-UTF8 diagnostic must fail",
        )?;
        let _ = server.finish()?;
        assert_eq!(error.kind(), ErrorKind::Generic);
        assert!(error.message().contains("HTTP status 500"));
        assert!(
            error.message().contains(r#"body: "\xff\n\x1b""#),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn response_partial_body_uses_one_absolute_deadline() -> Result<(), Box<dyn std::error::Error>>
    {
        let server = FakeServer::spawn_with_delay(
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".to_vec()],
            true,
            Duration::ZERO,
        )?;
        let started = Instant::now();
        let error = require_error(
            VmmApi::new(server.path(), Duration::from_millis(60)).vmm_ping(),
            "partial response must time out",
        )?;
        let elapsed = started.elapsed();
        let _ = server.finish()?;
        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert!(error.message().contains("reading response body"));
        assert!(elapsed < Duration::from_millis(300));
        Ok(())
    }

    #[test]
    fn socket_missing_reports_not_running_and_liveness_false()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let socket = directory.path().join("missing.sock");
        let error = require_error(
            VmmApi::new(&socket, TEST_TIMEOUT).vmm_ping(),
            "missing socket must fail",
        )?;
        assert_eq!(error.kind(), ErrorKind::NotRunning);
        assert!(error.message().contains("GET /api/v1/vmm.ping"));
        assert!(error.message().contains("missing.sock"));

        let probe = VmmApiLivenessProbe::new(TEST_TIMEOUT);
        assert!(!probe.ping(&socket)?);
        Ok(())
    }

    #[test]
    fn liveness_unexpected_status_and_malformed_response_are_ambiguous()
    -> Result<(), Box<dyn std::error::Error>> {
        for response in [
            response("204 No Content", b""),
            b"not HTTP\r\n\r\n".to_vec(),
            Vec::new(),
        ] {
            let server = FakeServer::spawn(vec![response], false)?;
            let probe = VmmApiLivenessProbe::new(TEST_TIMEOUT);
            let error = require_error(
                probe.ping(server.path()),
                "ambiguous liveness response must fail closed",
            )?;
            let _ = server.finish()?;
            assert_ne!(error.kind(), ErrorKind::NotRunning);
        }
        Ok(())
    }

    #[test]
    fn liveness_http_200_schema_drift_uses_status_without_identity_decode()
    -> Result<(), Box<dyn std::error::Error>> {
        let drifted_body = br#"{"build_version":"v54.0","version":"54.0.0","pid":42,"features":[],"new_field":true}"#;
        let liveness_server = FakeServer::spawn(vec![response("200 OK", drifted_body)], false)?;
        let probe = VmmApiLivenessProbe::new(TEST_TIMEOUT);
        assert!(probe.ping(liveness_server.path())?);
        let _ = liveness_server.finish()?;

        let identity_server = FakeServer::spawn(vec![response("200 OK", drifted_body)], false)?;
        let error = require_error(
            VmmApi::new(identity_server.path(), TEST_TIMEOUT).vmm_ping(),
            "typed ping identity must reject schema drift",
        )?;
        let _ = identity_server.finish()?;
        assert!(
            error
                .message()
                .contains("cannot decode vmm.ping JSON response")
        );
        Ok(())
    }

    #[test]
    fn vm_counters_request_decodes_device_map_and_bounds_its_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"_disk0":{"read_bytes":4096,"read_ops":2,"write_bytes":0,"write_latency_avg":9223372036854775815,"write_latency_max":18446744073709551615,"write_latency_min":18446744073709551615,"write_ops":0}}"#;
        let server = FakeServer::spawn(vec![response("200 OK", body)], false)?;
        let counters = VmmApi::new(server.path(), TEST_TIMEOUT).vm_counters()?;
        assert_eq!(
            server.finish()?,
            b"GET /api/v1/vm.counters HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
        );
        let disk = counters
            .get("_disk0")
            .ok_or_else(|| io::Error::other("vm.counters lost its device entry"))?;
        assert_eq!(disk.get("read_bytes"), Some(&4096));
        assert_eq!(disk.get("write_latency_min"), Some(&u64::MAX));

        let oversized = FakeServer::spawn(
            vec![
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    super::MAX_COUNTERS_BODY_BYTES + 1
                )
                .into_bytes(),
            ],
            false,
        )?;
        let error = require_error(
            VmmApi::new(oversized.path(), TEST_TIMEOUT).vm_counters(),
            "oversized counters response must fail",
        )?;
        let _ = oversized.finish()?;
        assert!(error.message().contains("GET /api/v1/vm.counters"));
        assert!(error.message().contains("65536-byte limit"));
        Ok(())
    }

    #[test]
    fn vm_counters_non_object_response_is_protocol_drift() -> Result<(), Box<dyn std::error::Error>>
    {
        let server = FakeServer::spawn(vec![response("200 OK", br#"{"_disk0":[1,2]}"#)], false)?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vm_counters(),
            "non-object counters must fail",
        )?;
        let _ = server.finish()?;
        assert!(
            error
                .message()
                .contains("cannot decode vm.counters JSON response")
        );
        Ok(())
    }

    #[test]
    fn vm_info_unknown_state_is_protocol_drift() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"config":{},"state":"Stopped","memory_actual_size":0}"#;
        let server = FakeServer::spawn(vec![response("200 OK", body)], false)?;
        let error = require_error(
            VmmApi::new(server.path(), TEST_TIMEOUT).vm_info(),
            "unknown v53 VM state must fail",
        )?;
        let _ = server.finish()?;
        assert!(
            error
                .message()
                .contains("cannot decode vm.info JSON response")
        );
        Ok(())
    }
}
