use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use ssh_key::PublicKey;

use super::{Arch, ByteSize, MachineSpec, NetMode};
use crate::{Catalog, ErrorKind, FirestoneError, Paths};

/// A non-fatal spec issue surfaced consistently by each interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecWarning {
    pub key: &'static str,
    pub message: String,
}

/// Host and filesystem operations used by spec validation.
///
/// Tests implement this trait to exercise Linux checks on any development host.
pub trait ValidationHost: Send + Sync {
    fn architecture(&self) -> Result<Arch, FirestoneError>;
    fn cpu_count(&self) -> usize;
    fn home_dir(&self) -> Option<&Path>;
    fn path_exists(&self, path: &Path) -> io::Result<bool>;
    fn path_is_readable(&self, path: &Path) -> io::Result<bool>;
    fn path_is_file(&self, path: &Path) -> io::Result<bool>;
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn tap_device_is_tap(&self, name: &str) -> io::Result<bool>;
    fn tun_is_accessible(&self) -> io::Result<()>;
}

/// Real host checks. Tap validation only inspects sysfs and opens `/dev/net/tun`.
/// It never opens, creates, or configures the named tap interface.
#[derive(Debug, Clone)]
pub struct RealValidationHost {
    home_dir: Option<PathBuf>,
}

impl RealValidationHost {
    #[must_use]
    pub fn new() -> Self {
        Self {
            home_dir: directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()),
        }
    }
}

impl Default for RealValidationHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationHost for RealValidationHost {
    fn architecture(&self) -> Result<Arch, FirestoneError> {
        Arch::current().map_err(|message| {
            FirestoneError::new(ErrorKind::Dependency, message)
                .with_hint("run Firestone on an x86_64 or aarch64 Linux host")
        })
    }

    fn cpu_count(&self) -> usize {
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home_dir.as_deref()
    }

    fn path_exists(&self, path: &Path) -> io::Result<bool> {
        match fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn path_is_readable(&self, path: &Path) -> io::Result<bool> {
        match File::open(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn path_is_file(&self, path: &Path) -> io::Result<bool> {
        fs::metadata(path).map(|metadata| metadata.is_file())
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn tap_device_is_tap(&self, name: &str) -> io::Result<bool> {
        let flags_path = Path::new("/sys/class/net").join(name).join("tun_flags");
        let flags = match fs::read_to_string(flags_path) {
            Ok(flags) => flags,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let (flags, radix) = match flags.trim().strip_prefix("0x") {
            Some(flags) => (flags, 16),
            None => (flags.trim(), 10),
        };
        let flags = u32::from_str_radix(flags, radix).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid tun_flags: {error}"),
            )
        })?;
        // Linux UAPI `include/uapi/linux/if_tun.h` defines IFF_TAP as 0x0002.
        Ok(flags & 0x0002 != 0)
    }

    fn tun_is_accessible(&self) -> io::Result<()> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map(drop)
    }
}

/// Inputs that keep machine validation deterministic and testable.
pub struct ValidationContext<'a> {
    pub host: &'a dyn ValidationHost,
    pub machine_dir: &'a Path,
    pub catalog: &'a Catalog,
    pub base_image_virtual_size: Option<ByteSize>,
}

impl<'a> ValidationContext<'a> {
    #[must_use]
    pub const fn new(
        host: &'a dyn ValidationHost,
        machine_dir: &'a Path,
        catalog: &'a Catalog,
    ) -> Self {
        Self {
            host,
            machine_dir,
            catalog,
            base_image_virtual_size: None,
        }
    }

    #[must_use]
    pub const fn with_base_image_virtual_size(mut self, size: ByteSize) -> Self {
        self.base_image_virtual_size = Some(size);
        self
    }
}

/// Resolves user paths and checks all §7.2 rules available before VM creation.
pub fn validate_machine_spec(
    spec: &mut MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<Vec<SpecWarning>, FirestoneError> {
    resolve_spec_paths(spec, context)?;
    let mut warnings = Vec::new();

    let arch = validate_arch(spec, context)?;
    validate_image(spec, arch, context)?;
    validate_capacity(spec, context, &mut warnings)?;
    validate_user(&spec.user)?;
    validate_network(spec, context)?;
    validate_mounts(spec, context)?;
    validate_cloud_init(spec, context)?;
    validate_vmm(spec, context)?;

    Ok(warnings)
}

fn resolve_spec_paths(
    spec: &mut MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    for (index, mount) in spec.mounts.iter_mut().enumerate() {
        mount.host = resolve_input_path(&mount.host, &format!("mount[{index}].host"), context)?;
    }
    if let Some(path) = &mut spec.cloud_init.user_data {
        *path = resolve_input_path(path, "cloud_init.user_data", context)?;
    }
    if let Some(path) = &mut spec.cloud_init.network_config {
        *path = resolve_input_path(path, "cloud_init.network_config", context)?;
    }
    for (index, path) in spec.cloud_init.ssh_keys.iter_mut().enumerate() {
        *path = resolve_input_path(path, &format!("cloud_init.ssh_keys[{index}]"), context)?;
    }
    if let Some(path) = &mut spec.vmm.binary {
        *path = resolve_input_path(path, "vmm.binary", context)?;
    }
    if let Some(path) = spec.vmm.firmware.as_path() {
        let resolved = resolve_input_path(path, "vmm.firmware", context)?;
        spec.vmm.firmware = super::Firmware::path(resolved);
    }
    Ok(())
}

pub(super) fn resolve_patch_paths(
    patch: &mut super::MachineSpecPatch,
    host: &dyn ValidationHost,
    base_dir: &Path,
) -> Result<(), FirestoneError> {
    if let Some(image) = &mut patch.image {
        let candidate = Paths::resolve_input_path(
            Path::new(image.as_str()),
            host.home_dir(),
            base_dir,
            "image",
        )?;
        let exists = host_path_exists(host, "image", &candidate)?;
        if exists || looks_like_path(image.as_str()) {
            *image = image_ref_from_path(&candidate)?;
        }
    }
    if let Some(mounts) = &mut patch.mounts {
        for (index, mount) in mounts.iter_mut().enumerate() {
            mount.host = Paths::resolve_input_path(
                &mount.host,
                host.home_dir(),
                base_dir,
                &format!("mount[{index}].host"),
            )?;
        }
    }
    if let Some(cloud_init) = &mut patch.cloud_init {
        if let Some(path) = &mut cloud_init.user_data {
            *path =
                Paths::resolve_input_path(path, host.home_dir(), base_dir, "cloud_init.user_data")?;
        }
        if let Some(path) = &mut cloud_init.network_config {
            *path = Paths::resolve_input_path(
                path,
                host.home_dir(),
                base_dir,
                "cloud_init.network_config",
            )?;
        }
        if let Some(paths) = &mut cloud_init.ssh_keys {
            for (index, path) in paths.iter_mut().enumerate() {
                *path = Paths::resolve_input_path(
                    path,
                    host.home_dir(),
                    base_dir,
                    &format!("cloud_init.ssh_keys[{index}]"),
                )?;
            }
        }
    }
    if let Some(vmm) = &mut patch.vmm {
        if let Some(path) = &mut vmm.binary {
            *path = Paths::resolve_input_path(path, host.home_dir(), base_dir, "vmm.binary")?;
        }
        if let Some(firmware) = &mut vmm.firmware {
            if let Some(path) = firmware.as_path() {
                *firmware = super::Firmware::path(Paths::resolve_input_path(
                    path,
                    host.home_dir(),
                    base_dir,
                    "vmm.firmware",
                )?);
            }
        }
    }
    Ok(())
}

fn resolve_input_path(
    path: &Path,
    key: &str,
    context: &ValidationContext<'_>,
) -> Result<PathBuf, FirestoneError> {
    Paths::resolve_input_path(path, context.host.home_dir(), context.machine_dir, key)
}

fn validate_image(
    spec: &mut MachineSpec,
    arch: Arch,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    let reference = spec.image.as_str().to_owned();
    if reference.trim().is_empty() {
        return Err(invalid(
            "image",
            "image reference is empty",
            "set image to a catalog reference, an https URL, or an existing local file",
        ));
    }

    if reference.contains("://") {
        let uri = reference.parse::<http::Uri>().map_err(|error| {
            invalid_with_source(
                "image",
                format!("image URL '{reference}' is malformed"),
                "use a complete https:// URL",
                error,
            )
        })?;
        if uri.scheme_str() != Some("https") {
            return Err(invalid(
                "image",
                format!("image URL '{reference}' does not use HTTPS"),
                "use https://; insecure HTTP downloads are not supported",
            ));
        }
        if uri.host().is_none_or(str::is_empty) {
            return Err(invalid(
                "image",
                format!("image URL '{reference}' is incomplete or malformed"),
                "use a complete https:// URL",
            ));
        }
        return Ok(());
    }

    let candidate = image_path(&reference, context)?;
    if host_path_exists(context.host, "image", &candidate)? {
        require_regular_file(
            context.host,
            "image",
            &candidate,
            "choose a qcow2 or raw image file, not a directory",
        )?;
        require_readable(
            context.host,
            "image",
            &candidate,
            "make the image file readable or choose another image",
        )?;
        spec.image = image_ref_from_path(&candidate)?;
        return Ok(());
    }

    if looks_like_path(&reference) {
        return Err(invalid(
            "image",
            format!("local image '{}' does not exist", candidate.display()),
            "correct the image path or choose a catalog reference such as 'ubuntu:24.04'",
        ));
    }

    validate_catalog_reference(&reference)?;
    context
        .catalog
        .resolve(&reference, arch.as_str())
        .map(|_| ())
        .map_err(|error| {
            let message = error.message().to_owned();
            let hint = error.hint().map_or_else(
                || "choose an image listed by `firestone images ls`".to_owned(),
                str::to_owned,
            );
            invalid_with_source("image", message, hint, error)
        })
}

fn image_path(reference: &str, context: &ValidationContext<'_>) -> Result<PathBuf, FirestoneError> {
    resolve_input_path(Path::new(reference), "image", context)
}

fn looks_like_path(reference: &str) -> bool {
    Path::new(reference).is_absolute()
        || reference.starts_with('.')
        || reference.starts_with('~')
        || reference.contains('/')
        || [".qcow2", ".raw", ".img"]
            .iter()
            .any(|suffix| reference.ends_with(suffix))
}

fn image_ref_from_path(path: &Path) -> Result<super::ImageRef, FirestoneError> {
    path.to_str().map(super::ImageRef::new).ok_or_else(|| {
        invalid(
            "image",
            format!("local image path '{}' is not valid UTF-8", path.display()),
            "move the image under a UTF-8 path before adding it to firestone.toml",
        )
    })
}

fn validate_catalog_reference(reference: &str) -> Result<(), FirestoneError> {
    let mut components = reference.split(':');
    let distro = components.next().map_or("", |value| value);
    let version = components.next();
    if components.next().is_some()
        || !valid_catalog_component(distro)
        || version.is_some_and(|value| !valid_catalog_component(value))
    {
        return Err(invalid(
            "image",
            format!("image reference '{reference}' is not a valid catalog name"),
            "use 'distro', 'distro:version', an https URL, or an existing local path",
        ));
    }
    Ok(())
}

fn valid_catalog_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_arch(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<Arch, FirestoneError> {
    let host_arch = context.host.architecture()?;
    if let Some(arch) = spec.arch {
        if arch != host_arch {
            return Err(invalid(
                "arch",
                format!(
                    "guest architecture '{arch}' does not match host architecture '{host_arch}'"
                ),
                "remove 'arch' or set it to the host architecture; cross-architecture emulation is not supported",
            ));
        }
    }
    Ok(match spec.arch {
        Some(arch) => arch,
        None => host_arch,
    })
}

fn validate_capacity(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
    warnings: &mut Vec<SpecWarning>,
) -> Result<(), FirestoneError> {
    if spec.cpus == 0 {
        return Err(invalid(
            "cpus",
            "CPU count must be at least 1",
            "set cpus to 1 or more",
        ));
    }
    let host_cpus = context.host.cpu_count();
    if usize::from(spec.cpus) > host_cpus {
        warnings.push(SpecWarning {
            key: "cpus",
            message: format!(
                "configured {} vCPUs but the host reports {host_cpus} logical CPUs",
                spec.cpus
            ),
        });
    }
    if spec.memory < ByteSize::from_mib(128) {
        return Err(invalid(
            "memory",
            format!("memory {} is below the 128M minimum", spec.memory),
            "set memory to at least '128M'",
        ));
    }
    if let Some(base_size) = context.base_image_virtual_size {
        if spec.disk < base_size {
            return Err(invalid(
                "disk",
                format!(
                    "disk {} is smaller than the base image virtual size {base_size}",
                    spec.disk
                ),
                format!("set disk to at least '{base_size}'"),
            ));
        }
    }
    Ok(())
}

fn validate_user(user: &str) -> Result<(), FirestoneError> {
    let mut bytes = user.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_');
    let valid_rest = bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    });
    if !valid_first || !valid_rest {
        return Err(invalid(
            "user",
            format!("login user '{user}' is invalid"),
            "use a name matching [a-z_][a-z0-9_-]*",
        ));
    }
    Ok(())
}

fn validate_network(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    if spec.network.mode != NetMode::Tap {
        return Ok(());
    }

    let tap = spec.network.tap.as_deref().ok_or_else(|| {
        invalid(
            "network.tap",
            "tap mode requires an existing tap interface",
            "set network.tap to a user-owned interface such as 'tap0'",
        )
    })?;
    if tap.is_empty() || tap.contains('/') || matches!(tap, "." | "..") {
        return Err(invalid(
            "network.tap",
            format!("tap interface name '{tap}' is invalid"),
            "use an interface name such as 'tap0', without path separators",
        ));
    }
    let exists = context.host.tap_device_is_tap(tap).map_err(|error| {
        dependency_with_source(
            "network.tap",
            format!("cannot inspect TAP interface '{tap}'"),
            "check that /sys/class/net is mounted and readable",
            error,
        )
    })?;
    if !exists {
        return Err(invalid(
            "network.tap",
            format!("interface '{tap}' does not exist or is not a TAP device"),
            format!("create a user-owned tap named '{tap}' or choose network.mode = 'passt'"),
        ));
    }
    context.host.tun_is_accessible().map_err(|error| {
        dependency_with_source(
            "network.tap",
            "/dev/net/tun is not accessible for tap mode",
            "grant this user read/write access to /dev/net/tun or choose network.mode = 'passt'",
            error,
        )
    })
}

fn validate_mounts(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    let mut tags = HashSet::new();
    for (index, mount) in spec.mounts.iter().enumerate() {
        let host_key = format!("mount[{index}].host");
        let guest_key = format!("mount[{index}].guest");
        let tag_key = format!("mount[{index}].tag");
        if !host_path_exists(context.host, &host_key, &mount.host)? {
            return Err(invalid(
                &host_key,
                format!("mount source '{}' does not exist", mount.host.display()),
                "correct the host path or create it before starting the machine",
            ));
        }
        if !mount.guest.is_absolute() {
            return Err(invalid(
                &guest_key,
                format!(
                    "guest mount path '{}' is not absolute",
                    mount.guest.display()
                ),
                "use an absolute guest path such as '/work'",
            ));
        }
        let tag = mount.effective_tag(index);
        if !tags.insert(tag.clone()) {
            return Err(invalid(
                &tag_key,
                format!("mount tag '{tag}' is used more than once"),
                "set a unique tag for each mount or omit tags to use share0, share1, and so on",
            ));
        }
    }
    Ok(())
}

fn validate_cloud_init(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    if let Some(path) = &spec.cloud_init.user_data {
        let contents = read_required_file(
            context.host,
            "cloud_init.user_data",
            path,
            "correct the path and make the file readable",
        )?;
        let first_line = match contents.split(|byte| *byte == b'\n').next() {
            Some(line) => match line.strip_suffix(b"\r") {
                Some(stripped) => stripped,
                None => line,
            },
            None => &[],
        };
        if first_line != b"#cloud-config" && !first_line.starts_with(b"#!") {
            return Err(invalid(
                "cloud_init.user_data",
                format!(
                    "user-data file '{}' has an unsupported first line",
                    path.display()
                ),
                "start the file with '#cloud-config' or '#!'; set provisioning = false when using a raw shell script",
            ));
        }
    }

    if let Some(path) = &spec.cloud_init.network_config {
        read_required_file(
            context.host,
            "cloud_init.network_config",
            path,
            "correct the path and make the file readable",
        )?;
    }

    for (index, path) in spec.cloud_init.ssh_keys.iter().enumerate() {
        let key = format!("cloud_init.ssh_keys[{index}]");
        let contents = read_required_file(
            context.host,
            &key,
            path,
            "correct the path and provide an OpenSSH public-key file",
        )?;
        let text = std::str::from_utf8(&contents).map_err(|error| {
            invalid_with_source(
                &key,
                format!("public-key file '{}' is not UTF-8", path.display()),
                "replace it with an OpenSSH public-key file",
                error,
            )
        })?;
        let mut parsed = 0_usize;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            PublicKey::from_openssh(line).map_err(|error| {
                invalid_with_source(
                    &key,
                    format!(
                        "public-key file '{}' contains an invalid OpenSSH key",
                        path.display()
                    ),
                    "replace the invalid line with an OpenSSH public key",
                    error,
                )
            })?;
            parsed += 1;
        }
        if parsed == 0 {
            return Err(invalid(
                &key,
                format!("public-key file '{}' contains no keys", path.display()),
                "add at least one OpenSSH public key",
            ));
        }
    }
    Ok(())
}

fn validate_vmm(spec: &MachineSpec, context: &ValidationContext<'_>) -> Result<(), FirestoneError> {
    if let Some(path) = &spec.vmm.binary {
        if !host_path_exists(context.host, "vmm.binary", path)? {
            return Err(invalid(
                "vmm.binary",
                format!("VMM binary '{}' does not exist", path.display()),
                "correct the path or remove vmm.binary to use the vendored VMM",
            ));
        }
        require_regular_file(
            context.host,
            "vmm.binary",
            path,
            "set vmm.binary to an executable file or remove the override",
        )?;
        require_readable(
            context.host,
            "vmm.binary",
            path,
            "make the VMM binary readable or remove the override",
        )?;
    }
    if let Some(path) = spec.vmm.firmware.as_path() {
        if !host_path_exists(context.host, "vmm.firmware", path)? {
            return Err(invalid(
                "vmm.firmware",
                format!("firmware path '{}' does not exist", path.display()),
                "correct the path or choose 'auto', 'rhf', or 'edk2'",
            ));
        }
        require_regular_file(
            context.host,
            "vmm.firmware",
            path,
            "set vmm.firmware to a firmware file or choose a named firmware",
        )?;
        require_readable(
            context.host,
            "vmm.firmware",
            path,
            "make the firmware file readable or choose a named firmware",
        )?;
    }
    Ok(())
}

fn host_path_exists(
    host: &dyn ValidationHost,
    key: &str,
    path: &Path,
) -> Result<bool, FirestoneError> {
    host.path_exists(path).map_err(|error| {
        invalid_with_source(
            key,
            format!("cannot inspect '{}'", path.display()),
            "check the path and its parent-directory permissions",
            error,
        )
    })
}

fn require_readable(
    host: &dyn ValidationHost,
    key: &str,
    path: &Path,
    hint: impl Into<String>,
) -> Result<(), FirestoneError> {
    match host.path_is_readable(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(invalid(
            key,
            format!("'{}' is not readable", path.display()),
            hint,
        )),
        Err(error) => Err(invalid_with_source(
            key,
            format!("cannot open '{}'", path.display()),
            hint,
            error,
        )),
    }
}

fn require_regular_file(
    host: &dyn ValidationHost,
    key: &str,
    path: &Path,
    hint: impl Into<String>,
) -> Result<(), FirestoneError> {
    match host.path_is_file(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(invalid(
            key,
            format!("'{}' is not a regular file", path.display()),
            hint,
        )),
        Err(error) => Err(invalid_with_source(
            key,
            format!("cannot inspect file type for '{}'", path.display()),
            hint,
            error,
        )),
    }
}

fn read_required_file(
    host: &dyn ValidationHost,
    key: &str,
    path: &Path,
    hint: impl Into<String>,
) -> Result<Vec<u8>, FirestoneError> {
    let hint = hint.into();
    if !host_path_exists(host, key, path)? {
        return Err(invalid(
            key,
            format!("file '{}' does not exist", path.display()),
            hint,
        ));
    }
    host.read_file(path).map_err(|error| {
        invalid_with_source(
            key,
            format!("cannot read '{}'", path.display()),
            hint,
            error,
        )
    })
}

fn invalid(key: &str, message: impl Into<String>, hint: impl Into<String>) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("invalid '{key}': {}", message.into()),
    )
    .with_hint(hint)
}

fn invalid_with_source(
    key: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FirestoneError {
    invalid(key, message, hint).with_source(source)
}

fn dependency_with_source(
    key: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot validate '{key}': {}", message.into()),
    )
    .with_hint(hint)
    .with_source(source)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        io,
        path::{Path, PathBuf},
    };

    use super::{SpecWarning, ValidationContext, ValidationHost, validate_machine_spec};
    use crate::{
        Arch, ByteSize, Catalog, CloudInitSpecPatch, ErrorKind, Firmware, MachineSpec,
        MachineSpecPatch, MountSpec, NetMode, VmmSpecPatch,
    };

    struct FakeHost {
        arch: Arch,
        cpus: usize,
        home: PathBuf,
        existing: HashSet<PathBuf>,
        readable: HashSet<PathBuf>,
        files: HashMap<PathBuf, Vec<u8>>,
        taps: HashSet<String>,
        tun_error: Option<io::ErrorKind>,
        catalog: Catalog,
    }

    impl Default for FakeHost {
        fn default() -> Self {
            Self {
                arch: Arch::X86_64,
                cpus: 8,
                home: PathBuf::from("/home/test"),
                existing: HashSet::new(),
                readable: HashSet::new(),
                files: HashMap::new(),
                taps: HashSet::new(),
                tun_error: None,
                catalog: Catalog::built_in().expect("valid built-in catalog"),
            }
        }
    }

    impl FakeHost {
        fn add_file(&mut self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) {
            let path = path.into();
            self.existing.insert(path.clone());
            self.readable.insert(path.clone());
            self.files.insert(path, contents.into());
        }

        fn context(&self) -> ValidationContext<'_> {
            ValidationContext::new(self, Path::new("/machines/dev"), &self.catalog)
        }
    }

    impl ValidationHost for FakeHost {
        fn architecture(&self) -> Result<Arch, crate::FirestoneError> {
            Ok(self.arch)
        }

        fn cpu_count(&self) -> usize {
            self.cpus
        }

        fn home_dir(&self) -> Option<&Path> {
            Some(&self.home)
        }

        fn path_exists(&self, path: &Path) -> io::Result<bool> {
            Ok(self.existing.contains(path) || self.files.contains_key(path))
        }

        fn path_is_readable(&self, path: &Path) -> io::Result<bool> {
            Ok(self.readable.contains(path))
        }

        fn path_is_file(&self, path: &Path) -> io::Result<bool> {
            Ok(self.files.contains_key(path))
        }

        fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn tap_device_is_tap(&self, name: &str) -> io::Result<bool> {
            Ok(self.taps.contains(name))
        }

        fn tun_is_accessible(&self) -> io::Result<()> {
            match self.tun_error {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn default_spec_supported_host_returns_no_warnings() -> Result<(), crate::FirestoneError> {
        let host = FakeHost::default();
        let mut spec = MachineSpec::default();
        assert!(validate_machine_spec(&mut spec, &host.context())?.is_empty());
        Ok(())
    }

    #[test]
    fn patch_paths_cli_base_resolves_before_layering() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        host.add_file("/work/base.qcow2", Vec::new());
        let mut patch = MachineSpecPatch {
            image: Some("base.qcow2".into()),
            mounts: Some(vec![MountSpec {
                host: PathBuf::from("./src"),
                guest: PathBuf::from("/src"),
                readonly: false,
                tag: None,
            }]),
            cloud_init: Some(CloudInitSpecPatch {
                user_data: Some(PathBuf::from("user-data.yaml")),
                ssh_keys: Some(vec![PathBuf::from("~/.ssh/id.pub")]),
                ..CloudInitSpecPatch::default()
            }),
            vmm: Some(VmmSpecPatch {
                binary: Some(PathBuf::from("bin/cloud-hypervisor")),
                ..VmmSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };

        patch.resolve_paths(&host, Path::new("/work"))?;

        assert_eq!(
            patch.image.as_ref().map(crate::ImageRef::as_str),
            Some("/work/base.qcow2")
        );
        assert_eq!(
            patch
                .mounts
                .as_ref()
                .and_then(|mounts| mounts.first())
                .map(|mount| mount.host.as_path()),
            Some(Path::new("/work/src"))
        );
        assert_eq!(
            patch
                .cloud_init
                .as_ref()
                .and_then(|cloud_init| cloud_init.user_data.as_deref()),
            Some(Path::new("/work/user-data.yaml"))
        );
        assert_eq!(
            patch
                .cloud_init
                .as_ref()
                .and_then(|cloud_init| cloud_init.ssh_keys.as_ref())
                .and_then(|keys| keys.first())
                .map(PathBuf::as_path),
            Some(Path::new("/home/test/.ssh/id.pub"))
        );
        assert_eq!(
            patch.vmm.as_ref().and_then(|vmm| vmm.binary.as_deref()),
            Some(Path::new("/work/bin/cloud-hypervisor"))
        );
        Ok(())
    }

    #[test]
    fn arch_different_from_host_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            arch: Some(Arch::Aarch64),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("arch mismatch");
        assert_invalid_key(&error, "arch");
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("cross-architecture"))
        );
    }

    #[test]
    fn cpus_zero_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            cpus: 0,
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("zero cpus");
        assert_invalid_key(&error, "cpus");
    }

    #[test]
    fn cpus_above_host_returns_warning() -> Result<(), crate::FirestoneError> {
        let host = FakeHost {
            cpus: 2,
            ..FakeHost::default()
        };
        let mut spec = MachineSpec {
            cpus: 3,
            ..MachineSpec::default()
        };
        assert_eq!(
            validate_machine_spec(&mut spec, &host.context())?,
            [SpecWarning {
                key: "cpus",
                message: "configured 3 vCPUs but the host reports 2 logical CPUs".to_owned(),
            }]
        );
        Ok(())
    }

    #[test]
    fn memory_below_minimum_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            memory: ByteSize::from_mib(127),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("low memory");
        assert_invalid_key(&error, "memory");
    }

    #[test]
    fn disk_below_known_base_size_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            disk: ByteSize::from_gib(4),
            ..MachineSpec::default()
        };
        let context = host
            .context()
            .with_base_image_virtual_size(ByteSize::from_gib(5));
        let error = validate_machine_spec(&mut spec, &context).expect_err("small disk");
        assert_invalid_key(&error, "disk");
    }

    #[test]
    fn user_outside_documented_grammar_returns_keyed_error() {
        let host = FakeHost::default();
        for user in ["", "Root", "1user", "user.name"] {
            let mut spec = MachineSpec {
                user: user.to_owned(),
                ..MachineSpec::default()
            };
            let error =
                validate_machine_spec(&mut spec, &host.context()).expect_err("invalid user");
            assert_invalid_key(&error, "user");
        }
    }

    #[test]
    fn image_http_url_returns_https_hint() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "http://example.com/image.qcow2".into(),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("insecure URL");
        assert_invalid_key(&error, "image");
        assert!(error.hint().is_some_and(|hint| hint.contains("https://")));
    }

    #[test]
    fn image_missing_local_path_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "./base.qcow2".into(),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("missing image");
        assert_invalid_key(&error, "image");
        assert!(error.message().contains("/machines/dev/base.qcow2"));
    }

    #[test]
    fn image_unreadable_local_path_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.files
            .insert(PathBuf::from("/machines/dev/base.qcow2"), Vec::new());
        let mut spec = MachineSpec {
            image: "base.qcow2".into(),
            ..MachineSpec::default()
        };
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("unreadable image");
        assert_invalid_key(&error, "image");
        assert!(error.message().contains("not readable"));
    }

    #[test]
    fn image_readable_relative_path_is_persisted_absolute() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        host.add_file("/machines/dev/base.qcow2", Vec::new());
        let mut spec = MachineSpec {
            image: "./base.qcow2".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(&mut spec, &host.context())?;
        assert_eq!(spec.image.as_str(), "/machines/dev/base.qcow2");
        Ok(())
    }

    #[test]
    fn image_malformed_https_url_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "https:///image.qcow2".into(),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("malformed URL");
        assert_invalid_key(&error, "image");
    }

    #[test]
    fn image_unknown_catalog_reference_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "ubunut:24.04".into(),
            ..MachineSpec::default()
        };
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("unknown catalog image");
        assert_invalid_key(&error, "image");
        assert!(error.message().contains("closest catalog images"));
        assert!(error.hint().is_some_and(|hint| hint.contains("images ls")));
    }

    #[test]
    fn tap_mode_missing_name_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec::default();
        spec.network.mode = NetMode::Tap;
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("missing tap");
        assert_invalid_key(&error, "network.tap");
    }

    #[test]
    fn tap_mode_missing_interface_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec::default();
        spec.network.mode = NetMode::Tap;
        spec.network.tap = Some("tap0".to_owned());
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("missing tap");
        assert_invalid_key(&error, "network.tap");
    }

    #[test]
    fn tap_mode_inaccessible_tun_returns_dependency_error() {
        let mut host = FakeHost {
            tun_error: Some(io::ErrorKind::PermissionDenied),
            ..FakeHost::default()
        };
        host.taps.insert("tap0".to_owned());
        let mut spec = MachineSpec::default();
        spec.network.mode = NetMode::Tap;
        spec.network.tap = Some("tap0".to_owned());
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("inaccessible tun");
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("network.tap"));
    }

    #[test]
    fn tap_mode_existing_interface_and_tun_is_accepted() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        host.taps.insert("tap0".to_owned());
        let mut spec = MachineSpec::default();
        spec.network.mode = NetMode::Tap;
        spec.network.tap = Some("tap0".to_owned());
        validate_machine_spec(&mut spec, &host.context())?;
        Ok(())
    }

    #[test]
    fn mount_tilde_source_expands_and_validates() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        host.existing.insert(PathBuf::from("/home/test/code"));
        let mut spec = MachineSpec::default();
        spec.mounts.push(MountSpec {
            host: PathBuf::from("~/code"),
            guest: PathBuf::from("/work"),
            readonly: false,
            tag: None,
        });
        validate_machine_spec(&mut spec, &host.context())?;
        assert_eq!(spec.mounts[0].host, PathBuf::from("/home/test/code"));
        Ok(())
    }

    #[test]
    fn mount_relative_guest_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.existing.insert(PathBuf::from("/machines/dev/code"));
        let mut spec = MachineSpec::default();
        spec.mounts.push(MountSpec {
            host: PathBuf::from("code"),
            guest: PathBuf::from("work"),
            readonly: false,
            tag: None,
        });
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("relative guest");
        assert_invalid_key(&error, "mount[0].guest");
    }

    #[test]
    fn mount_missing_host_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            mounts: vec![MountSpec {
                host: PathBuf::from("missing"),
                guest: PathBuf::from("/work"),
                readonly: false,
                tag: None,
            }],
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("missing mount");
        assert_invalid_key(&error, "mount[0].host");
    }

    #[test]
    fn mount_duplicate_effective_tag_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.existing.extend([
            PathBuf::from("/machines/dev/a"),
            PathBuf::from("/machines/dev/b"),
        ]);
        let mut spec = MachineSpec {
            mounts: vec![
                MountSpec {
                    host: PathBuf::from("a"),
                    guest: PathBuf::from("/a"),
                    readonly: false,
                    tag: Some("same".to_owned()),
                },
                MountSpec {
                    host: PathBuf::from("b"),
                    guest: PathBuf::from("/b"),
                    readonly: false,
                    tag: Some("same".to_owned()),
                },
            ],
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("duplicate tags");
        assert_invalid_key(&error, "mount[1].tag");
    }

    #[test]
    fn cloud_init_user_data_wrong_header_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.add_file("/machines/dev/user-data", b"hostname: dev\n".to_vec());
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("user-data"));
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("bad header");
        assert_invalid_key(&error, "cloud_init.user_data");
    }

    #[test]
    fn cloud_init_shell_script_header_is_accepted() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        host.add_file("/machines/dev/user-data", b"#!/bin/sh\necho ok\n".to_vec());
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("user-data"));
        validate_machine_spec(&mut spec, &host.context())?;
        Ok(())
    }

    #[test]
    fn cloud_init_missing_network_config_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec::default();
        spec.cloud_init.network_config = Some(PathBuf::from("network-config.yaml"));
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("missing network config");
        assert_invalid_key(&error, "cloud_init.network_config");
    }

    #[test]
    fn cloud_init_invalid_public_key_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.add_file(
            "/home/test/.ssh/id.pub",
            b"ssh-ed25519 not-base64 user@test\n".to_vec(),
        );
        let mut spec = MachineSpec::default();
        spec.cloud_init
            .ssh_keys
            .push(PathBuf::from("~/.ssh/id.pub"));
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("invalid key");
        assert_invalid_key(&error, "cloud_init.ssh_keys[0]");
    }

    #[test]
    fn cloud_init_valid_public_key_is_accepted() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        host.add_file(
            "/home/test/.ssh/id.pub",
            b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P test@example\n".to_vec(),
        );
        let mut spec = MachineSpec::default();
        spec.cloud_init
            .ssh_keys
            .push(PathBuf::from("~/.ssh/id.pub"));
        validate_machine_spec(&mut spec, &host.context())?;
        Ok(())
    }

    #[test]
    fn firmware_missing_path_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            vmm: crate::VmmSpec {
                firmware: Firmware::path("firmware.fd"),
                ..crate::VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("missing firmware");
        assert_invalid_key(&error, "vmm.firmware");
        assert!(error.message().contains("/machines/dev/firmware.fd"));
    }

    #[test]
    fn firmware_empty_path_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            vmm: crate::VmmSpec {
                firmware: Firmware::path(""),
                ..crate::VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("empty firmware");
        assert_invalid_key(&error, "vmm.firmware");
    }

    #[test]
    fn firmware_directory_path_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.existing
            .insert(PathBuf::from("/machines/dev/firmware"));
        host.readable
            .insert(PathBuf::from("/machines/dev/firmware"));
        let mut spec = MachineSpec {
            vmm: crate::VmmSpec {
                firmware: Firmware::path("firmware"),
                ..crate::VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("firmware directory");
        assert_invalid_key(&error, "vmm.firmware");
        assert!(error.message().contains("not a regular file"));
    }

    fn assert_invalid_key(error: &crate::FirestoneError, key: &str) {
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error.message().contains(&format!("'{key}'")),
            "{}",
            error.message()
        );
        assert!(error.hint().is_some());
    }
}
