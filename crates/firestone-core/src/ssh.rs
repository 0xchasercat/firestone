use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsFd, AsRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    str::FromStr,
    sync::mpsc,
    thread,
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
use ssh_key::{Algorithm, PublicKey};

use crate::{Cmd, ErrorKind, FirestoneError, Paths};

const PRIVATE_KEY_MODE: u32 = 0o600;
const PUBLIC_KEY_MODE: u32 = 0o644;
const IDENTITY_LOCK_MODE: u32 = 0o600;
const IDENTITY_LOCK_TIMEOUT: Duration = Duration::from_secs(35);
const IDENTITY_LOCK_POLL: Duration = Duration::from_millis(10);
const KEYGEN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PUBLIC_KEY_BYTES: u64 = 16 * 1024;

/// Maximum complete acknowledgement frame accepted from Cloud Hypervisor v53.
pub const VSOCK_HANDSHAKE_MAX_BYTES: usize = 64;
/// Absolute bound covering the proxy's Unix connect and v53 acknowledgement.
pub const VSOCK_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// One validated non-zero guest vsock port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VsockPort(u32);

impl VsockPort {
    pub const SSH: Self = Self(22);

    pub fn new(value: u32) -> Result<Self, FirestoneError> {
        if value == 0 {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                "vsock port must be between 1 and 4294967295",
            )
            .with_hint("use the guest service's non-zero 32-bit vsock port"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl FromStr for VsockPort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = value
            .parse::<u32>()
            .map_err(|_| "vsock port must be between 1 and 4294967295".to_owned())?;
        Self::new(parsed).map_err(|error| error.message().to_owned())
    }
}

impl std::fmt::Display for VsockPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Paths for Firestone's complete, validated host SSH identity.
///
/// This value contains paths only. Private key bytes are never loaded, returned, or logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshIdentity {
    private_key: PathBuf,
    public_key: PathBuf,
}

impl SshIdentity {
    #[must_use]
    pub fn private_key(&self) -> &Path {
        &self.private_key
    }

    #[must_use]
    pub fn public_key(&self) -> &Path {
        &self.public_key
    }
}

/// A completed Cloud Hypervisor v53 host-to-guest vsock handshake.
#[derive(Debug)]
pub struct VsockConnection {
    stream: UnixStream,
    allocated_port: u32,
}

impl VsockConnection {
    #[must_use]
    pub const fn allocated_port(&self) -> u32 {
        self.allocated_port
    }

    #[must_use]
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

#[derive(Debug)]
struct IdentityLock {
    _file: File,
}

/// Generates Firestone's Ed25519 host identity on first use and returns its paths.
///
/// Generation is serialized across processes. The `.generating` marker and identity lock make the
/// public key pair unavailable to every Firestone consumer until both final files have been
/// validated and synced.
pub fn ensure_ssh_identity(paths: &Paths) -> Result<SshIdentity, FirestoneError> {
    let hostname = nix::unistd::gethostname()
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                "cannot read the host name for the Firestone SSH key comment",
            )
            .with_hint("configure a valid host name and retry")
            .with_source(io::Error::from(source))
        })?
        .into_string()
        .map_err(|_| {
            FirestoneError::new(
                ErrorKind::Dependency,
                "host name for the Firestone SSH key comment is not UTF-8",
            )
            .with_hint("configure a UTF-8 host name and retry")
        })?;
    ensure_ssh_identity_with(paths, OsStr::new("ssh-keygen"), &hostname)
}

pub(crate) fn ensure_ssh_identity_with(
    paths: &Paths,
    keygen: &OsStr,
    hostname: &str,
) -> Result<SshIdentity, FirestoneError> {
    if !valid_hostname_comment(hostname) {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            "cannot generate Firestone SSH key with an empty or control-character hostname",
        )
        .with_hint("configure a non-empty host name without control characters and retry"));
    }

    paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
    let _lock = acquire_identity_lock(paths)?;
    paths.validate_owned_data_directory(paths.data_dir(), "data directory", false)?;

    let ssh_dir = paths.ssh_dir();
    let directory_exists = match fs::symlink_metadata(&ssh_dir) {
        Ok(_) => {
            paths.validate_ssh_data_directory()?;
            true
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => false,
        Err(source) => return Err(identity_io_error("inspect", &ssh_dir, source)),
    };

    if directory_exists && marker_exists(paths)? {
        if let Ok(identity) = validate_identity(paths) {
            remove_generation_marker(paths)?;
            return Ok(identity);
        }
        rollback_generation(paths)?;
    }

    if directory_exists {
        match identity_presence(paths)? {
            IdentityPresence::Complete => return validate_identity(paths),
            IdentityPresence::Incomplete => return Err(incomplete_identity_error(paths)),
            IdentityPresence::Missing => ensure_identity_directory_is_empty(paths)?,
        }
    } else {
        paths.ensure_owned_data_directory(&ssh_dir, "SSH directory", false)?;
        paths.validate_ssh_data_directory()?;
    }

    create_generation_marker(paths)?;
    let private_key = paths.ssh_private_key();
    let comment = format!("firestone@{hostname}");
    let generated = Cmd::new(keygen)
        .args([OsStr::new("-t"), OsStr::new("ed25519"), OsStr::new("-N")])
        .secret_arg(OsString::new())
        .args([
            OsStr::new("-C"),
            OsStr::new(&comment),
            OsStr::new("-f"),
            private_key.as_os_str(),
        ])
        .stdin_null()
        .timeout(KEYGEN_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .run();

    if let Err(source) = generated {
        let rollback = rollback_generation(paths);
        let mut error = FirestoneError::new(
            source.kind(),
            format!(
                "cannot generate Firestone SSH identity: {}",
                source.message()
            ),
        )
        .with_hint("install the OpenSSH client package, then retry")
        .with_source(source);
        if let Err(rollback) = rollback {
            error = FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "SSH key generation failed and partial-file cleanup also failed: {}",
                    rollback.message()
                ),
            )
            .with_hint("remove the named partial key files and `.generating`, then retry")
            .with_source(error);
        }
        return Err(error);
    }

    if let Err(source) = finalize_generated_identity(paths) {
        let rollback = rollback_generation(paths);
        if let Err(rollback) = rollback {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "generated SSH identity is invalid and partial-file cleanup failed: {}",
                    rollback.message()
                ),
            )
            .with_hint("remove the named partial key files and `.generating`, then retry")
            .with_source(source));
        }
        return Err(source);
    }

    let identity = validate_identity(paths)?;
    remove_generation_marker(paths)?;
    Ok(identity)
}

fn valid_hostname_comment(hostname: &str) -> bool {
    !hostname.trim().is_empty() && !hostname.chars().any(char::is_control)
}

fn prepare_identity_lock_path(paths: &Paths, path: &Path) -> Result<(), FirestoneError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(identity_io_error("inspect identity lock", path, source)),
    };
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != paths.uid()
        || mode & !0o600 != 0
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH identity lock '{}' is insecure: expected a uid {} owner-only regular file, found uid {} and mode {mode:04o}",
                path.display(),
                paths.uid(),
                metadata.uid()
            ),
        )
        .with_hint("replace the lock with a regular file owned and protected by the Firestone user"));
    }
    if mode != IDENTITY_LOCK_MODE {
        fs::set_permissions(path, fs::Permissions::from_mode(IDENTITY_LOCK_MODE))
            .map_err(|source| identity_io_error("recover identity lock mode", path, source))?;
        sync_directory(paths.data_dir(), "data directory")?;
    }
    Ok(())
}

fn acquire_identity_lock(paths: &Paths) -> Result<IdentityLock, FirestoneError> {
    let path = paths.ssh_identity_lock();
    prepare_identity_lock_path(paths, &path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(IDENTITY_LOCK_MODE)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    let file = options
        .open(&path)
        .map_err(|source| identity_io_error("open identity lock", &path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| identity_io_error("inspect identity lock", &path, source))?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.uid() != paths.uid() || mode != IDENTITY_LOCK_MODE {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH identity lock '{}' is insecure: expected uid {} and mode 0600, found uid {} and mode {mode:04o}",
                path.display(),
                paths.uid(),
                metadata.uid()
            ),
        )
        .with_hint("replace the lock with a regular file owned and protected by the Firestone user"));
    }

    let deadline = Instant::now() + IDENTITY_LOCK_TIMEOUT;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(IdentityLock { _file: file }),
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(FirestoneError::new(
                        ErrorKind::Busy,
                        "another Firestone process is generating the SSH identity",
                    )
                    .with_hint("wait for the other first-use operation to finish and retry"));
                }
                thread::sleep(IDENTITY_LOCK_POLL.min(remaining));
            }
            Err(source) => {
                return Err(identity_io_error("lock identity", &path, source));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityPresence {
    Missing,
    Incomplete,
    Complete,
}

fn identity_presence(paths: &Paths) -> Result<IdentityPresence, FirestoneError> {
    let private = path_exists(&paths.ssh_private_key())?;
    let public = path_exists(&paths.ssh_public_key())?;
    Ok(match (private, public) {
        (false, false) => IdentityPresence::Missing,
        (true, true) => IdentityPresence::Complete,
        _ => IdentityPresence::Incomplete,
    })
}

fn path_exists(path: &Path) -> Result<bool, FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(identity_io_error("inspect", path, source)),
    }
}

fn validate_identity(paths: &Paths) -> Result<SshIdentity, FirestoneError> {
    paths.validate_ssh_data_directory()?;
    let private_key = paths.ssh_private_key();
    let public_key = paths.ssh_public_key();
    paths.validate_owned_data_file(
        &private_key,
        "Firestone SSH private key",
        PRIVATE_KEY_MODE,
        false,
    )?;
    paths.validate_owned_data_file(
        &public_key,
        "Firestone SSH public key",
        PUBLIC_KEY_MODE,
        false,
    )?;
    validate_public_key_file(paths, &public_key)?;
    Ok(SshIdentity {
        private_key,
        public_key,
    })
}

fn validate_public_key_file(paths: &Paths, path: &Path) -> Result<(), FirestoneError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    let file = options
        .open(path)
        .map_err(|source| identity_io_error("open SSH public key", path, source))?;
    paths.validate_owned_data_file_handle(
        path,
        "Firestone SSH public key",
        PUBLIC_KEY_MODE,
        &file,
    )?;
    let mut bytes = Vec::new();
    let read = (&file)
        .take(MAX_PUBLIC_KEY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| identity_io_error("read SSH public key", path, source))?;
    if read as u64 > MAX_PUBLIC_KEY_BYTES {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH public key '{}' exceeds 16 KiB",
                path.display()
            ),
        )
        .with_hint("replace it with the Ed25519 public key generated by Firestone"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("Firestone SSH public key '{}' is not UTF-8", path.display()),
        )
        .with_hint("replace it with the Ed25519 public key generated by Firestone")
        .with_source(source)
    })?;
    let key = PublicKey::from_openssh(text.trim()).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("Firestone SSH public key '{}' is invalid", path.display()),
        )
        .with_hint("replace it with the Ed25519 public key generated by Firestone")
        .with_source(source)
    })?;
    if key.algorithm() != Algorithm::Ed25519 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH public key '{}' is not Ed25519",
                path.display()
            ),
        )
        .with_hint("replace it with the Ed25519 public key generated by Firestone"));
    }
    Ok(())
}

fn incomplete_identity_error(paths: &Paths) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "Firestone SSH identity in {} is incomplete",
            paths.ssh_dir().display()
        ),
    )
    .with_hint(
        "move the incomplete key pair aside and retry; Firestone never overwrites identity files",
    )
}

fn ensure_identity_directory_is_empty(paths: &Paths) -> Result<(), FirestoneError> {
    let directory = paths.ssh_dir();
    let marker = paths.ssh_generation_marker();
    let mut entries = fs::read_dir(&directory)
        .map_err(|source| identity_io_error("read", &directory, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| identity_io_error("read", &directory, source))?;
    entries.retain(|entry| entry.path() != marker);
    if entries.is_empty() {
        return Ok(());
    }
    entries.sort_by_key(|entry| entry.file_name());
    let first = entries
        .first()
        .map(|entry| entry.path())
        .unwrap_or_else(|| directory.clone());
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "Firestone SSH directory contains unexpected entry {}",
            first.display()
        ),
    )
    .with_hint("move unexpected files out of the Firestone SSH directory and retry"))
}

fn create_generation_marker(paths: &Paths) -> Result<(), FirestoneError> {
    let marker = paths.ssh_generation_marker();
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    let file = options
        .open(&marker)
        .map_err(|source| identity_io_error("create generation marker", &marker, source))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| identity_io_error("set generation marker mode", &marker, source))?;
    file.sync_all()
        .map_err(|source| identity_io_error("fsync generation marker", &marker, source))?;
    sync_directory(&paths.ssh_dir(), "SSH directory")
}

fn marker_exists(paths: &Paths) -> Result<bool, FirestoneError> {
    let marker = paths.ssh_generation_marker();
    match fs::symlink_metadata(&marker) {
        Ok(_) => {
            paths.validate_owned_data_file(
                &marker,
                "SSH generation marker",
                IDENTITY_LOCK_MODE,
                false,
            )?;
            Ok(true)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(identity_io_error(
            "inspect generation marker",
            &marker,
            source,
        )),
    }
}

fn remove_generation_marker(paths: &Paths) -> Result<(), FirestoneError> {
    let marker = paths.ssh_generation_marker();
    match fs::remove_file(&marker) {
        Ok(()) => sync_directory(&paths.ssh_dir(), "SSH directory"),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(identity_io_error(
            "remove generation marker",
            &marker,
            source,
        )),
    }
}

fn finalize_generated_identity(paths: &Paths) -> Result<(), FirestoneError> {
    protect_generated_file(
        paths,
        &paths.ssh_private_key(),
        "SSH private key",
        PRIVATE_KEY_MODE,
    )?;
    protect_generated_file(
        paths,
        &paths.ssh_public_key(),
        "SSH public key",
        PUBLIC_KEY_MODE,
    )?;
    sync_directory(&paths.ssh_dir(), "SSH directory")
}

fn protect_generated_file(
    paths: &Paths,
    path: &Path,
    label: &str,
    mode: u32,
) -> Result<(), FirestoneError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    let file = options
        .open(path)
        .map_err(|source| identity_io_error("open generated file", path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| identity_io_error("inspect generated file", path, source))?;
    if !metadata.is_file() || metadata.uid() != paths.uid() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "generated {label} '{}' is not a current-user regular file",
                path.display()
            ),
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|source| identity_io_error("set generated file mode", path, source))?;
    file.sync_all()
        .map_err(|source| identity_io_error("fsync generated file", path, source))?;
    paths.validate_owned_data_file(path, label, mode, false)
}

fn rollback_generation(paths: &Paths) -> Result<(), FirestoneError> {
    for path in [paths.ssh_public_key(), paths.ssh_private_key()] {
        remove_interrupted_identity_file(&path)?;
    }
    remove_generation_marker(paths)
}

fn remove_interrupted_identity_file(path: &Path) -> Result<(), FirestoneError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(identity_io_error(
                "inspect partial identity file",
                path,
                source,
            ));
        }
    };
    if metadata.file_type().is_dir() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "partial SSH identity path '{}' is a directory",
                path.display()
            ),
        )
        .with_hint("move the unexpected directory aside, remove `.generating`, and retry"));
    }
    fs::remove_file(path)
        .map_err(|source| identity_io_error("remove partial identity file", path, source))
}

/// Deletes a machine's owned host-key trust file when cloud-init identity changes.
///
/// Equal instance ids preserve trust. A differing or previously absent id removes the trust file
/// before the caller durably publishes the new instance id.
pub fn machine_known_hosts_path(paths: &Paths, name: &str) -> Result<PathBuf, FirestoneError> {
    paths.validate_machine_data_directory(name)?;
    let known_hosts = paths.machine_known_hosts(name)?;
    let metadata = match fs::symlink_metadata(&known_hosts) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(known_hosts),
        Err(source) => {
            return Err(identity_io_error(
                "inspect known_hosts",
                &known_hosts,
                source,
            ));
        }
    };
    validate_known_hosts_metadata(paths, &known_hosts, &metadata)?;
    Ok(known_hosts)
}

fn validate_known_hosts_metadata(
    paths: &Paths,
    known_hosts: &Path,
    metadata: &fs::Metadata,
) -> Result<(), FirestoneError> {
    let mode = metadata.mode() & 0o7777;
    if metadata.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == paths.uid()
        && mode & 0o022 == 0
        && mode & 0o600 == 0o600
    {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "machine known_hosts '{}' is insecure: expected a current-user protected regular file, found uid {} and mode {mode:04o}",
            known_hosts.display(),
            metadata.uid()
        ),
    )
    .with_hint("replace it with a regular file writable only by the Firestone user"))
}

/// Deletes a machine's owned host-key trust file when cloud-init identity changes.
///
/// Equal instance ids preserve trust. A differing or previously absent id removes the trust file
/// before the caller durably publishes the new instance id.
pub fn invalidate_known_hosts_for_seed(
    paths: &Paths,
    name: &str,
    previous_instance_id: Option<&str>,
    next_instance_id: &str,
) -> Result<bool, FirestoneError> {
    let known_hosts = machine_known_hosts_path(paths, name)?;
    if previous_instance_id == Some(next_instance_id) || !path_exists(&known_hosts)? {
        return Ok(false);
    }
    fs::remove_file(&known_hosts)
        .map_err(|source| identity_io_error("remove known_hosts", &known_hosts, source))?;
    let machine_dir = paths.machine_dir(name)?;
    sync_directory(&machine_dir, "machine directory")?;
    Ok(true)
}

/// Connects to the one Paths-resolved Cloud Hypervisor v53 vsock socket and completes its
/// `CONNECT <port>\n` / `OK <allocated-port>\n` protocol ([verify 12]).
pub fn connect_vsock(
    paths: &Paths,
    name: &str,
    port: VsockPort,
    timeout: Duration,
) -> Result<VsockConnection, FirestoneError> {
    let socket_path = validate_vsock_socket(paths, name)?;
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Usage,
            "vsock handshake timeout exceeds the host clock range",
        )
        .with_hint("use a shorter vsock handshake timeout")
    })?;
    let mut stream = connect_vsock_socket(name, &socket_path, deadline)?;
    let request = format!("CONNECT {}\n", port.get());
    write_vsock_handshake(&mut stream, request.as_bytes(), deadline, name)?;
    let response = read_vsock_response(&mut stream, deadline, name)?;
    let allocated_port = parse_vsock_response(&response)?;
    stream.set_nonblocking(false).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot make machine `{name}` vsock connection blocking"),
        )
        .with_hint("retry the vsock connection")
        .with_source(source)
    })?;
    Ok(VsockConnection {
        stream,
        allocated_port,
    })
}

fn validate_vsock_socket(paths: &Paths, name: &str) -> Result<PathBuf, FirestoneError> {
    let socket = paths.machine_vsock_socket(name)?;
    match paths.validate_machine_runtime_dir(name) {
        Ok(_) => {}
        Err(error) if path_missing(&paths.machine_runtime_dir(name)?) => {
            return Err(not_running_error(name, &socket, None).with_source(error));
        }
        Err(error) => return Err(error),
    }
    let metadata = match fs::symlink_metadata(&socket) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(not_running_error(name, &socket, Some(source)));
        }
        Err(source) => return Err(identity_io_error("inspect vsock socket", &socket, source)),
    };
    let mode = metadata.mode() & 0o7777;
    if !metadata.file_type().is_socket()
        || metadata.file_type().is_symlink()
        || metadata.uid() != paths.uid()
        || mode & 0o022 != 0
        || mode & 0o200 == 0
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "machine vsock path '{}' is insecure: expected a current-user protected Unix socket, found uid {} and mode {mode:04o}",
                socket.display(),
                metadata.uid()
            ),
        )
        .with_hint("stop the machine, remove its stale runtime directory, and start it again"));
    }
    Ok(socket)
}

fn path_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(source) if source.kind() == io::ErrorKind::NotFound
    )
}

fn connect_vsock_socket(
    name: &str,
    path: &Path,
    deadline: Instant,
) -> Result<UnixStream, FirestoneError> {
    if Instant::now() >= deadline {
        return Err(vsock_timeout(name, "connecting"));
    }
    let address = UnixAddr::new(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::NotRunning,
            format!(
                "cannot address machine `{name}` vsock socket {}",
                path.display()
            ),
        )
        .with_hint("start the machine and retry")
        .with_source(io::Error::from(source))
    })?;
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
    .map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot create vsock proxy socket")
            .with_source(io::Error::from(source))
    })?;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        fcntl(&descriptor, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                "cannot make vsock proxy socket nonblocking",
            )
            .with_source(io::Error::from(source))
        })?;
        fcntl(&descriptor, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                "cannot make vsock proxy socket close-on-exec",
            )
            .with_source(io::Error::from(source))
        })?;
    }
    match connect(descriptor.as_raw_fd(), &address) {
        Ok(()) => Ok(UnixStream::from(descriptor)),
        Err(Errno::EINPROGRESS | Errno::EALREADY) => {
            let stream = UnixStream::from(descriptor);
            wait_for_vsock(&stream, PollFlags::POLLOUT, deadline, name, "connecting")?;
            let pending = getsockopt(&stream, SocketError).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot inspect machine `{name}` vsock connection"),
                )
                .with_source(io::Error::from(source))
            })?;
            if pending == 0 {
                Ok(stream)
            } else {
                let source = io::Error::from_raw_os_error(pending);
                if stale_socket_error(source.kind()) {
                    Err(not_running_error(name, path, Some(source)))
                } else {
                    Err(FirestoneError::new(
                        ErrorKind::Generic,
                        format!("cannot connect to machine `{name}` vsock socket"),
                    )
                    .with_hint("check the machine runtime directory and retry")
                    .with_source(source))
                }
            }
        }
        Err(source @ (Errno::ENOENT | Errno::ECONNREFUSED | Errno::EAGAIN)) => {
            Err(not_running_error(name, path, Some(io::Error::from(source))))
        }
        Err(source) => Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot connect to machine `{name}` vsock socket"),
        )
        .with_hint("check the machine runtime directory and retry")
        .with_source(io::Error::from(source))),
    }
}

fn stale_socket_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

fn write_vsock_handshake(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
    name: &str,
) -> Result<(), FirestoneError> {
    let mut written = 0;
    while written < bytes.len() {
        if Instant::now() >= deadline {
            return Err(vsock_timeout(name, "writing the CONNECT request"));
        }
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!("machine `{name}` vsock socket closed during CONNECT request"),
                )
                .with_hint("verify that the machine is still running and retry"));
            }
            Ok(count) => written += count,
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                wait_for_vsock(
                    stream,
                    PollFlags::POLLOUT,
                    deadline,
                    name,
                    "writing the CONNECT request",
                )?;
            }
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot write machine `{name}` vsock CONNECT request"),
                )
                .with_hint("verify that the machine is still running and retry")
                .with_source(source));
            }
        }
    }
    Ok(())
}

fn read_vsock_response(
    stream: &mut UnixStream,
    deadline: Instant,
    name: &str,
) -> Result<Vec<u8>, FirestoneError> {
    let mut response = Vec::with_capacity(16);
    let mut byte = [0_u8; 1];
    loop {
        if Instant::now() >= deadline {
            return Err(vsock_timeout(name, "waiting for the vsock acknowledgement"));
        }
        if response.len() >= VSOCK_HANDSHAKE_MAX_BYTES {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "machine `{name}` vsock handshake response exceeds {VSOCK_HANDSHAKE_MAX_BYTES} bytes"
                ),
            )
            .with_hint("the running VMM did not return the pinned v53 acknowledgement"));
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!(
                        "machine `{name}` vsock handshake ended before a complete response line: `{}`",
                        escape_protocol_line(&response)
                    ),
                )
                .with_hint("verify that the guest service is listening and retry"));
            }
            Ok(_) if byte[0] == b'\n' => return Ok(response),
            Ok(_) => response.push(byte[0]),
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                wait_for_vsock(
                    stream,
                    PollFlags::POLLIN,
                    deadline,
                    name,
                    "waiting for the vsock acknowledgement",
                )?;
            }
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot read machine `{name}` vsock handshake response"),
                )
                .with_hint("verify that the machine is still running and retry")
                .with_source(source));
            }
        }
    }
}

fn parse_vsock_response(response: &[u8]) -> Result<u32, FirestoneError> {
    let allocation = response
        .strip_prefix(b"OK ")
        .filter(|bytes| !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit))
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.parse::<u32>().ok())
        .filter(|port| *port != 0);
    allocation.ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!(
                "vsock handshake returned `{}`; expected `OK <allocated-port>`",
                escape_protocol_line(response)
            ),
        )
        .with_hint("the running VMM did not return the pinned v53 acknowledgement")
    })
}

fn escape_protocol_line(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b' '..=b'~' => escaped.push(char::from(*byte)),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\x{byte:02x}");
            }
        }
    }
    escaped
}

fn wait_for_vsock(
    stream: &UnixStream,
    events: PollFlags,
    deadline: Instant,
    name: &str,
    phase: &'static str,
) -> Result<(), FirestoneError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(vsock_timeout(name, phase));
        }
        let timeout = match PollTimeout::try_from(remaining) {
            Ok(timeout) => timeout,
            Err(_) => PollTimeout::MAX,
        };
        let mut descriptors = [PollFd::new(stream.as_fd(), events)];
        match poll(&mut descriptors, timeout) {
            Ok(0) => return Err(vsock_timeout(name, phase)),
            Ok(_) if Instant::now() < deadline => return Ok(()),
            Ok(_) => return Err(vsock_timeout(name, phase)),
            Err(Errno::EINTR) => {}
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot poll machine `{name}` vsock socket while {phase}"),
                )
                .with_source(io::Error::from(source)));
            }
        }
    }
}

fn vsock_timeout(name: &str, phase: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Timeout,
        format!("machine `{name}` vsock handshake timed out while {phase}"),
    )
    .with_hint("verify that the guest service is listening, then retry")
}

fn not_running_error(name: &str, path: &Path, source: Option<io::Error>) -> FirestoneError {
    let error = FirestoneError::new(
        ErrorKind::NotRunning,
        format!(
            "machine `{name}` is not running: vsock socket {} is unavailable",
            path.display()
        ),
    )
    .with_hint(format!("start machine `{name}` and retry"));
    match source {
        Some(source) => error.with_source(source),
        None => error,
    }
}

/// Relays stdin and stdout through one completed vsock connection without event or Result framing.
pub fn run_vsock_proxy<R, W>(
    paths: &Paths,
    name: &str,
    port: VsockPort,
    input: R,
    output: W,
) -> Result<(), FirestoneError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let connection = connect_vsock(paths, name, port, VSOCK_HANDSHAKE_TIMEOUT)?;
    relay_vsock(connection.into_stream(), input, output, name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayDirection {
    InputToSocket,
    SocketToOutput,
}

struct RelayCompletion {
    direction: RelayDirection,
    result: io::Result<()>,
}

fn relay_vsock<R, W>(
    stream: UnixStream,
    mut input: R,
    mut output: W,
    name: &str,
) -> Result<(), FirestoneError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut upload = stream.try_clone().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot clone machine `{name}` vsock stream for stdin relay"),
        )
        .with_source(source)
    })?;
    let mut download = stream.try_clone().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot clone machine `{name}` vsock stream for stdout relay"),
        )
        .with_source(source)
    })?;
    let (sender, receiver) = mpsc::channel();
    let upload_sender = sender.clone();
    let _upload_thread = thread::Builder::new()
        .name("firestone-vsock-upload".to_owned())
        .spawn(move || {
            let result = io::copy(&mut input, &mut upload).map(|_| ());
            let _ = upload.shutdown(Shutdown::Write);
            let _ = upload_sender.send(RelayCompletion {
                direction: RelayDirection::InputToSocket,
                result,
            });
        })
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot start machine `{name}` stdin relay"),
            )
            .with_source(source)
        })?;
    let download_sender = sender.clone();
    let _download_thread = thread::Builder::new()
        .name("firestone-vsock-download".to_owned())
        .spawn(move || {
            let result = io::copy(&mut download, &mut output)
                .and_then(|_| output.flush())
                .map(|_| ());
            let _ = download_sender.send(RelayCompletion {
                direction: RelayDirection::SocketToOutput,
                result,
            });
        })
        .map_err(|source| {
            let _ = stream.shutdown(Shutdown::Both);
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot start machine `{name}` stdout relay"),
            )
            .with_source(source)
        })?;
    drop(sender);

    loop {
        let completion = receiver.recv().map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("machine `{name}` vsock relay workers stopped unexpectedly"),
            )
            .with_source(source)
        })?;
        match completion.direction {
            RelayDirection::InputToSocket => match completion.result {
                Ok(()) => {}
                Err(source) if relay_closed(&source) => {}
                Err(source) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return Err(relay_error(
                        name,
                        "read stdin or write the vsock socket",
                        source,
                    ));
                }
            },
            RelayDirection::SocketToOutput => {
                let _ = stream.shutdown(Shutdown::Both);
                return match completion.result {
                    Ok(()) => Ok(()),
                    Err(source) if relay_closed(&source) => Ok(()),
                    Err(source) => Err(relay_error(
                        name,
                        "read the vsock socket or write stdout",
                        source,
                    )),
                };
            }
        }
    }
}

fn relay_closed(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

fn relay_error(name: &str, phase: &str, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {phase} for machine `{name}`"),
    )
    .with_hint("retry after checking the local pipe and machine state")
    .with_source(source)
}

fn sync_directory(path: &Path, label: &str) -> Result<(), FirestoneError> {
    let directory = File::open(path)
        .map_err(|source| identity_io_error(&format!("open {label}"), path, source))?;
    directory
        .sync_all()
        .map_err(|source| identity_io_error(&format!("fsync {label}"), path, source))
}

fn identity_io_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot {operation} {}", path.display()),
    )
    .with_hint("check Firestone path ownership and permissions, then retry")
    .with_source(source)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        path::{Path, PathBuf},
        sync::{Arc, Barrier},
        thread,
    };

    use tempfile::TempDir;

    use crate::{ErrorKind, PathInputs, Paths};

    use super::{
        VSOCK_HANDSHAKE_MAX_BYTES, VsockPort, ensure_ssh_identity_with,
        invalidate_known_hosts_for_seed, machine_known_hosts_path, parse_vsock_response,
    };

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        paths: Paths,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = TempDir::new()?;
            let root = fs::canonicalize(temp.path())?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
            let paths = Paths::from_inputs(&PathInputs {
                current_dir: root.clone(),
                home_dir: None,
                firestone_home: Some(root.join("home")),
                firestone_config_dir: None,
                firestone_data_dir: None,
                firestone_runtime_dir: None,
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: fs::metadata(&root)?.uid(),
            })?;
            paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
            paths.ensure_owned_data_directory(
                &paths.machines_dir(),
                "machines directory",
                false,
            )?;
            paths.ensure_owned_data_directory(
                &paths.machine_dir("demo")?,
                "machine directory",
                false,
            )?;
            Ok(Self {
                _temp: temp,
                root,
                paths,
            })
        }

        fn keygen(&self, body: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
            let keygen = self.root.join("ssh-keygen");
            fs::write(&keygen, format!("#!/bin/sh\nset -eu\n{body}\n"))?;
            fs::set_permissions(&keygen, fs::Permissions::from_mode(0o700))?;
            Ok(keygen)
        }
    }

    fn quoted(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
    }

    fn successful_keygen_body(
        fixture: &Fixture,
        delay: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let arguments = fixture.root.join("keygen.args");
        let calls = fixture.root.join("keygen.calls");
        let sleep = if delay { "sleep 0.1" } else { ":" };
        Ok(format!(
            r#"printf '%s\n' "$@" > {arguments}
    printf 'called\n' >> {calls}
    {sleep}
    key=''
    previous=''
    for argument in "$@"; do
      if [ "$previous" = -f ]; then key=$argument; fi
      previous=$argument
    done
    [ -n "$key" ]
    umask 077
    printf 'PRIVATE-TEST-BYTES' > "$key"
    printf 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P firestone@test\n' > "$key.pub"
    chmod 600 "$key"
    chmod 644 "$key.pub""#,
            arguments = quoted(&arguments),
            calls = quoted(&calls),
        ))
    }

    #[test]
    fn identity_first_use_uses_exact_keygen_contract_and_modes()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let keygen = fixture.keygen(&successful_keygen_body(&fixture, false)?)?;

        let identity =
            ensure_ssh_identity_with(&fixture.paths, keygen.as_os_str(), "fixture-host")?;

        assert_eq!(identity.private_key(), fixture.paths.ssh_private_key());
        assert_eq!(identity.public_key(), fixture.paths.ssh_public_key());
        assert_eq!(
            fs::metadata(identity.private_key())?.permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(identity.public_key())?.permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            fs::metadata(fixture.paths.ssh_dir())?.permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(fixture.paths.ssh_identity_lock())?
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        assert!(!fixture.paths.ssh_generation_marker().exists());
        let arguments = fs::read_to_string(fixture.root.join("keygen.args"))?;
        let expected = format!(
            "-t\ned25519\n-N\n\n-C\nfirestone@fixture-host\n-f\n{}\n",
            fixture.paths.ssh_private_key().display()
        );
        assert_eq!(arguments, expected);
        Ok(())
    }

    #[test]
    fn identity_concurrent_first_use_publishes_one_complete_pair()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let keygen = Arc::new(fixture.keygen(&successful_keygen_body(&fixture, true)?)?);
        let paths = Arc::new(fixture.paths.clone());
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let keygen = Arc::clone(&keygen);
            let paths = Arc::clone(&paths);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                ensure_ssh_identity_with(&paths, keygen.as_os_str(), "fixture-host")
            }));
        }
        for worker in workers {
            let identity = worker.join().expect("identity worker")?;
            assert_eq!(identity.private_key(), paths.ssh_private_key());
            assert_eq!(identity.public_key(), paths.ssh_public_key());
        }
        assert_eq!(
            fs::read_to_string(fixture.root.join("keygen.calls"))?,
            "called\n"
        );
        assert!(!paths.ssh_generation_marker().exists());
        Ok(())
    }

    #[test]
    fn identity_failed_generation_rolls_back_and_retry_succeeds()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let failure = fixture.keygen(
            r#"key=''
previous=''
for argument in "$@"; do
  if [ "$previous" = -f ]; then key=$argument; fi
  previous=$argument
done
[ -n "$key" ]
umask 000
printf partial > "$key"
exit 23"#,
        )?;
        let error = ensure_ssh_identity_with(&fixture.paths, failure.as_os_str(), "fixture-host")
            .expect_err("generation must fail");
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(!fixture.paths.ssh_private_key().exists());
        assert!(!fixture.paths.ssh_public_key().exists());
        assert!(!fixture.paths.ssh_generation_marker().exists());

        let success = fixture.keygen(&successful_keygen_body(&fixture, false)?)?;
        ensure_ssh_identity_with(&fixture.paths, success.as_os_str(), "fixture-host")?;
        assert!(fixture.paths.ssh_private_key().is_file());
        assert!(fixture.paths.ssh_public_key().is_file());
        Ok(())
    }

    #[test]
    fn identity_incomplete_unmarked_pair_is_preserved_with_stable_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.paths.ensure_owned_data_directory(
            &fixture.paths.ssh_dir(),
            "SSH directory",
            false,
        )?;
        let outside = fixture.root.join("outside-private");
        fs::write(&outside, b"do-not-touch")?;
        symlink(&outside, fixture.paths.ssh_private_key())?;
        let keygen = fixture.keygen(&successful_keygen_body(&fixture, false)?)?;

        let error = ensure_ssh_identity_with(&fixture.paths, keygen.as_os_str(), "fixture-host")
            .expect_err("incomplete identity must fail");

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(
            error.message(),
            format!(
                "Firestone SSH identity in {} is incomplete",
                fixture.paths.ssh_dir().display()
            )
        );
        assert!(error.hint().is_some());
        assert_eq!(fs::read(outside)?, b"do-not-touch");
        assert!(!fixture.root.join("keygen.calls").exists());
        Ok(())
    }

    #[test]
    fn identity_interrupted_marker_recovers_only_owned_partial_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.paths.ensure_owned_data_directory(
            &fixture.paths.ssh_dir(),
            "SSH directory",
            false,
        )?;
        fs::write(fixture.paths.ssh_generation_marker(), b"")?;
        fs::set_permissions(
            fixture.paths.ssh_generation_marker(),
            fs::Permissions::from_mode(0o600),
        )?;
        fs::write(fixture.paths.ssh_private_key(), b"partial")?;
        fs::set_permissions(
            fixture.paths.ssh_private_key(),
            fs::Permissions::from_mode(0o600),
        )?;
        let keygen = fixture.keygen(&successful_keygen_body(&fixture, false)?)?;

        ensure_ssh_identity_with(&fixture.paths, keygen.as_os_str(), "fixture-host")?;

        assert_eq!(
            fs::read(fixture.paths.ssh_private_key())?,
            b"PRIVATE-TEST-BYTES"
        );
        assert!(!fixture.paths.ssh_generation_marker().exists());
        Ok(())
    }

    #[test]
    fn known_hosts_unchanged_seed_preserves_and_changed_seed_deletes()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let known_hosts = fixture.paths.machine_known_hosts("demo")?;
        fs::write(&known_hosts, b"host-key")?;
        fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600))?;

        assert!(!invalidate_known_hosts_for_seed(
            &fixture.paths,
            "demo",
            Some("iid-same"),
            "iid-same",
        )?);
        assert_eq!(fs::read(&known_hosts)?, b"host-key");
        assert!(invalidate_known_hosts_for_seed(
            &fixture.paths,
            "demo",
            Some("iid-old"),
            "iid-new",
        )?);
        assert!(!known_hosts.exists());
        Ok(())
    }

    #[test]
    fn known_hosts_symlink_is_rejected_without_touching_target()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let outside = fixture.root.join("outside-known-hosts");
        fs::write(&outside, b"host-key")?;
        let known_hosts = fixture.paths.machine_known_hosts("demo")?;
        symlink(&outside, &known_hosts)?;

        let error = machine_known_hosts_path(&fixture.paths, "demo")
            .expect_err("known_hosts symlink must fail");

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(outside)?, b"host-key");
        assert!(known_hosts.symlink_metadata()?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    fn vsock_ports_and_acknowledgements_require_nonzero_u32_values() {
        assert_eq!("22".parse::<VsockPort>().map(VsockPort::get), Ok(22));
        assert!("0".parse::<VsockPort>().is_err());
        assert!("4294967296".parse::<VsockPort>().is_err());
        assert_eq!(
            parse_vsock_response(b"OK 1073741824").ok(),
            Some(1_073_741_824)
        );
        for response in [
            b"OK 0".as_slice(),
            b"OK -1".as_slice(),
            b"OK 4294967296".as_slice(),
            b"OK 1 extra".as_slice(),
            b"ERR refused".as_slice(),
        ] {
            let error = parse_vsock_response(response).expect_err("invalid response");
            assert_eq!(error.kind(), ErrorKind::Generic);
        }
        assert!(VSOCK_HANDSHAKE_MAX_BYTES >= b"OK 4294967295\n".len());
    }

    #[test]
    fn identity_interrupted_owner_only_lock_mode_recovers() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        fs::write(fixture.paths.ssh_identity_lock(), b"")?;
        fs::set_permissions(
            fixture.paths.ssh_identity_lock(),
            fs::Permissions::from_mode(0o000),
        )?;
        let keygen = fixture.keygen(&successful_keygen_body(&fixture, false)?)?;

        ensure_ssh_identity_with(&fixture.paths, keygen.as_os_str(), "fixture-host")?;

        assert_eq!(
            fs::metadata(fixture.paths.ssh_identity_lock())?
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn identity_real_ssh_keygen_produces_ed25519_host_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;

        let identity = ensure_ssh_identity_with(
            &fixture.paths,
            std::ffi::OsStr::new("ssh-keygen"),
            "real-fixture-host",
        )?;

        let public = fs::read_to_string(identity.public_key())?;
        assert!(public.starts_with("ssh-ed25519 "));
        assert!(public.trim_end().ends_with(" firestone@real-fixture-host"));
        assert_eq!(
            fs::metadata(identity.private_key())?.permissions().mode() & 0o7777,
            0o600
        );
        Ok(())
    }
}
