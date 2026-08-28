use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsFd, AsRawFd},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, FdFlag, OFlag, fcntl},
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{
        AddressFamily, SockFlag, SockProtocol, SockType, UnixAddr, connect, getsockopt, socket,
        sockopt::SocketError,
    },
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder as TempBuilder;

use crate::{
    Cmd, DependencyArtifact, DependencyManifest, ErrorKind, FirestoneError, MachineLock, Paths,
    ReconcileRewrite, StateStore, VmmPingProbe, observe_liveness, reconcile, reconciled_state,
};

const MINIMUM_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_DEPENDENCY_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const DOCTOR_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MINIMUM_PASST_DATE: u32 = 20_241_211;
const MINIMUM_PASST_VERSION: &str = "2024_12_11.09478d5";
const CHECK_IDS: [DoctorCheckId; 13] = [
    DoctorCheckId::HostArch,
    DoctorCheckId::Kvm,
    DoctorCheckId::NestedVirtualization,
    DoctorCheckId::RuntimeDir,
    DoctorCheckId::VendoredBinaries,
    DoctorCheckId::Virtiofsd,
    DoctorCheckId::Passt,
    DoctorCheckId::QemuImg,
    DoctorCheckId::Ssh,
    DoctorCheckId::UserNamespaces,
    DoctorCheckId::SshKey,
    DoctorCheckId::DataSpace,
    DoctorCheckId::StaleState,
];
const VENDORED_DEPENDENCIES: [&str; 3] = [
    "cloud-hypervisor",
    "rust-hypervisor-firmware",
    "cloud-hypervisor-edk2",
];

/// Stable result level for one host check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

/// Stable identifier for one SPEC 17.3 check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckId {
    HostArch,
    Kvm,
    NestedVirtualization,
    RuntimeDir,
    VendoredBinaries,
    Virtiofsd,
    Passt,
    QemuImg,
    Ssh,
    UserNamespaces,
    SshKey,
    DataSpace,
    StaleState,
}

/// One deterministic host diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: DoctorCheckId,
    pub status: DoctorStatus,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl DoctorCheck {
    fn new(id: DoctorCheckId, status: DoctorStatus, reason: impl Into<String>) -> Self {
        Self {
            id,
            status,
            reason: reason.into(),
            fix: None,
            hint: None,
        }
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Ordered output shared by the CLI and REST adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == DoctorStatus::Fail)
    }
}

/// A stale state reconciled while running doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleStateObservation {
    machine: String,
    reason: String,
}

/// A machine state that doctor could not reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleStateFailure {
    machine: String,
    reason: String,
}

/// Resolved Firestone paths and injectable host facts used by doctor.
#[derive(Debug, Clone)]
pub struct DoctorContext {
    pub paths: Paths,
    pub operating_system: String,
    pub architecture: String,
    pub kvm_device: PathBuf,
    pub nested_parameters: Vec<PathBuf>,
    pub user_namespace_sysctl: PathBuf,
    pub os_release: PathBuf,
    pub group_file: PathBuf,
    pub search_path: OsString,
    pub hostname: String,
    pub manifest: DependencyManifest,
    pub proc_root: PathBuf,
    pub reconciled_at: String,
    pub minimum_data_free_bytes: u64,
}

impl DoctorContext {
    /// Builds production host facts from the process-wide resolved paths.
    #[must_use]
    pub fn from_paths(
        paths: Paths,
        manifest: DependencyManifest,
        hostname: impl Into<String>,
        reconciled_at: impl Into<String>,
    ) -> Self {
        Self {
            paths,
            operating_system: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            kvm_device: PathBuf::from("/dev/kvm"),
            nested_parameters: vec![
                PathBuf::from("/sys/module/kvm_intel/parameters/nested"),
                PathBuf::from("/sys/module/kvm_amd/parameters/nested"),
            ],
            user_namespace_sysctl: PathBuf::from("/proc/sys/user/max_user_namespaces"),
            os_release: PathBuf::from("/etc/os-release"),
            group_file: PathBuf::from("/etc/group"),
            search_path: std::env::var_os("PATH").unwrap_or_default(),
            hostname: hostname.into(),
            manifest,
            proc_root: PathBuf::from("/proc"),
            reconciled_at: reconciled_at.into(),
            minimum_data_free_bytes: MINIMUM_FREE_BYTES,
        }
    }

    /// The product threshold from SPEC 17.3, exposed for explicit contexts.
    #[must_use]
    pub const fn default_minimum_data_free_bytes() -> u64 {
        MINIMUM_FREE_BYTES
    }
}

/// Runs all checks and optionally applies only the unprivileged fixes allowed by
/// SPEC 17.3. Every successful fix is checked again before the report is built.
pub fn run_doctor(context: &DoctorContext, fix: bool) -> Result<DoctorReport, FirestoneError> {
    if !fix {
        let stale_state = reconcile_machine_states(context, &HttpVmmPing);
        return Ok(inspect(context, &BTreeMap::new(), &stale_state));
    }

    match HttpsDownloader::new() {
        Ok(downloader) => Ok(run_doctor_with(context, true, &downloader)),
        Err(error) => {
            let downloader = FailedDownloader {
                reason: firestone_error_reason(&error),
                hint: error.hint().map(str::to_owned),
            };
            Ok(run_doctor_with(context, true, &downloader))
        }
    }
}

fn run_doctor_with(
    context: &DoctorContext,
    fix: bool,
    fetcher: &dyn ArtifactFetcher,
) -> DoctorReport {
    let failures = if fix {
        perform_fixes(context, fetcher)
    } else {
        BTreeMap::new()
    };
    let stale_state = reconcile_machine_states(context, &HttpVmmPing);
    inspect(context, &failures, &stale_state)
}

#[derive(Debug, Default)]
struct StaleStateReport {
    observations: Vec<StaleStateObservation>,
    failures: Vec<StaleStateFailure>,
}

struct HttpVmmPing;

impl HttpVmmPing {
    fn ping_with_timeout(
        &self,
        api_socket: &Path,
        timeout: Duration,
    ) -> Result<bool, FirestoneError> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "VMM ping deadline is out of range")
                .with_hint("use a finite VMM ping timeout")
        })?;
        let Some(mut stream) = connect_vmm_socket(api_socket, deadline)? else {
            return Ok(false);
        };
        if !write_vmm_ping_request(&mut stream, api_socket, deadline)? {
            return Ok(false);
        }

        let mut response = [0_u8; 1024];
        let mut used = 0;
        while used < response.len() && !response[..used].contains(&b'\n') {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            match stream.read(&mut response[used..]) {
                Ok(0) => break,
                Ok(read) => used += read,
                Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                    if !wait_for_vmm_socket(
                        &stream,
                        PollFlags::POLLIN,
                        deadline,
                        api_socket,
                        "wait to read from",
                    )? {
                        return Ok(false);
                    }
                }
                Err(source) if vmm_is_unresponsive(source.kind()) => return Ok(false),
                Err(source) => return Err(vmm_ping_error(api_socket, "read from", source)),
            }
        }
        let Some(line_end) = response[..used].iter().position(|byte| *byte == b'\n') else {
            return Ok(false);
        };
        let Some(line) = std::str::from_utf8(&response[..line_end]).ok() else {
            return Ok(false);
        };
        let mut fields = line.split_ascii_whitespace();
        let protocol = fields.next();
        let status = fields.next();
        Ok(matches!(protocol, Some("HTTP/1.0" | "HTTP/1.1")) && status == Some("200"))
    }
}

impl VmmPingProbe for HttpVmmPing {
    fn ping(&self, api_socket: &Path) -> Result<bool, FirestoneError> {
        self.ping_with_timeout(api_socket, DOCTOR_PROBE_TIMEOUT)
    }
}

fn connect_vmm_socket(
    api_socket: &Path,
    deadline: Instant,
) -> Result<Option<UnixStream>, FirestoneError> {
    let address = UnixAddr::new(api_socket)
        .map_err(|source| vmm_ping_error(api_socket, "address", std::io::Error::from(source)))?;

    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let descriptor = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None::<SockProtocol>,
        )
        .map_err(|source| {
            vmm_ping_error(
                api_socket,
                "create socket for",
                std::io::Error::from(source),
            )
        })?;
        fcntl(&descriptor, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(|source| {
            vmm_ping_error(
                api_socket,
                "set nonblocking mode on",
                std::io::Error::from(source),
            )
        })?;
        fcntl(&descriptor, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| {
            vmm_ping_error(
                api_socket,
                "set close-on-exec on",
                std::io::Error::from(source),
            )
        })?;

        match connect(descriptor.as_raw_fd(), &address) {
            Ok(()) => return Ok(Some(UnixStream::from(descriptor))),
            Err(Errno::EINPROGRESS | Errno::EALREADY) => {
                let stream = UnixStream::from(descriptor);
                if !wait_for_vmm_socket(
                    &stream,
                    PollFlags::POLLOUT,
                    deadline,
                    api_socket,
                    "wait to connect to",
                )? {
                    return Ok(None);
                }
                let pending = getsockopt(&stream, SocketError).map_err(|source| {
                    vmm_ping_error(
                        api_socket,
                        "inspect connection to",
                        std::io::Error::from(source),
                    )
                })?;
                if pending == 0 {
                    return Ok(Some(stream));
                }
                let source = std::io::Error::from_raw_os_error(pending);
                if vmm_is_unresponsive(source.kind()) {
                    return Ok(None);
                }
                return Err(vmm_ping_error(api_socket, "connect to", source));
            }
            Err(Errno::EAGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(source) => {
                let source = std::io::Error::from(source);
                if vmm_is_unresponsive(source.kind()) {
                    return Ok(None);
                }
                return Err(vmm_ping_error(api_socket, "connect to", source));
            }
        }
    }
}

fn write_vmm_ping_request(
    stream: &mut UnixStream,
    api_socket: &Path,
    deadline: Instant,
) -> Result<bool, FirestoneError> {
    const REQUEST: &[u8] =
        b"GET /api/v1/vmm.ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let mut written = 0;
    while written < REQUEST.len() {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match stream.write(&REQUEST[written..]) {
            Ok(0) => return Ok(false),
            Ok(count) => written += count,
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                if !wait_for_vmm_socket(
                    stream,
                    PollFlags::POLLOUT,
                    deadline,
                    api_socket,
                    "wait to write to",
                )? {
                    return Ok(false);
                }
            }
            Err(source) if vmm_is_unresponsive(source.kind()) => return Ok(false),
            Err(source) => return Err(vmm_ping_error(api_socket, "write to", source)),
        }
    }
    Ok(true)
}

fn wait_for_vmm_socket(
    stream: &UnixStream,
    events: PollFlags,
    deadline: Instant,
    api_socket: &Path,
    operation: &str,
) -> Result<bool, FirestoneError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
        let mut descriptors = [PollFd::new(stream.as_fd(), events)];
        match poll(&mut descriptors, timeout) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(Errno::EINTR) => {}
            Err(source) => {
                return Err(vmm_ping_error(
                    api_socket,
                    operation,
                    std::io::Error::from(source),
                ));
            }
        }
    }
}

fn vmm_is_unresponsive(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::UnexpectedEof
    )
}

fn vmm_ping_error(api_socket: &Path, operation: &str, source: std::io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {operation} VMM API socket {}", api_socket.display()),
    )
    .with_hint("check the machine runtime directory and VMM process")
    .with_source(source)
}
#[derive(Debug, Clone)]
struct FixFailure {
    reason: String,
    hint: Option<String>,
}

impl From<FirestoneError> for FixFailure {
    fn from(error: FirestoneError) -> Self {
        Self {
            reason: firestone_error_reason(&error),
            hint: error.hint().map(str::to_owned),
        }
    }
}

trait ArtifactFetcher {
    fn fetch(&self, url: &Url, output: &mut dyn Write) -> Result<(), FirestoneError>;
}

struct FailedDownloader {
    reason: String,
    hint: Option<String>,
}

impl ArtifactFetcher for FailedDownloader {
    fn fetch(&self, _url: &Url, _output: &mut dyn Write) -> Result<(), FirestoneError> {
        let mut error = FirestoneError::new(ErrorKind::Dependency, self.reason.clone());
        if let Some(hint) = &self.hint {
            error = error.with_hint(hint.clone());
        }
        Err(error)
    }
}

struct HttpsDownloader {
    client: Client,
}

impl HttpsDownloader {
    fn new() -> Result<Self, FirestoneError> {
        let policy = Policy::custom(|attempt| {
            if let Some(reason) = redirect_rejection(attempt.url(), attempt.previous().len()) {
                return attempt.error(reason);
            }
            attempt.follow()
        });
        let client = Client::builder()
            .redirect(policy)
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "cannot initialize the dependency downloader",
                )
                .with_hint("check the host TLS configuration")
                .with_source(source)
            })?;
        Ok(Self { client })
    }
}

fn redirect_rejection(url: &Url, previous_redirects: usize) -> Option<&'static str> {
    if previous_redirects >= 10 {
        Some("too many dependency download redirects")
    } else if url.scheme() != "https" {
        Some("dependency download redirect is not HTTPS")
    } else {
        None
    }
}

impl ArtifactFetcher for HttpsDownloader {
    fn fetch(&self, url: &Url, output: &mut dyn Write) -> Result<(), FirestoneError> {
        require_https(url)?;
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("cannot download dependency artifact from {url}"),
                )
                .with_hint("check network access and retry `firestone doctor --fix`")
                .with_source(source)
            })?;
        if content_length_exceeds_limit(response.content_length()) {
            return Err(artifact_too_large(url));
        }
        copy_bounded(&mut response, output, MAX_DEPENDENCY_ARTIFACT_BYTES, url)?;
        Ok(())
    }
}

fn content_length_exceeds_limit(content_length: Option<u64>) -> bool {
    content_length.is_some_and(|length| length > MAX_DEPENDENCY_ARTIFACT_BYTES)
}

fn copy_bounded(
    reader: &mut dyn Read,
    output: &mut dyn Write,
    maximum: u64,
    url: &Url,
) -> Result<(), FirestoneError> {
    let mut limited = reader.take(maximum.saturating_add(1));
    let copied = std::io::copy(&mut limited, output).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot write dependency download from {url}"),
        )
        .with_hint("check free space and retry `firestone doctor --fix`")
        .with_source(source)
    })?;
    if copied > maximum {
        return Err(artifact_too_large(url));
    }
    Ok(())
}

fn artifact_too_large(url: &Url) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "dependency artifact from {url} exceeds the {} byte safety limit",
            MAX_DEPENDENCY_ARTIFACT_BYTES
        ),
    )
    .with_hint("verify the pinned release asset before retrying")
}

fn inspect(
    context: &DoctorContext,
    fix_failures: &BTreeMap<DoctorCheckId, FixFailure>,
    stale_state: &StaleStateReport,
) -> DoctorReport {
    let kvm = check_kvm(context);
    let mut checks = vec![
        check_architecture(context),
        kvm.check,
        check_nested_virtualization(context, kvm.device_exists, kvm.device_accessible),
        check_runtime_dir(context),
        check_vendored(context),
        check_virtiofsd(context),
        check_passt(context),
        check_program(
            context,
            DoctorCheckId::QemuImg,
            "qemu-img",
            Package::QemuImg,
            "qemu-img is available",
        ),
        check_ssh(context),
        check_user_namespaces(context),
        check_ssh_key(context),
        check_data_space(context),
        check_stale_state(stale_state),
    ];

    for check in &mut checks {
        if let Some(failure) = fix_failures.get(&check.id) {
            check.status = DoctorStatus::Fail;
            check.reason = format!("fix failed: {}", failure.reason);
            if failure.hint.is_some() {
                check.hint.clone_from(&failure.hint);
            }
        }
    }

    debug_assert_eq!(
        checks.iter().map(|check| check.id).collect::<Vec<_>>(),
        CHECK_IDS
    );
    DoctorReport { checks }
}

fn check_architecture(context: &DoctorContext) -> DoctorCheck {
    if context.operating_system != "linux" {
        return DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Fail,
            format!(
                "host operating system {} is unsupported",
                context.operating_system
            ),
        )
        .with_hint("Firestone v0.1 requires Linux on x86_64 or aarch64");
    }
    if matches!(context.architecture.as_str(), "x86_64" | "aarch64") {
        DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Ok,
            format!("host architecture {} is supported", context.architecture),
        )
    } else {
        DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Fail,
            format!("host architecture {} is unsupported", context.architecture),
        )
        .with_hint("Firestone v0.1 requires an x86_64 or aarch64 Linux host")
    }
}

struct KvmCheck {
    check: DoctorCheck,
    device_exists: bool,
    device_accessible: bool,
}

fn check_kvm(context: &DoctorContext) -> KvmCheck {
    let metadata = match fs::metadata(&context.kvm_device) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return KvmCheck {
                check: DoctorCheck::new(
                    DoctorCheckId::Kvm,
                    DoctorStatus::Fail,
                    format!("{} does not exist", context.kvm_device.display()),
                )
                .with_hint("enable hardware virtualization and load kvm_intel or kvm_amd"),
                device_exists: false,
                device_accessible: false,
            };
        }
        Err(source) => {
            return KvmCheck {
                check: DoctorCheck::new(
                    DoctorCheckId::Kvm,
                    DoctorStatus::Fail,
                    format!("cannot inspect {}: {source}", context.kvm_device.display()),
                )
                .with_hint("run `ls -l /dev/kvm` and repair access to the KVM device"),
                device_exists: false,
                device_accessible: false,
            };
        }
    };

    let group = device_group(&context.group_file, metadata.gid());
    let group_label = group
        .clone()
        .unwrap_or_else(|| format!("GID {}", metadata.gid()));
    match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&context.kvm_device)
    {
        Ok(_) => KvmCheck {
            check: DoctorCheck::new(
                DoctorCheckId::Kvm,
                DoctorStatus::Ok,
                format!(
                    "{} opens read/write; device group is {group_label}",
                    context.kvm_device.display()
                ),
            ),
            device_exists: true,
            device_accessible: true,
        },
        Err(source) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::Kvm,
                DoctorStatus::Fail,
                format!(
                    "{} does not open read/write: {source}; device group is {group_label}",
                    context.kvm_device.display()
                ),
            );
            if let Some(group) = group {
                check = check
                    .with_fix(format!("sudo usermod -aG {group} $USER"))
                    .with_hint("log out and back in after changing group membership");
            } else {
                check = check.with_hint(format!(
                    "grant $USER read/write access to device group GID {}",
                    metadata.gid()
                ));
            }
            KvmCheck {
                check,
                device_exists: true,
                device_accessible: false,
            }
        }
    }
}

fn check_nested_virtualization(
    context: &DoctorContext,
    kvm_exists: bool,
    kvm_accessible: bool,
) -> DoctorCheck {
    let nested = context
        .nested_parameters
        .iter()
        .find_map(|path| fs::read_to_string(path).ok().map(|value| (path, value)));
    let detail = nested.map_or_else(
        || "nested virtualization parameter is unavailable".to_owned(),
        |(path, value)| {
            let enabled = matches!(value.trim(), "1" | "Y" | "y");
            format!(
                "{} reports nested virtualization {}",
                path.display(),
                if enabled { "enabled" } else { "disabled" }
            )
        },
    );

    if kvm_accessible {
        DoctorCheck::new(
            DoctorCheckId::NestedVirtualization,
            DoctorStatus::Ok,
            format!("KVM is usable; {detail}"),
        )
    } else if kvm_exists {
        DoctorCheck::new(
            DoctorCheckId::NestedVirtualization,
            DoctorStatus::Warn,
            format!("KVM exists but is not usable; {detail}"),
        )
        .with_hint("repair read/write access to /dev/kvm before starting a machine")
    } else {
        DoctorCheck::new(
            DoctorCheckId::NestedVirtualization,
            DoctorStatus::Warn,
            format!("KVM device is absent; {detail}"),
        )
        .with_hint("enable virtualization in firmware, or nested KVM in the outer hypervisor")
    }
}

fn check_runtime_dir(context: &DoctorContext) -> DoctorCheck {
    let runtime_dir = context.paths.runtime_dir();
    match context.paths.validate_runtime_dir() {
        Ok(()) if context.paths.uses_runtime_fallback() => DoctorCheck::new(
            DoctorCheckId::RuntimeDir,
            DoctorStatus::Warn,
            format!(
                "XDG_RUNTIME_DIR is unavailable; using secure fallback {}",
                runtime_dir.display()
            ),
        )
        .with_hint("set XDG_RUNTIME_DIR to a user-owned runtime directory"),
        Ok(()) => DoctorCheck::new(
            DoctorCheckId::RuntimeDir,
            DoctorStatus::Ok,
            format!("runtime directory {} is secure", runtime_dir.display()),
        ),
        Err(error) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::RuntimeDir,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_fix("firestone doctor --fix");
            if let Some(hint) = error.hint() {
                check = check.with_hint(hint);
            }
            check
        }
    }
}

fn check_vendored(context: &DoctorContext) -> DoctorCheck {
    let mut failures = Vec::new();
    let mut installed = Vec::new();
    let mut hint = None;
    for dependency in VENDORED_DEPENDENCIES {
        match context.manifest.artifact(dependency, &context.architecture) {
            Ok(artifact) => match artifact_state(&context.paths.bin_dir(), &artifact) {
                Ok(()) => installed.push(format!("{} {}", artifact.dependency, artifact.version)),
                Err(reason) => failures.push(reason),
            },
            Err(error) => {
                failures.push(firestone_error_reason(&error));
                if hint.is_none() {
                    hint = error.hint().map(str::to_owned);
                }
            }
        }
    }

    if failures.is_empty() {
        DoctorCheck::new(
            DoctorCheckId::VendoredBinaries,
            DoctorStatus::Ok,
            format!("verified {}", installed.join(", ")),
        )
    } else {
        let mut check = DoctorCheck::new(
            DoctorCheckId::VendoredBinaries,
            DoctorStatus::Fail,
            failures.join("; "),
        )
        .with_fix("firestone doctor --fix");
        if let Some(hint) = hint {
            check = check.with_hint(hint);
        }
        check
    }
}

fn check_virtiofsd(context: &DoctorContext) -> DoctorCheck {
    match context
        .manifest
        .artifact("virtiofsd", &context.architecture)
    {
        Ok(artifact) => match artifact_state(&context.paths.bin_dir(), &artifact) {
            Ok(()) => DoctorCheck::new(
                DoctorCheckId::Virtiofsd,
                DoctorStatus::Ok,
                format!("verified virtiofsd {}", artifact.version),
            ),
            Err(reason) => DoctorCheck::new(DoctorCheckId::Virtiofsd, DoctorStatus::Fail, reason)
                .with_fix("firestone doctor --fix"),
        },
        Err(error) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::Virtiofsd,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            );
            if let Some(hint) = error.hint() {
                check = check.with_hint(hint);
            }
            check
        }
    }
}

/// Checks the local passt capability required by `[verify 14]`.
///
/// Version and help output establish only that the installed binary exposes
/// vhost-user mode. They do not resolve runtime interoperability with Cloud
/// Hypervisor; the M3 network test remains the verify-14 gate.
fn check_passt(context: &DoctorContext) -> DoctorCheck {
    let Some(program) = find_on_path("passt", &context.search_path) else {
        return missing_passt_check(context);
    };
    let version_output = match Cmd::new(&program)
        .arg("--version")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return DoctorCheck::new(
                DoctorCheckId::Passt,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_hint("repair the passt installation and retry");
        }
    };
    if !version_output.success() {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt --version failed: {}",
                command_failure_reason(&version_output)
            ),
        )
        .with_hint("repair the passt installation and retry");
    }
    let help_output = match Cmd::new(program)
        .arg("--help")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return DoctorCheck::new(
                DoctorCheckId::Passt,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_hint("repair the passt installation and retry");
        }
    };
    if !help_output.success() {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt --help failed: {}",
                command_failure_reason(&help_output)
            ),
        )
        .with_hint("repair the passt installation and retry");
    }

    let combined_version = format!(
        "{}\n{}",
        version_output.stdout_lossy(),
        version_output.stderr_lossy()
    );
    let Some(version) = parse_passt_version(&combined_version) else {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            "passt version output has no date-and-hash release tag",
        )
        .with_hint(format!("install passt {MINIMUM_PASST_VERSION} or newer"));
    };
    let help = format!(
        "{}\n{}",
        help_output.stdout_lossy(),
        help_output.stderr_lossy()
    );
    let has_vhost_user = help.split_whitespace().any(|token| token == "--vhost-user");
    if version.date < MINIMUM_PASST_DATE || !has_vhost_user {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt {} does not meet the {MINIMUM_PASST_VERSION} vhost-user minimum",
                version.raw
            ),
        )
        .with_hint(format!(
            "install passt {MINIMUM_PASST_VERSION} or newer with --vhost-user support"
        ));
    }

    DoctorCheck::new(
        DoctorCheckId::Passt,
        DoctorStatus::Ok,
        format!(
            "passt {} is installed and exposes --vhost-user",
            version.raw
        ),
    )
}

fn check_program(
    context: &DoctorContext,
    id: DoctorCheckId,
    program: &str,
    package: Package,
    ok_reason: &str,
) -> DoctorCheck {
    let Some(program_path) = find_on_path(program, &context.search_path) else {
        return missing_program_check(context, id, package, program);
    };
    match Cmd::new(program_path)
        .arg("--version")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) if output.success() => DoctorCheck::new(id, DoctorStatus::Ok, ok_reason),
        Ok(output) => DoctorCheck::new(
            id,
            DoctorStatus::Fail,
            format!(
                "{program} --version failed: {}",
                command_failure_reason(&output)
            ),
        )
        .with_hint(format!("repair the {program} installation and retry")),
        Err(error) => DoctorCheck::new(id, DoctorStatus::Fail, firestone_error_reason(&error))
            .with_hint(format!("repair the {program} installation and retry")),
    }
}

fn check_ssh(context: &DoctorContext) -> DoctorCheck {
    let ssh = probe_ssh_tool(context, "ssh", &["-V"], true);
    let keygen = probe_ssh_tool(context, "ssh-keygen", &["-?"], false);
    if ssh.is_ok() && keygen.is_ok() {
        DoctorCheck::new(
            DoctorCheckId::Ssh,
            DoctorStatus::Ok,
            "ssh and ssh-keygen are available",
        )
    } else {
        let reasons = [ssh.err(), keygen.err()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
        let mut check = DoctorCheck::new(DoctorCheckId::Ssh, DoctorStatus::Fail, reasons);
        if let Some(fix) = install_command(context, Package::OpenSsh) {
            check = check.with_fix(fix);
        } else {
            check = check.with_hint("install the OpenSSH client package for this distribution");
        }
        check
    }
}

fn probe_ssh_tool(
    context: &DoctorContext,
    program: &str,
    args: &[&str],
    require_success: bool,
) -> Result<(), String> {
    let path = find_on_path(program, &context.search_path)
        .ok_or_else(|| format!("{program} not found on PATH"))?;
    let output = Cmd::new(path)
        .args(args)
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
        .map_err(|error| firestone_error_reason(&error))?;
    if require_success && !output.success() {
        return Err(format!(
            "{program} probe failed: {}",
            command_failure_reason(&output)
        ));
    }
    Ok(())
}

/// Checks the host prerequisite for virtiofsd's `[verify 16]` namespace sandbox.
fn check_user_namespaces(context: &DoctorContext) -> DoctorCheck {
    let max_namespaces = match fs::read_to_string(&context.user_namespace_sysctl) {
        Ok(value) => value.trim().parse::<u64>().ok(),
        Err(_) => None,
    };
    if max_namespaces == Some(0) {
        return DoctorCheck::new(
            DoctorCheckId::UserNamespaces,
            DoctorStatus::Warn,
            "user namespaces are disabled by user.max_user_namespaces=0",
        )
        .with_hint("virtiofsd will run with --sandbox none");
    }
    let Some(unshare) = find_on_path("unshare", &context.search_path) else {
        return DoctorCheck::new(
            DoctorCheckId::UserNamespaces,
            DoctorStatus::Warn,
            "cannot prove user namespace support because unshare is not installed",
        )
        .with_hint("virtiofsd will run with --sandbox none");
    };
    let output = match Cmd::new(unshare)
        .args(["-U", "true"])
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return DoctorCheck::new(
                DoctorCheckId::UserNamespaces,
                DoctorStatus::Warn,
                firestone_error_reason(&error),
            )
            .with_hint("virtiofsd will run with --sandbox none");
        }
    };
    if output.success() && max_namespaces.is_some_and(|value| value > 0) {
        DoctorCheck::new(
            DoctorCheckId::UserNamespaces,
            DoctorStatus::Ok,
            format!(
                "user namespaces are enabled; unshare -U true succeeded (maximum {})",
                max_namespaces.unwrap_or_default()
            ),
        )
    } else {
        let reason = if max_namespaces.is_none() {
            "cannot read user.max_user_namespaces".to_owned()
        } else {
            format!(
                "unshare -U true failed: {}",
                command_failure_reason(&output)
            )
        };
        DoctorCheck::new(DoctorCheckId::UserNamespaces, DoctorStatus::Warn, reason)
            .with_hint("virtiofsd will run with --sandbox none")
    }
}

fn check_ssh_key(context: &DoctorContext) -> DoctorCheck {
    let private_path = context.paths.ssh_private_key();
    let public_path = context.paths.ssh_public_key();
    let private = fs::symlink_metadata(&private_path);
    let public = fs::symlink_metadata(&public_path);
    match (private, public) {
        (Ok(private), Ok(public)) if private.is_file() && public.is_file() => {
            let mode = private.permissions().mode() & 0o777;
            if mode == 0o600 {
                DoctorCheck::new(
                    DoctorCheckId::SshKey,
                    DoctorStatus::Ok,
                    format!(
                        "Firestone SSH key is present at {} with mode 0600",
                        private_path.display()
                    ),
                )
            } else {
                let check = DoctorCheck::new(
                    DoctorCheckId::SshKey,
                    DoctorStatus::Fail,
                    format!(
                        "Firestone SSH private key {} has mode {mode:04o}",
                        private_path.display()
                    ),
                );
                match shell_quote(&private_path) {
                    Some(path) => check.with_fix(format!("chmod 600 -- {path}")),
                    None => check.with_hint(
                        "set mode 0600 with a filesystem tool that preserves non-UTF-8 paths",
                    ),
                }
            }
        }
        (Err(private_error), Err(public_error))
            if private_error.kind() == std::io::ErrorKind::NotFound
                && public_error.kind() == std::io::ErrorKind::NotFound =>
        {
            DoctorCheck::new(
                DoctorCheckId::SshKey,
                DoctorStatus::Fail,
                "Firestone SSH key is missing",
            )
            .with_fix("firestone doctor --fix")
        }
        _ => DoctorCheck::new(
            DoctorCheckId::SshKey,
            DoctorStatus::Fail,
            "Firestone SSH key pair is incomplete or unreadable",
        )
        .with_hint(format!(
            "move the incomplete key pair out of {} and run `firestone doctor --fix`",
            private_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .display()
        )),
    }
}

fn check_data_space(context: &DoctorContext) -> DoctorCheck {
    let data_dir = context.paths.data_dir();
    let Some(path) = nearest_existing_ancestor(data_dir) else {
        return DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Warn,
            format!(
                "cannot find an existing filesystem for {}",
                data_dir.display()
            ),
        );
    };
    match fs2::available_space(path) {
        Ok(bytes) if bytes >= context.minimum_data_free_bytes => DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Ok,
            format!(
                "data filesystem has {} bytes free (minimum {})",
                bytes, context.minimum_data_free_bytes
            ),
        ),
        Ok(bytes) => DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Warn,
            format!(
                "data filesystem has {} bytes free; {} bytes are required",
                bytes, context.minimum_data_free_bytes
            ),
        )
        .with_hint("free space on the Firestone data filesystem before pulling an image"),
        Err(source) => DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Warn,
            format!("cannot measure free space for {}: {source}", path.display()),
        ),
    }
}

fn check_stale_state(stale_state: &StaleStateReport) -> DoctorCheck {
    if !stale_state.failures.is_empty() {
        let failures = stale_state
            .failures
            .iter()
            .map(|failure| format!("{} ({})", failure.machine, failure.reason))
            .collect::<Vec<_>>()
            .join(", ");
        let observations = stale_state
            .observations
            .iter()
            .map(|observation| format!("{} ({})", observation.machine, observation.reason))
            .collect::<Vec<_>>()
            .join(", ");
        let reason = if observations.is_empty() {
            format!("could not reconcile machine states: {failures}")
        } else {
            format!(
                "could not reconcile machine states: {failures}; reconciled stale machine states: {observations}"
            )
        };
        return DoctorCheck::new(DoctorCheckId::StaleState, DoctorStatus::Fail, reason)
            .with_hint("repair the named machine state or lock error and rerun doctor");
    }
    if stale_state.observations.is_empty() {
        return DoctorCheck::new(
            DoctorCheckId::StaleState,
            DoctorStatus::Ok,
            "no stale machine states were observed",
        );
    }
    let details = stale_state
        .observations
        .iter()
        .map(|observation| format!("{} ({})", observation.machine, observation.reason))
        .collect::<Vec<_>>()
        .join(", ");
    DoctorCheck::new(
        DoctorCheckId::StaleState,
        DoctorStatus::Ok,
        format!("reconciled stale machine states: {details}"),
    )
}

fn reconcile_machine_states(context: &DoctorContext, vmm: &dyn VmmPingProbe) -> StaleStateReport {
    let mut report = StaleStateReport::default();
    let entries = match fs::read_dir(context.paths.machines_dir()) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return report,
        Err(source) => {
            report.failures.push(StaleStateFailure {
                machine: "<machines>".to_owned(),
                reason: format!("cannot enumerate machine directories: {source}"),
            });
            return report;
        }
    };

    let mut machine_entries = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => machine_entries.push((entry.file_name(), entry)),
            Err(source) => report.failures.push(StaleStateFailure {
                machine: "<machines>".to_owned(),
                reason: format!("cannot read machine directory entry: {source}"),
            }),
        }
    }
    machine_entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (name_os, entry) in machine_entries {
        let display_name = name_os.to_string_lossy().into_owned();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(source) => {
                report.failures.push(StaleStateFailure {
                    machine: display_name,
                    reason: format!("cannot inspect machine directory type: {source}"),
                });
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = name_os.to_str() else {
            report.failures.push(StaleStateFailure {
                machine: display_name,
                reason: "machine directory name is not valid UTF-8".to_owned(),
            });
            continue;
        };
        match reconcile_machine_state(context, name, vmm) {
            Ok(Some(observation)) => report.observations.push(observation),
            Ok(None) => {}
            Err(error) => report.failures.push(StaleStateFailure {
                machine: name.to_owned(),
                reason: firestone_error_reason(&error),
            }),
        }
    }
    report.observations.sort_by(|left, right| {
        left.machine
            .cmp(&right.machine)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    report.failures.sort_by(|left, right| {
        left.machine
            .cmp(&right.machine)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    report
}

/// Reads one machine through live process and VMM observations.
///
/// The common read-only path does not take the machine lock. A stale active
/// state is re-read and persisted only after acquiring the lock.
pub fn read_reconciled_machine_state_live(
    paths: &Paths,
    name: &str,
    reconciled_at: &str,
) -> Result<crate::MachineState, FirestoneError> {
    let (state, _) = read_reconciled_machine_state_with(
        paths,
        name,
        Path::new("/proc"),
        reconciled_at,
        &HttpVmmPing,
    )?;
    Ok(state)
}

fn reconcile_machine_state(
    context: &DoctorContext,
    name: &str,
    vmm: &dyn VmmPingProbe,
) -> Result<Option<StaleStateObservation>, FirestoneError> {
    let (_, reason) = read_reconciled_machine_state_with(
        &context.paths,
        name,
        &context.proc_root,
        &context.reconciled_at,
        vmm,
    )?;
    Ok(reason.map(|reason| StaleStateObservation {
        machine: name.to_owned(),
        reason,
    }))
}

fn read_reconciled_machine_state_with(
    paths: &Paths,
    name: &str,
    proc_root: &Path,
    reconciled_at: &str,
    vmm: &dyn VmmPingProbe,
) -> Result<(crate::MachineState, Option<String>), FirestoneError> {
    let state_store = StateStore::new(paths.machine_state(name)?);
    let runtime_dir = paths.machine_runtime_dir(name)?;
    let api_socket = paths.machine_api_socket(name)?;
    let mut state = state_store.read()?;
    let observation = observe_liveness(name, &state, &runtime_dir, &api_socket, proc_root, vmm)?;
    let mut reconciliation = reconcile(state.status, observation);

    if reconciliation.rewrite.is_none() {
        state.status = reconciliation.status;
        return Ok((state, None));
    }

    let lock_path = paths.machine_lock(name)?;
    let mut events = |_event: crate::Event| Ok(());
    let lock = MachineLock::acquire(name, &lock_path, &mut events)?;
    // The shim may have completed its final atomic write while this reader was
    // waiting. Re-read and re-observe under the lock before deciding to write.
    state = state_store.read()?;
    let observation = observe_liveness(name, &state, &runtime_dir, &api_socket, proc_root, vmm)?;
    reconciliation = reconcile(state.status, observation);

    let reason = reconciliation
        .rewrite
        .as_ref()
        .map(|ReconcileRewrite::Stopped { reason }| reason.as_str().to_owned());
    if let Some(effective) = reconciled_state(&state, &reconciliation, reconciled_at) {
        state_store.write_reconciliation(&state, &reconciliation, reconciled_at, &lock)?;
        return Ok((effective, reason));
    }

    state.status = reconciliation.status;
    Ok((state, None))
}

fn perform_fixes(
    context: &DoctorContext,
    fetcher: &dyn ArtifactFetcher,
) -> BTreeMap<DoctorCheckId, FixFailure> {
    let mut failures = BTreeMap::new();

    let data_ready = record_directory_fix(
        &mut failures,
        context.paths.data_dir(),
        DoctorCheckId::DataSpace,
    );
    let bin_dir = context.paths.bin_dir();
    let bin_ready = data_ready
        && record_directory_fix(&mut failures, &bin_dir, DoctorCheckId::VendoredBinaries);
    if let Err(error) = context.paths.ensure_runtime_dir() {
        record_fix_failure(&mut failures, DoctorCheckId::RuntimeDir, error);
    }
    let ssh_private_key = context.paths.ssh_private_key();
    let ssh_public_key = context.paths.ssh_public_key();
    let key_ready = if data_ready {
        ssh_private_key.parent().is_some_and(|key_dir| {
            record_directory_fix(&mut failures, key_dir, DoctorCheckId::SshKey)
        })
    } else {
        false
    };

    if bin_ready {
        for dependency in VENDORED_DEPENDENCIES {
            fix_artifact(
                context,
                dependency,
                DoctorCheckId::VendoredBinaries,
                fetcher,
                &mut failures,
            );
        }
        fix_artifact(
            context,
            "virtiofsd",
            DoctorCheckId::Virtiofsd,
            fetcher,
            &mut failures,
        );
    }

    if key_ready && path_is_missing(&ssh_private_key) && path_is_missing(&ssh_public_key) {
        if let Err(error) = generate_ssh_key(context) {
            record_fix_failure(&mut failures, DoctorCheckId::SshKey, error);
        }
    }

    failures
}

fn record_directory_fix(
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
    path: &Path,
    check_id: DoctorCheckId,
) -> bool {
    match create_firestone_dir(path, 0o700) {
        Ok(()) => true,
        Err(error) => {
            record_fix_failure(failures, check_id, error);
            false
        }
    }
}

fn fix_artifact(
    context: &DoctorContext,
    dependency: &str,
    check_id: DoctorCheckId,
    fetcher: &dyn ArtifactFetcher,
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
) {
    let artifact = match context.manifest.artifact(dependency, &context.architecture) {
        Ok(artifact) => artifact,
        Err(error) => {
            if dependency != "virtiofsd" {
                record_fix_failure(failures, check_id, error);
            }
            return;
        }
    };
    if artifact_state(&context.paths.bin_dir(), &artifact).is_ok() {
        return;
    }
    if let Err(error) = install_artifact(&context.paths.bin_dir(), &artifact, fetcher) {
        record_fix_failure(failures, check_id, error);
    }
}

fn record_fix_failure(
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
    check_id: DoctorCheckId,
    error: FirestoneError,
) {
    let next = FixFailure::from(error);
    failures
        .entry(check_id)
        .and_modify(|failure| {
            failure.reason.push_str("; ");
            failure.reason.push_str(&next.reason);
            if failure.hint.is_none() {
                failure.hint.clone_from(&next.hint);
            }
        })
        .or_insert(next);
}

fn install_artifact(
    bin_dir: &Path,
    artifact: &DependencyArtifact,
    fetcher: &dyn ArtifactFetcher,
) -> Result<(), FirestoneError> {
    let url = Url::parse(&artifact.url).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("invalid download URL for `{}`", artifact.dependency),
        )
        .with_source(source)
    })?;
    require_https(&url)?;

    let mut partial = TempBuilder::new()
        .prefix(&format!(".{}.", artifact.install_name))
        .suffix(".partial")
        .tempfile_in(bin_dir)
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot create partial download in {}", bin_dir.display()),
            )
            .with_hint("check directory permissions and free space")
            .with_source(source)
        })?;
    fetcher.fetch(&url, partial.as_file_mut())?;
    partial.as_file_mut().flush().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot flush download for `{}`", artifact.dependency),
        )
        .with_source(source)
    })?;
    partial.as_file_mut().sync_all().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot sync download for `{}`", artifact.dependency),
        )
        .with_source(source)
    })?;
    partial
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot rewind download for `{}`", artifact.dependency),
            )
            .with_source(source)
        })?;
    let actual = sha256_reader(partial.as_file_mut()).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Checksum,
            format!("cannot hash download for `{}`", artifact.dependency),
        )
        .with_source(source)
    })?;
    if actual != artifact.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "checksum mismatch for `{}`: expected {}, got {actual}",
                artifact.dependency, artifact.sha256
            ),
        )
        .with_hint("the partial download was removed; retry from a trusted network"));
    }

    partial
        .as_file()
        .set_permissions(fs::Permissions::from_mode(artifact.expected_mode()))
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot set mode for `{}`", artifact.dependency),
            )
            .with_source(source)
        })?;
    partial.as_file().sync_all().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot sync mode for `{}`", artifact.dependency),
        )
        .with_source(source)
    })?;

    let destination = bin_dir.join(&artifact.install_name);
    partial.persist(&destination).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot atomically install `{}` at {}",
                artifact.dependency,
                destination.display()
            ),
        )
        .with_source(error.error)
    })?;
    sync_directory(bin_dir)?;
    artifact_state(bin_dir, artifact).map_err(|reason| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "installed `{}` failed readback: {reason}",
                artifact.dependency
            ),
        )
        .with_hint("remove the artifact and retry `firestone doctor --fix`")
    })
}

fn generate_ssh_key(context: &DoctorContext) -> Result<(), FirestoneError> {
    let keygen = find_on_path("ssh-keygen", &context.search_path).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "cannot generate Firestone SSH key: ssh-keygen is not on PATH",
        )
        .with_hint("install the OpenSSH client package")
    })?;
    let private_key = context.paths.ssh_private_key();
    let public_key = context.paths.ssh_public_key();
    let key_dir = private_key.parent().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH key path {} has no parent",
                private_key.display()
            ),
        )
        .with_hint("choose a Firestone data directory with a key parent directory")
    })?;
    let temporary = TempBuilder::new()
        .prefix(".ssh-key.")
        .tempdir_in(key_dir)
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "cannot create temporary SSH key directory in {}",
                    key_dir.display()
                ),
            )
            .with_hint("check directory permissions and retry 'firestone doctor --fix'")
            .with_source(source)
        })?;
    let temporary_private = temporary.path().join("id_ed25519");
    let temporary_public = temporary.path().join("id_ed25519.pub");

    Cmd::new(keygen)
        .args([OsStr::new("-t"), OsStr::new("ed25519"), OsStr::new("-N")])
        .secret_arg(OsString::new())
        .args([
            OsStr::new("-C"),
            OsStr::new(&format!("firestone@{}", context.hostname)),
            OsStr::new("-f"),
            temporary_private.as_os_str(),
        ])
        .stdin_null()
        .timeout(Duration::from_secs(30))
        .error_kind(ErrorKind::Dependency)
        .run()?;

    for (kind, path) in [
        ("private", temporary_private.as_path()),
        ("public", temporary_public.as_path()),
    ] {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("generated SSH {kind} key {} is unavailable", path.display()),
            )
            .with_source(source)
        })?;
        if !metadata.is_file() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "generated SSH {kind} key {} is not a regular file",
                    path.display()
                ),
            ));
        }
    }
    fs::set_permissions(&temporary_private, fs::Permissions::from_mode(0o600)).map_err(
        |source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot set mode 0600 on {}", temporary_private.display()),
            )
            .with_source(source)
        },
    )?;

    fs::hard_link(&temporary_private, &private_key).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot install Firestone SSH private key at {}",
                private_key.display()
            ),
        )
        .with_hint("move any existing key pair aside and retry 'firestone doctor --fix'")
        .with_source(source)
    })?;
    if let Err(source) = fs::hard_link(&temporary_public, &public_key) {
        let cleanup = remove_generated_key(&private_key).err();
        let mut error = FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot install Firestone SSH public key at {}",
                public_key.display()
            ),
        )
        .with_source(source);
        if let Some(cleanup) = cleanup {
            error = error.with_hint(format!(
                "remove partial private key {}; cleanup failed: {}",
                private_key.display(),
                bounded_detail(&cleanup.to_string())
            ));
        }
        return Err(error);
    }

    let check = check_ssh_key(context);
    if check.status == DoctorStatus::Ok {
        return Ok(());
    }

    let mut cleanup_failures = Vec::new();
    for path in [&public_key, &private_key] {
        if let Err(source) = remove_generated_key(path) {
            cleanup_failures.push(format!(
                "{}: {}",
                path.display(),
                bounded_detail(&source.to_string())
            ));
        }
    }
    let mut error = FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "generated Firestone SSH key failed readback: {}",
            check.reason
        ),
    );
    if !cleanup_failures.is_empty() {
        error = error.with_hint(format!(
            "remove the partial key pair; cleanup failed for {}",
            cleanup_failures.join(", ")
        ));
    }
    Err(error)
}

fn remove_generated_key(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(source),
    }
}

fn create_firestone_dir(path: &Path, mode: u32) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "refusing to replace non-directory Firestone path {}",
                    path.display()
                ),
            )
            .with_hint("move the existing path aside and rerun `firestone doctor --fix`"));
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot inspect Firestone directory {}", path.display()),
            )
            .with_source(source));
        }
    }

    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(mode);
    if let Err(source) = builder.create(path) {
        if source.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot create Firestone directory {}", path.display()),
            )
            .with_source(source));
        }
    }

    let metadata = fs::symlink_metadata(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot read back Firestone directory {}", path.display()),
        )
        .with_source(source)
    })?;
    if !metadata.is_dir() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone directory {} changed to a non-directory during creation",
                path.display()
            ),
        ));
    }
    let actual_mode = metadata.permissions().mode() & 0o777;
    if actual_mode != mode {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "created Firestone directory {} has mode {actual_mode:04o}; expected {mode:04o}",
                path.display()
            ),
        )
        .with_hint(format!("run `chmod {mode:04o} {}`", path.display())));
    }
    Ok(())
}

fn artifact_state(bin_dir: &Path, artifact: &DependencyArtifact) -> Result<(), String> {
    let path = bin_dir.join(&artifact.install_name);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|source| format!("{} is unavailable: {source}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let actual =
        sha256_path(&path).map_err(|source| format!("cannot hash {}: {source}", path.display()))?;
    if actual != artifact.sha256 {
        return Err(format!(
            "{} checksum mismatch: expected {}, got {actual}",
            path.display(),
            artifact.sha256
        ));
    }
    let actual_mode = metadata.permissions().mode() & 0o777;
    let expected_mode = artifact.expected_mode();
    if actual_mode != expected_mode {
        return Err(format!(
            "{} has mode {actual_mode:04o}; expected {expected_mode:04o}",
            path.display()
        ));
    }
    Ok(())
}

fn path_is_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn sha256_path(path: &Path) -> Result<String, std::io::Error> {
    sha256_reader(&mut File::open(path)?)
}

fn sha256_reader(reader: &mut dyn Read) -> Result<String, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn require_https(url: &Url) -> Result<(), FirestoneError> {
    if url.scheme() == "https" {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("refusing non-HTTPS dependency download URL {url}"),
        )
        .with_hint("dependency downloads and every redirect must use HTTPS"))
    }
}

fn device_group(group_file: &Path, gid: u32) -> Option<String> {
    let contents = fs::read_to_string(group_file).ok()?;
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let group_gid = fields.next()?.parse::<u32>().ok()?;
        (group_gid == gid).then(|| name.to_owned())
    })
}

fn find_on_path(program: &str, search_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path).find_map(|directory| {
        let candidate = directory.join(program);
        let metadata = fs::metadata(&candidate).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}

fn command_failure_reason(output: &crate::CmdOutput) -> String {
    let status = output
        .status()
        .code()
        .map_or_else(|| "signal".to_owned(), |code| format!("status {code}"));
    let stderr = output.last_stderr_lines();
    if stderr.is_empty() {
        status
    } else {
        format!("{status}; {}", stderr.join(" | "))
    }
}

fn firestone_error_reason(error: &FirestoneError) -> String {
    let mut reason = error.message().to_owned();
    let mut source = std::error::Error::source(error);
    for _ in 0..4 {
        let Some(next) = source else {
            break;
        };
        reason.push_str(": ");
        reason.push_str(&bounded_detail(&next.to_string()));
        source = next.source();
    }
    reason
}

fn bounded_detail(detail: &str) -> String {
    let mut characters = detail.chars();
    let mut bounded = characters.by_ref().take(1024).collect::<String>();
    if characters.next().is_some() {
        bounded.push_str("...[truncated]");
    }
    bounded
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PasstVersion {
    raw: String,
    date: u32,
}

fn parse_passt_version(output: &str) -> Option<PasstVersion> {
    output.split_whitespace().find_map(|token| {
        let trimmed = token.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '.'
        });
        let (date_part, commit) = trimmed.split_once('.')?;
        if !(7..=40).contains(&commit.len()) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let mut components = date_part.split('_');
        let year = components.next()?.parse::<u32>().ok()?;
        let month = components.next()?.parse::<u32>().ok()?;
        let day = components.next()?.parse::<u32>().ok()?;
        if components.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let date = year
            .checked_mul(10_000)?
            .checked_add(month.checked_mul(100)?)?
            .checked_add(day)?;
        Some(PasstVersion {
            raw: trimmed.to_owned(),
            date,
        })
    })
}

fn missing_passt_check(context: &DoctorContext) -> DoctorCheck {
    let package_hint = install_command(context, Package::Passt)
        .map(|command| format!("the detected package command is `{command}`, but verify that its candidate is new enough"))
        .unwrap_or_else(|| "install the passt package for this distribution".to_owned());
    DoctorCheck::new(
        DoctorCheckId::Passt,
        DoctorStatus::Fail,
        "passt not found on PATH",
    )
    .with_hint(format!(
        "{package_hint}; Firestone requires {MINIMUM_PASST_VERSION} or newer with --vhost-user support"
    ))
}

#[derive(Debug, Clone, Copy)]
enum Package {
    Passt,
    QemuImg,
    OpenSsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DistroFamily {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

fn missing_program_check(
    context: &DoctorContext,
    id: DoctorCheckId,
    package: Package,
    program: &str,
) -> DoctorCheck {
    let mut check = DoctorCheck::new(
        id,
        DoctorStatus::Fail,
        format!("{program} not found on PATH"),
    );
    if let Some(fix) = install_command(context, package) {
        check = check.with_fix(fix);
    } else {
        check = check.with_hint(format!("install the package that provides {program}"));
    }
    check
}

fn install_command(context: &DoctorContext, package: Package) -> Option<String> {
    let family = distro_family(&context.os_release)?;
    Some(
        match (family, package) {
            (DistroFamily::Apt, Package::Passt) => "sudo apt-get install passt",
            (DistroFamily::Apt, Package::QemuImg) => "sudo apt-get install qemu-utils",
            (DistroFamily::Apt, Package::OpenSsh) => "sudo apt-get install openssh-client",
            (DistroFamily::Dnf, Package::Passt) => "sudo dnf install passt",
            (DistroFamily::Dnf, Package::QemuImg) => "sudo dnf install qemu-img",
            (DistroFamily::Dnf, Package::OpenSsh) => "sudo dnf install openssh-clients",
            (DistroFamily::Pacman, Package::Passt) => "sudo pacman -S passt",
            (DistroFamily::Pacman, Package::QemuImg) => "sudo pacman -S qemu-img",
            (DistroFamily::Pacman, Package::OpenSsh) => "sudo pacman -S openssh",
            (DistroFamily::Zypper, Package::Passt) => "sudo zypper install passt",
            (DistroFamily::Zypper, Package::QemuImg) => "sudo zypper install qemu-tools",
            (DistroFamily::Zypper, Package::OpenSsh) => "sudo zypper install openssh",
        }
        .to_owned(),
    )
}

fn distro_family(os_release: &Path) -> Option<DistroFamily> {
    let fields = parse_os_release(&fs::read_to_string(os_release).ok()?);
    let id = fields.get("ID").map(String::as_str).unwrap_or_default();
    let like = fields
        .get("ID_LIKE")
        .map(String::as_str)
        .unwrap_or_default();
    let candidates = std::iter::once(id).chain(like.split_whitespace());
    for candidate in candidates {
        match candidate {
            "debian" | "ubuntu" => return Some(DistroFamily::Apt),
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => {
                return Some(DistroFamily::Dnf);
            }
            "arch" | "manjaro" => return Some(DistroFamily::Pacman),
            "opensuse" | "suse" | "sles" => return Some(DistroFamily::Zypper),
            _ => {}
        }
    }
    None
}

fn parse_os_release(contents: &str) -> BTreeMap<String, String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, raw_value) = line.split_once('=')?;
            let value = raw_value
                .trim()
                .trim_matches(|character| character == '"' || character == '\'')
                .to_owned();
            Some((key.to_owned(), value))
        })
        .collect()
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

fn shell_quote(path: &Path) -> Option<String> {
    path.to_str()
        .map(|path| format!("'{}'", path.replace('\'', "'\"'\"'")))
}

fn sync_directory(path: &Path) -> Result<(), FirestoneError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot sync dependency directory {}", path.display()),
            )
            .with_source(source)
        })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        ffi::OsString,
        fs,
        io::{Cursor, Read, Write},
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixListener,
        },
        path::{Path, PathBuf},
        thread,
        time::{Duration, Instant},
    };

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::{
        ArtifactFetcher, CHECK_IDS, DoctorCheck, DoctorCheckId, DoctorContext, DoctorReport,
        DoctorStatus, HttpVmmPing, MAX_DEPENDENCY_ARTIFACT_BYTES, MINIMUM_PASST_DATE, Package,
        artifact_state, check_kvm, check_passt, check_user_namespaces,
        content_length_exceeds_limit, copy_bounded, create_firestone_dir, distro_family,
        firestone_error_reason, generate_ssh_key, install_artifact, install_command,
        parse_passt_version, read_reconciled_machine_state_with, reconcile_machine_state,
        redirect_rejection, require_https, run_doctor_with,
    };
    use crate::{
        DependencyArtifact, DependencyManifest, ErrorKind, ExitReason, FirestoneError, LastExit,
        MachineLock, MachineState, MachineStatus, PathInputs, Paths, StateStore, VmmPingProbe,
    };

    struct FakeFetcher {
        payloads: BTreeMap<String, Vec<u8>>,
        calls: RefCell<Vec<String>>,
        fail_after_write: bool,
    }

    impl FakeFetcher {
        fn new(payloads: BTreeMap<String, Vec<u8>>) -> Self {
            Self {
                payloads,
                calls: RefCell::new(Vec::new()),
                fail_after_write: false,
            }
        }

        fn failing(payloads: BTreeMap<String, Vec<u8>>) -> Self {
            Self {
                payloads,
                calls: RefCell::new(Vec::new()),
                fail_after_write: true,
            }
        }
    }

    impl ArtifactFetcher for FakeFetcher {
        fn fetch(&self, url: &reqwest::Url, output: &mut dyn Write) -> Result<(), FirestoneError> {
            self.calls.borrow_mut().push(url.to_string());
            let payload = self.payloads.get(url.as_str()).ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("fake fetcher has no payload for {url}"),
                )
            })?;
            output.write_all(payload).map_err(|source| {
                FirestoneError::new(ErrorKind::Dependency, "fake fetch write failed")
                    .with_source(source)
            })?;
            if self.fail_after_write {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    "injected stream failure",
                ));
            }
            Ok(())
        }
    }

    struct FinalStateDuringPing {
        store: StateStore,
        state: MachineState,
        wrote: Cell<bool>,
    }

    impl VmmPingProbe for FinalStateDuringPing {
        fn ping(&self, _api_socket: &Path) -> Result<bool, FirestoneError> {
            if !self.wrote.replace(true) {
                self.store.write_from_shim(&self.state)?;
            }
            Ok(false)
        }
    }

    struct FixedPing(bool);

    impl VmmPingProbe for FixedPing {
        fn ping(&self, _api_socket: &Path) -> Result<bool, FirestoneError> {
            Ok(self.0)
        }
    }

    struct Fixture {
        _temp: TempDir,
        context: DoctorContext,
        payloads: BTreeMap<String, Vec<u8>>,
        privileged_marker: PathBuf,
    }

    impl Fixture {
        fn healthy() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = TempDir::new()?;
            let canonical_root = fs::canonicalize(temp.path())?;
            let root = canonical_root.as_path();
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            let data_dir = root.join("data");
            let bin_dir = data_dir.join("bin");
            let runtime_dir = root.join("run");
            let key_dir = data_dir.join("ssh");
            let fake_bin = root.join("fake-bin");
            for path in [&data_dir, &bin_dir, &runtime_dir, &key_dir, &fake_bin] {
                fs::create_dir_all(path)?;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }

            let payloads = artifact_payloads();
            let manifest = manifest_for_payloads(&payloads)?;
            for dependency in [
                "cloud-hypervisor",
                "rust-hypervisor-firmware",
                "cloud-hypervisor-edk2",
                "virtiofsd",
            ] {
                let artifact = manifest.artifact(dependency, "x86_64")?;
                let payload = payloads
                    .get(&artifact.url)
                    .ok_or("fixture payload is missing")?;
                let path = bin_dir.join(&artifact.install_name);
                fs::write(&path, payload)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(artifact.expected_mode()))?;
            }

            let ssh_private_key = key_dir.join("id_ed25519");
            let ssh_public_key = key_dir.join("id_ed25519.pub");
            fs::write(&ssh_private_key, "private fixture")?;
            fs::write(&ssh_public_key, "ssh-ed25519 fixture")?;
            fs::set_permissions(&ssh_private_key, fs::Permissions::from_mode(0o600))?;
            fs::set_permissions(&ssh_public_key, fs::Permissions::from_mode(0o644))?;

            let kvm_device = root.join("kvm");
            fs::write(&kvm_device, "fixture")?;
            fs::set_permissions(&kvm_device, fs::Permissions::from_mode(0o660))?;
            let gid = fs::metadata(&kvm_device)?.gid();
            let group_file = root.join("group");
            fs::write(&group_file, format!("kvm:x:{gid}:\n"))?;
            let nested = root.join("nested");
            fs::write(&nested, "Y\n")?;
            let sysctl = root.join("max_user_namespaces");
            fs::write(&sysctl, "1024\n")?;
            let os_release = root.join("os-release");
            fs::write(&os_release, "ID=ubuntu\nID_LIKE=debian\n")?;

            let privileged_marker = root.join("sudo-ran");
            write_executable(
                &fake_bin.join("sudo"),
                &format!("printf ran > '{}'; exit 99", privileged_marker.display()),
            )?;
            write_executable(
                &fake_bin.join("passt"),
                "case \"$1\" in --version) printf 'passt 2024_12_11.09478d5\\n' ;; --help) printf '%s\\n' '--vhost-user' ;; *) exit 2 ;; esac",
            )?;
            write_executable(&fake_bin.join("qemu-img"), "printf 'qemu-img fixture\\n'")?;
            write_executable(&fake_bin.join("ssh"), "printf 'OpenSSH fixture\\n' >&2")?;
            write_executable(
                &fake_bin.join("ssh-keygen"),
                "key=''; while [ \"$#\" -gt 0 ]; do if [ \"$1\" = '-f' ]; then shift; key=$1; fi; shift; done; [ -n \"$key\" ] || exit 2; umask 077; printf private > \"$key\"; printf public > \"$key.pub\"",
            )?;
            write_executable(&fake_bin.join("unshare"), "exit 0")?;

            let paths = Paths::from_inputs(&PathInputs {
                current_dir: root.to_path_buf(),
                home_dir: None,
                firestone_home: None,
                firestone_config_dir: Some(root.join("config")),
                firestone_data_dir: Some(data_dir),
                firestone_runtime_dir: Some(runtime_dir),
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: fs::metadata(root)?.uid(),
            })?;
            let proc_root = root.join("proc");
            fs::create_dir(&proc_root)?;
            let context = DoctorContext {
                paths,
                operating_system: "linux".to_owned(),
                architecture: "x86_64".to_owned(),
                kvm_device,
                nested_parameters: vec![nested],
                user_namespace_sysctl: sysctl,
                os_release,
                group_file,
                search_path: OsString::from(fake_bin),
                hostname: "fixture".to_owned(),
                manifest,
                proc_root,
                reconciled_at: "2026-08-28T00:00:00Z".to_owned(),
                minimum_data_free_bytes: 0,
            };

            Ok(Self {
                _temp: temp,
                context,
                payloads,
                privileged_marker,
            })
        }
    }

    fn write_running_state(paths: &Paths, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        fs::create_dir_all(paths.machines_dir())?;
        fs::create_dir(paths.machine_dir(name)?)?;
        let state = serde_json::json!({
            "version": 1,
            "status": "running",
            "image": {"ref": "ubuntu:24.04", "id": "fixture", "sha256": "fixture"},
            "mac": null,
            "cid": 3,
            "instance_id": "iid-fixture",
            "shim_pid": null,
            "vmm_pid": null,
            "sidecar_pids": {},
            "runtime_dir": paths.machine_runtime_dir(name)?,
            "started_at": "2026-08-28T00:00:00Z",
            "forwards": [],
            "degraded": [],
            "last_exit": null
        });
        fs::write(
            paths.machine_state(name)?,
            serde_json::to_vec_pretty(&state)?,
        )?;
        Ok(())
    }

    fn artifact_payloads() -> BTreeMap<String, Vec<u8>> {
        [
            (
                "https://example.invalid/cloud-hypervisor",
                b"cloud-hypervisor".to_vec(),
            ),
            (
                "https://example.invalid/rhf",
                b"rust-hypervisor-firmware".to_vec(),
            ),
            (
                "https://example.invalid/edk2",
                b"cloud-hypervisor-edk2".to_vec(),
            ),
            ("https://example.invalid/virtiofsd", b"virtiofsd".to_vec()),
        ]
        .into_iter()
        .map(|(url, payload)| (url.to_owned(), payload))
        .collect()
    }

    fn manifest_for_payloads(
        payloads: &BTreeMap<String, Vec<u8>>,
    ) -> Result<DependencyManifest, FirestoneError> {
        let entries = [
            (
                "cloud-hypervisor",
                "v53.0",
                "cloud-hypervisor-v53.0",
                "https://example.invalid/cloud-hypervisor",
            ),
            (
                "rust-hypervisor-firmware",
                "0.5.0",
                "hypervisor-fw-0.5.0",
                "https://example.invalid/rhf",
            ),
            (
                "cloud-hypervisor-edk2",
                "ch-test",
                "CLOUDHV-ch-test.fd",
                "https://example.invalid/edk2",
            ),
            (
                "virtiofsd",
                "v1.14.0",
                "virtiofsd-v1.14.0",
                "https://example.invalid/virtiofsd",
            ),
        ];
        let mut input = "manifest_version = 1\n".to_owned();
        for (dependency, version, install_name, url) in entries {
            let payload = payloads.get(url).ok_or_else(|| {
                FirestoneError::new(ErrorKind::Dependency, "fixture payload missing")
            })?;
            input.push_str(&format!(
                "\n[dependency.{dependency}]\nversion = \"{version}\"\navailability = \"binary\"\n[dependency.{dependency}.x86_64]\nasset = \"asset\"\ninstall_name = \"{install_name}\"\nurl = \"{url}\"\nsha256 = \"{}\"\n",
                sha256(payload)
            ));
        }
        DependencyManifest::parse(&input)
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn write_executable(path: &Path, body: &str) -> Result<(), std::io::Error> {
        fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
    }

    fn check(report: &DoctorReport, id: DoctorCheckId) -> &DoctorCheck {
        let index = match id {
            DoctorCheckId::HostArch => 0,
            DoctorCheckId::Kvm => 1,
            DoctorCheckId::NestedVirtualization => 2,
            DoctorCheckId::RuntimeDir => 3,
            DoctorCheckId::VendoredBinaries => 4,
            DoctorCheckId::Virtiofsd => 5,
            DoctorCheckId::Passt => 6,
            DoctorCheckId::QemuImg => 7,
            DoctorCheckId::Ssh => 8,
            DoctorCheckId::UserNamespaces => 9,
            DoctorCheckId::SshKey => 10,
            DoctorCheckId::DataSpace => 11,
            DoctorCheckId::StaleState => 12,
        };
        &report.checks[index]
    }

    #[test]
    fn report_healthy_context_contains_thirteen_ordered_checks()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let fetcher = FakeFetcher::new(fixture.payloads.clone());
        let report = run_doctor_with(&fixture.context, false, &fetcher);

        assert_eq!(report.checks.len(), 13);
        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.id)
                .collect::<Vec<_>>(),
            CHECK_IDS
        );
        assert!(!report.has_failures(), "{report:#?}");
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == DoctorStatus::Ok)
        );
        assert!(fetcher.calls.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn report_mixed_failures_preserves_spec_order_and_does_not_short_circuit()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.operating_system = "macos".to_owned();
        fixture.context.architecture = "riscv64".to_owned();
        fs::remove_file(&fixture.context.kvm_device)?;
        fs::remove_file(fixture.context.paths.ssh_private_key())?;
        fixture.context.minimum_data_free_bytes = u64::MAX;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.id)
                .collect::<Vec<_>>(),
            CHECK_IDS
        );
        assert_eq!(
            check(&report, DoctorCheckId::HostArch).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check(&report, DoctorCheckId::Kvm).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check(&report, DoctorCheckId::NestedVirtualization).status,
            DoctorStatus::Warn
        );
        assert_eq!(
            check(&report, DoctorCheckId::RuntimeDir).status,
            DoctorStatus::Ok
        );
        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check(&report, DoctorCheckId::DataSpace).status,
            DoctorStatus::Warn
        );
        assert!(report.has_failures());
        Ok(())
    }

    #[test]
    fn runtime_missing_fails_without_read_only_creation() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        fs::remove_dir(fixture.context.paths.runtime_dir())?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::RuntimeDir).status,
            DoctorStatus::Fail
        );
        assert!(!fixture.context.paths.runtime_dir().exists());
        Ok(())
    }

    #[test]
    fn runtime_missing_fix_creates_secure_directory() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::remove_dir(fixture.context.paths.runtime_dir())?;

        let report = run_doctor_with(
            &fixture.context,
            true,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::RuntimeDir).status,
            DoctorStatus::Ok
        );
        let mode = fs::metadata(fixture.context.paths.runtime_dir())?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        Ok(())
    }

    #[test]
    fn doctor_context_from_paths_preserves_resolved_paths() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        let context = DoctorContext::from_paths(
            fixture.context.paths.clone(),
            fixture.context.manifest.clone(),
            "test-host",
            "2026-08-28T01:02:03Z",
        );

        assert_eq!(context.paths.data_dir(), fixture.context.paths.data_dir());
        assert_eq!(
            context.paths.runtime_dir(),
            fixture.context.paths.runtime_dir()
        );
        assert_eq!(context.hostname, "test-host");
        assert_eq!(context.reconciled_at, "2026-08-28T01:02:03Z");
        assert_eq!(
            context.minimum_data_free_bytes,
            DoctorContext::default_minimum_data_free_bytes()
        );
        Ok(())
    }

    fn probe_vmm_status(status: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let socket = directory.path().join("api.sock");
        let listener = UnixListener::bind(&socket)?;
        let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\n\r\n");
        let server = thread::spawn(move || -> Result<(), std::io::Error> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0_u8; 256];
            let mut used = 0;
            while used < request.len()
                && !request[..used]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
            {
                let read = stream.read(&mut request[used..])?;
                if read == 0 {
                    break;
                }
                used += read;
            }
            assert!(request[..used].starts_with(b"GET /api/v1/vmm.ping HTTP/1.1\r\n"));
            stream.write_all(response.as_bytes())?;
            Ok(())
        });

        let pinged = HttpVmmPing.ping(&socket)?;
        server
            .join()
            .map_err(|_| std::io::Error::other("VMM ping fixture panicked"))??;
        Ok(pinged)
    }

    #[test]
    fn vmm_ping_probe_requires_http_200() -> Result<(), Box<dyn std::error::Error>> {
        assert!(probe_vmm_status("200")?);
        assert!(!probe_vmm_status("204")?);
        Ok(())
    }

    #[test]
    fn vmm_ping_probe_uses_one_absolute_deadline() -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let socket = directory.path().join("api.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = thread::spawn(move || -> Result<(), std::io::Error> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0_u8; 256];
            let mut used = 0;
            while used < request.len()
                && !request[..used]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
            {
                let read = stream.read(&mut request[used..])?;
                if read == 0 {
                    return Ok(());
                }
                used += read;
            }
            for byte in b"HTTP/1.1 200 Slow\r\n" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Ok(())
        });

        let started = Instant::now();
        let pinged = HttpVmmPing.ping_with_timeout(&socket, Duration::from_millis(80))?;
        let elapsed = started.elapsed();
        server
            .join()
            .map_err(|_| std::io::Error::other("VMM slow-ping fixture panicked"))??;

        assert!(!pinged);
        assert!(elapsed < Duration::from_millis(250));
        Ok(())
    }
    #[test]
    fn report_serialization_preserves_stable_ids_and_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );
        let value = serde_json::to_value(report)?;
        let Some(checks) = value["checks"].as_array() else {
            return Err(std::io::Error::other("checks should be an array").into());
        };
        assert_eq!(checks[0]["id"], "host_arch");
        assert_eq!(checks[12]["id"], "stale_state");
        assert_eq!(checks[0]["status"], "ok");
        Ok(())
    }

    #[test]
    fn fix_missing_artifacts_and_key_rechecks_modes_without_privileged_commands()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        for dependency in [
            "cloud-hypervisor",
            "rust-hypervisor-firmware",
            "cloud-hypervisor-edk2",
            "virtiofsd",
        ] {
            let artifact = fixture.context.manifest.artifact(dependency, "x86_64")?;
            fs::remove_file(fixture.context.paths.bin_dir().join(artifact.install_name))?;
        }
        fs::remove_file(fixture.context.paths.ssh_private_key())?;
        fs::remove_file(fixture.context.paths.ssh_public_key())?;
        let fetcher = FakeFetcher::new(fixture.payloads.clone());

        let report = run_doctor_with(&fixture.context, true, &fetcher);

        assert_eq!(
            check(&report, DoctorCheckId::VendoredBinaries).status,
            DoctorStatus::Ok
        );
        assert_eq!(
            check(&report, DoctorCheckId::Virtiofsd).status,
            DoctorStatus::Ok
        );
        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Ok
        );
        assert_eq!(fetcher.calls.borrow().len(), 4);
        assert!(!fixture.privileged_marker.exists());
        for dependency in [
            "cloud-hypervisor",
            "rust-hypervisor-firmware",
            "cloud-hypervisor-edk2",
            "virtiofsd",
        ] {
            let artifact = fixture.context.manifest.artifact(dependency, "x86_64")?;
            artifact_state(&fixture.context.paths.bin_dir(), &artifact)?;
        }

        let second = run_doctor_with(&fixture.context, true, &fetcher);
        assert!(!second.has_failures());
        assert_eq!(fetcher.calls.borrow().len(), 4);
        Ok(())
    }

    #[test]
    fn download_checksum_mismatch_preserves_target_and_removes_partial()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let destination = dir.path().join("tool-v1");
        fs::write(&destination, "existing")?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
        let artifact = DependencyArtifact {
            dependency: "cloud-hypervisor".to_owned(),
            version: "v1".to_owned(),
            asset: "tool".to_owned(),
            install_name: "tool-v1".to_owned(),
            url: "https://example.invalid/tool".to_owned(),
            sha256: sha256(b"expected"),
        };
        let fetcher = FakeFetcher::new(BTreeMap::from([(
            artifact.url.clone(),
            b"different".to_vec(),
        )]));

        let error = match install_artifact(dir.path(), &artifact, &fetcher) {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("checksum mismatch should fail").into()),
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert_eq!(fs::read(&destination)?, b"existing");
        let names = fs::read_dir(dir.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(names, [OsString::from("tool-v1")]);
        Ok(())
    }

    #[test]
    fn download_stream_failure_removes_partial_and_valid_download_sets_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let payload = b"valid-tool".to_vec();
        let artifact = DependencyArtifact {
            dependency: "cloud-hypervisor".to_owned(),
            version: "v1".to_owned(),
            asset: "tool".to_owned(),
            install_name: "tool-v1".to_owned(),
            url: "https://example.invalid/tool".to_owned(),
            sha256: sha256(&payload),
        };
        let payloads = BTreeMap::from([(artifact.url.clone(), payload)]);
        let destination = dir.path().join("tool-v1");
        fs::write(&destination, "existing corrupt artifact")?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
        let failing = FakeFetcher::failing(payloads.clone());
        assert!(install_artifact(dir.path(), &artifact, &failing).is_err());
        assert_eq!(fs::read(&destination)?, b"existing corrupt artifact");
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);

        install_artifact(dir.path(), &artifact, &FakeFetcher::new(payloads))?;
        artifact_state(dir.path(), &artifact)?;
        assert_eq!(fs::read(&destination)?, b"valid-tool");
        assert_eq!(
            fs::metadata(dir.path().join("tool-v1"))?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        Ok(())
    }

    #[test]
    fn artifact_matching_symlink_is_not_trusted() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let artifact = fixture
            .context
            .manifest
            .artifact("cloud-hypervisor", "x86_64")?;
        let path = fixture.context.paths.bin_dir().join(&artifact.install_name);
        let outside = fixture.context.paths.data_dir().join("outside-tool");
        fs::write(&outside, fs::read(&path)?)?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755))?;
        fs::remove_file(&path)?;
        std::os::unix::fs::symlink(&outside, &path)?;

        let reason = match artifact_state(&fixture.context.paths.bin_dir(), &artifact) {
            Err(reason) => reason,
            Ok(()) => return Err(std::io::Error::other("symlink should not be accepted").into()),
        };

        assert!(reason.contains("not a regular file"));
        Ok(())
    }

    #[test]
    fn download_http_and_https_downgrade_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let http = reqwest::Url::parse("http://example.invalid/tool")?;
        let https = reqwest::Url::parse("https://example.invalid/tool")?;

        assert!(require_https(&http).is_err());
        assert_eq!(
            redirect_rejection(&http, 0),
            Some("dependency download redirect is not HTTPS")
        );
        assert_eq!(
            redirect_rejection(&https, 10),
            Some("too many dependency download redirects")
        );
        assert_eq!(redirect_rejection(&https, 0), None);
        Ok(())
    }

    #[test]
    fn download_declared_and_streamed_oversize_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(content_length_exceeds_limit(Some(
            MAX_DEPENDENCY_ARTIFACT_BYTES + 1
        )));
        assert!(!content_length_exceeds_limit(Some(
            MAX_DEPENDENCY_ARTIFACT_BYTES
        )));

        let url = reqwest::Url::parse("https://example.invalid/oversize")?;
        let mut reader = Cursor::new(b"123456".to_vec());
        let mut output = Vec::new();
        let error = match copy_bounded(&mut reader, &mut output, 4, &url) {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("oversize body should fail").into()),
        };
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(output, b"12345");
        assert!(error.message().contains("safety limit"));
        Ok(())
    }

    #[test]
    fn passt_version_and_capability_parsing_enforces_authoritative_minimum()
    -> Result<(), Box<dyn std::error::Error>> {
        let minimum = parse_passt_version("passt 2024_12_11.09478d5")
            .ok_or_else(|| std::io::Error::other("minimum version should parse"))?;
        let old = parse_passt_version("passt 2024_11_27.c0fbc7e")
            .ok_or_else(|| std::io::Error::other("old version should parse"))?;
        assert_eq!(minimum.date, MINIMUM_PASST_DATE);
        assert!(old.date < MINIMUM_PASST_DATE);
        assert!(parse_passt_version("passt 4294967295_12_31.abcdef0").is_none());

        let fixture = Fixture::healthy()?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2024_12_11.09478d5\\n' ;; --help) printf 'socket mode only\\n' ;; esac",
        )?;
        let check = check_passt(&fixture.context);
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.reason.contains("vhost-user minimum"));
        Ok(())
    }

    #[test]
    fn passt_bare_date_substring_option_and_nonzero_help_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(parse_passt_version("passt 2024_12_11").is_none());
        assert!(parse_passt_version("passt 2024_12_11.not-hash").is_none());

        let fixture = Fixture::healthy()?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2024_12_11.09478d5\\n' ;; --help) printf '%s\\n' '--vhost-user-old' ;; esac",
        )?;
        let substring = check_passt(&fixture.context);
        assert_eq!(substring.status, DoctorStatus::Fail);

        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2024_12_11.09478d5\\n' ;; --help) printf '%s\\n' '--vhost-user'; exit 2 ;; esac",
        )?;
        let nonzero = check_passt(&fixture.context);
        assert_eq!(nonzero.status, DoctorStatus::Fail);
        assert!(nonzero.reason.contains("--help failed"));
        Ok(())
    }

    #[test]
    fn passt_missing_on_unverified_distro_has_no_ineffective_fix()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        fs::remove_file(passt)?;
        fs::write(
            &fixture.context.os_release,
            "ID=ubuntu\nVERSION_ID=\"24.04\"\nID_LIKE=debian\n",
        )?;

        let check = check_passt(&fixture.context);

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.fix.is_none());
        assert!(
            check
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("apt-get"))
        );
        assert!(
            check
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("2024_12_11"))
        );
        Ok(())
    }

    #[test]
    fn distro_families_produce_exact_install_suggestions() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        for (contents, expected) in [
            ("ID=ubuntu\n", "sudo apt-get install qemu-utils"),
            ("ID=fedora\n", "sudo dnf install qemu-img"),
            ("ID=arch\n", "sudo pacman -S qemu-img"),
            (
                "ID=opensuse-leap\nID_LIKE=\"suse opensuse\"\n",
                "sudo zypper install qemu-tools",
            ),
        ] {
            fs::write(&fixture.context.os_release, contents)?;
            assert!(distro_family(&fixture.context.os_release).is_some());
            assert_eq!(
                install_command(&fixture.context, Package::QemuImg).as_deref(),
                Some(expected)
            );
        }
        fs::write(&fixture.context.os_release, "ID=unknown\n")?;
        assert!(install_command(&fixture.context, Package::QemuImg).is_none());
        Ok(())
    }

    #[test]
    fn user_namespaces_zero_or_failed_unshare_warns_with_sandbox_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::write(&fixture.context.user_namespace_sysctl, "0\n")?;
        let zero = check_user_namespaces(&fixture.context);
        assert_eq!(zero.status, DoctorStatus::Warn);
        assert!(
            zero.hint
                .is_some_and(|hint| hint.contains("--sandbox none"))
        );

        fs::write(&fixture.context.user_namespace_sysctl, "1024\n")?;
        let unshare = PathBuf::from(&fixture.context.search_path).join("unshare");
        write_executable(&unshare, "printf denied >&2; exit 1")?;
        let denied = check_user_namespaces(&fixture.context);
        assert_eq!(denied.status, DoctorStatus::Warn);
        assert!(denied.reason.contains("denied"));
        Ok(())
    }

    #[test]
    fn kvm_open_failure_reports_detected_device_group_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.kvm_device = fixture.context.paths.runtime_dir().to_path_buf();

        let kvm = check_kvm(&fixture.context);

        assert_eq!(kvm.check.status, DoctorStatus::Fail);
        assert_eq!(kvm.check.fix.as_deref(), Some("sudo usermod -aG kvm $USER"));
        assert!(kvm.check.reason.contains("device group is kvm"));
        Ok(())
    }

    #[test]
    fn directory_fix_rejects_symlink_without_changing_target_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        std::os::unix::fs::symlink(&target, &link)?;

        let error = match create_firestone_dir(&link, 0o700) {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("directory symlink should fail").into()),
        };

        assert!(error.message().contains("non-directory"));
        assert_eq!(fs::metadata(target)?.permissions().mode() & 0o777, 0o755);
        Ok(())
    }

    #[test]
    fn ssh_key_complete_symlink_pair_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let private_target = fixture.context.paths.data_dir().join("private-target");
        let public_target = fixture.context.paths.data_dir().join("public-target");
        fs::write(&private_target, "private")?;
        fs::write(&public_target, "public")?;
        fs::set_permissions(&private_target, fs::Permissions::from_mode(0o600))?;
        fs::remove_file(fixture.context.paths.ssh_private_key())?;
        fs::remove_file(fixture.context.paths.ssh_public_key())?;
        std::os::unix::fs::symlink(&private_target, fixture.context.paths.ssh_private_key())?;
        std::os::unix::fs::symlink(&public_target, fixture.context.paths.ssh_public_key())?;

        let report = run_doctor_with(
            &fixture.context,
            true,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Fail
        );
        assert!(
            fs::symlink_metadata(fixture.context.paths.ssh_private_key())?
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(private_target)?, b"private");
        Ok(())
    }

    #[test]
    fn stale_machine_states_reconcile_in_sorted_order() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        write_running_state(&fixture.context.paths, "zeta")?;
        write_running_state(&fixture.context.paths, "alpha")?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        let stale = check(&report, DoctorCheckId::StaleState);
        assert_eq!(stale.status, DoctorStatus::Ok);
        assert!(
            stale.reason.find("alpha").is_some_and(|alpha| {
                stale.reason.find("zeta").is_some_and(|zeta| alpha < zeta)
            })
        );
        for name in ["alpha", "zeta"] {
            let state: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.context.paths.machine_state(name)?)?)?;
            assert_eq!(state["status"], "stopped");
            assert_eq!(state["last_exit"]["reason"], "host reboot");
        }
        Ok(())
    }

    #[test]
    fn live_read_reports_ping_status_without_taking_lock_or_rewriting()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let name = "live";
        write_running_state(&fixture.context.paths, name)?;
        fs::create_dir(fixture.context.paths.machine_runtime_dir(name)?)?;
        let store = StateStore::new(fixture.context.paths.machine_state(name)?);
        let mut stored = store.read()?;
        stored.status = MachineStatus::Starting;
        store.write_from_shim(&stored)?;
        let mut events = Vec::new();
        let lock = MachineLock::acquire(
            name,
            &fixture.context.paths.machine_lock(name)?,
            &mut events,
        )?;

        let (effective, reason) = read_reconciled_machine_state_with(
            &fixture.context.paths,
            name,
            &fixture.context.proc_root,
            &fixture.context.reconciled_at,
            &FixedPing(true),
        )?;

        assert_eq!(effective.status, MachineStatus::Running);
        assert_eq!(reason, None);
        assert_eq!(store.read()?.status, MachineStatus::Starting);
        drop(lock);
        Ok(())
    }

    #[test]
    fn reconciliation_rereads_after_shim_final_state_write()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let name = "race";
        write_running_state(&fixture.context.paths, name)?;
        fs::create_dir(fixture.context.paths.machine_runtime_dir(name)?)?;
        let store = StateStore::new(fixture.context.paths.machine_state(name)?);
        let mut final_state = store.read()?;
        final_state.status = MachineStatus::Stopped;
        final_state.started_at = None;
        final_state.last_exit = Some(LastExit {
            at: "2026-08-28T00:00:01Z".to_owned(),
            code: Some(17),
            signal: None,
            reason: ExitReason::Failure("vmm exited".to_owned()),
        });
        let expected = final_state.clone();
        let ping = FinalStateDuringPing {
            store: store.clone(),
            state: final_state,
            wrote: Cell::new(false),
        };

        let observation = reconcile_machine_state(&fixture.context, name, &ping)?;

        assert!(observation.is_none());
        assert_eq!(store.read()?, expected);
        Ok(())
    }

    #[test]
    fn stale_machine_failure_does_not_abort_later_reconciliation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        write_running_state(&fixture.context.paths, "zeta")?;
        fs::create_dir(fixture.context.paths.machine_dir("alpha")?)?;
        fs::write(fixture.context.paths.machine_state("alpha")?, b"{")?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        let stale = check(&report, DoctorCheckId::StaleState);
        assert_eq!(stale.status, DoctorStatus::Fail);
        assert!(stale.reason.contains("alpha"));
        assert!(stale.reason.contains("zeta (host reboot)"));
        let zeta: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.context.paths.machine_state("zeta")?)?)?;
        assert_eq!(zeta["status"], "stopped");
        assert!(
            report
                .checks
                .iter()
                .filter(|check| check.id != DoctorCheckId::StaleState)
                .all(|check| check.status == DoctorStatus::Ok)
        );
        Ok(())
    }
    #[test]
    fn report_error_reason_includes_bounded_source_context() {
        let error = FirestoneError::new(ErrorKind::Dependency, "cannot run probe")
            .with_source(std::io::Error::from(std::io::ErrorKind::PermissionDenied));

        let reason = firestone_error_reason(&error);

        assert!(reason.contains("cannot run probe"));
        assert!(reason.contains("permission denied"));
    }

    #[test]
    fn fix_incomplete_ssh_key_does_not_overwrite_existing_private_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let original = fs::read(fixture.context.paths.ssh_private_key())?;
        fs::remove_file(fixture.context.paths.ssh_public_key())?;

        let report = run_doctor_with(
            &fixture.context,
            true,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Fail
        );
        assert_eq!(fs::read(fixture.context.paths.ssh_private_key())?, original);
        assert!(!fixture.context.paths.ssh_public_key().exists());
        Ok(())
    }

    #[test]
    fn failed_ssh_key_generation_rolls_back_and_can_retry() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        let private_key = fixture.context.paths.ssh_private_key();
        let public_key = fixture.context.paths.ssh_public_key();
        fs::remove_file(&private_key)?;
        fs::remove_file(&public_key)?;
        let keygen = PathBuf::from(&fixture.context.search_path).join("ssh-keygen");
        write_executable(
            &keygen,
            r#"key=''; while [ "$#" -gt 0 ]; do if [ "$1" = '-f' ]; then shift; key=$1; fi; shift; done; printf private > "$key"; exit 23"#,
        )?;

        assert!(generate_ssh_key(&fixture.context).is_err());
        assert!(!private_key.exists());
        assert!(!public_key.exists());

        write_executable(
            &keygen,
            r#"key=''; while [ "$#" -gt 0 ]; do if [ "$1" = '-f' ]; then shift; key=$1; fi; shift; done; [ -n "$key" ] || exit 2; umask 077; printf private > "$key"; printf public > "$key.pub""#,
        )?;
        generate_ssh_key(&fixture.context)?;

        assert!(private_key.is_file());
        assert!(public_key.is_file());
        assert_eq!(
            fs::metadata(private_key)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn fix_stream_failure_retains_failed_check_and_complete_report()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let artifact = fixture
            .context
            .manifest
            .artifact("cloud-hypervisor", "x86_64")?;
        fs::remove_file(fixture.context.paths.bin_dir().join(&artifact.install_name))?;
        let fetcher = FakeFetcher::failing(fixture.payloads.clone());

        let report = run_doctor_with(&fixture.context, true, &fetcher);

        assert_eq!(report.checks.len(), 13);
        let vendored = check(&report, DoctorCheckId::VendoredBinaries);
        assert_eq!(vendored.status, DoctorStatus::Fail);
        assert!(
            vendored
                .reason
                .contains("fix failed: injected stream failure")
        );
        assert!(
            fs::read_dir(fixture.context.paths.bin_dir())?
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".partial"))
        );
        Ok(())
    }

    #[test]
    fn source_only_virtiofsd_reports_stable_release_blocker()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.manifest = DependencyManifest::bundled()?;
        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );
        let check = check(&report, DoctorCheckId::Virtiofsd);
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.reason.contains("no immutable x86_64 binary"));
        assert!(
            check
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("M0-05c"))
        );
        Ok(())
    }
}
