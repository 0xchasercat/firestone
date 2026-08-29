use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::OsString,
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{Arch, Cmd, DependencyManifest, ErrorKind, FirestoneError, MountSpec, Paths};

/// Product limit for independently supervised virtiofsd processes per machine.
pub const MAX_VIRTIOFS_MOUNTS: usize = 16;
/// Virtio-fs tag width used by virtiofsd v1.14.0 and Cloud Hypervisor v53.
pub const VIRTIOFS_TAG_MAX_BYTES: usize = 36;
/// Linux pathname limit excluding the trailing NUL.
pub const VIRTIOFS_PATH_MAX_BYTES: usize = 4095;
/// Linux pathname-component limit.
pub const VIRTIOFS_COMPONENT_MAX_BYTES: usize = 255;
/// Linux `sockaddr_un.sun_path` capacity excluding the trailing NUL.
pub const VHOST_USER_SOCKET_MAX_BYTES: usize = 107;
/// Cloud Hypervisor v53 request queue count required by SPEC section 9.2.
pub const VIRTIOFS_NUM_QUEUES: usize = 1;
/// Cloud Hypervisor v53 request queue size required by SPEC section 9.2.
pub const VIRTIOFS_QUEUE_SIZE: u16 = 1024;

const VIRTIOFSD_SOCKET_MODE: u32 = 0o700;
const VIRTIOFSD_PID_MODE: u32 = 0o600;
pub const DEFAULT_SOCKET_READINESS_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_SOCKET_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Isolation selected after the user-namespace doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtiofsSandbox {
    Namespace,
    None,
}

impl VirtiofsSandbox {
    /// Selects the rootless namespace sandbox when available and the specified fallback otherwise.
    #[must_use]
    pub const fn for_user_namespaces(available: bool) -> Self {
        if available {
            Self::Namespace
        } else {
            Self::None
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::None => "none",
        }
    }
}

/// Sidecar cancellation behavior while its listening socket is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketCancellationPolicy {
    AbortLaunch,
}

/// Validated bounds for one sidecar socket publication wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketReadinessPlan {
    timeout: Duration,
    poll_interval: Duration,
    cancellation: SocketCancellationPolicy,
}

impl SocketReadinessPlan {
    pub fn new(timeout: Duration, poll_interval: Duration) -> Result<Self, FirestoneError> {
        if timeout.is_zero() {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                "sidecar socket readiness timeout must be greater than zero",
            )
            .with_hint("use the default 10 second readiness timeout"));
        }
        if poll_interval.is_zero() || poll_interval > timeout {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                "sidecar socket readiness poll interval must be greater than zero and no longer than its timeout",
            )
            .with_hint("use the default 10 millisecond poll interval"));
        }
        Ok(Self {
            timeout,
            poll_interval,
            cancellation: SocketCancellationPolicy::AbortLaunch,
        })
    }

    #[must_use]
    pub const fn timeout(self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub const fn poll_interval(self) -> Duration {
        self.poll_interval
    }

    #[must_use]
    pub const fn cancellation(self) -> SocketCancellationPolicy {
        self.cancellation
    }
}

impl Default for SocketReadinessPlan {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SOCKET_READINESS_TIMEOUT,
            poll_interval: DEFAULT_SOCKET_READINESS_POLL_INTERVAL,
            cancellation: SocketCancellationPolicy::AbortLaunch,
        }
    }
}

/// One validated virtiofsd command and the exact paths shared with Cloud Hypervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtiofsPlan {
    index: usize,
    tag: String,
    host: PathBuf,
    guest: PathBuf,
    readonly: bool,
    sandbox: VirtiofsSandbox,
    program: PathBuf,
    args: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    socket: PathBuf,
    pid_file: PathBuf,
    log: PathBuf,
    readiness: SocketReadinessPlan,
    owner_uid: u32,
}

impl VirtiofsPlan {
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    #[must_use]
    pub fn host(&self) -> &Path {
        &self.host
    }

    #[must_use]
    pub fn guest(&self) -> &Path {
        &self.guest
    }

    #[must_use]
    pub const fn readonly(&self) -> bool {
        self.readonly
    }

    #[must_use]
    pub const fn sandbox(&self) -> VirtiofsSandbox {
        self.sandbox
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    #[must_use]
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    #[must_use]
    pub fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    #[must_use]
    pub fn log(&self) -> &Path {
        &self.log
    }
    #[must_use]
    pub const fn readiness(&self) -> SocketReadinessPlan {
        self.readiness
    }

    #[must_use]
    pub const fn num_queues(&self) -> usize {
        VIRTIOFS_NUM_QUEUES
    }

    #[must_use]
    pub const fn queue_size(&self) -> u16 {
        VIRTIOFS_QUEUE_SIZE
    }

    /// Builds the shared process wrapper without starting the sidecar.
    #[must_use]
    pub fn command(&self) -> Cmd {
        let mut command = Cmd::new(self.program.as_os_str())
            .args(self.args.clone())
            .cwd("/")
            .env_clear()
            .stdin_null()
            .stdout_append(&self.log)
            .stderr_append(&self.log)
            .error_kind(ErrorKind::Dependency);
        for (key, value) in &self.environment {
            command = command.env(key, value);
        }
        command
    }

    /// Waits for the pinned daemon's pid file and listening socket publication.
    ///
    /// Readiness is metadata-only. Connecting here would consume Cloud Hypervisor's one
    /// vhost-user connection and can make virtiofsd exit before VM creation.
    pub fn wait_ready(
        &self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), FirestoneError> {
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FirestoneError::new(
                    ErrorKind::Interrupted,
                    format!("virtiofsd readiness for mount `{}` was cancelled", self.tag),
                )
                .with_hint("stop the partially launched sidecars before retrying"));
            }

            match self.validate_ready_files() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => return Err(error),
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(FirestoneError::new(
                    ErrorKind::Timeout,
                    format!(
                        "virtiofsd for mount `{}` did not publish socket '{}' before the deadline",
                        self.tag,
                        self.socket.display()
                    ),
                )
                .with_hint(format!(
                    "inspect {} for the virtiofsd error",
                    self.log.display()
                )));
            }
            thread::sleep(
                self.readiness
                    .poll_interval()
                    .min(deadline.saturating_duration_since(now)),
            );
        }
    }

    fn validate_ready_files(&self) -> Result<bool, FirestoneError> {
        let socket_metadata = match fs::symlink_metadata(&self.socket) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(readiness_io_error(
                    &self.tag,
                    "inspect",
                    &self.socket,
                    source,
                ));
            }
        };
        validate_runtime_parent(&self.socket, self.owner_uid, &self.tag)?;
        validate_socket_metadata(&self.socket, &socket_metadata, self.owner_uid, &self.tag)?;

        let pid_metadata = match fs::symlink_metadata(&self.pid_file) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(readiness_io_error(
                    &self.tag,
                    "inspect",
                    &self.pid_file,
                    source,
                ));
            }
        };
        validate_pid_metadata(&self.pid_file, &pid_metadata, self.owner_uid, &self.tag)?;
        Ok(true)
    }
}

#[derive(Debug)]
struct MountShape {
    tag: String,
    guest: PathBuf,
}

/// Exact Cloud Hypervisor v53 `FsConfig` shape. v53 has no DAX field.
#[derive(Debug, Serialize)]
pub(crate) struct CloudHypervisorFsConfig {
    pub(crate) tag: String,
    pub(crate) socket: PathBuf,
    pub(crate) num_queues: usize,
    pub(crate) queue_size: u16,
}

impl CloudHypervisorFsConfig {
    pub(crate) fn from_mount(
        paths: &Paths,
        name: &str,
        index: usize,
        mount: &MountSpec,
    ) -> Result<Self, FirestoneError> {
        let tag = mount.effective_tag(index);
        validate_tag(&tag, index)?;
        let socket = paths.machine_fs_socket(name, index)?;
        validate_socket_path(&socket)?;
        Ok(Self {
            tag,
            socket,
            num_queues: VIRTIOFS_NUM_QUEUES,
            queue_size: VIRTIOFS_QUEUE_SIZE,
        })
    }
}

/// Validates every mount and builds one sidecar plan in declaration order.
///
/// This function only reads metadata. It does not create a guest mount point, socket, pid file,
/// log, or host directory, and it never starts a process.
pub fn prepare_virtiofs_plans(
    paths: &Paths,
    manifest: &DependencyManifest,
    name: &str,
    architecture: Arch,
    mounts: &[MountSpec],
    sandbox: VirtiofsSandbox,
) -> Result<Vec<VirtiofsPlan>, FirestoneError> {
    prepare_virtiofs_plans_with_readiness(
        paths,
        manifest,
        name,
        architecture,
        mounts,
        sandbox,
        SocketReadinessPlan::default(),
    )
}

pub fn prepare_virtiofs_plans_with_readiness(
    paths: &Paths,
    manifest: &DependencyManifest,
    name: &str,
    architecture: Arch,
    mounts: &[MountSpec],
    sandbox: VirtiofsSandbox,
    readiness: SocketReadinessPlan,
) -> Result<Vec<VirtiofsPlan>, FirestoneError> {
    let shapes = mount_shapes(mounts)?;
    if shapes.is_empty() {
        return Ok(Vec::new());
    }

    paths.validate_bin_data_directory()?;
    paths.validate_machine_data_directory(name)?;
    paths.validate_machine_runtime_dir(name)?;

    let artifact = manifest.artifact("virtiofsd", architecture.as_str())?;
    let program = paths.binary_file(&artifact.install_name)?;
    paths.validate_owned_data_file(
        &program,
        "pinned virtiofsd executable",
        artifact.expected_mode(),
        false,
    )?;

    let environment = reduced_environment();
    let mut plans = Vec::with_capacity(mounts.len());
    let mut canonical_hosts = Vec::<PathBuf>::with_capacity(mounts.len());

    for (index, (mount, shape)) in mounts.iter().zip(shapes).enumerate() {
        let host = canonical_host_path(&mount.host, paths.uid(), index)?;
        for (previous_index, previous) in canonical_hosts.iter().enumerate() {
            if paths_overlap(previous, &host) {
                return Err(mount_error(
                    index,
                    "host",
                    format!(
                        "canonical host path '{}' overlaps mount[{previous_index}].host '{}'",
                        host.display(),
                        previous.display()
                    ),
                    "use disjoint host directories for separate virtio-fs devices",
                ));
            }
        }

        let socket = paths.machine_fs_socket(name, index)?;
        validate_socket_path(&socket)?;
        let pid_file = paths.machine_fs_pid_file(name, index)?;
        let log = paths.machine_virtiofsd_log(name, index)?;
        validate_absent_runtime_node(&socket, &shape.tag, "socket")?;
        validate_absent_runtime_node(&pid_file, &shape.tag, "pid file")?;
        paths.validate_owned_data_file(&log, "virtiofsd log", 0o600, true)?;

        let mut args = vec![
            OsString::from("--socket-path"),
            socket.as_os_str().to_owned(),
            OsString::from("--shared-dir"),
            host.as_os_str().to_owned(),
            OsString::from("--sandbox"),
            OsString::from(sandbox.as_str()),
            OsString::from("--cache"),
            OsString::from("auto"),
            OsString::from("--announce-submounts"),
        ];
        if mount.readonly {
            args.push(OsString::from("--readonly"));
        }
        args.extend([OsString::from("--log-level"), OsString::from("warn")]);

        canonical_hosts.push(host.clone());
        plans.push(VirtiofsPlan {
            index,
            tag: shape.tag,
            host,
            guest: shape.guest,
            readonly: mount.readonly,
            sandbox,
            program: program.clone(),
            args,
            environment: environment.clone(),
            socket,
            pid_file,
            log,
            owner_uid: paths.uid(),
            readiness,
        });
    }

    Ok(plans)
}

pub(crate) fn validate_mount_spec_layout(mounts: &[MountSpec]) -> Result<(), FirestoneError> {
    mount_shapes(mounts).map(drop)
}

fn mount_shapes(mounts: &[MountSpec]) -> Result<Vec<MountShape>, FirestoneError> {
    if mounts.len() > MAX_VIRTIOFS_MOUNTS {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "'mount': has {} entries; at most {MAX_VIRTIOFS_MOUNTS} are supported",
                mounts.len()
            ),
        )
        .with_hint("remove mounts until no more than 16 virtiofsd sidecars are required"));
    }

    let mut tags = HashMap::<String, usize>::with_capacity(mounts.len());
    let mut shapes = Vec::<MountShape>::with_capacity(mounts.len());
    for (index, mount) in mounts.iter().enumerate() {
        let tag = mount.effective_tag(index);
        validate_tag(&tag, index)?;
        if let Some(previous) = tags.insert(tag.clone(), index) {
            return Err(mount_error(
                index,
                "tag",
                format!("tag '{tag}' conflicts with mount[{previous}].tag"),
                "set a unique tag or omit tags to use share0, share1, and so on",
            ));
        }

        validate_host_spelling(&mount.host, index)?;
        for (previous_index, previous) in mounts[..index].iter().enumerate() {
            if paths_overlap(&previous.host, &mount.host) {
                return Err(mount_error(
                    index,
                    "host",
                    format!(
                        "host path '{}' overlaps mount[{previous_index}].host '{}'",
                        mount.host.display(),
                        previous.host.display()
                    ),
                    "use disjoint host directories for separate virtio-fs devices",
                ));
            }
        }

        let guest = validate_guest_path(&mount.guest, index)?;
        for (previous_index, previous) in shapes.iter().enumerate() {
            if paths_overlap(&previous.guest, &guest) {
                return Err(mount_error(
                    index,
                    "guest",
                    format!(
                        "guest path '{}' overlaps mount[{previous_index}].guest '{}'",
                        guest.display(),
                        previous.guest.display()
                    ),
                    "use disjoint absolute guest mount points",
                ));
            }
        }
        shapes.push(MountShape { tag, guest });
    }
    Ok(shapes)
}

fn validate_tag(tag: &str, index: usize) -> Result<(), FirestoneError> {
    let valid = !tag.is_empty()
        && tag.len() <= VIRTIOFS_TAG_MAX_BYTES
        && tag.as_bytes()[0].is_ascii_alphanumeric()
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        return Ok(());
    }
    Err(mount_error(
        index,
        "tag",
        format!(
            "tag '{tag}' must be 1 to {VIRTIOFS_TAG_MAX_BYTES} ASCII bytes and contain only letters, digits, '.', '_', or '-'"
        ),
        "use a tag such as 'source' or omit it to use share0, share1, and so on",
    ))
}

fn validate_host_spelling(path: &Path, index: usize) -> Result<(), FirestoneError> {
    if !path.is_absolute() {
        return Err(mount_error(
            index,
            "host",
            format!("host path '{}' is not absolute", path.display()),
            "resolve the host path through Paths before preparing virtiofsd",
        ));
    }
    validate_path_bytes(path, index, "host")?;
    validate_canonical_spelling(path, index, "host")
}

fn validate_guest_path(path: &Path, index: usize) -> Result<PathBuf, FirestoneError> {
    if !path.is_absolute() {
        return Err(mount_error(
            index,
            "guest",
            format!("guest path '{}' is not absolute", path.display()),
            "use an absolute guest path such as '/work'",
        ));
    }
    if path == Path::new("/") {
        return Err(mount_error(
            index,
            "guest",
            "guest path '/' would replace the guest root filesystem",
            "use a dedicated mount point such as '/work'",
        ));
    }
    validate_path_bytes(path, index, "guest")?;
    validate_canonical_spelling(path, index, "guest")?;
    for component in path.components() {
        if let Component::Normal(value) = component {
            if value.as_bytes().iter().any(u8::is_ascii_control) {
                return Err(mount_error(
                    index,
                    "guest",
                    format!("guest path '{}' contains a control byte", path.display()),
                    "remove control bytes from the guest mount point",
                ));
            }
        }
    }
    Ok(path.to_path_buf())
}

fn validate_path_bytes(path: &Path, index: usize, field: &str) -> Result<(), FirestoneError> {
    let bytes = path.as_os_str().as_bytes();
    if path.to_str().is_none() {
        return Err(mount_error(
            index,
            field,
            format!("{field} path '{}' is not UTF-8", path.display()),
            format!("use a UTF-8 mount {field} path"),
        ));
    }
    if bytes.len() > VIRTIOFS_PATH_MAX_BYTES {
        return Err(mount_error(
            index,
            field,
            format!(
                "{field} path is {} bytes; Linux permits at most {VIRTIOFS_PATH_MAX_BYTES}",
                bytes.len()
            ),
            format!("use a shorter mount {field} path"),
        ));
    }
    if bytes.contains(&0) {
        return Err(mount_error(
            index,
            field,
            format!("{field} path contains a NUL byte"),
            format!("remove the NUL byte from the mount {field} path"),
        ));
    }
    for component in path.components() {
        if let Component::Normal(value) = component {
            let length = value.as_bytes().len();
            if length > VIRTIOFS_COMPONENT_MAX_BYTES {
                return Err(mount_error(
                    index,
                    field,
                    format!(
                        "{field} path component is {length} bytes; Linux permits at most {VIRTIOFS_COMPONENT_MAX_BYTES}"
                    ),
                    format!("shorten the mount {field} path component"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_canonical_spelling(
    path: &Path,
    index: usize,
    field: &str,
) -> Result<(), FirestoneError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > 1 && (bytes.ends_with(b"/") || bytes.windows(2).any(|pair| pair == b"//")) {
        return Err(noncanonical_path_error(index, field, path));
    }
    if bytes
        .split(|byte| *byte == b'/')
        .any(|component| matches!(component, b"." | b".."))
    {
        return Err(noncanonical_path_error(index, field, path));
    }
    Ok(())
}

fn canonical_host_path(path: &Path, uid: u32, index: usize) -> Result<PathBuf, FirestoneError> {
    let canonical = fs::canonicalize(path).map_err(|source| {
        mount_error(
            index,
            "host",
            format!(
                "cannot canonicalize host path '{}': {source}",
                path.display()
            ),
            "correct the host path and make every ancestor searchable",
        )
        .with_source(source)
    })?;
    if canonical != path {
        return Err(mount_error(
            index,
            "host",
            format!(
                "host path '{}' resolves through a symlink or alias to '{}'",
                path.display(),
                canonical.display()
            ),
            "use the canonical path without symlinks, '.', '..', or duplicate separators",
        ));
    }

    let mut current = PathBuf::from("/");
    let components = canonical
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.is_empty() {
        return Err(mount_error(
            index,
            "host",
            "host path '/' is not a user-owned shared directory",
            "choose a directory owned by the current user",
        ));
    }

    for (position, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|source| {
            mount_error(
                index,
                "host",
                format!("cannot inspect host path '{}': {source}", current.display()),
                "correct the host path and make every ancestor searchable",
            )
            .with_source(source)
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(mount_error(
                index,
                "host",
                format!(
                    "host path component '{}' is not a real directory",
                    current.display()
                ),
                "replace the symlink or non-directory component with a real directory",
            ));
        }
        let final_component = position + 1 == components.len();
        if final_component {
            validate_host_leaf(&current, &metadata, uid, index)?;
        } else {
            validate_host_ancestor(&current, &metadata, uid, index)?;
        }
    }
    Ok(canonical)
}

fn validate_host_leaf(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
    index: usize,
) -> Result<(), FirestoneError> {
    let mode = metadata.mode() & 0o7777;
    if metadata.uid() == uid && mode & 0o022 == 0 {
        return Ok(());
    }
    Err(mount_error(
        index,
        "host",
        format!(
            "host directory '{}' is insecure: expected uid {uid} without group/world write, found uid {} and mode {mode:04o}",
            path.display(),
            metadata.uid()
        ),
        "choose a current-user-owned directory and remove group/world write access",
    ))
}

fn validate_host_ancestor(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
    index: usize,
) -> Result<(), FirestoneError> {
    let owner = metadata.uid();
    let mode = metadata.mode() & 0o7777;
    let protected = mode & 0o022 == 0;
    let root_sticky = owner == 0 && mode & 0o1000 != 0;
    if (owner == uid || owner == 0) && (protected || root_sticky) {
        return Ok(());
    }
    Err(mount_error(
        index,
        "host",
        format!(
            "host directory ancestor '{}' is renameable by another user: uid {owner}, mode {mode:04o}",
            path.display()
        ),
        "move the shared directory below current-user or protected root-owned ancestry",
    ))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn validate_socket_path(path: &Path) -> Result<(), FirestoneError> {
    let bytes = path.as_os_str().as_bytes();
    if path.to_str().is_none() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("virtiofsd socket path '{}' is not UTF-8", path.display()),
        )
        .with_hint("use a UTF-8 FIRESTONE_RUNTIME_DIR"));
    }
    if bytes.len() > VHOST_USER_SOCKET_MAX_BYTES {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "virtiofsd socket path '{}' is {} bytes; Linux permits at most {VHOST_USER_SOCKET_MAX_BYTES}",
                path.display(),
                bytes.len()
            ),
        )
        .with_hint("set FIRESTONE_RUNTIME_DIR to a shorter absolute path"));
    }
    Ok(())
}

fn validate_absent_runtime_node(path: &Path, tag: &str, label: &str) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "virtiofsd {label} '{}' already exists for mount `{tag}`",
                path.display()
            ),
        )
        .with_hint("reconcile or stop the machine before preparing a new sidecar")),
        Err(source) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot inspect virtiofsd {label} '{}': {source}",
                path.display()
            ),
        )
        .with_hint("check the private machine runtime directory")
        .with_source(source)),
    }
}

fn reduced_environment() -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::new();
    for key in [
        "PATH",
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        if let Some(value) = env::var_os(key).filter(|value| !value.is_empty()) {
            environment.insert(OsString::from(key), value);
        }
    }
    for (key, value) in env::vars_os() {
        if key.as_os_str().as_bytes().starts_with(b"FIRESTONE_") {
            environment.insert(key, value);
        }
    }
    environment
}

fn validate_runtime_parent(path: &Path, uid: u32, tag: &str) -> Result<(), FirestoneError> {
    let parent = path.parent().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "virtiofsd socket '{}' has no runtime parent",
                path.display()
            ),
        )
    })?;
    let metadata = fs::symlink_metadata(parent)
        .map_err(|source| readiness_io_error(tag, "inspect", parent, source))?;
    let mode = metadata.mode() & 0o7777;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != uid
        || mode != 0o700
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "virtiofsd runtime directory '{}' is insecure: expected a current-user mode-0700 real directory",
                parent.display()
            ),
        )
        .with_hint("stop and start the machine to recreate its private runtime directory"));
    }
    Ok(())
}

fn validate_socket_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
    tag: &str,
) -> Result<(), FirestoneError> {
    let mode = metadata.mode() & 0o7777;
    if !metadata.file_type().is_symlink()
        && metadata.file_type().is_socket()
        && metadata.uid() == uid
        && mode == VIRTIOFSD_SOCKET_MODE
    {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "virtiofsd socket '{}' for mount `{tag}` is insecure: expected uid {uid} and mode {VIRTIOFSD_SOCKET_MODE:04o}",
            path.display()
        ),
    )
    .with_hint("stop and start the machine to replace the unsafe runtime node"))
}

fn validate_pid_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
    tag: &str,
) -> Result<(), FirestoneError> {
    let mode = metadata.mode() & 0o7777;
    if !metadata.file_type().is_symlink()
        && metadata.is_file()
        && metadata.uid() == uid
        && mode == VIRTIOFSD_PID_MODE
    {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "virtiofsd pid file '{}' for mount `{tag}` is insecure: expected uid {uid} and mode {VIRTIOFSD_PID_MODE:04o}",
            path.display()
        ),
    )
    .with_hint("stop and start the machine to replace the unsafe runtime node"))
}

fn readiness_io_error(
    tag: &str,
    operation: &str,
    path: &Path,
    source: std::io::Error,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "cannot {operation} virtiofsd readiness path '{}' for mount `{tag}`: {source}",
            path.display()
        ),
    )
    .with_hint("inspect the virtiofsd log and private runtime directory")
    .with_source(source)
}

fn noncanonical_path_error(index: usize, field: &str, path: &Path) -> FirestoneError {
    mount_error(
        index,
        field,
        format!("{field} path '{}' is not canonical", path.display()),
        format!(
            "remove '.', '..', duplicate separators, and trailing separators from mount.{field}"
        ),
    )
}

fn mount_error(
    index: usize,
    field: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("'mount[{index}].{field}': {}", message.into()),
    )
    .with_hint(hint)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
            net::UnixListener,
        },
        path::{Path, PathBuf},
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use crate::{Arch, DependencyManifest, ErrorKind, MountSpec, PathInputs, Paths};

    use super::{
        DEFAULT_SOCKET_READINESS_POLL_INTERVAL, DEFAULT_SOCKET_READINESS_TIMEOUT,
        MAX_VIRTIOFS_MOUNTS, SocketCancellationPolicy, SocketReadinessPlan,
        VHOST_USER_SOCKET_MAX_BYTES, VIRTIOFS_NUM_QUEUES, VIRTIOFS_QUEUE_SIZE,
        VIRTIOFS_TAG_MAX_BYTES, VirtiofsPlan, VirtiofsSandbox, prepare_virtiofs_plans,
        validate_host_leaf, validate_socket_path,
    };

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        paths: Paths,
        manifest: DependencyManifest,
        executable: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = tempfile::tempdir()?;
            let root = fs::canonicalize(temp.path())?;
            let paths = paths_for_root(&root)?;
            let machine_dir = paths.machine_dir("demo")?;
            let runtime_dir = paths.machine_runtime_dir("demo")?;
            let bin_dir = paths.bin_dir();
            fs::create_dir_all(&machine_dir)?;
            fs::create_dir_all(&runtime_dir)?;
            fs::create_dir_all(&bin_dir)?;
            for directory in [
                root.clone(),
                root.join("firestone"),
                paths.data_dir().to_path_buf(),
                paths.machines_dir(),
                machine_dir,
                paths.runtime_dir().to_path_buf(),
                runtime_dir,
                bin_dir,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
            let manifest = DependencyManifest::bundled()?;
            let artifact = manifest.artifact("virtiofsd", Arch::X86_64.as_str())?;
            let executable = paths.binary_file(&artifact.install_name)?;
            fs::write(
                &executable,
                br#"#!/bin/sh
printf '%s\n' "$@"
printf 'HOME=%s\nPATH=%s\nCARGO_MANIFEST_DIR=%s\n' "${HOME-unset}" "${PATH-unset}" "${CARGO_MANIFEST_DIR-unset}"
env
"#,
            )?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
            Ok(Self {
                _temp: temp,
                root,
                paths,
                manifest,
                executable,
            })
        }

        fn host(&self, name: &str) -> Result<PathBuf, std::io::Error> {
            let path = self.root.join(name);
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            Ok(path)
        }

        fn mount(
            &self,
            name: &str,
            guest: &str,
            readonly: bool,
        ) -> Result<MountSpec, std::io::Error> {
            Ok(MountSpec {
                host: self.host(name)?,
                guest: PathBuf::from(guest),
                readonly,
                tag: None,
            })
        }

        fn plans(
            &self,
            mounts: &[MountSpec],
            sandbox: VirtiofsSandbox,
        ) -> Result<Vec<VirtiofsPlan>, crate::FirestoneError> {
            prepare_virtiofs_plans(
                &self.paths,
                &self.manifest,
                "demo",
                Arch::X86_64,
                mounts,
                sandbox,
            )
        }
    }

    #[test]
    fn prepare_zero_mounts_needs_no_installed_binary_or_runtime()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let paths = paths_for_root(&root)?;
        let plans = prepare_virtiofs_plans(
            &paths,
            &DependencyManifest::bundled()?,
            "missing",
            Arch::X86_64,
            &[],
            VirtiofsSandbox::Namespace,
        )?;
        assert!(plans.is_empty());
        Ok(())
    }

    #[test]
    fn prepare_one_mount_has_exact_namespace_rw_command() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let mount = fixture.mount("source tree", "/work", false)?;
        let plans = fixture.plans(&[mount], VirtiofsSandbox::Namespace)?;
        let plan = &plans[0];

        assert_eq!(plan.index(), 0);
        assert_eq!(plan.tag(), "share0");
        assert_eq!(plan.guest(), Path::new("/work"));
        assert!(!plan.readonly());
        assert_eq!(plan.program(), fixture.executable);
        assert_eq!(plan.num_queues(), VIRTIOFS_NUM_QUEUES);
        assert_eq!(plan.queue_size(), VIRTIOFS_QUEUE_SIZE);
        assert_eq!(
            plan.args(),
            &[
                "--socket-path".into(),
                plan.socket().as_os_str().to_owned(),
                "--shared-dir".into(),
                plan.host().as_os_str().to_owned(),
                "--sandbox".into(),
                "namespace".into(),
                "--cache".into(),
                "auto".into(),
                "--announce-submounts".into(),
                "--log-level".into(),
                "warn".into(),
            ]
        );
        assert!(plan.environment().keys().all(|key| {
            matches!(
                key.to_str(),
                Some("PATH" | "HOME" | "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "XDG_RUNTIME_DIR")
            ) || key
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(b"FIRESTONE_")
        }));
        assert!(!plan.guest().exists());
        Ok(())
    }

    #[test]
    fn command_fake_executable_observes_exact_argv_and_reduced_environment()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let mount = fixture.mount("source", "/work", false)?;
        let plans = fixture.plans(&[mount], VirtiofsSandbox::Namespace)?;
        let plan = &plans[0];
        let output = plan.command().run()?;
        let stdout = String::from_utf8(output.stdout().to_vec())?;
        let expected_args = plan
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(stdout.starts_with(&format!("{expected_args}\n")));
        assert!(stdout.contains("CARGO_MANIFEST_DIR=unset\n"));
        for (key, value) in plan.environment() {
            assert!(stdout.contains(&format!(
                "{}={}\n",
                key.to_string_lossy(),
                value.to_string_lossy()
            )));
        }
        Ok(())
    }

    #[test]
    fn command_supervised_fake_routes_output_to_owned_log() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let mount = fixture.mount("logged", "/work", false)?;
        let plan = fixture
            .plans(&[mount], VirtiofsSandbox::Namespace)?
            .remove(0);
        let mut child = plan.command().spawn_process_group()?;

        assert!(child.wait()?.success());
        let log = fs::read_to_string(plan.log())?;
        assert!(log.lines().any(|line| line == "--socket-path"));
        assert_eq!(
            fs::metadata(plan.log())?.permissions().mode() & 0o7777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn prepare_readonly_mount_adds_only_readonly_flag() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let rw = fixture.mount("rw", "/rw", false)?;
        let ro = fixture.mount("ro", "/ro", true)?;
        let rw_plan = fixture.plans(&[rw], VirtiofsSandbox::None)?.remove(0);
        let ro_plan = fixture.plans(&[ro], VirtiofsSandbox::None)?.remove(0);

        assert!(!rw_plan.args().iter().any(|arg| arg == "--readonly"));
        assert!(ro_plan.args().iter().any(|arg| arg == "--readonly"));
        assert!(
            ro_plan
                .args()
                .windows(2)
                .any(|args| args == ["--sandbox", "none"])
        );
        assert_eq!(rw_plan.num_queues(), ro_plan.num_queues());
        assert_eq!(rw_plan.queue_size(), ro_plan.queue_size());
        Ok(())
    }

    #[test]
    fn prepare_many_mounts_preserves_order_and_unique_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let mounts = (0..MAX_VIRTIOFS_MOUNTS)
            .map(|index| {
                fixture.mount(
                    &format!("host-{index}"),
                    &format!("/guest-{index}"),
                    index % 2 == 0,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let plans = fixture.plans(&mounts, VirtiofsSandbox::Namespace)?;

        assert_eq!(plans.len(), MAX_VIRTIOFS_MOUNTS);
        for (index, plan) in plans.iter().enumerate() {
            assert_eq!(plan.index(), index);
            assert_eq!(plan.tag(), format!("share{index}"));
            assert_eq!(
                plan.socket(),
                fixture.paths.machine_fs_socket("demo", index)?
            );
            assert_eq!(
                plan.log(),
                fixture.paths.machine_virtiofsd_log("demo", index)?
            );
        }
        Ok(())
    }

    #[test]
    fn prepare_mount_count_over_limit_is_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let mounts = (0..=MAX_VIRTIOFS_MOUNTS)
            .map(|index| fixture.mount(&format!("host-{index}"), &format!("/guest-{index}"), false))
            .collect::<Result<Vec<_>, _>>()?;
        let error = fixture
            .plans(&mounts, VirtiofsSandbox::Namespace)
            .err()
            .ok_or("accepted too many mounts")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("at most 16"));
        Ok(())
    }

    #[test]
    fn prepare_tags_reject_hostile_length_grammar_and_default_conflict()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        for (case, tag) in [
            String::new(),
            "../escape".to_owned(),
            "two words".to_owned(),
            "é".to_owned(),
            "x".repeat(VIRTIOFS_TAG_MAX_BYTES + 1),
        ]
        .into_iter()
        .enumerate()
        {
            let mut mount = fixture.mount(&format!("host-{case}"), "/work", false)?;
            mount.tag = Some(tag);
            let error = fixture
                .plans(&[mount], VirtiofsSandbox::Namespace)
                .err()
                .ok_or("accepted hostile tag")?;
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert!(error.message().starts_with("'mount[0].tag':"));
        }

        let mut first = fixture.mount("first", "/first", false)?;
        first.tag = Some("share1".to_owned());
        let second = fixture.mount("second", "/second", false)?;
        let error = fixture
            .plans(&[first, second], VirtiofsSandbox::Namespace)
            .err()
            .ok_or("accepted generated tag conflict")?;
        assert!(error.message().contains("mount[0].tag"));
        Ok(())
    }

    #[test]
    fn prepare_paths_reject_relative_noncanonical_duplicate_and_overlap()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let host = fixture.host("host")?;
        for (host_path, guest) in [
            (PathBuf::from("relative"), PathBuf::from("/work")),
            (host.join("..").join("host"), PathBuf::from("/work")),
            (host.clone(), PathBuf::from("relative")),
            (host.clone(), PathBuf::from("/")),
            (host.clone(), PathBuf::from("/work/../escape")),
        ] {
            let mount = MountSpec {
                host: host_path,
                guest,
                readonly: false,
                tag: None,
            };
            let error = fixture
                .plans(&[mount], VirtiofsSandbox::Namespace)
                .err()
                .ok_or("accepted hostile path")?;
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        }

        let parent = fixture.host("parent")?;
        let child = parent.join("child");
        fs::create_dir(&child)?;
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700))?;
        let mounts = [
            MountSpec {
                host: parent,
                guest: PathBuf::from("/one"),
                readonly: false,
                tag: None,
            },
            MountSpec {
                host: child,
                guest: PathBuf::from("/two"),
                readonly: false,
                tag: None,
            },
        ];
        let error = fixture
            .plans(&mounts, VirtiofsSandbox::Namespace)
            .err()
            .ok_or("accepted overlapping host paths")?;
        assert!(error.message().contains("overlaps"));

        let first = fixture.mount("guest-a", "/work", false)?;
        let second = fixture.mount("guest-b", "/work/src", false)?;
        let error = fixture
            .plans(&[first, second], VirtiofsSandbox::Namespace)
            .err()
            .ok_or("accepted overlapping guest paths")?;
        assert!(error.message().contains("mount[1].guest"));
        Ok(())
    }

    #[test]
    fn prepare_paths_reject_non_utf8_and_oversized_values() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let host = fixture.host("host")?;
        let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
        let error = fixture
            .plans(
                &[MountSpec {
                    host: non_utf8,
                    guest: PathBuf::from("/work"),
                    readonly: false,
                    tag: None,
                }],
                VirtiofsSandbox::Namespace,
            )
            .err()
            .ok_or("accepted non-UTF-8 host")?;
        assert!(error.message().contains("not UTF-8"));

        let oversized_host = fixture.root.join("x".repeat(256));
        let error = fixture
            .plans(
                &[MountSpec {
                    host: oversized_host,
                    guest: PathBuf::from("/work"),
                    readonly: false,
                    tag: None,
                }],
                VirtiofsSandbox::Namespace,
            )
            .err()
            .ok_or("accepted overlong host component")?;
        assert!(error.message().contains("at most 255"));

        for (guest, expected) in [
            (
                PathBuf::from(format!("/{}", "x".repeat(256))),
                "at most 255",
            ),
            (
                PathBuf::from(format!("/{}", "a/".repeat(2050))),
                "at most 4095",
            ),
        ] {
            let error = fixture
                .plans(
                    &[MountSpec {
                        host: host.clone(),
                        guest,
                        readonly: false,
                        tag: None,
                    }],
                    VirtiofsSandbox::Namespace,
                )
                .err()
                .ok_or("accepted oversized guest path")?;
            assert!(error.message().contains(expected), "{}", error.message());
        }
        Ok(())
    }

    #[test]
    fn prepare_host_policy_rejects_symlink_modes_and_wrong_owner()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let target = fixture.host("target")?;
        let link = fixture.root.join("link");
        symlink(&target, &link)?;
        let error = fixture
            .plans(
                &[MountSpec {
                    host: link,
                    guest: PathBuf::from("/work"),
                    readonly: false,
                    tag: None,
                }],
                VirtiofsSandbox::Namespace,
            )
            .err()
            .ok_or("accepted host symlink")?;
        assert!(error.message().contains("symlink or alias"));

        let metadata = fs::symlink_metadata(&target)?;
        let error = validate_host_leaf(&target, &metadata, fixture.paths.uid() + 1, 0)
            .err()
            .ok_or("accepted wrong-owner host leaf")?;
        assert!(error.message().contains("expected uid"));

        fs::set_permissions(&target, fs::Permissions::from_mode(0o722))?;
        let error = fixture
            .plans(
                &[MountSpec {
                    host: target,
                    guest: PathBuf::from("/work"),
                    readonly: false,
                    tag: None,
                }],
                VirtiofsSandbox::Namespace,
            )
            .err()
            .ok_or("accepted writable host")?;
        assert!(error.message().contains("mode 0722"));
        Ok(())
    }

    #[test]
    fn prepare_socket_conflict_and_length_fail_before_spawn()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let mount = fixture.mount("source", "/work", false)?;
        fs::write(fixture.paths.machine_fs_socket("demo", 0)?, b"sentinel")?;
        let error = fixture
            .plans(&[mount], VirtiofsSandbox::Namespace)
            .err()
            .ok_or("accepted socket conflict")?;
        assert_eq!(error.kind(), ErrorKind::Conflict);

        let exact = PathBuf::from(format!("/{}", "s".repeat(VHOST_USER_SOCKET_MAX_BYTES - 1)));
        assert_eq!(
            exact.as_os_str().as_encoded_bytes().len(),
            VHOST_USER_SOCKET_MAX_BYTES
        );
        validate_socket_path(&exact)?;
        let over = PathBuf::from(format!("/{}", "s".repeat(VHOST_USER_SOCKET_MAX_BYTES)));
        let error = validate_socket_path(&over)
            .err()
            .ok_or("accepted overlong socket path")?;
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("at most 107"));
        Ok(())
    }

    #[test]
    fn readiness_accepts_owned_modes_and_does_not_connect() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let mount = fixture.mount("source", "/work", false)?;
        let plan = fixture
            .plans(&[mount], VirtiofsSandbox::Namespace)?
            .remove(0);
        let listener = UnixListener::bind(plan.socket())?;
        fs::set_permissions(plan.socket(), fs::Permissions::from_mode(0o700))?;
        fs::write(plan.pid_file(), format!("{}\n", std::process::id()))?;
        fs::set_permissions(plan.pid_file(), fs::Permissions::from_mode(0o600))?;

        plan.wait_ready(
            Instant::now() + Duration::from_millis(100),
            &AtomicBool::new(false),
        )?;
        listener.set_nonblocking(true)?;
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        Ok(())
    }

    #[test]
    fn readiness_timeout_cancel_and_unsafe_mode_are_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let mount = fixture.mount("source", "/work", false)?;
        let plan = fixture
            .plans(&[mount], VirtiofsSandbox::Namespace)?
            .remove(0);

        let timeout = plan
            .wait_ready(Instant::now(), &AtomicBool::new(false))
            .err()
            .ok_or("missing readiness timeout")?;
        assert_eq!(timeout.kind(), ErrorKind::Timeout);

        let cancelled = plan
            .wait_ready(
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(true),
            )
            .err()
            .ok_or("missing readiness cancellation")?;
        assert_eq!(cancelled.kind(), ErrorKind::Interrupted);

        let _listener = UnixListener::bind(plan.socket())?;
        fs::set_permissions(plan.socket(), fs::Permissions::from_mode(0o777))?;
        let unsafe_socket = plan
            .wait_ready(
                Instant::now() + Duration::from_millis(100),
                &AtomicBool::new(false),
            )
            .err()
            .ok_or("accepted unsafe socket mode")?;
        assert_eq!(unsafe_socket.kind(), ErrorKind::Dependency);
        assert!(unsafe_socket.message().contains("mode 0700"));
        Ok(())
    }

    #[test]
    fn readiness_plan_rejects_zero_and_inverted_bounds() -> Result<(), Box<dyn std::error::Error>> {
        for (timeout, poll) in [
            (Duration::ZERO, Duration::from_millis(1)),
            (Duration::from_secs(1), Duration::ZERO),
            (Duration::from_millis(1), Duration::from_millis(2)),
        ] {
            let error = SocketReadinessPlan::new(timeout, poll)
                .err()
                .ok_or("accepted invalid readiness bounds")?;
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        }

        let plan = SocketReadinessPlan::default();
        assert_eq!(plan.timeout(), DEFAULT_SOCKET_READINESS_TIMEOUT);
        assert_eq!(plan.poll_interval(), DEFAULT_SOCKET_READINESS_POLL_INTERVAL);
        assert_eq!(plan.cancellation(), SocketCancellationPolicy::AbortLaunch);
        Ok(())
    }

    #[test]
    fn sandbox_selection_is_exact() {
        assert_eq!(
            VirtiofsSandbox::for_user_namespaces(true),
            VirtiofsSandbox::Namespace
        );
        assert_eq!(
            VirtiofsSandbox::for_user_namespaces(false),
            VirtiofsSandbox::None
        );
        assert_eq!(VirtiofsSandbox::Namespace.as_str(), "namespace");
        assert_eq!(VirtiofsSandbox::None.as_str(), "none");
    }

    fn paths_for_root(root: &Path) -> Result<Paths, crate::FirestoneError> {
        Paths::from_inputs(&PathInputs {
            current_dir: root.to_path_buf(),
            home_dir: Some(root.to_path_buf()),
            firestone_home: Some(root.join("firestone")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: nix::unistd::getuid().as_raw(),
        })
    }
}
