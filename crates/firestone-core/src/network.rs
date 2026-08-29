use std::{
    ffi::{OsStr, OsString},
    fs, io,
    net::IpAddr,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use nix::sys::socket::UnixAddr;

use crate::{
    Cmd, ErrorKind, FirestoneError, MacAddr, NetMode, NetworkSpec, Paths, PortForward, Protocol,
    ValidationHost,
};

/// Passt release whose command grammar is the M3 contract.
pub const PINNED_PASST_VERSION: &str = "2025_02_17.a1e48a0";
/// Default sidecar socket deadline, matching the shim readiness budget.
pub const DEFAULT_NETWORK_READINESS_TIMEOUT: Duration = Duration::from_secs(10);
/// Socket polling cadence used without consuming the vhost-user connection.
pub const DEFAULT_NETWORK_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

const MAX_NETWORK_READINESS_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const PASST_SOCKET_MODE: u32 = 0o700;
const PASST_LOG_MODE: u32 = 0o600;

/// Host operations needed to validate a user-managed TAP device.
pub trait TapHost: Send + Sync {
    fn tap_device_is_tap(&self, name: &str) -> io::Result<bool>;
    fn tun_is_accessible(&self) -> io::Result<()>;
}

impl<T: ValidationHost + ?Sized> TapHost for T {
    fn tap_device_is_tap(&self, name: &str) -> io::Result<bool> {
        ValidationHost::tap_device_is_tap(self, name)
    }

    fn tun_is_accessible(&self) -> io::Result<()> {
        ValidationHost::tun_is_accessible(self)
    }
}

/// Inputs for deterministic network validation and command preparation.
pub struct NetworkPlanOptions<'a> {
    pub paths: &'a Paths,
    pub name: &'a str,
    pub spec: &'a NetworkSpec,
    pub mac: MacAddr,
    pub passt_program: &'a OsStr,
    pub tap_host: &'a dyn TapHost,
    pub readiness_timeout: Duration,
    pub readiness_poll_interval: Duration,
}

impl<'a> NetworkPlanOptions<'a> {
    #[must_use]
    pub const fn new(
        paths: &'a Paths,
        name: &'a str,
        spec: &'a NetworkSpec,
        mac: MacAddr,
        passt_program: &'a OsStr,
        tap_host: &'a dyn TapHost,
    ) -> Self {
        Self {
            paths,
            name,
            spec,
            mac,
            passt_program,
            tap_host,
            readiness_timeout: DEFAULT_NETWORK_READINESS_TIMEOUT,
            readiness_poll_interval: DEFAULT_NETWORK_READINESS_POLL_INTERVAL,
        }
    }
}

/// Fully validated network work for one machine. No variant starts a process.
#[derive(Debug, Clone)]
pub enum NetworkPlan {
    None,
    Passt(Box<PasstPlan>),
    Tap(TapPlan),
}

impl NetworkPlan {
    #[must_use]
    pub const fn mac(&self) -> Option<MacAddr> {
        match self {
            Self::None => None,
            Self::Passt(plan) => Some(plan.mac),
            Self::Tap(plan) => Some(plan.mac),
        }
    }

    #[must_use]
    pub fn forwards(&self) -> &[PortForward] {
        match self {
            Self::Passt(plan) => &plan.forwards,
            Self::None | Self::Tap(_) => &[],
        }
    }
}

/// One current-user path and the exact node mode expected at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedPathExpectation {
    path: PathBuf,
    uid: u32,
    mode: u32,
}

impl OwnedPathExpectation {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    #[must_use]
    pub const fn mode(&self) -> u32 {
        self.mode
    }
}

/// Passt vhost-user sidecar command and its owned artifacts.
#[derive(Debug, Clone)]
pub struct PasstPlan {
    command: Cmd,
    socket: OwnedPathExpectation,
    log: OwnedPathExpectation,
    readiness: SocketReadinessPlan,
    forwards: Vec<PortForward>,
    mac: MacAddr,
}

impl PasstPlan {
    #[must_use]
    pub const fn command(&self) -> &Cmd {
        &self.command
    }

    #[must_use]
    pub const fn socket(&self) -> &OwnedPathExpectation {
        &self.socket
    }

    #[must_use]
    pub const fn log(&self) -> &OwnedPathExpectation {
        &self.log
    }

    #[must_use]
    pub const fn readiness(&self) -> &SocketReadinessPlan {
        &self.readiness
    }

    #[must_use]
    pub fn forwards(&self) -> &[PortForward] {
        &self.forwards
    }

    #[must_use]
    pub const fn mac(&self) -> MacAddr {
        self.mac
    }

    /// Waits for the socket node only. Connecting here would consume passt's
    /// single `--one-off` vhost-user client before Cloud Hypervisor can connect.
    pub fn wait_ready(
        &self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), FirestoneError> {
        self.readiness.wait_ready(deadline, cancelled)
    }
}

/// Assumption Firestone makes about a TAP interface it never creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapOwnership {
    ExistingUserOwned,
}

/// Existing TAP device attached directly by Cloud Hypervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapPlan {
    name: String,
    mac: MacAddr,
    ownership: TapOwnership,
    ip: Option<IpAddr>,
    mask: Option<IpAddr>,
}

impl TapPlan {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn mac(&self) -> MacAddr {
        self.mac
    }

    #[must_use]
    pub const fn ownership(&self) -> TapOwnership {
        self.ownership
    }

    #[must_use]
    pub const fn ip(&self) -> Option<IpAddr> {
        self.ip
    }

    #[must_use]
    pub const fn mask(&self) -> Option<IpAddr> {
        self.mask
    }
}

/// Cancellation result required while waiting for a sidecar socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessCancellation {
    AbortLaunch,
}

/// Result of a metadata-only sidecar socket probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketReadiness {
    Pending,
    Ready,
}

/// Bounded readiness contract for one current-user Unix socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketReadinessPlan {
    socket: OwnedPathExpectation,
    timeout: Duration,
    poll_interval: Duration,
    cancellation: ReadinessCancellation,
}

impl SocketReadinessPlan {
    pub fn new(
        socket: OwnedPathExpectation,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, FirestoneError> {
        if timeout.is_zero() {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                "network sidecar readiness timeout must be greater than zero",
            )
            .with_hint("use a readiness timeout from 1 ms through 1 hour"));
        }
        if timeout > MAX_NETWORK_READINESS_TIMEOUT {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                "network sidecar readiness timeout exceeds the 3600 second limit",
            )
            .with_hint("use a readiness timeout of at most 1 hour"));
        }
        if poll_interval.is_zero() || poll_interval > timeout {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                "network sidecar readiness poll interval must be nonzero and no greater than its timeout",
            )
            .with_hint("use the default 10 ms poll interval"));
        }
        Ok(Self {
            socket,
            timeout,
            poll_interval,
            cancellation: ReadinessCancellation::AbortLaunch,
        })
    }

    #[must_use]
    pub const fn socket(&self) -> &OwnedPathExpectation {
        &self.socket
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    #[must_use]
    pub const fn cancellation(&self) -> ReadinessCancellation {
        self.cancellation
    }

    pub fn inspect(&self) -> Result<SocketReadiness, FirestoneError> {
        let metadata = match fs::symlink_metadata(&self.socket.path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(SocketReadiness::Pending);
            }
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "cannot inspect passt socket '{}'",
                        self.socket.path.display()
                    ),
                )
                .with_hint("check the private machine runtime directory and retry")
                .with_source(source));
            }
        };
        if !metadata.file_type().is_socket() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "passt socket '{}' is not a Unix socket",
                    self.socket.path.display()
                ),
            )
            .with_hint("remove the stale runtime entry and retry start"));
        }
        let mode = metadata.mode() & 0o7777;
        if metadata.uid() != self.socket.uid || mode != self.socket.mode {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "passt socket '{}' is insecure: expected uid {} and mode {:04o}, found uid {} and mode {mode:04o}",
                    self.socket.path.display(),
                    self.socket.uid,
                    self.socket.mode,
                    metadata.uid(),
                ),
            )
            .with_hint("remove the stale socket and let the Firestone shim recreate it"));
        }
        Ok(SocketReadiness::Ready)
    }

    pub fn bounded_deadline(
        &self,
        started: Instant,
        overall_deadline: Instant,
    ) -> Result<Instant, FirestoneError> {
        let plan_deadline = started.checked_add(self.timeout).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Usage,
                "network sidecar readiness deadline overflows the monotonic clock",
            )
            .with_hint("use a readiness timeout of at most 1 hour")
        })?;
        Ok(plan_deadline.min(overall_deadline))
    }

    pub fn wait_ready(
        &self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), FirestoneError> {
        let deadline = self.bounded_deadline(Instant::now(), deadline)?;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FirestoneError::new(
                    ErrorKind::Interrupted,
                    format!(
                        "cancelled while waiting for passt socket '{}'",
                        self.socket.path.display()
                    ),
                )
                .with_hint("the launch must stop passt and roll back before starting the VMM"));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(FirestoneError::new(
                    ErrorKind::Timeout,
                    format!(
                        "passt socket '{}' was not ready before its deadline",
                        self.socket.path.display()
                    ),
                )
                .with_hint("inspect the mode-0600 passt log for the startup failure"));
            }
            if self.inspect()? == SocketReadiness::Ready {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                thread::sleep(self.poll_interval.min(remaining));
            }
        }
    }
}

/// Validates the network mode and builds its process/device plan without spawning.
pub fn prepare_network(options: NetworkPlanOptions<'_>) -> Result<NetworkPlan, FirestoneError> {
    // Every mode validates the machine name through the authoritative Paths join.
    let _ = options.paths.machine_runtime_dir(options.name)?;

    match options.spec.mode {
        NetMode::None => {
            reject_forwards_without_passt(options.spec)?;
            Ok(NetworkPlan::None)
        }
        NetMode::Tap => {
            reject_forwards_without_passt(options.spec)?;
            let name = options.spec.tap.as_deref().ok_or_else(|| {
                invalid_network(
                    "network.tap",
                    "tap mode requires an existing tap interface",
                    "set network.tap to a user-owned interface such as 'tap0'",
                )
            })?;
            validate_tap(name, options.tap_host)?;
            Ok(NetworkPlan::Tap(TapPlan {
                name: name.to_owned(),
                mac: options.mac,
                ownership: TapOwnership::ExistingUserOwned,
                ip: None,
                mask: None,
            }))
        }
        NetMode::Passt => prepare_passt(options).map(|plan| NetworkPlan::Passt(Box::new(plan))),
    }
}

/// Validates a Linux interface name and the non-privileged checks Firestone can make.
pub fn validate_tap<H: TapHost + ?Sized>(name: &str, host: &H) -> Result<(), FirestoneError> {
    validate_tap_name(name)?;
    let exists = host.tap_device_is_tap(name).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("invalid 'network.tap': cannot inspect TAP interface '{name}'"),
        )
        .with_hint("check that /sys/class/net is mounted and readable")
        .with_source(source)
    })?;
    if !exists {
        return Err(invalid_network(
            "network.tap",
            format!("interface '{name}' does not exist or is not a TAP device"),
            format!("create a user-owned tap named '{name}' or choose network.mode = 'passt'"),
        ));
    }
    host.tun_is_accessible().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "invalid 'network.tap': /dev/net/tun is not accessible for tap mode",
        )
        .with_hint(
            "grant this user read/write access to /dev/net/tun or choose network.mode = 'passt'",
        )
        .with_source(source)
    })
}

/// Exact pinned passt `-t`/`-u` value for one Firestone forward.
///
/// This depends on SPEC verify item 15 and passt commit
/// `a1e48a02ff3550eb7875a7df6726086e9b3a1213`.
pub fn passt_forward_argument(forward: PortForward) -> Result<String, FirestoneError> {
    validate_translation_bounds(forward)?;
    let mapping = format!("{}:{}", forward.host(), forward.guest());
    Ok(match forward.bind() {
        None => mapping,
        Some(IpAddr::V4(address)) => format!("{address}/{mapping}"),
        Some(IpAddr::V6(address)) => format!("[{address}]/{mapping}"),
    })
}

fn prepare_passt(options: NetworkPlanOptions<'_>) -> Result<PasstPlan, FirestoneError> {
    if options.passt_program.as_bytes().is_empty() {
        return Err(
            FirestoneError::new(ErrorKind::Dependency, "passt program path is empty").with_hint(
                format!("install passt {PINNED_PASST_VERSION} or newer and run firestone doctor"),
            ),
        );
    }
    validate_forward_set(&options.spec.forward)?;
    options
        .paths
        .validate_machine_data_directory(options.name)?;
    options.paths.validate_machine_runtime_dir(options.name)?;

    let socket_path = options.paths.machine_net_socket(options.name)?;
    validate_socket_destination(&socket_path)?;
    UnixAddr::new(&socket_path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "passt socket path '{}' cannot be represented as a Unix socket",
                socket_path.display()
            ),
        )
        .with_hint("shorten FIRESTONE_HOME or FIRESTONE_RUNTIME_DIR")
        .with_source(source)
    })?;
    if socket_path.to_str().is_none() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("passt socket path '{}' is not UTF-8", socket_path.display()),
        )
        .with_hint("use a UTF-8 Firestone runtime directory"));
    }

    let log_path = options.paths.machine_passt_log(options.name)?;
    options
        .paths
        .validate_owned_data_file(&log_path, "passt log", PASST_LOG_MODE, true)?;

    let socket = OwnedPathExpectation {
        path: socket_path.clone(),
        uid: options.paths.uid(),
        mode: PASST_SOCKET_MODE,
    };
    let log = OwnedPathExpectation {
        path: log_path.clone(),
        uid: options.paths.uid(),
        mode: PASST_LOG_MODE,
    };
    let readiness = SocketReadinessPlan::new(
        socket.clone(),
        options.readiness_timeout,
        options.readiness_poll_interval,
    )?;

    let additional = options.spec.forward.len().checked_mul(2).ok_or_else(|| {
        invalid_network(
            "network.forward",
            "too many port forwards to construct passt arguments",
            "reduce the number of network.forward entries",
        )
    })?;
    let mut arguments = Vec::with_capacity(9_usize.saturating_add(additional));
    arguments.extend([
        OsString::from("--foreground"),
        OsString::from("--one-off"),
        OsString::from("--vhost-user"),
        OsString::from("--socket"),
        socket_path.into_os_string(),
        OsString::from("--log-file"),
        log_path.clone().into_os_string(),
    ]);
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        for forward in options
            .spec
            .forward
            .iter()
            .copied()
            .filter(|forward| forward.protocol() == protocol)
        {
            arguments.push(OsString::from(match protocol {
                Protocol::Tcp => "-t",
                Protocol::Udp => "-u",
            }));
            arguments.push(OsString::from(passt_forward_argument(forward)?));
        }
    }
    // Pinned conf.c ends its second option pass at --repair-path, so this
    // option and its argument must be the final argv pair.
    arguments.extend([OsString::from("--repair-path"), OsString::from("none")]);

    let command = Cmd::new(options.passt_program)
        .args(arguments)
        .cwd("/")
        .stdin_null()
        .stdout_append(&log_path)
        .stderr_append(&log_path)
        .error_kind(ErrorKind::Dependency)
        .reduced_environment();

    Ok(PasstPlan {
        command,
        socket,
        log,
        readiness,
        forwards: options.spec.forward.clone(),
        mac: options.mac,
    })
}

fn validate_tap_name(name: &str) -> Result<(), FirestoneError> {
    let max_bytes = nix::libc::IFNAMSIZ.saturating_sub(1);
    if name.is_empty()
        || name.len() > max_bytes
        || name.contains('/')
        || name.as_bytes().contains(&0)
        || matches!(name, "." | "..")
    {
        return Err(invalid_network(
            "network.tap",
            format!("tap interface name '{name}' is invalid"),
            format!(
                "use a Linux interface name of at most {max_bytes} bytes, such as 'tap0', without path separators"
            ),
        ));
    }
    Ok(())
}

fn reject_forwards_without_passt(spec: &NetworkSpec) -> Result<(), FirestoneError> {
    if spec.forward.is_empty() {
        return Ok(());
    }
    Err(invalid_network(
        "network.forward",
        "port forwards require network.mode = 'passt'",
        "clear network.forward or select network.mode = 'passt'",
    ))
}

fn validate_forward_set(forwards: &[PortForward]) -> Result<(), FirestoneError> {
    for (later_index, later) in forwards.iter().copied().enumerate() {
        validate_translation_bounds(later)?;
        for (earlier_index, earlier) in forwards[..later_index].iter().copied().enumerate() {
            if earlier.protocol() != later.protocol()
                || earlier.host().end() < later.host().start()
                || later.host().end() < earlier.host().start()
            {
                continue;
            }
            if earlier == later {
                return Err(invalid_network(
                    &format!("network.forward[{later_index}]"),
                    format!(
                        "duplicate {} host-port mapping '{}' also appears at network.forward[{earlier_index}]",
                        later.protocol(),
                        later.host()
                    ),
                    "remove the duplicate network.forward entry",
                ));
            }
            return Err(invalid_network(
                &format!("network.forward[{later_index}]"),
                format!(
                    "{} host ports '{}' conflict with network.forward[{earlier_index}] host ports '{}'",
                    later.protocol(),
                    later.host(),
                    earlier.host()
                ),
                "use non-overlapping host ports for each protocol; bind addresses do not create separate passt mappings",
            ));
        }
    }
    Ok(())
}

fn validate_translation_bounds(forward: PortForward) -> Result<(), FirestoneError> {
    let span = u32::from(forward.host().end()) - u32::from(forward.host().start());
    let mapped_end = u32::from(forward.guest().start())
        .checked_add(span)
        .filter(|end| *end <= u32::from(u16::MAX));
    if mapped_end != Some(u32::from(forward.guest().end())) {
        return Err(invalid_network(
            "network.forward",
            format!("port translation '{forward}' overflows the valid port range"),
            "use equal-length host and guest ranges within ports 1 through 65535",
        ));
    }
    Ok(())
}

fn validate_socket_destination(path: &Path) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot inspect passt socket destination '{}'",
                path.display()
            ),
        )
        .with_hint("check the private machine runtime directory and retry")
        .with_source(source)),
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "passt socket destination '{}' already exists",
                path.display()
            ),
        )
        .with_hint("remove the stale runtime entry or stop the running machine before retrying")),
    }
}

fn invalid_network(
    key: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("invalid '{key}': {}", message.into()),
    )
    .with_hint(hint)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        fs, io,
        os::unix::{
            fs::{PermissionsExt, symlink},
            net::UnixListener,
        },
        path::{Path, PathBuf},
        str::FromStr,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use crate::{
        ErrorKind, MacAddr, NetMode, NetworkSpec, PathInputs, Paths, PortForward,
        network::{
            NetworkPlan, NetworkPlanOptions, ReadinessCancellation, SocketReadiness, TapHost,
            TapOwnership, passt_forward_argument, prepare_network,
        },
    };

    struct Fixture {
        _temp: TempDir,
        paths: Paths,
        passt: PathBuf,
        mac: MacAddr,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = tempfile::tempdir()?;
            let root = fs::canonicalize(temp.path())?;
            let paths = paths_for_root(root.clone())?;
            let machine = paths.machine_dir("demo")?;
            let runtime = paths.machine_runtime_dir("demo")?;
            fs::create_dir_all(&machine)?;
            fs::create_dir_all(&runtime)?;
            for directory in [
                root,
                paths.data_dir().to_path_buf(),
                paths.machines_dir(),
                machine,
                paths.runtime_dir().to_path_buf(),
                runtime,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
            let passt = temp.path().join("passt-2025_02_17.a1e48a0");
            write_executable(
                &passt,
                "for argument in \"$@\"; do printf '%s\\n' \"$argument\"; done",
            )?;
            Ok(Self {
                _temp: temp,
                paths,
                passt,
                mac: MacAddr::from_str("52:54:00:9a:1f:c3")?,
            })
        }

        fn options<'a>(
            &'a self,
            spec: &'a NetworkSpec,
            host: &'a dyn TapHost,
        ) -> NetworkPlanOptions<'a> {
            NetworkPlanOptions::new(
                &self.paths,
                "demo",
                spec,
                self.mac,
                self.passt.as_os_str(),
                host,
            )
        }
    }

    #[derive(Default)]
    struct FakeTapHost {
        is_tap: bool,
        tun_error: Option<io::ErrorKind>,
    }

    impl TapHost for FakeTapHost {
        fn tap_device_is_tap(&self, _name: &str) -> io::Result<bool> {
            Ok(self.is_tap)
        }

        fn tun_is_accessible(&self) -> io::Result<()> {
            match self.tun_error {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn passt_plan_executes_fake_binary_with_pinned_argv_and_reduced_environment()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let host = FakeTapHost::default();
        let spec = NetworkSpec {
            mode: NetMode::Passt,
            forward: vec![
                "udp:127.0.0.1:5353:53".parse()?,
                "8080:80".parse()?,
                "tcp:[2001:db8::1]:8000-8010:9000-9010".parse()?,
            ],
            ..NetworkSpec::default()
        };
        let plan = prepare_network(fixture.options(&spec, &host))?;
        let NetworkPlan::Passt(plan) = plan else {
            return Err("expected passt plan".into());
        };
        let command = plan.command();
        assert_eq!(command.program(), fixture.passt.as_os_str());
        assert_eq!(command.working_directory(), Some(Path::new("/")));
        assert_eq!(command.stdout_append_path(), Some(plan.log().path()));
        assert_eq!(command.stderr_append_path(), Some(plan.log().path()));
        assert!(command.clears_environment());
        assert!(command.environment().keys().all(|key| {
            matches!(
                key.to_str(),
                Some("PATH" | "HOME" | "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "XDG_RUNTIME_DIR")
            ) || key.to_string_lossy().starts_with("FIRESTONE_")
        }));

        let expected = vec![
            OsString::from("--foreground"),
            OsString::from("--one-off"),
            OsString::from("--vhost-user"),
            OsString::from("--socket"),
            plan.socket().path().as_os_str().to_owned(),
            OsString::from("--log-file"),
            plan.log().path().as_os_str().to_owned(),
            OsString::from("-t"),
            OsString::from("8080:80"),
            OsString::from("-t"),
            OsString::from("[2001:db8::1]/8000-8010:9000-9010"),
            OsString::from("-u"),
            OsString::from("127.0.0.1/5353:53"),
            OsString::from("--repair-path"),
            OsString::from("none"),
        ];
        assert_eq!(
            command.arguments().map(OsStr::to_owned).collect::<Vec<_>>(),
            expected
        );

        let mut child = command.spawn_process_group()?;
        assert!(child.wait()?.success());
        let output = fs::read_to_string(plan.log().path())?;
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            expected
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>()
        );
        assert_eq!(plan.socket().uid(), fixture.paths.uid());
        assert_eq!(plan.socket().mode(), 0o700);
        assert_eq!(plan.log().mode(), 0o600);
        Ok(())
    }

    #[test]
    fn passt_forward_mapping_covers_bind_range_and_port_boundaries()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            ("1:65535", "1:65535"),
            ("1-65535:1-65535", "1-65535:1-65535"),
            ("127.0.0.1:2222:22", "127.0.0.1/2222:22"),
            ("udp:0.0.0.0:5353:53", "0.0.0.0/5353:53"),
            ("[::1]:65535:1", "[::1]/65535:1"),
            (
                "udp:2001:db8::1:9-10:65534-65535",
                "[2001:db8::1]/9-10:65534-65535",
            ),
        ];
        for (source, expected) in cases {
            let forward = PortForward::from_str(source)?;
            assert_eq!(passt_forward_argument(forward)?, expected);
        }
        Ok(())
    }

    #[test]
    fn passt_plan_rejects_duplicate_conflicting_and_non_passt_forwards()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let host = FakeTapHost::default();
        for (forwards, needle) in [
            (vec!["8080:80", "8080:80"], "duplicate"),
            (
                vec!["127.0.0.1:8000-8010:80-90", "[::1]:8010-8020:100-110"],
                "conflict",
            ),
        ] {
            let spec = NetworkSpec {
                forward: forwards
                    .into_iter()
                    .map(PortForward::from_str)
                    .collect::<Result<Vec<_>, _>>()?,
                ..NetworkSpec::default()
            };
            let error = prepare_network(fixture.options(&spec, &host))
                .expect_err("overlapping forward must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert!(error.message().contains(needle));
            assert!(error.hint().is_some());
        }

        let allowed = NetworkSpec {
            forward: vec!["8080:80".parse()?, "udp:8080:80".parse()?],
            ..NetworkSpec::default()
        };
        prepare_network(fixture.options(&allowed, &host))?;

        for mode in [NetMode::Tap, NetMode::None] {
            let spec = NetworkSpec {
                mode,
                forward: vec!["8080:80".parse()?],
                tap: (mode == NetMode::Tap).then(|| "tap0".to_owned()),
                ..NetworkSpec::default()
            };
            let error = prepare_network(fixture.options(&spec, &host))
                .expect_err("forward without passt must fail");
            assert!(error.message().contains("require network.mode = 'passt'"));
        }
        Ok(())
    }

    #[test]
    fn tap_plan_validates_name_device_access_and_emits_no_sidecar()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let valid_host = FakeTapHost {
            is_tap: true,
            tun_error: None,
        };
        let spec = NetworkSpec {
            mode: NetMode::Tap,
            tap: Some("tap0".to_owned()),
            ..NetworkSpec::default()
        };
        let plan = prepare_network(fixture.options(&spec, &valid_host))?;
        let NetworkPlan::Tap(plan) = plan else {
            return Err("expected tap plan".into());
        };
        assert_eq!(plan.name(), "tap0");
        assert_eq!(plan.mac(), fixture.mac);
        assert_eq!(plan.ownership(), TapOwnership::ExistingUserOwned);
        assert_eq!(plan.ip(), None);
        assert_eq!(plan.mask(), None);

        for name in ["", ".", "..", "bad/name", "0123456789abcdef"] {
            let invalid = NetworkSpec {
                mode: NetMode::Tap,
                tap: Some(name.to_owned()),
                ..NetworkSpec::default()
            };
            let error = prepare_network(fixture.options(&invalid, &valid_host))
                .expect_err("invalid tap name must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert!(error.hint().is_some());
        }

        let missing = FakeTapHost::default();
        let error =
            prepare_network(fixture.options(&spec, &missing)).expect_err("missing tap must fail");
        assert!(error.message().contains("does not exist or is not a TAP"));
        let inaccessible = FakeTapHost {
            is_tap: true,
            tun_error: Some(io::ErrorKind::PermissionDenied),
        };
        let error = prepare_network(fixture.options(&spec, &inaccessible))
            .expect_err("inaccessible tun must fail");
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.hint().is_some());
        Ok(())
    }

    #[test]
    fn none_plan_has_no_sidecar_mac_or_forwards() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let host = FakeTapHost::default();
        let spec = NetworkSpec {
            mode: NetMode::None,
            ..NetworkSpec::default()
        };
        let plan = prepare_network(fixture.options(&spec, &host))?;
        assert!(matches!(plan, NetworkPlan::None));
        assert_eq!(plan.mac(), None);
        assert!(plan.forwards().is_empty());
        Ok(())
    }

    #[test]
    fn passt_plan_refuses_socket_log_and_ancestry_hazards() -> Result<(), Box<dyn std::error::Error>>
    {
        let host = FakeTapHost::default();

        let fixture = Fixture::new()?;
        symlink("missing", fixture.paths.machine_net_socket("demo")?)?;
        let error = prepare_network(fixture.options(&NetworkSpec::default(), &host))
            .expect_err("socket symlink must fail");
        assert_eq!(error.kind(), ErrorKind::Conflict);

        let fixture = Fixture::new()?;
        let log = fixture.paths.machine_passt_log("demo")?;
        fs::write(&log, b"old")?;
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644))?;
        let error = prepare_network(fixture.options(&NetworkSpec::default(), &host))
            .expect_err("permissive log must fail");
        assert_eq!(error.kind(), ErrorKind::Dependency);

        let fixture = Fixture::new()?;
        let log = fixture.paths.machine_passt_log("demo")?;
        let target = fixture._temp.path().join("external.log");
        fs::write(&target, b"sentinel")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        symlink(&target, &log)?;
        let error = prepare_network(fixture.options(&NetworkSpec::default(), &host))
            .expect_err("log symlink must fail");
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&target)?, b"sentinel");

        let fixture = Fixture::new()?;
        fs::set_permissions(
            fixture.paths.machine_dir("demo")?,
            fs::Permissions::from_mode(0o777),
        )?;
        let error = prepare_network(fixture.options(&NetworkSpec::default(), &host))
            .expect_err("insecure machine ancestry must fail");
        assert_eq!(error.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[test]
    fn passt_readiness_validates_node_mode_timeout_and_cancellation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let host = FakeTapHost::default();
        let plan = prepare_network(fixture.options(&NetworkSpec::default(), &host))?;
        let NetworkPlan::Passt(plan) = plan else {
            return Err("expected passt plan".into());
        };
        assert_eq!(plan.readiness().inspect()?, SocketReadiness::Pending);
        assert_eq!(
            plan.readiness().cancellation(),
            ReadinessCancellation::AbortLaunch
        );
        let started = Instant::now();
        let long_overall = started + Duration::from_secs(60);
        assert_eq!(
            plan.readiness().bounded_deadline(started, long_overall)?,
            started + plan.readiness().timeout()
        );
        let short_overall = started + Duration::from_millis(1);
        assert_eq!(
            plan.readiness().bounded_deadline(started, short_overall)?,
            short_overall
        );

        let cancelled = AtomicBool::new(true);
        let error = plan
            .wait_ready(Instant::now() + Duration::from_secs(1), &cancelled)
            .expect_err("cancelled readiness must fail");
        assert_eq!(error.kind(), ErrorKind::Interrupted);

        let cancelled = AtomicBool::new(false);
        let error = plan
            .wait_ready(Instant::now(), &cancelled)
            .expect_err("expired readiness must fail");
        assert_eq!(error.kind(), ErrorKind::Timeout);

        let listener = UnixListener::bind(plan.socket().path())?;
        fs::set_permissions(
            plan.socket().path(),
            fs::Permissions::from_mode(plan.socket().mode()),
        )?;
        assert_eq!(plan.readiness().inspect()?, SocketReadiness::Ready);
        plan.wait_ready(Instant::now() + Duration::from_secs(1), &cancelled)?;
        drop(listener);

        fs::set_permissions(plan.socket().path(), fs::Permissions::from_mode(0o777))?;
        let error = plan
            .readiness()
            .inspect()
            .expect_err("wrong mode must fail");
        assert_eq!(error.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[test]
    fn passt_readiness_configuration_rejects_zero_excessive_and_slow_polling()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let host = FakeTapHost::default();
        let spec = NetworkSpec::default();
        for (timeout, poll) in [
            (Duration::ZERO, Duration::from_millis(1)),
            (Duration::from_secs(3601), Duration::from_millis(1)),
            (Duration::from_secs(1), Duration::ZERO),
            (Duration::from_millis(1), Duration::from_millis(2)),
        ] {
            let mut options = fixture.options(&spec, &host);
            options.readiness_timeout = timeout;
            options.readiness_poll_interval = poll;
            let error = prepare_network(options).expect_err("invalid readiness budget must fail");
            assert_eq!(error.kind(), ErrorKind::Usage);
            assert!(error.hint().is_some());
        }
        Ok(())
    }

    fn paths_for_root(root: PathBuf) -> Result<Paths, crate::FirestoneError> {
        Paths::from_inputs(&PathInputs {
            current_dir: root.clone(),
            home_dir: Some(root.clone()),
            firestone_home: Some(root),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: nix::unistd::getuid().as_raw(),
        })
    }

    fn write_executable(path: &Path, body: &str) -> io::Result<()> {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
}
