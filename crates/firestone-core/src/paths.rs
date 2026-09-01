use std::env;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, Metadata, Permissions};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::{FchmodatFlags, Mode, fchmod, fchmodat, mkdirat},
    unistd::getuid,
};

use crate::{ErrorKind, FirestoneError};

const FIRESTONE_HOME: &str = "FIRESTONE_HOME";
const FIRESTONE_CONFIG_DIR: &str = "FIRESTONE_CONFIG_DIR";
const FIRESTONE_DATA_DIR: &str = "FIRESTONE_DATA_DIR";
const FIRESTONE_RUNTIME_DIR: &str = "FIRESTONE_RUNTIME_DIR";
const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
const XDG_DATA_HOME: &str = "XDG_DATA_HOME";
const XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";

/// Process values used to resolve Firestone's filesystem layout.
///
/// Call [`Self::capture`] once at process startup. Tests can construct this
/// value directly without changing process-wide environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInputs {
    pub current_dir: PathBuf,
    pub home_dir: Option<PathBuf>,
    pub firestone_home: Option<PathBuf>,
    pub firestone_config_dir: Option<PathBuf>,
    pub firestone_data_dir: Option<PathBuf>,
    pub firestone_runtime_dir: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub uid: u32,
}

impl PathInputs {
    /// Captures the current directory, relevant environment variables, and uid.
    pub fn capture() -> Result<Self, FirestoneError> {
        let current_dir = env::current_dir().map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot read the process current directory: {source}"),
            )
            .with_hint("change to an accessible directory and retry")
            .with_source(source)
        })?;

        Ok(Self {
            current_dir,
            home_dir: process_path("HOME"),
            firestone_home: process_path(FIRESTONE_HOME),
            firestone_config_dir: process_path(FIRESTONE_CONFIG_DIR),
            firestone_data_dir: process_path(FIRESTONE_DATA_DIR),
            firestone_runtime_dir: process_path(FIRESTONE_RUNTIME_DIR),
            xdg_config_home: process_path(XDG_CONFIG_HOME),
            xdg_data_home: process_path(XDG_DATA_HOME),
            xdg_runtime_dir: process_path(XDG_RUNTIME_DIR),
            uid: getuid().as_raw(),
        })
    }
}

/// Every filesystem location used by Firestone, resolved once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    home_dir: Option<PathBuf>,
    runtime_uid: u32,
    runtime_provenance: RuntimeProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeProvenance {
    FirestoneHome,
    FirestoneRuntimeDir,
    XdgRuntimeDir { base: PathBuf },
    Fallback,
}

impl Paths {
    /// Captures process inputs and resolves all base directories.
    pub fn from_process() -> Result<Self, FirestoneError> {
        Self::from_inputs(&PathInputs::capture()?)
    }

    /// Resolves all base directories from injected inputs.
    pub fn from_inputs(inputs: &PathInputs) -> Result<Self, FirestoneError> {
        if !inputs.current_dir.is_absolute() {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!(
                    "path resolution current directory is not absolute: '{}'",
                    inputs.current_dir.display()
                ),
            )
            .with_hint("supply an absolute current directory"));
        }

        let home_dir = inputs
            .home_dir
            .as_deref()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| absolute_path(path, &inputs.current_dir));

        if let Some(home) = selected_override(&inputs.firestone_home, FIRESTONE_HOME)? {
            let home = absolute_path(home, &inputs.current_dir);
            return Ok(Self {
                config_dir: home.join("config"),
                data_dir: home.join("data"),
                runtime_dir: home.join("run"),
                home_dir,
                runtime_uid: inputs.uid,
                runtime_provenance: RuntimeProvenance::FirestoneHome,
            });
        }

        let config_dir = if let Some(path) =
            selected_override(&inputs.firestone_config_dir, FIRESTONE_CONFIG_DIR)?
        {
            absolute_path(path, &inputs.current_dir)
        } else if let Some(path) = selected_xdg(&inputs.xdg_config_home) {
            path.join("firestone")
        } else {
            home_default(home_dir.as_deref(), ".config", "config")?
        };

        let data_dir = if let Some(path) =
            selected_override(&inputs.firestone_data_dir, FIRESTONE_DATA_DIR)?
        {
            absolute_path(path, &inputs.current_dir)
        } else if let Some(path) = selected_xdg(&inputs.xdg_data_home) {
            path.join("firestone")
        } else {
            home_default(home_dir.as_deref(), ".local/share", "data")?
        };

        let (runtime_dir, runtime_provenance) = if let Some(path) =
            selected_override(&inputs.firestone_runtime_dir, FIRESTONE_RUNTIME_DIR)?
        {
            (
                trim_trailing_separators(&absolute_path(path, &inputs.current_dir)),
                RuntimeProvenance::FirestoneRuntimeDir,
            )
        } else if let Some(path) = selected_xdg(&inputs.xdg_runtime_dir) {
            let base = trim_trailing_separators(path);
            (
                base.join("firestone"),
                RuntimeProvenance::XdgRuntimeDir { base },
            )
        } else {
            (
                PathBuf::from(format!("/tmp/firestone-{}", inputs.uid)),
                RuntimeProvenance::Fallback,
            )
        };

        Ok(Self {
            config_dir,
            data_dir,
            runtime_dir,
            home_dir,
            runtime_uid: inputs.uid,
            runtime_provenance,
        })
    }

    /// Resolves a user-supplied path without reading the filesystem.
    ///
    /// A bare `~` or `~/` prefix expands to the HOME value captured at startup.
    /// Other leading-tilde forms are rejected. Relative paths resolve under
    /// `base_dir`. Dot components remain for the kernel to resolve.
    pub fn resolve_input_path(
        &self,
        path: &Path,
        base_dir: &Path,
        key: &str,
    ) -> Result<PathBuf, FirestoneError> {
        if path.as_os_str().is_empty() {
            return Err(invalid_input_path(
                key,
                "path is empty",
                "set a non-empty absolute path or a path relative to the input file",
            ));
        }

        let mut components = path.components();
        let first = components.next();
        let expanded = if matches!(first, Some(Component::Normal(value)) if value == OsStr::new("~"))
        {
            let home = self.home_dir.as_deref().ok_or_else(|| {
                invalid_input_path(
                    key,
                    "cannot expand '~' because the home directory is unavailable",
                    "use an absolute path or set HOME",
                )
            })?;

            let mut expanded = home.to_path_buf();
            for component in components {
                expanded.push(component.as_os_str());
            }
            expanded
        } else if matches!(first, Some(Component::Normal(value)) if value.as_bytes().first() == Some(&b'~'))
        {
            return Err(invalid_input_path(
                key,
                format!("user-home syntax is not supported in '{}'", path.display()),
                "use '~/' for the current user or an absolute path",
            ));
        } else {
            path.to_path_buf()
        };

        if expanded.is_absolute() {
            Ok(expanded)
        } else {
            Ok(base_dir.join(expanded))
        }
    }

    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    #[must_use]
    pub fn uses_runtime_fallback(&self) -> bool {
        self.runtime_provenance == RuntimeProvenance::Fallback
    }

    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.runtime_uid
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    #[must_use]
    pub fn catalog_file(&self) -> PathBuf {
        self.config_dir.join("catalog.toml")
    }

    /// The Docker CLI configuration file read for registry credentials (§8.5).
    ///
    /// Returns `None` when no home directory is available, which the registry
    /// client treats exactly like a missing file: anonymous access.
    #[must_use]
    pub fn docker_config_file(&self) -> Option<PathBuf> {
        self.home_dir
            .as_ref()
            .map(|home| home.join(".docker").join("config.json"))
    }

    #[must_use]
    pub fn machines_dir(&self) -> PathBuf {
        self.data_dir.join("machines")
    }

    #[must_use]
    pub fn images_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    /// User-owned staging directory for the generated AppArmor profile.
    #[must_use]
    pub fn apparmor_staging_dir(&self) -> PathBuf {
        self.data_dir.join("apparmor")
    }

    /// Generated profile staged before an explicitly authorized root install.
    #[must_use]
    pub fn apparmor_passt_staged_profile(&self) -> PathBuf {
        self.apparmor_staging_dir()
            .join("firestone-passt-2025_02_17.a1e48a0")
    }

    /// Literal versioned root-owned passt attachment target.
    #[must_use]
    pub fn apparmor_passt_executable(&self) -> PathBuf {
        PathBuf::from("/usr/libexec/firestone/passt-2025_02_17.a1e48a0")
    }

    /// Installed AppArmor policy for the literal passt attachment target.
    #[must_use]
    pub fn apparmor_passt_profile(&self) -> PathBuf {
        PathBuf::from("/etc/apparmor.d/firestone-passt-2025_02_17.a1e48a0")
    }

    /// Kernel list used to confirm that the passt policy is loaded.
    #[must_use]
    pub fn apparmor_loaded_profiles(&self) -> PathBuf {
        PathBuf::from("/sys/kernel/security/apparmor/profiles")
    }
    #[must_use]
    pub fn ssh_dir(&self) -> PathBuf {
        self.data_dir.join("ssh")
    }

    #[must_use]
    pub fn ssh_private_key(&self) -> PathBuf {
        self.ssh_dir().join("id_ed25519")
    }

    #[must_use]
    pub fn ssh_public_key(&self) -> PathBuf {
        self.ssh_dir().join("id_ed25519.pub")
    }

    #[must_use]
    pub fn ssh_identity_lock(&self) -> PathBuf {
        self.data_dir.join(".ssh-identity.lock")
    }

    #[must_use]
    pub fn ssh_generation_marker(&self) -> PathBuf {
        self.ssh_dir().join(".generating")
    }

    pub fn binary_file(&self, file_name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.bin_dir(), "binary file name", file_name)
    }

    pub fn image_file(&self, file_name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.images_dir(), "image file name", file_name)
    }

    pub fn image_store_lock(&self) -> Result<PathBuf, FirestoneError> {
        self.image_file(".lock")
    }

    pub fn image_base(&self, id: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!("{id}.qcow2"))
    }

    pub fn image_metadata(&self, id: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!("{id}.json"))
    }

    pub fn image_base_removal(&self, id: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!("{id}.qcow2.removing"))
    }

    pub fn image_metadata_removal(&self, id: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!("{id}.json.removing"))
    }

    pub fn image_source_partial(&self, operation: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!(".pull-{operation}.source.partial"))
    }

    pub fn image_stored_partial(&self, operation: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!(".pull-{operation}.stored.partial"))
    }

    /// One downloaded OCI layer blob of an in-flight pull (SPEC §8.5).
    pub fn image_layer_partial(
        &self,
        operation: &str,
        index: usize,
    ) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!(".pull-{operation}.layer{index}.partial"))
    }

    /// The canonical merged tar `mkfs.ext4 -d` consumes (SPEC §8.5).
    pub fn image_rootfs_tar_partial(&self, operation: &str) -> Result<PathBuf, FirestoneError> {
        self.image_file(&format!(".pull-{operation}.tar.partial"))
    }

    pub fn machine_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.machines_dir(), "machine name", name)
    }
    pub fn machine_removal_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        let removal = format!(".removing-{name}");
        checked_join(&self.machines_dir(), "machine removal name", &removal)
    }

    pub fn machine_spec(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("firestone.toml"))
    }

    pub fn machine_state(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("state.json"))
    }

    pub fn machine_lock(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("lock"))
    }

    pub fn machine_disk(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("disk.qcow2"))
    }

    pub fn machine_disk_partial(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("disk.qcow2.partial"))
    }

    pub fn machine_seed_image(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("seed.img"))
    }
    pub fn machine_vmconfig(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("vmconfig.json"))
    }

    pub fn machine_vmm_executable(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("vmm.bin"))
    }

    pub fn machine_seed_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("seed"))
    }

    pub fn machine_seed_file(
        &self,
        name: &str,
        file_name: &str,
    ) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.machine_seed_dir(name)?, "seed file name", file_name)
    }

    /// The `firestone-init` config disk of an OCI machine (SPEC §10.5).
    ///
    /// It occupies the same `disks[1]` slot as `seed.img` does for a
    /// firmware machine; exactly one of the two exists per machine.
    pub fn machine_config_image(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("config.img"))
    }

    /// The inspection copy of the config document, beside the disk it framed.
    pub fn machine_config_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("config"))
    }

    pub fn machine_config_file(
        &self,
        name: &str,
        file_name: &str,
    ) -> Result<PathBuf, FirestoneError> {
        checked_join(
            &self.machine_config_dir(name)?,
            "config file name",
            file_name,
        )
    }

    pub fn machine_user_data(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("user-data.yaml"))
    }

    pub fn machine_known_hosts(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("known_hosts"))
    }

    pub fn machine_console_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("console.log"))
    }

    pub fn machine_console_previous_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("console.log.previous"))
    }

    pub fn machine_vmm_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("vmm.log"))
    }

    pub fn machine_shim_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("shim.log"))
    }

    pub fn machine_passt_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("passt.log"))
    }

    pub fn machine_virtiofsd_log(
        &self,
        name: &str,
        index: usize,
    ) -> Result<PathBuf, FirestoneError> {
        Ok(self
            .machine_dir(name)?
            .join(format!("virtiofsd-{index}.log")))
    }

    /// Directory holding every published snapshot of one machine (SPEC §23).
    pub fn machine_snapshots_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("snapshots"))
    }

    /// Published directory of one immutable snapshot.
    pub fn machine_snapshot_dir(
        &self,
        name: &str,
        snapshot: &str,
    ) -> Result<PathBuf, FirestoneError> {
        checked_join(
            &self.machine_snapshots_dir(name)?,
            "snapshot name",
            snapshot,
        )
    }

    /// Directory one snapshot is assembled in before its publishing rename.
    ///
    /// The leading dot keeps a partial snapshot out of `snapshot list`.
    pub fn machine_snapshot_partial_dir(
        &self,
        name: &str,
        snapshot: &str,
    ) -> Result<PathBuf, FirestoneError> {
        checked_join(
            &self.machine_snapshots_dir(name)?,
            "snapshot partial name",
            &format!(".partial-{snapshot}"),
        )
    }

    /// Directory one snapshot is renamed into before it is deleted.
    pub fn machine_snapshot_removal_dir(
        &self,
        name: &str,
        snapshot: &str,
    ) -> Result<PathBuf, FirestoneError> {
        checked_join(
            &self.machine_snapshots_dir(name)?,
            "snapshot removal name",
            &format!(".removing-{snapshot}"),
        )
    }

    /// Advisory lock serializing snapshot operations on one machine.
    ///
    /// A running machine's shim owns the machine lock for its whole lifetime,
    /// so warm snapshots cannot take that lock; this one keeps two snapshot
    /// operations on the same machine from interleaving.
    pub fn machine_snapshot_lock(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_snapshots_dir(name)?.join(".lock"))
    }

    /// Marker that turns the next launch of one machine into a warm restore.
    pub fn machine_restore_request(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("restore-request.json"))
    }

    #[must_use]
    pub fn snapshot_metadata(snapshot_dir: &Path) -> PathBuf {
        snapshot_dir.join("metadata.json")
    }

    #[must_use]
    pub fn snapshot_disk(snapshot_dir: &Path) -> PathBuf {
        snapshot_dir.join("disk.qcow2")
    }

    #[must_use]
    pub fn snapshot_disk_partial(snapshot_dir: &Path) -> PathBuf {
        snapshot_dir.join("disk.qcow2.partial")
    }

    #[must_use]
    pub fn snapshot_spec(snapshot_dir: &Path) -> PathBuf {
        snapshot_dir.join("spec.toml")
    }

    #[must_use]
    pub fn snapshot_vmconfig(snapshot_dir: &Path) -> PathBuf {
        snapshot_dir.join("vmconfig.json")
    }

    #[must_use]
    pub fn snapshot_vmstate_dir(snapshot_dir: &Path) -> PathBuf {
        snapshot_dir.join("vmstate")
    }

    #[must_use]
    pub fn serve_socket(&self) -> PathBuf {
        self.runtime_dir.join("serve.sock")
    }
    #[must_use]
    pub fn serve_lock(&self) -> PathBuf {
        self.runtime_dir.join(".serve.lock")
    }

    pub fn machine_runtime_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.runtime_dir, "machine name", name)
    }

    pub fn machine_shim_socket(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("shim.sock"))
    }

    pub fn machine_api_socket(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("api.sock"))
    }

    pub fn machine_vsock_socket(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("vsock.sock"))
    }

    pub fn machine_console_socket(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("console.sock"))
    }

    pub fn machine_console_pty_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("console.pty.log"))
    }

    pub fn machine_net_socket(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("net.sock"))
    }

    pub fn machine_fs_socket(&self, name: &str, index: usize) -> Result<PathBuf, FirestoneError> {
        Ok(self
            .machine_runtime_dir(name)?
            .join(format!("fs{index}.sock")))
    }

    pub fn machine_fs_pid_file(&self, name: &str, index: usize) -> Result<PathBuf, FirestoneError> {
        Ok(self
            .machine_runtime_dir(name)?
            .join(format!("fs{index}.sock.pid")))
    }

    pub fn machine_shim_pid(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("shim.pid"))
    }

    pub fn machine_shim_plan(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("launch.json"))
    }

    pub fn machine_process_identity(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("identity.json"))
    }

    /// Creates or validates one private per-machine runtime directory.
    pub fn ensure_machine_runtime_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        self.ensure_runtime_dir()?;
        let path = self.machine_runtime_dir(name)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => self.validate_private_runtime_directory(&path, &metadata)?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.recursive(false).mode(0o700);
                let created = match builder.create(&path) {
                    Ok(()) => true,
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(source) => return Err(runtime_io_error("create", &path, source)),
                };
                if created {
                    fs::set_permissions(&path, Permissions::from_mode(0o700))
                        .map_err(|source| runtime_io_error("set mode 0700 on", &path, source))?;
                    let parent = File::open(self.runtime_dir())
                        .map_err(|source| runtime_io_error("open", self.runtime_dir(), source))?;
                    parent
                        .sync_all()
                        .map_err(|source| runtime_io_error("fsync", self.runtime_dir(), source))?;
                }
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|source| runtime_io_error("inspect", &path, source))?;
                self.validate_private_runtime_directory(&path, &metadata)?;
            }
            Err(source) => return Err(runtime_io_error("inspect", &path, source)),
        }
        Ok(path)
    }

    /// Validates one existing private per-machine runtime directory.
    pub fn validate_machine_runtime_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        self.validate_runtime_dir()?;
        let path = self.machine_runtime_dir(name)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| runtime_io_error("inspect", &path, source))?;
        self.validate_private_runtime_directory(&path, &metadata)?;
        Ok(path)
    }

    /// Unlinks every non-directory entry from a validated machine runtime dir.
    ///
    /// This never follows symlinks and refuses nested directories. With
    /// `remove_directory`, the now-empty machine directory is removed as well.
    pub fn clear_machine_runtime_dir(
        &self,
        name: &str,
        remove_directory: bool,
    ) -> Result<(), FirestoneError> {
        self.validate_runtime_dir()?;
        let path = self.machine_runtime_dir(name)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(runtime_io_error("inspect", &path, source)),
        };
        self.validate_private_runtime_directory(&path, &metadata)?;
        let mut entries = fs::read_dir(&path)
            .map_err(|source| runtime_io_error("read", &path, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| runtime_io_error("read", &path, source))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)
                .map_err(|source| runtime_io_error("inspect", &entry_path, source))?;
            if metadata.file_type().is_dir() {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "machine runtime debris '{}' is a directory",
                        entry_path.display()
                    ),
                )
                .with_hint("remove the unexpected nested directory and retry"));
            }
            fs::remove_file(&entry_path)
                .map_err(|source| runtime_io_error("remove", &entry_path, source))?;
        }
        let directory =
            File::open(&path).map_err(|source| runtime_io_error("open", &path, source))?;
        directory
            .sync_all()
            .map_err(|source| runtime_io_error("fsync", &path, source))?;
        if remove_directory {
            fs::remove_dir(&path).map_err(|source| runtime_io_error("remove", &path, source))?;
            let parent = File::open(self.runtime_dir())
                .map_err(|source| runtime_io_error("open", self.runtime_dir(), source))?;
            parent
                .sync_all()
                .map_err(|source| runtime_io_error("fsync", self.runtime_dir(), source))?;
        }
        Ok(())
    }

    fn validate_private_runtime_directory(
        &self,
        path: &Path,
        metadata: &Metadata,
    ) -> Result<(), FirestoneError> {
        validate_directory_type(path, "machine runtime directory", metadata)?;
        let mode = metadata.mode() & 0o7777;
        if metadata.uid() != self.runtime_uid || mode != 0o700 {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "machine runtime directory '{}' is insecure: expected uid {} and mode 0700, found uid {} and mode {mode:04o}",
                    path.display(),
                    self.runtime_uid,
                    metadata.uid()
                ),
            )
            .with_hint("remove the stale runtime directory and retry"));
        }
        Ok(())
    }

    /// Creates the runtime directory with mode 0700 when missing.
    ///
    /// An existing runtime path must be a real directory owned by the captured
    /// uid with exactly mode 0700. Insecure paths are rejected and never repaired.
    pub fn ensure_runtime_dir(&self) -> Result<(), FirestoneError> {
        self.validate_runtime_prerequisites()?;

        match fs::symlink_metadata(&self.runtime_dir) {
            Ok(metadata) => self.validate_runtime_metadata(&metadata),
            Err(source) if source.kind() == io::ErrorKind::NotFound => self.create_runtime_dir(),
            Err(source) => Err(runtime_io_error("inspect", &self.runtime_dir, source)),
        }
    }

    /// Validates the runtime base, ancestry, and final directory without mutation.
    ///
    /// Unlike [Self::ensure_runtime_dir], this method returns a dependency
    /// error when the final directory is missing. It never creates a directory
    /// or changes permissions.
    pub fn validate_runtime_dir(&self) -> Result<(), FirestoneError> {
        self.validate_runtime_prerequisites()?;
        self.inspect_runtime_dir()
    }

    /// Validates a Firestone-owned data directory without mutating it.
    ///
    /// Existing ancestors must be real directories owned by the captured uid or
    /// root and protected from rename by other users. A root-owned sticky shared
    /// directory such as /tmp is accepted. When the final directory exists, it
    /// must be owned by the captured uid and must not be group- or
    /// world-writable. With allow_missing, missing components are accepted
    /// without canonicalizing or lexically normalizing the supplied path.
    pub fn validate_owned_data_directory(
        &self,
        path: &Path,
        label: &str,
        allow_missing: bool,
    ) -> Result<(), FirestoneError> {
        self.validate_directory_path(path, label, allow_missing, true)
    }

    /// Creates a Firestone-owned data directory with mode 0700 when missing.
    ///
    /// Existing paths and ancestry are validated before any mutation. The
    /// returned value is true only when this call created the directory.
    pub fn ensure_owned_data_directory(
        &self,
        path: &Path,
        label: &str,
        recursive: bool,
    ) -> Result<bool, FirestoneError> {
        if !path.is_absolute() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} '{}' is not absolute", path.display()),
            )
            .with_hint("use an absolute Firestone data directory"));
        }

        let components = path
            .components()
            .filter_map(|component| match component {
                Component::RootDir => None,
                Component::Normal(value) => Some(Ok(value)),
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => Some(Err(
                    FirestoneError::new(
                        ErrorKind::Dependency,
                        format!(
                            "{label} '{}' contains an unsafe path component",
                            path.display()
                        ),
                    )
                    .with_hint("use an absolute path without dot or parent components"),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if components.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} cannot be the filesystem root"),
            ));
        }

        let mut directory = File::open("/")
            .map_err(|source| directory_io_error("open root for", label, path, source))?;
        let root_metadata = directory
            .metadata()
            .map_err(|source| directory_io_error("inspect root for", label, path, source))?;
        self.validate_trusted_ancestor_metadata(Path::new("/"), label, &root_metadata)?;

        let mut current_path = PathBuf::from("/");
        let mut final_created = false;
        for (index, component) in components.iter().enumerate() {
            let is_final = index + 1 == components.len();
            current_path.push(component);
            let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
            let (descriptor, created) =
                match openat(&directory, Path::new(component), flags, Mode::empty()) {
                    Ok(descriptor) => (descriptor, false),
                    Err(Errno::ENOENT) if recursive || is_final => {
                        let created = match mkdirat(
                            &directory,
                            Path::new(component),
                            Mode::from_bits_truncate(0o700),
                        ) {
                            Ok(()) => true,
                            Err(Errno::EEXIST) => false,
                            Err(source) => {
                                return Err(directory_io_error(
                                    "create",
                                    label,
                                    &current_path,
                                    io::Error::from_raw_os_error(source as i32),
                                ));
                            }
                        };
                        if created {
                            fchmodat(
                                &directory,
                                Path::new(component),
                                Mode::from_bits_truncate(0o700),
                                FchmodatFlags::NoFollowSymlink,
                            )
                            .map_err(|source| {
                                directory_io_error(
                                    "set initial mode 0700 on created",
                                    label,
                                    &current_path,
                                    io::Error::from_raw_os_error(source as i32),
                                )
                            })?;
                        }
                        let descriptor =
                            openat(&directory, Path::new(component), flags, Mode::empty())
                                .map_err(|source| {
                                    descriptor_directory_open_error(
                                        "open created",
                                        label,
                                        &current_path,
                                        source,
                                    )
                                })?;
                        if created {
                            fchmod(&descriptor, Mode::from_bits_truncate(0o700)).map_err(
                                |source| {
                                    directory_io_error(
                                        "set mode 0700 on created",
                                        label,
                                        &current_path,
                                        io::Error::from_raw_os_error(source as i32),
                                    )
                                },
                            )?;
                            directory.sync_all().map_err(|source| {
                                directory_io_error("fsync parent of", label, &current_path, source)
                            })?;
                        }
                        (descriptor, created)
                    }
                    Err(source) => {
                        return Err(descriptor_directory_open_error(
                            "open",
                            label,
                            &current_path,
                            source,
                        ));
                    }
                };

            let opened = File::from(descriptor);
            let metadata = opened.metadata().map_err(|source| {
                directory_io_error("inspect open", label, &current_path, source)
            })?;
            validate_directory_type(&current_path, label, &metadata)?;
            if is_final {
                self.validate_owned_directory_metadata(&current_path, label, &metadata)?;
                if created && metadata.mode() & 0o7777 != 0o700 {
                    return Err(FirestoneError::new(
                        ErrorKind::Dependency,
                        format!(
                            "created {label} '{}' has mode {:04o}; expected 0700",
                            current_path.display(),
                            metadata.mode() & 0o7777
                        ),
                    ));
                }
                final_created = created;
            } else {
                self.validate_trusted_ancestor_metadata(&current_path, label, &metadata)?;
                if created
                    && (metadata.uid() != self.runtime_uid || metadata.mode() & 0o7777 != 0o700)
                {
                    return Err(FirestoneError::new(
                        ErrorKind::Dependency,
                        format!(
                            "created {label} ancestor '{}' is insecure",
                            current_path.display()
                        ),
                    ));
                }
            }
            directory = opened;
        }
        Ok(final_created)
    }
    /// Validates one regular Firestone-owned data file and its ancestry.
    pub fn validate_owned_data_file(
        &self,
        path: &Path,
        label: &str,
        expected_mode: u32,
        allow_missing: bool,
    ) -> Result<(), FirestoneError> {
        let parent = path.parent().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} '{}' has no parent directory", path.display()),
            )
            .with_hint("use a file below the Firestone data directory")
        })?;
        self.validate_owned_data_directory(parent, &format!("{label} parent"), false)?;

        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(source) if allow_missing && source.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(source) => return Err(directory_io_error("inspect", label, path, source)),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} '{}' is not a regular owned file", path.display()),
            )
            .with_hint("replace the symlink or special file with a regular Firestone-owned file"));
        }

        let actual_uid = metadata.uid();
        let actual_mode = metadata.mode() & 0o7777;
        if actual_uid != self.runtime_uid || actual_mode != expected_mode {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "{label} '{}' is insecure: expected uid {} and mode {expected_mode:04o}, found uid {actual_uid} and mode {actual_mode:04o}",
                    path.display(),
                    self.runtime_uid
                ),
            )
            .with_hint("replace the file with one owned and protected by the Firestone user"));
        }
        Ok(())
    }

    pub fn validate_owned_data_file_handle(
        &self,
        path: &Path,
        label: &str,
        expected_mode: u32,
        file: &File,
    ) -> Result<(), FirestoneError> {
        let parent = path.parent().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} '{}' has no parent directory", path.display()),
            )
            .with_hint("use a file below the Firestone data directory")
        })?;
        self.validate_owned_data_directory(parent, &format!("{label} parent"), false)?;
        let metadata = file
            .metadata()
            .map_err(|source| directory_io_error("inspect open", label, path, source))?;
        if !metadata.is_file() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} '{}' is not a regular owned file", path.display()),
            )
            .with_hint("replace the special file with a regular Firestone-owned file"));
        }
        let actual_uid = metadata.uid();
        let actual_mode = metadata.mode() & 0o7777;
        if actual_uid != self.runtime_uid || actual_mode != expected_mode {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "{label} '{}' is insecure: expected uid {} and mode {expected_mode:04o}, found uid {actual_uid} and mode {actual_mode:04o}",
                    path.display(),
                    self.runtime_uid
                ),
            )
            .with_hint("replace the file with one owned and protected by the Firestone user"));
        }
        Ok(())
    }

    /// Revalidates every owned directory that contains a machine publication.
    ///
    /// Writers call this immediately before atomic publication so a replaced
    /// data, machines, or machine directory cannot redirect output.
    pub fn validate_machine_data_directory(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        self.validate_owned_data_directory(self.data_dir(), "data directory", false)?;
        let machines_dir = self.machines_dir();
        self.validate_owned_data_directory(&machines_dir, "machines directory", false)?;
        let machine_dir = self.machine_dir(name)?;
        self.validate_owned_data_directory(&machine_dir, "machine directory", false)?;
        Ok(machine_dir)
    }

    /// Revalidates the owned data and SSH directories before key access.
    pub fn validate_ssh_data_directory(&self) -> Result<(), FirestoneError> {
        self.validate_owned_data_subdirectory(&self.ssh_dir(), "SSH directory")
    }

    /// Revalidates the owned data and binary directories before artifact access.
    pub fn validate_bin_data_directory(&self) -> Result<(), FirestoneError> {
        self.validate_owned_data_subdirectory(&self.bin_dir(), "binary directory")
    }

    fn validate_owned_data_subdirectory(
        &self,
        path: &Path,
        label: &str,
    ) -> Result<(), FirestoneError> {
        self.validate_owned_data_directory(self.data_dir(), "data directory", false)?;
        self.validate_owned_data_directory(path, label, false)
    }
    fn validate_runtime_prerequisites(&self) -> Result<(), FirestoneError> {
        match &self.runtime_provenance {
            RuntimeProvenance::XdgRuntimeDir { base } => {
                let parent = base.parent().ok_or_else(|| {
                    insecure_runtime_ancestry_error(base, "XDG_RUNTIME_DIR has no parent directory")
                })?;
                self.validate_runtime_ancestry(parent)?;
                self.validate_xdg_runtime_base(base)?;
            }
            RuntimeProvenance::FirestoneHome
            | RuntimeProvenance::FirestoneRuntimeDir
            | RuntimeProvenance::Fallback => {
                let parent = self.runtime_dir.parent().ok_or_else(|| {
                    insecure_runtime_ancestry_error(
                        &self.runtime_dir,
                        "runtime directory has no parent directory",
                    )
                })?;
                self.validate_runtime_ancestry(parent)?;
            }
        }
        Ok(())
    }

    fn create_runtime_dir(&self) -> Result<(), FirestoneError> {
        let mut builder = DirBuilder::new();
        builder.recursive(false).mode(0o700);

        match builder.create(&self.runtime_dir) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return self.inspect_runtime_dir();
            }
            Err(source) => {
                return Err(runtime_io_error("create", &self.runtime_dir, source));
            }
        }

        let metadata = fs::symlink_metadata(&self.runtime_dir)
            .map_err(|source| runtime_io_error("inspect", &self.runtime_dir, source))?;
        validate_directory_type(&self.runtime_dir, "runtime directory", &metadata)?;

        fs::set_permissions(&self.runtime_dir, Permissions::from_mode(0o700))
            .map_err(|source| runtime_io_error("set mode 0700 on", &self.runtime_dir, source))?;

        self.inspect_runtime_dir()
    }

    fn validate_xdg_runtime_base(&self, base: &Path) -> Result<(), FirestoneError> {
        let metadata = fs::symlink_metadata(base)
            .map_err(|source| runtime_io_error("inspect XDG_RUNTIME_DIR", base, source))?;
        validate_directory_type(base, "XDG_RUNTIME_DIR", &metadata)?;

        let actual_uid = metadata.uid();
        let actual_mode = metadata.mode() & 0o7777;
        if actual_uid == self.runtime_uid && actual_mode == 0o700 {
            return Ok(());
        }

        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "XDG_RUNTIME_DIR '{}' is insecure: expected uid {} and mode 0700, found uid {actual_uid} and mode {actual_mode:04o}",
                base.display(),
                self.runtime_uid
            ),
        )
        .with_hint("set XDG_RUNTIME_DIR to the private runtime directory created for this uid"))
    }

    fn validate_runtime_ancestry(&self, parent: &Path) -> Result<(), FirestoneError> {
        self.validate_directory_path(parent, "runtime directory ancestor", false, false)
    }

    fn validate_directory_path(
        &self,
        path: &Path,
        label: &str,
        allow_missing: bool,
        owned_final: bool,
    ) -> Result<(), FirestoneError> {
        if !path.is_absolute() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} '{}' is not absolute", path.display()),
            )
            .with_hint("use an absolute Firestone data directory"));
        }

        let mut current = PathBuf::new();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            current.push(component.as_os_str());
            let is_final = components.peek().is_none();
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(source) if allow_missing && source.kind() == io::ErrorKind::NotFound => {
                    continue;
                }
                Err(source) => {
                    return Err(directory_io_error("inspect", label, &current, source));
                }
            };

            validate_directory_type(&current, label, &metadata)?;
            if owned_final && is_final {
                self.validate_owned_directory_metadata(&current, label, &metadata)?;
            } else {
                self.validate_trusted_ancestor_metadata(&current, label, &metadata)?;
            }
        }

        Ok(())
    }

    fn validate_trusted_ancestor_metadata(
        &self,
        path: &Path,
        label: &str,
        metadata: &Metadata,
    ) -> Result<(), FirestoneError> {
        let actual_uid = metadata.uid();
        let actual_mode = metadata.mode() & 0o7777;
        let trusted_owner = actual_uid == self.runtime_uid || actual_uid == 0;
        let writable_by_other_users = actual_mode & 0o022 != 0;
        let safe_sticky_root = actual_uid == 0 && actual_mode & 0o1000 != 0;
        if trusted_owner && (!writable_by_other_users || safe_sticky_root) {
            return Ok(());
        }

        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{label} '{}' has insecure ancestry: expected uid {} or root with protected rename permissions, found uid {actual_uid} and mode {actual_mode:04o}",
                path.display(),
                self.runtime_uid
            ),
        )
        .with_hint(
            "move the directory below private user-owned ancestry or a root-owned protected directory",
        ))
    }

    fn validate_owned_directory_metadata(
        &self,
        path: &Path,
        label: &str,
        metadata: &Metadata,
    ) -> Result<(), FirestoneError> {
        let actual_uid = metadata.uid();
        let actual_mode = metadata.mode() & 0o7777;
        if actual_uid == self.runtime_uid && actual_mode & 0o022 == 0 {
            return Ok(());
        }

        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{label} '{}' is insecure: expected uid {} without group/world write access, found uid {actual_uid} and mode {actual_mode:04o}",
                path.display(),
                self.runtime_uid
            ),
        )
        .with_hint("move the existing directory aside or restrict it to its owning user"))
    }

    fn inspect_runtime_dir(&self) -> Result<(), FirestoneError> {
        let metadata = fs::symlink_metadata(&self.runtime_dir)
            .map_err(|source| runtime_io_error("inspect", &self.runtime_dir, source))?;
        self.validate_runtime_metadata(&metadata)
    }

    fn validate_runtime_metadata(&self, metadata: &Metadata) -> Result<(), FirestoneError> {
        validate_directory_type(&self.runtime_dir, "runtime directory", metadata)?;

        let expected_uid = self.runtime_uid;
        let actual_uid = metadata.uid();
        let actual_mode = metadata.mode() & 0o7777;
        if actual_uid == expected_uid && actual_mode == 0o700 {
            return Ok(());
        }

        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "runtime directory '{}' is insecure: expected uid {expected_uid} and mode 0700, found uid {actual_uid} and mode {actual_mode:04o}",
                self.runtime_dir.display()
            ),
        )
        .with_hint(format!(
            "check the directory contents, remove '{}', then run 'firestone doctor --fix'",
            self.runtime_dir.display()
        )))
    }
}

fn process_path(variable: &str) -> Option<PathBuf> {
    env::var_os(variable).map(PathBuf::from)
}

fn selected_override<'a>(
    path: &'a Option<PathBuf>,
    variable: &str,
) -> Result<Option<&'a Path>, FirestoneError> {
    match path.as_deref() {
        Some(path) if path.as_os_str().is_empty() => Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("{variable} is set to an empty path"),
        )
        .with_hint(format!("set {variable} to a directory or unset it"))),
        selected => Ok(selected),
    }
}

fn selected_xdg(path: &Option<PathBuf>) -> Option<&Path> {
    path.as_deref()
        .filter(|path| !path.as_os_str().is_empty() && path.is_absolute())
}

fn absolute_path(path: &Path, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        current_dir.join(path)
    }
}

fn trim_trailing_separators(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()))
}

fn invalid_input_path(
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

fn home_default(
    home_dir: Option<&Path>,
    relative_base: &str,
    purpose: &str,
) -> Result<PathBuf, FirestoneError> {
    let home = home_dir.ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot resolve the {purpose} directory because HOME is not set"),
        )
        .with_hint(format!(
            "set FIRESTONE_HOME, FIRESTONE_{}_DIR, or HOME",
            purpose.to_ascii_uppercase()
        ))
    })?;

    Ok(home.join(relative_base).join("firestone"))
}

fn checked_join(base: &Path, label: &str, value: &str) -> Result<PathBuf, FirestoneError> {
    let mut components = Path::new(value).components();
    let valid = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !value.contains(std::path::MAIN_SEPARATOR)
        && !value.chars().any(char::is_control);

    if valid {
        return Ok(base.join(value));
    }

    Err(FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("{label} must be one non-empty path component: {value:?}"),
    )
    .with_hint("remove path separators, '.' components, and control characters"))
}

fn insecure_runtime_ancestry_error(path: &Path, message: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("{message}: '{}'", path.display()),
    )
    .with_hint(
        "move the runtime directory below private user-owned ancestry or a root-owned protected directory",
    )
}

fn validate_directory_type(
    path: &Path,
    label: &str,
    metadata: &Metadata,
) -> Result<(), FirestoneError> {
    if metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} '{}' is a symbolic link", path.display()),
        )
        .with_hint("replace it with a directory owned by the Firestone user"));
    }

    if metadata.is_dir() {
        return Ok(());
    }

    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!("{label} '{}' is not a directory", path.display()),
    )
    .with_hint("move the existing path and run 'firestone doctor --fix'"))
}

fn runtime_io_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    directory_io_error(operation, "runtime directory", path, source)
}

fn descriptor_directory_open_error(
    operation: &str,
    label: &str,
    path: &Path,
    source: Errno,
) -> FirestoneError {
    if matches!(source, Errno::ELOOP | Errno::ENOTDIR) {
        return FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{label} '{}' is not a directory or contains a symbolic link",
                path.display()
            ),
        )
        .with_hint("replace the symbolic link or non-directory path and retry");
    }
    directory_io_error(
        operation,
        label,
        path,
        io::Error::from_raw_os_error(source as i32),
    )
}

fn directory_io_error(
    operation: &str,
    label: &str,
    path: &Path,
    source: io::Error,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot {operation} {label} '{}': {source}", path.display()),
    )
    .with_hint("check the parent directory permissions and run 'firestone doctor --fix'")
    .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use nix::unistd::getuid;
    use tempfile::tempdir;

    use super::{PathInputs, Paths, RuntimeProvenance};
    use crate::ErrorKind;

    fn inputs() -> PathInputs {
        PathInputs {
            current_dir: PathBuf::from("/work"),
            home_dir: Some(PathBuf::from("/home/alice")),
            firestone_home: None,
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: 1234,
        }
    }

    #[test]
    fn paths_firestone_home_set_overrides_all_other_inputs() -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.firestone_home = Some(PathBuf::from("/firestone"));
        inputs.firestone_config_dir = Some(PathBuf::from("/individual/config"));
        inputs.firestone_data_dir = Some(PathBuf::from("/individual/data"));
        inputs.firestone_runtime_dir = Some(PathBuf::from("/individual/run"));
        inputs.xdg_config_home = Some(PathBuf::from("/xdg/config"));
        inputs.xdg_data_home = Some(PathBuf::from("/xdg/data"));
        inputs.xdg_runtime_dir = Some(PathBuf::from("/xdg/run"));

        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(paths.config_dir(), Path::new("/firestone/config"));
        assert_eq!(paths.data_dir(), Path::new("/firestone/data"));
        assert_eq!(paths.runtime_dir(), Path::new("/firestone/run"));
        assert!(!paths.uses_runtime_fallback());
        Ok(())
    }

    #[test]
    fn paths_individual_overrides_set_override_xdg_inputs() -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.firestone_config_dir = Some(PathBuf::from("/individual/config"));
        inputs.firestone_data_dir = Some(PathBuf::from("/individual/data"));
        inputs.firestone_runtime_dir = Some(PathBuf::from("/individual/run"));
        inputs.xdg_config_home = Some(PathBuf::from("/xdg/config"));
        inputs.xdg_data_home = Some(PathBuf::from("/xdg/data"));
        inputs.xdg_runtime_dir = Some(PathBuf::from("/xdg/run"));

        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(paths.config_dir(), Path::new("/individual/config"));
        assert_eq!(paths.data_dir(), Path::new("/individual/data"));
        assert_eq!(paths.runtime_dir(), Path::new("/individual/run"));
        assert!(!paths.uses_runtime_fallback());
        Ok(())
    }

    #[test]
    fn paths_xdg_inputs_set_override_home_defaults() -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.xdg_config_home = Some(PathBuf::from("/xdg/config"));
        inputs.xdg_data_home = Some(PathBuf::from("/xdg/data"));
        inputs.xdg_runtime_dir = Some(PathBuf::from("/xdg/run"));

        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(paths.config_dir(), Path::new("/xdg/config/firestone"));
        assert_eq!(paths.data_dir(), Path::new("/xdg/data/firestone"));
        assert_eq!(paths.runtime_dir(), Path::new("/xdg/run/firestone"));
        assert!(!paths.uses_runtime_fallback());
        Ok(())
    }

    #[test]
    fn paths_xdg_inputs_empty_use_home_and_runtime_fallback() -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.xdg_config_home = Some(PathBuf::new());
        inputs.xdg_data_home = Some(PathBuf::new());
        inputs.xdg_runtime_dir = Some(PathBuf::new());

        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(
            paths.config_dir(),
            Path::new("/home/alice/.config/firestone")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/home/alice/.local/share/firestone")
        );
        assert_eq!(paths.runtime_dir(), Path::new("/tmp/firestone-1234"));
        assert!(paths.uses_runtime_fallback());
        Ok(())
    }

    #[test]
    fn paths_xdg_inputs_relative_use_home_and_runtime_fallback() -> Result<(), crate::FirestoneError>
    {
        let mut inputs = inputs();
        inputs.xdg_config_home = Some(PathBuf::from("xdg/config"));
        inputs.xdg_data_home = Some(PathBuf::from("xdg/data"));
        inputs.xdg_runtime_dir = Some(PathBuf::from("xdg/run"));

        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(
            paths.config_dir(),
            Path::new("/home/alice/.config/firestone")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/home/alice/.local/share/firestone")
        );
        assert_eq!(paths.runtime_dir(), Path::new("/tmp/firestone-1234"));
        assert!(paths.uses_runtime_fallback());
        Ok(())
    }

    #[test]
    fn paths_relative_overrides_set_resolve_against_current_directory()
    -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.firestone_config_dir = Some(PathBuf::from("config"));
        inputs.firestone_data_dir = Some(PathBuf::from("data"));
        inputs.firestone_runtime_dir = Some(PathBuf::from("run"));

        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(paths.config_dir(), Path::new("/work/config"));
        assert_eq!(paths.data_dir(), Path::new("/work/data"));
        assert_eq!(paths.runtime_dir(), Path::new("/work/run"));
        Ok(())
    }

    #[test]
    fn paths_home_missing_for_default_returns_dependency() {
        let mut inputs = inputs();
        inputs.home_dir = None;

        let error = Paths::from_inputs(&inputs).err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(
            error
                .and_then(|error| error.hint().map(str::to_owned))
                .is_some()
        );
    }

    #[test]
    fn paths_empty_firestone_override_returns_invalid_spec() {
        let mut inputs = inputs();
        inputs.firestone_config_dir = Some(PathBuf::new());

        let error = Paths::from_inputs(&inputs).err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::InvalidSpec)
        );
        assert!(
            error
                .and_then(|error| error.hint().map(str::to_owned))
                .is_some()
        );
    }

    #[test]
    fn input_path_bare_tilde_set_expands_home() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let resolved =
            paths.resolve_input_path(Path::new("~"), Path::new("/machines/demo"), "mount.host")?;

        assert_eq!(resolved, PathBuf::from("/home/alice"));
        Ok(())
    }

    #[test]
    fn input_path_tilde_prefix_with_parent_preserves_kernel_resolution()
    -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let resolved = paths.resolve_input_path(
            Path::new("~/projects/./firestone/../image.qcow2"),
            Path::new("/machines/demo"),
            "image",
        )?;

        assert_eq!(
            resolved,
            PathBuf::from("/home/alice/projects/firestone/../image.qcow2")
        );
        Ok(())
    }

    #[test]
    fn input_path_tilde_home_missing_returns_invalid_spec() -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.home_dir = None;
        inputs.firestone_home = Some(PathBuf::from("/firestone"));
        let paths = Paths::from_inputs(&inputs)?;
        let error = paths
            .resolve_input_path(
                Path::new("~/image.qcow2"),
                Path::new("/machines/demo"),
                "image",
            )
            .err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::InvalidSpec)
        );
        assert!(
            error
                .as_ref()
                .is_some_and(|error| error.message().contains("invalid 'image'"))
        );
        assert!(
            error
                .and_then(|error| error.hint().map(str::to_owned))
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn input_path_named_home_returns_invalid_spec() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let error = paths
            .resolve_input_path(
                Path::new("~root/image.qcow2"),
                Path::new("/machines/demo"),
                "image",
            )
            .err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::InvalidSpec)
        );
        assert!(
            error
                .as_ref()
                .is_some_and(|error| error.message().contains("~root/image.qcow2"))
        );
        Ok(())
    }

    #[test]
    fn input_path_relative_with_dot_components_preserves_kernel_resolution()
    -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let resolved = paths.resolve_input_path(
            Path::new("seed/./parts/../user-data.yaml"),
            Path::new("/data/machines/demo"),
            "cloud_init.user_data",
        )?;

        assert_eq!(
            resolved,
            PathBuf::from("/data/machines/demo/seed/./parts/../user-data.yaml")
        );
        Ok(())
    }

    #[test]
    fn input_path_parent_components_set_preserves_kernel_resolution()
    -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let resolved = paths.resolve_input_path(
            Path::new("../../images/missing.qcow2"),
            Path::new("/data/machines/demo"),
            "image",
        )?;

        assert_eq!(
            resolved,
            PathBuf::from("/data/machines/demo/../../images/missing.qcow2")
        );
        Ok(())
    }

    #[test]
    fn input_path_absolute_missing_set_preserves_kernel_resolution()
    -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let resolved = paths.resolve_input_path(
            Path::new("/does/not/exist/./child/../image.qcow2"),
            Path::new("/unused"),
            "image",
        )?;

        assert_eq!(
            resolved,
            PathBuf::from("/does/not/exist/./child/../image.qcow2")
        );
        Ok(())
    }

    #[test]
    fn input_path_tilde_repeated_separators_stays_below_home() -> Result<(), crate::FirestoneError>
    {
        let paths = Paths::from_inputs(&inputs())?;

        let resolved =
            paths.resolve_input_path(Path::new("~//etc"), Path::new("/unused"), "mount.host")?;

        assert_eq!(resolved, PathBuf::from("/home/alice/etc"));
        Ok(())
    }

    #[test]
    fn input_path_relative_home_set_captures_absolute_startup_home()
    -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.home_dir = Some(PathBuf::from("home/alice"));
        let paths = Paths::from_inputs(&inputs)?;

        let resolved =
            paths.resolve_input_path(Path::new("~/image.qcow2"), Path::new("/unused"), "image")?;

        assert_eq!(resolved, PathBuf::from("/work/home/alice/image.qcow2"));
        Ok(())
    }

    #[test]
    fn input_path_non_utf8_component_set_preserves_bytes() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;
        let path = PathBuf::from(OsString::from_vec(vec![b'~', b'/', 0xff]));

        let resolved = paths.resolve_input_path(&path, Path::new("/unused"), "mount.host")?;

        assert_eq!(
            resolved.file_name().map(OsStr::as_bytes),
            Some([0xff].as_slice())
        );
        assert_eq!(resolved.parent(), Some(Path::new("/home/alice")));
        Ok(())
    }

    #[test]
    fn input_path_missing_prefix_before_parent_remains_missing()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        fs::create_dir(base.join("target"))?;
        let mut inputs = inputs();
        inputs.current_dir = base.clone();
        let paths = Paths::from_inputs(&inputs)?;

        let resolved =
            paths.resolve_input_path(Path::new("missing/../target"), &base, "mount.host")?;

        assert_eq!(resolved, base.join("missing/../target"));
        assert!(fs::canonicalize(resolved).is_err());
        Ok(())
    }

    #[test]
    fn input_path_symlink_before_parent_uses_kernel_target_parent()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        fs::create_dir_all(base.join("real/nested"))?;
        fs::create_dir(base.join("real/marker"))?;
        fs::create_dir(base.join("marker"))?;
        symlink(base.join("real/nested"), base.join("link"))?;
        let mut inputs = inputs();
        inputs.current_dir = base.clone();
        let paths = Paths::from_inputs(&inputs)?;

        let resolved =
            paths.resolve_input_path(Path::new("link/../marker"), &base, "mount.host")?;

        assert_eq!(resolved, base.join("link/../marker"));
        assert_eq!(fs::canonicalize(resolved)?, base.join("real/marker"));
        Ok(())
    }

    #[test]
    fn path_helpers_valid_components_return_spec_layout() -> Result<(), crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.firestone_home = Some(PathBuf::from("/firestone"));
        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(
            paths.config_file(),
            PathBuf::from("/firestone/config/config.toml")
        );
        assert_eq!(
            paths.catalog_file(),
            PathBuf::from("/firestone/config/catalog.toml")
        );
        assert_eq!(
            paths.binary_file("cloud-hypervisor-1")?,
            PathBuf::from("/firestone/data/bin/cloud-hypervisor-1")
        );
        assert_eq!(
            paths.apparmor_passt_staged_profile(),
            PathBuf::from("/firestone/data/apparmor/firestone-passt-2025_02_17.a1e48a0")
        );
        assert_eq!(
            paths.apparmor_passt_executable(),
            PathBuf::from("/usr/libexec/firestone/passt-2025_02_17.a1e48a0")
        );
        assert_eq!(
            paths.apparmor_passt_profile(),
            PathBuf::from("/etc/apparmor.d/firestone-passt-2025_02_17.a1e48a0")
        );
        assert_eq!(
            paths.apparmor_loaded_profiles(),
            PathBuf::from("/sys/kernel/security/apparmor/profiles")
        );
        assert_eq!(
            paths.image_file("ubuntu.qcow2")?,
            PathBuf::from("/firestone/data/images/ubuntu.qcow2")
        );
        assert_eq!(
            paths.ssh_private_key(),
            PathBuf::from("/firestone/data/ssh/id_ed25519")
        );
        assert_eq!(
            paths.ssh_public_key(),
            PathBuf::from("/firestone/data/ssh/id_ed25519.pub")
        );
        assert_eq!(
            paths.ssh_identity_lock(),
            PathBuf::from("/firestone/data/.ssh-identity.lock")
        );
        assert_eq!(
            paths.ssh_generation_marker(),
            PathBuf::from("/firestone/data/ssh/.generating")
        );
        assert_eq!(
            paths.machine_spec("demo")?,
            PathBuf::from("/firestone/data/machines/demo/firestone.toml")
        );
        assert_eq!(
            paths.machine_state("demo")?,
            PathBuf::from("/firestone/data/machines/demo/state.json")
        );
        assert_eq!(
            paths.machine_lock("demo")?,
            PathBuf::from("/firestone/data/machines/demo/lock")
        );
        assert_eq!(
            paths.machine_disk("demo")?,
            PathBuf::from("/firestone/data/machines/demo/disk.qcow2")
        );
        assert_eq!(
            paths.machine_seed_image("demo")?,
            PathBuf::from("/firestone/data/machines/demo/seed.img")
        );
        assert_eq!(
            paths.machine_seed_file("demo", "meta-data")?,
            PathBuf::from("/firestone/data/machines/demo/seed/meta-data")
        );
        assert_eq!(
            paths.machine_user_data("demo")?,
            PathBuf::from("/firestone/data/machines/demo/user-data.yaml")
        );
        assert_eq!(
            paths.machine_known_hosts("demo")?,
            PathBuf::from("/firestone/data/machines/demo/known_hosts")
        );
        assert_eq!(
            paths.machine_console_log("demo")?,
            PathBuf::from("/firestone/data/machines/demo/console.log")
        );
        assert_eq!(
            paths.machine_vmm_executable("demo")?,
            PathBuf::from("/firestone/data/machines/demo/vmm.bin")
        );
        assert_eq!(
            paths.machine_vmm_log("demo")?,
            PathBuf::from("/firestone/data/machines/demo/vmm.log")
        );
        assert_eq!(
            paths.machine_shim_log("demo")?,
            PathBuf::from("/firestone/data/machines/demo/shim.log")
        );
        assert_eq!(
            paths.machine_passt_log("demo")?,
            PathBuf::from("/firestone/data/machines/demo/passt.log")
        );
        assert_eq!(
            paths.machine_virtiofsd_log("demo", 0)?,
            PathBuf::from("/firestone/data/machines/demo/virtiofsd-0.log")
        );
        assert_eq!(
            paths.serve_socket(),
            PathBuf::from("/firestone/run/serve.sock")
        );
        assert_eq!(
            paths.machine_shim_socket("demo")?,
            PathBuf::from("/firestone/run/demo/shim.sock")
        );
        assert_eq!(
            paths.machine_api_socket("demo")?,
            PathBuf::from("/firestone/run/demo/api.sock")
        );
        assert_eq!(
            paths.machine_vsock_socket("demo")?,
            PathBuf::from("/firestone/run/demo/vsock.sock")
        );
        assert_eq!(
            paths.machine_console_socket("demo")?,
            PathBuf::from("/firestone/run/demo/console.sock")
        );
        assert_eq!(
            paths.machine_console_pty_log("demo")?,
            PathBuf::from("/firestone/run/demo/console.pty.log")
        );
        assert_eq!(
            paths.machine_net_socket("demo")?,
            PathBuf::from("/firestone/run/demo/net.sock")
        );
        assert_eq!(
            paths.machine_fs_socket("demo", 0)?,
            PathBuf::from("/firestone/run/demo/fs0.sock")
        );
        assert_eq!(
            paths.machine_fs_pid_file("demo", 0)?,
            PathBuf::from("/firestone/run/demo/fs0.sock.pid")
        );
        assert_eq!(
            paths.machine_shim_pid("demo")?,
            PathBuf::from("/firestone/run/demo/shim.pid")
        );
        Ok(())
    }

    #[test]
    fn machine_name_parent_component_returns_invalid_spec() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;

        let error = paths.machine_dir("../other").err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::InvalidSpec)
        );
        assert!(
            error
                .and_then(|error| error.hint().map(str::to_owned))
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn image_name_trailing_separator_returns_invalid_spec() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;

        let error = paths.image_file("ubuntu.qcow2/").err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::InvalidSpec)
        );
        Ok(())
    }

    #[test]
    fn owned_names_controls_return_invalid_spec() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;

        let errors = [
            paths.machine_dir("bad\nname").err(),
            paths.image_file("bad\timage").err(),
            paths.binary_file("bad\u{7f}binary").err(),
            paths.machine_seed_file("demo", "bad\rseed").err(),
            paths.machine_dir("bad\u{85}name").err(),
        ];

        assert!(errors.iter().all(Option::is_some));
        assert!(errors.iter().all(|error| {
            error.as_ref().map(crate::FirestoneError::kind) == Some(ErrorKind::InvalidSpec)
        }));
        Ok(())
    }

    #[test]
    fn owned_names_invalid_components_return_invalid_spec() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;

        for value in [
            "",
            ".",
            "..",
            "/absolute",
            "nested/name",
            "nested//name",
            "nul\0name",
        ] {
            let error = paths.machine_dir(value).err();
            assert_eq!(
                error.as_ref().map(crate::FirestoneError::kind),
                Some(ErrorKind::InvalidSpec),
                "value {value:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn owned_names_unicode_component_returns_joined_path() -> Result<(), crate::FirestoneError> {
        let paths = Paths::from_inputs(&inputs())?;

        let machine = paths.machine_dir("máquina-猫")?;

        assert_eq!(
            machine,
            PathBuf::from("/home/alice/.local/share/firestone/machines/máquina-猫")
        );
        Ok(())
    }

    #[test]
    fn runtime_dir_missing_with_parent_existing_creates_mode_0700()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("run");
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        paths.ensure_runtime_dir()?;

        let metadata = fs::symlink_metadata(runtime_dir)?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn runtime_dir_read_only_missing_returns_dependency_without_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("run");
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.validate_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!runtime_dir.exists());
        Ok(())
    }

    #[test]
    fn runtime_dir_read_only_valid_returns_ok_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("run");
        fs::create_dir(&runtime_dir)?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        paths.validate_runtime_dir()?;

        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn runtime_dir_read_only_insecure_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("run");
        fs::create_dir(&runtime_dir)?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755))?;
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.validate_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o755);
        Ok(())
    }

    #[test]
    fn runtime_dir_existing_world_accessible_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("run");
        fs::create_dir(&runtime_dir)?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755))?;
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o755);
        Ok(())
    }

    #[test]
    fn runtime_dir_missing_parent_returns_dependency_without_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("missing/run");
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!base.join("missing").exists());
        Ok(())
    }

    #[test]
    fn runtime_dir_symlink_ancestor_returns_dependency_without_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let target = base.join("target");
        let link = base.join("link");
        fs::create_dir(&target)?;
        symlink(&target, &link)?;
        let runtime_dir = link.join("run");
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!target.join("run").exists());
        Ok(())
    }

    #[test]
    fn runtime_dir_trailing_slash_symlink_returns_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let target = base.join("target");
        let link = base.join("runtime-link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        symlink(&target, &link)?;
        let mut inputs = inputs();
        inputs.current_dir = base;
        inputs.home_dir = Some(target);
        inputs.firestone_runtime_dir = Some(with_trailing_separators(&link, 2));
        inputs.uid = getuid().as_raw();
        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(paths.runtime_dir(), link);
        assert_eq!(
            paths
                .validate_runtime_dir()
                .err()
                .as_ref()
                .map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(
            paths
                .ensure_runtime_dir()
                .err()
                .as_ref()
                .map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }

    #[test]
    fn runtime_dir_trailing_separators_regular_directory_is_accepted()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime = base.join("runtime");
        fs::create_dir(&runtime)?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        let mut inputs = inputs();
        inputs.current_dir = base.clone();
        inputs.home_dir = Some(base);
        inputs.firestone_runtime_dir = Some(with_trailing_separators(&runtime, 3));
        inputs.uid = getuid().as_raw();
        let paths = Paths::from_inputs(&inputs)?;

        assert_eq!(paths.runtime_dir(), runtime);
        paths.validate_runtime_dir()?;
        Ok(())
    }

    #[test]
    fn runtime_dir_world_writable_parent_returns_dependency_without_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let unsafe_parent = base.join("unsafe");
        fs::create_dir(&unsafe_parent)?;
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777))?;
        let runtime_dir = unsafe_parent.join("run");
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!runtime_dir.exists());
        Ok(())
    }

    #[test]
    fn runtime_dir_root_sticky_tmp_ancestry_creates_leaf() -> Result<(), Box<dyn std::error::Error>>
    {
        let shared_tmp = fs::canonicalize(Path::new("/tmp"))?;
        let temporary = tempfile::tempdir_in(shared_tmp)?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let parent = fs::canonicalize(temporary.path())?;
        let runtime_dir = parent.join("run");
        let paths = explicit_paths(runtime_dir.clone(), getuid().as_raw());

        paths.ensure_runtime_dir()?;

        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn runtime_dir_wrong_owner_ancestor_returns_dependency_without_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        let runtime_dir = base.join("run");
        let uid = getuid().as_raw();
        let paths = explicit_paths(runtime_dir.clone(), uid.wrapping_add(1));

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!runtime_dir.exists());
        Ok(())
    }

    #[test]
    fn firestone_home_runtime_parent_private_creates_only_run_leaf()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let home = fs::canonicalize(temporary.path())?;
        let mut inputs = inputs();
        inputs.current_dir = home.clone();
        inputs.home_dir = Some(home.clone());
        inputs.firestone_home = Some(home.clone());
        inputs.uid = getuid().as_raw();
        let paths = Paths::from_inputs(&inputs)?;

        paths.ensure_runtime_dir()?;

        let metadata = fs::symlink_metadata(home.join("run"))?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        assert!(!home.join("config").exists());
        assert!(!home.join("data").exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_base_missing_returns_dependency_without_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let parent = fs::canonicalize(temporary.path())?;
        let base = parent.join("missing");
        let paths = xdg_paths(base.clone(), getuid().as_raw())?;

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!base.exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_base_symlink_returns_dependency_without_child()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let parent = fs::canonicalize(temporary.path())?;
        let target = parent.join("target");
        let link = parent.join("runtime-link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        symlink(&target, &link)?;
        let paths = xdg_paths(link, getuid().as_raw())?;

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!target.join("firestone").exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_base_trailing_slash_symlink_returns_dependency_without_child()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let parent = fs::canonicalize(temporary.path())?;
        let target = parent.join("target");
        let link = parent.join("runtime-link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        symlink(&target, &link)?;
        let paths = xdg_paths(with_trailing_separators(&link, 2), getuid().as_raw())?;

        assert_eq!(
            paths
                .ensure_runtime_dir()
                .err()
                .as_ref()
                .map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!target.join("firestone").exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_base_wrong_owner_returns_dependency_without_child()
    -> Result<(), Box<dyn std::error::Error>> {
        let shared_tmp = fs::canonicalize(Path::new("/tmp"))?;
        let temporary = tempfile::tempdir_in(shared_tmp)?;
        let base = fs::canonicalize(temporary.path())?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
        let paths = xdg_paths(base.clone(), getuid().as_raw().wrapping_add(1))?;

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!base.join("firestone").exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_base_wrong_mode_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755))?;
        let paths = xdg_paths(base.clone(), getuid().as_raw())?;

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(&base)?.mode() & 0o7777, 0o755);
        assert!(!base.join("firestone").exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_read_only_insecure_base_returns_dependency_without_child()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755))?;
        let paths = xdg_paths(base.clone(), getuid().as_raw())?;

        let error = paths.validate_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!base.join("firestone").exists());
        Ok(())
    }

    #[test]
    fn xdg_runtime_base_private_creates_only_firestone_child()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let base = fs::canonicalize(temporary.path())?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
        let paths = xdg_paths(base.clone(), getuid().as_raw())?;

        paths.ensure_runtime_dir()?;

        let metadata = fs::symlink_metadata(base.join("firestone"))?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), getuid().as_raw());
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn fallback_runtime_correct_owner_and_mode_is_accepted()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let runtime_dir = fs::canonicalize(temporary.path())?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
        let paths = fallback_paths(runtime_dir.clone(), getuid().as_raw());

        paths.ensure_runtime_dir()?;

        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn fallback_runtime_missing_created_with_owner_and_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let parent = fs::canonicalize(temporary.path())?;
        let runtime_dir = parent.join("runtime");
        let uid = getuid().as_raw();
        let paths = fallback_paths(runtime_dir.clone(), uid);

        paths.ensure_runtime_dir()?;

        let metadata = fs::symlink_metadata(runtime_dir)?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn fallback_runtime_wrong_mode_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let runtime_dir = fs::canonicalize(temporary.path())?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755))?;
        let paths = fallback_paths(runtime_dir.clone(), getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o755);
        Ok(())
    }

    #[test]
    fn fallback_runtime_symlink_returns_dependency() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let parent = fs::canonicalize(temporary.path())?;
        let target = parent.join("target");
        let link = parent.join("runtime");
        fs::create_dir(&target)?;
        symlink(target, &link)?;
        let paths = fallback_paths(link, getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }
    #[test]
    fn owned_data_directory_permissive_final_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let root = fs::canonicalize(temporary.path())?;
        let data_dir = root.join("data");
        fs::create_dir(&data_dir)?;
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o777))?;
        let paths = explicit_paths(root.join("run"), fs::metadata(&root)?.uid());

        let error = paths
            .validate_owned_data_directory(&data_dir, "data directory", false)
            .err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(data_dir)?.mode() & 0o7777, 0o777);
        Ok(())
    }

    #[test]
    fn owned_data_directory_symlink_ancestor_returns_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let root = fs::canonicalize(temporary.path())?;
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        symlink(&target, &link)?;
        let paths = explicit_paths(root.join("run"), fs::metadata(&root)?.uid());

        let error = paths
            .validate_owned_data_directory(&link.join("data"), "data directory", true)
            .err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }

    #[test]
    fn owned_data_directory_user_owned_sticky_ancestor_returns_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let root = fs::canonicalize(temporary.path())?;
        let shared = root.join("shared");
        fs::create_dir(&shared)?;
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777))?;
        let paths = explicit_paths(root.join("run"), fs::metadata(&root)?.uid());

        let error = paths
            .validate_owned_data_directory(&shared.join("data"), "data directory", true)
            .err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }

    #[test]
    fn owned_data_directory_missing_prefix_before_parent_remains_missing()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let root = fs::canonicalize(temporary.path())?;
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        symlink(&target, &link)?;
        let paths = explicit_paths(root.join("run"), fs::metadata(&root)?.uid());
        let unresolved = root.join("missing").join("..").join("link");

        paths.validate_owned_data_directory(&unresolved, "data directory", true)?;

        assert!(!root.join("missing").exists());
        Ok(())
    }

    #[test]
    fn machine_runtime_existing_wrong_mode_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let root = fs::canonicalize(temporary.path())?;
        let runtime = root.join("run");
        fs::create_dir(&runtime)?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        let machine = runtime.join("demo");
        fs::create_dir(&machine)?;
        fs::set_permissions(&machine, fs::Permissions::from_mode(0o755))?;
        let paths = explicit_paths(runtime, fs::metadata(&root)?.uid());

        let error = paths.ensure_machine_runtime_dir("demo").err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(machine)?.mode() & 0o7777, 0o755);
        Ok(())
    }

    fn explicit_paths(runtime_dir: PathBuf, uid: u32) -> Paths {
        test_paths(runtime_dir, uid, RuntimeProvenance::FirestoneRuntimeDir)
    }

    fn fallback_paths(runtime_dir: PathBuf, uid: u32) -> Paths {
        test_paths(runtime_dir, uid, RuntimeProvenance::Fallback)
    }

    fn xdg_paths(base: PathBuf, uid: u32) -> Result<Paths, crate::FirestoneError> {
        let mut inputs = inputs();
        inputs.current_dir = base
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        inputs.xdg_runtime_dir = Some(base);
        inputs.uid = uid;
        Paths::from_inputs(&inputs)
    }

    fn test_paths(runtime_dir: PathBuf, uid: u32, runtime_provenance: RuntimeProvenance) -> Paths {
        Paths {
            config_dir: PathBuf::from("/unused/config"),
            data_dir: PathBuf::from("/unused/data"),
            runtime_dir,
            home_dir: Some(PathBuf::from("/home/alice")),
            runtime_uid: uid,
            runtime_provenance,
        }
    }

    fn with_trailing_separators(path: &Path, count: usize) -> PathBuf {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.extend(std::iter::repeat_n(b'/', count));
        PathBuf::from(OsString::from_vec(bytes))
    }

    #[test]
    fn paths_docker_config_file_follows_home_directory() -> Result<(), crate::FirestoneError> {
        let mut with_home = inputs();
        with_home.firestone_home = Some(PathBuf::from("/firestone"));

        let paths = Paths::from_inputs(&with_home)?;

        assert_eq!(
            paths.docker_config_file(),
            Some(PathBuf::from("/home/alice/.docker/config.json"))
        );
        Ok(())
    }

    #[test]
    fn paths_docker_config_file_without_home_is_none() -> Result<(), crate::FirestoneError> {
        let mut without_home = inputs();
        without_home.home_dir = None;
        without_home.firestone_home = Some(PathBuf::from("/firestone"));

        let paths = Paths::from_inputs(&without_home)?;

        assert_eq!(paths.docker_config_file(), None);
        Ok(())
    }
}
