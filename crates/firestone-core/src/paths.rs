use std::env;
use std::fs::{self, DirBuilder, Metadata, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use nix::unistd::getuid;

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
    runtime_uid: u32,
    runtime_fallback: bool,
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

        if let Some(home) = selected_override(&inputs.firestone_home, FIRESTONE_HOME)? {
            let home = absolute_path(home, &inputs.current_dir);
            return Ok(Self {
                config_dir: home.join("config"),
                data_dir: home.join("data"),
                runtime_dir: home.join("run"),
                runtime_uid: inputs.uid,
                runtime_fallback: false,
            });
        }

        let config_dir = if let Some(path) =
            selected_override(&inputs.firestone_config_dir, FIRESTONE_CONFIG_DIR)?
        {
            absolute_path(path, &inputs.current_dir)
        } else if let Some(path) = selected_xdg(&inputs.xdg_config_home) {
            absolute_path(path, &inputs.current_dir).join("firestone")
        } else {
            home_default(inputs, ".config", "config")?
        };

        let data_dir = if let Some(path) =
            selected_override(&inputs.firestone_data_dir, FIRESTONE_DATA_DIR)?
        {
            absolute_path(path, &inputs.current_dir)
        } else if let Some(path) = selected_xdg(&inputs.xdg_data_home) {
            absolute_path(path, &inputs.current_dir).join("firestone")
        } else {
            home_default(inputs, ".local/share", "data")?
        };

        let (runtime_dir, runtime_fallback) = if let Some(path) =
            selected_override(&inputs.firestone_runtime_dir, FIRESTONE_RUNTIME_DIR)?
        {
            (absolute_path(path, &inputs.current_dir), false)
        } else if let Some(path) = selected_xdg(&inputs.xdg_runtime_dir) {
            (
                absolute_path(path, &inputs.current_dir).join("firestone"),
                false,
            )
        } else {
            (
                PathBuf::from(format!("/tmp/firestone-{}", inputs.uid)),
                true,
            )
        };

        Ok(Self {
            config_dir,
            data_dir,
            runtime_dir,
            runtime_uid: inputs.uid,
            runtime_fallback,
        })
    }

    /// Resolves a user-supplied path without reading the filesystem.
    ///
    /// A bare `~` or `~/` prefix expands to `home_dir`. Other leading-tilde
    /// forms are rejected. Relative paths resolve under `base_dir`, then `.`
    /// and `..` components are normalized lexically.
    pub fn resolve_input_path(
        path: &Path,
        home_dir: Option<&Path>,
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
        let text = path.to_string_lossy();
        let expanded = if text == "~" || text.starts_with("~/") {
            let home = home_dir.ok_or_else(|| {
                invalid_input_path(
                    key,
                    "cannot expand '~' because the home directory is unavailable",
                    "use an absolute path or set HOME",
                )
            })?;
            if text == "~" {
                home.to_path_buf()
            } else {
                home.join(text.trim_start_matches("~/"))
            }
        } else if text.starts_with('~') {
            return Err(invalid_input_path(
                key,
                format!("user-home syntax is not supported in '{}'", path.display()),
                "use '~/' for the current user or an absolute path",
            ));
        } else {
            path.to_path_buf()
        };

        if expanded.is_absolute() {
            Ok(normalize_path(&expanded))
        } else {
            Ok(normalize_path(&base_dir.join(expanded)))
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
        self.runtime_fallback
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    #[must_use]
    pub fn catalog_file(&self) -> PathBuf {
        self.config_dir.join("catalog.toml")
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

    pub fn binary_file(&self, file_name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.bin_dir(), "binary file name", file_name)
    }

    pub fn image_file(&self, file_name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.images_dir(), "image file name", file_name)
    }

    pub fn machine_dir(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        checked_join(&self.machines_dir(), "machine name", name)
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

    pub fn machine_seed_image(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("seed.img"))
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

    pub fn machine_user_data(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("user-data.yaml"))
    }

    pub fn machine_known_hosts(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("known_hosts"))
    }

    pub fn machine_console_log(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_dir(name)?.join("console.log"))
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

    #[must_use]
    pub fn serve_socket(&self) -> PathBuf {
        self.runtime_dir.join("serve.sock")
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

    pub fn machine_net_socket(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("net.sock"))
    }

    pub fn machine_fs_socket(&self, name: &str, index: usize) -> Result<PathBuf, FirestoneError> {
        Ok(self
            .machine_runtime_dir(name)?
            .join(format!("fs{index}.sock")))
    }

    pub fn machine_shim_pid(&self, name: &str) -> Result<PathBuf, FirestoneError> {
        Ok(self.machine_runtime_dir(name)?.join("shim.pid"))
    }

    /// Creates the runtime directory with mode 0700 when missing.
    ///
    /// An existing runtime path must be a real directory owned by the captured
    /// uid with exactly mode 0700. Insecure paths are rejected and never repaired.
    pub fn ensure_runtime_dir(&self) -> Result<(), FirestoneError> {
        match fs::symlink_metadata(&self.runtime_dir) {
            Ok(metadata) => self.validate_runtime_dir(&metadata),
            Err(source) if source.kind() == io::ErrorKind::NotFound => self.create_runtime_dir(),
            Err(source) => Err(runtime_io_error("inspect", &self.runtime_dir, source)),
        }
    }

    fn create_runtime_dir(&self) -> Result<(), FirestoneError> {
        let mut builder = DirBuilder::new();
        builder.recursive(!self.runtime_fallback).mode(0o700);

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
        validate_directory_type(&self.runtime_dir, &metadata)?;

        fs::set_permissions(&self.runtime_dir, Permissions::from_mode(0o700))
            .map_err(|source| runtime_io_error("set mode 0700 on", &self.runtime_dir, source))?;

        self.inspect_runtime_dir()
    }

    fn inspect_runtime_dir(&self) -> Result<(), FirestoneError> {
        let metadata = fs::symlink_metadata(&self.runtime_dir)
            .map_err(|source| runtime_io_error("inspect", &self.runtime_dir, source))?;
        self.validate_runtime_dir(&metadata)
    }

    fn validate_runtime_dir(&self, metadata: &Metadata) -> Result<(), FirestoneError> {
        validate_directory_type(&self.runtime_dir, metadata)?;

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
    path.as_deref().filter(|path| !path.as_os_str().is_empty())
}

fn absolute_path(path: &Path, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        current_dir.join(path)
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
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
    inputs: &PathInputs,
    relative_base: &str,
    purpose: &str,
) -> Result<PathBuf, FirestoneError> {
    let home = inputs
        .home_dir
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot resolve the {purpose} directory because HOME is not set"),
            )
            .with_hint(format!(
                "set FIRESTONE_HOME, FIRESTONE_{}_DIR, or HOME",
                purpose.to_ascii_uppercase()
            ))
        })?;

    Ok(absolute_path(home, &inputs.current_dir)
        .join(relative_base)
        .join("firestone"))
}

fn checked_join(base: &Path, label: &str, value: &str) -> Result<PathBuf, FirestoneError> {
    let mut components = Path::new(value).components();
    let valid = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !value.contains(std::path::MAIN_SEPARATOR)
        && !value.contains('\0');

    if valid {
        return Ok(base.join(value));
    }

    Err(FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("{label} must be one non-empty path component: {value:?}"),
    )
    .with_hint("remove path separators, '.' components, and NUL bytes"))
}

fn validate_directory_type(path: &Path, metadata: &Metadata) -> Result<(), FirestoneError> {
    if metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("runtime directory '{}' is a symbolic link", path.display()),
        )
        .with_hint("replace it with a directory owned by the Firestone user with mode 0700"));
    }

    if metadata.is_dir() {
        return Ok(());
    }

    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!("runtime path '{}' is not a directory", path.display()),
    )
    .with_hint("move the existing path and run 'firestone doctor --fix'"))
}

fn runtime_io_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "cannot {operation} runtime directory '{}': {source}",
            path.display()
        ),
    )
    .with_hint("check the parent directory permissions and run 'firestone doctor --fix'")
    .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use nix::unistd::getuid;
    use tempfile::tempdir;

    use super::{PathInputs, Paths};
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
        let resolved = Paths::resolve_input_path(
            Path::new("~"),
            Some(Path::new("/home/alice")),
            Path::new("/machines/demo"),
            "mount.host",
        )?;

        assert_eq!(resolved, PathBuf::from("/home/alice"));
        Ok(())
    }

    #[test]
    fn input_path_tilde_prefix_set_expands_and_normalizes() -> Result<(), crate::FirestoneError> {
        let resolved = Paths::resolve_input_path(
            Path::new("~/projects/./firestone/../image.qcow2"),
            Some(Path::new("/home/alice")),
            Path::new("/machines/demo"),
            "image",
        )?;

        assert_eq!(resolved, PathBuf::from("/home/alice/projects/image.qcow2"));
        Ok(())
    }

    #[test]
    fn input_path_tilde_home_missing_returns_invalid_spec() {
        let error = Paths::resolve_input_path(
            Path::new("~/image.qcow2"),
            None,
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
    }

    #[test]
    fn input_path_named_home_returns_invalid_spec() {
        let error = Paths::resolve_input_path(
            Path::new("~root/image.qcow2"),
            Some(Path::new("/home/alice")),
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
    }

    #[test]
    fn input_path_relative_set_joins_base_and_normalizes() -> Result<(), crate::FirestoneError> {
        let resolved = Paths::resolve_input_path(
            Path::new("seed/./parts/../user-data.yaml"),
            None,
            Path::new("/data/machines/demo"),
            "cloud_init.user_data",
        )?;

        assert_eq!(
            resolved,
            PathBuf::from("/data/machines/demo/seed/user-data.yaml")
        );
        Ok(())
    }

    #[test]
    fn input_path_parent_components_set_normalizes_lexically() -> Result<(), crate::FirestoneError>
    {
        let resolved = Paths::resolve_input_path(
            Path::new("../../images/missing.qcow2"),
            None,
            Path::new("/data/machines/demo"),
            "image",
        )?;

        assert_eq!(resolved, PathBuf::from("/data/images/missing.qcow2"));
        Ok(())
    }

    #[test]
    fn input_path_absolute_missing_set_normalizes_without_filesystem_access()
    -> Result<(), crate::FirestoneError> {
        let resolved = Paths::resolve_input_path(
            Path::new("/does/not/exist/./child/../image.qcow2"),
            None,
            Path::new("/unused"),
            "image",
        )?;

        assert_eq!(resolved, PathBuf::from("/does/not/exist/image.qcow2"));
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
            paths.machine_net_socket("demo")?,
            PathBuf::from("/firestone/run/demo/net.sock")
        );
        assert_eq!(
            paths.machine_fs_socket("demo", 0)?,
            PathBuf::from("/firestone/run/demo/fs0.sock")
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
    fn runtime_dir_missing_created_with_mode_0700() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let runtime_dir = temporary.path().join("nested/run");
        let paths = Paths {
            config_dir: temporary.path().join("config"),
            data_dir: temporary.path().join("data"),
            runtime_dir: runtime_dir.clone(),
            runtime_uid: getuid().as_raw(),
            runtime_fallback: false,
        };

        paths.ensure_runtime_dir()?;

        let metadata = fs::symlink_metadata(runtime_dir)?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[test]
    fn runtime_dir_existing_world_accessible_returns_dependency_without_chmod()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let runtime_dir = temporary.path().join("run");
        fs::create_dir(&runtime_dir)?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755))?;
        let paths = Paths {
            config_dir: temporary.path().join("config"),
            data_dir: temporary.path().join("data"),
            runtime_dir: runtime_dir.clone(),
            runtime_uid: getuid().as_raw(),
            runtime_fallback: false,
        };

        let error = paths.ensure_runtime_dir().err();

        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(fs::symlink_metadata(runtime_dir)?.mode() & 0o7777, 0o755);
        Ok(())
    }

    #[test]
    fn fallback_runtime_correct_owner_and_mode_is_accepted()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let paths = fallback_paths(temporary.path().to_owned(), getuid().as_raw());

        paths.ensure_runtime_dir()?;

        assert_eq!(
            fs::symlink_metadata(temporary.path())?.mode() & 0o7777,
            0o700
        );
        Ok(())
    }

    #[test]
    fn fallback_runtime_missing_created_with_owner_and_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let runtime_dir = temporary.path().join("runtime");
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
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o755))?;
        let paths = fallback_paths(temporary.path().to_owned(), getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert_eq!(
            fs::symlink_metadata(temporary.path())?.mode() & 0o7777,
            0o755
        );
        Ok(())
    }

    #[test]
    fn fallback_runtime_wrong_owner_returns_dependency() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let uid = getuid().as_raw();
        let paths = fallback_paths(temporary.path().to_owned(), uid.wrapping_add(1));

        let error = paths.ensure_runtime_dir().err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }

    #[test]
    fn fallback_runtime_symlink_returns_dependency() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempdir()?;
        let target = temporary.path().join("target");
        let link = temporary.path().join("runtime");
        fs::create_dir(&target)?;
        symlink(target, &link)?;
        let paths = fallback_paths(link, getuid().as_raw());

        let error = paths.ensure_runtime_dir().err();

        assert!(error.is_some());
        assert_eq!(
            error.as_ref().map(crate::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }

    fn fallback_paths(runtime_dir: PathBuf, uid: u32) -> Paths {
        Paths {
            config_dir: PathBuf::from("/unused/config"),
            data_dir: PathBuf::from("/unused/data"),
            runtime_dir,
            runtime_uid: uid,
            runtime_fallback: true,
        }
    }
}
