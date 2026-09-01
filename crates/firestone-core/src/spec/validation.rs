use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use ssh_key::PublicKey;

use super::{Arch, ByteSize, MachineSpec, NetMode};
use crate::{
    Catalog, ErrorKind, FirestoneError, Paths,
    bounded::{self, BoundedReadError},
    cloudinit::{
        MAX_INLINE_USER_DATA_BYTES, MAX_NETWORK_CONFIG_BYTES, MAX_PASSWORD_BYTES,
        MAX_SSH_KEY_FILE_BYTES, MAX_USER_DATA_BYTES,
    },
};

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
    fn path_exists(&self, path: &Path) -> io::Result<bool>;
    fn path_is_readable(&self, path: &Path) -> io::Result<bool>;
    fn path_is_file(&self, path: &Path) -> io::Result<bool>;
    fn canonicalize_regular_nofollow(&self, path: &Path) -> io::Result<PathBuf>;
    fn path_is_executable(&self, path: &Path) -> io::Result<bool>;
    fn read_regular_file(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>>;
    fn tap_device_is_tap(&self, name: &str) -> io::Result<bool>;
    fn tun_is_accessible(&self) -> io::Result<()>;
}

/// Real host checks. Tap validation only inspects sysfs and opens `/dev/net/tun`.
/// It never opens, creates, or configures the named tap interface.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealValidationHost;

impl RealValidationHost {
    #[must_use]
    pub const fn new() -> Self {
        Self
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

    fn canonicalize_regular_nofollow(&self, path: &Path) -> io::Result<PathBuf> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not a non-symlink regular file",
            ));
        }
        fs::canonicalize(path)
    }

    fn path_is_executable(&self, path: &Path) -> io::Result<bool> {
        match nix::unistd::access(path, nix::unistd::AccessFlags::X_OK) {
            Ok(()) => Ok(true),
            Err(nix::errno::Errno::EACCES | nix::errno::Errno::EPERM) => Ok(false),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
    fn read_regular_file(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.custom_flags(
                nix::fcntl::OFlag::O_NONBLOCK.bits() | nix::fcntl::OFlag::O_CLOEXEC.bits(),
            );
        }

        let mut file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{}' is not a regular file", path.display()),
            ));
        }
        bounded::read_to_end(&mut file, limit).map_err(|error| match error {
            BoundedReadError::Io(error) => error,
            BoundedReadError::LimitExceeded => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("'{}' exceeds the read limit", path.display()),
            ),
        })
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
    pub paths: &'a Paths,
    pub machine_dir: &'a Path,
    pub catalog: &'a Catalog,
    pub base_image_virtual_size: Option<ByteSize>,
    pub pinned_image_ref: Option<&'a str>,
}

impl<'a> ValidationContext<'a> {
    #[must_use]
    pub const fn new(
        host: &'a dyn ValidationHost,
        paths: &'a Paths,
        machine_dir: &'a Path,
        catalog: &'a Catalog,
    ) -> Self {
        Self {
            host,
            paths,
            machine_dir,
            catalog,
            base_image_virtual_size: None,
            pinned_image_ref: None,
        }
    }

    #[must_use]
    pub const fn with_base_image_virtual_size(mut self, size: ByteSize) -> Self {
        self.base_image_virtual_size = Some(size);
        self
    }

    #[must_use]
    pub const fn with_pinned_image_ref(mut self, reference: &'a str) -> Self {
        self.pinned_image_ref = Some(reference);
        self
    }
}

/// Resolves user paths and checks all §7.2 rules available before VM creation.
pub fn validate_machine_spec(
    spec: &mut MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<Vec<SpecWarning>, FirestoneError> {
    validate_machine_spec_with_image_base(spec, context, context.machine_dir)
}

pub(super) fn validate_machine_spec_with_image_base(
    spec: &mut MachineSpec,
    context: &ValidationContext<'_>,
    image_base_dir: &Path,
) -> Result<Vec<SpecWarning>, FirestoneError> {
    resolve_spec_paths(spec, context)?;
    let mut warnings = Vec::new();

    let arch = validate_arch(spec, context)?;
    validate_image(spec, arch, context, image_base_dir)?;
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
        spec.vmm.firmware = super::Firmware::path(resolved).map_err(|error| {
            invalid_with_source(
                "vmm.firmware",
                "firmware path is empty",
                "set a non-empty firmware path or choose 'auto', 'rhf', or 'edk2'",
                error,
            )
        })?;
    }
    Ok(())
}

pub(super) fn resolve_patch_paths(
    patch: &mut super::MachineSpecPatch,
    paths: &Paths,
    base_dir: &Path,
) -> Result<(), FirestoneError> {
    if let Some(mounts) = &mut patch.mounts {
        for (index, mount) in mounts.iter_mut().enumerate() {
            mount.host =
                paths.resolve_input_path(&mount.host, base_dir, &format!("mount[{index}].host"))?;
        }
    }
    if let Some(cloud_init) = &mut patch.cloud_init {
        if let Some(path) = &mut cloud_init.user_data {
            *path = paths.resolve_input_path(path, base_dir, "cloud_init.user_data")?;
        }
        if let Some(path) = &mut cloud_init.network_config {
            *path = paths.resolve_input_path(path, base_dir, "cloud_init.network_config")?;
        }
        if let Some(key_paths) = &mut cloud_init.ssh_keys {
            for (index, path) in key_paths.iter_mut().enumerate() {
                *path = paths.resolve_input_path(
                    path,
                    base_dir,
                    &format!("cloud_init.ssh_keys[{index}]"),
                )?;
            }
        }
    }
    if let Some(vmm) = &mut patch.vmm {
        if let Some(path) = &mut vmm.binary {
            *path = paths.resolve_input_path(path, base_dir, "vmm.binary")?;
        }
        if let Some(firmware) = &mut vmm.firmware {
            if let Some(path) = firmware.as_path() {
                *firmware = super::Firmware::path(paths.resolve_input_path(
                    path,
                    base_dir,
                    "vmm.firmware",
                )?)
                .map_err(|error| {
                    invalid_with_source(
                        "vmm.firmware",
                        "firmware path is empty",
                        "set a non-empty firmware path or choose 'auto', 'rhf', or 'edk2'",
                        error,
                    )
                })?;
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
    context
        .paths
        .resolve_input_path(path, context.machine_dir, key)
}

fn validate_image(
    spec: &mut MachineSpec,
    arch: Arch,
    context: &ValidationContext<'_>,
    image_base_dir: &Path,
) -> Result<(), FirestoneError> {
    let reference = spec.image.as_str().to_owned();
    if reference.trim().is_empty() {
        return Err(invalid(
            "image",
            "image reference is empty",
            "set image to a catalog reference, an https URL, or an existing local file",
        ));
    }

    if context.pinned_image_ref == Some(reference.as_str()) {
        return Ok(());
    }
    let candidate = image_path(&reference, context, image_base_dir)?;
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
        let canonical = context
            .host
            .canonicalize_regular_nofollow(&candidate)
            .map_err(|source| {
                invalid_with_source(
                    "image",
                    format!("cannot canonicalize local image '{}'", candidate.display()),
                    "use a readable non-symlink regular image file",
                    source,
                )
            })?;
        spec.image = image_ref_from_path(&canonical)?;
        return Ok(());
    }

    // §8.5. This precedes the `looks_like_url` shape check because that check
    // claims every `scheme://…` string, `docker://` included. The order is
    // still equivalent to §8.2 for URLs: no `scheme://…` reference parses as an
    // OCI reference, so `https://…` and `http://…` fall through unchanged.
    if let Some(classification) = crate::oci::classify(&reference) {
        match crate::oci::OciReference::parse(&reference) {
            Ok(parsed) => {
                spec.image = super::ImageRef::new(parsed.to_string());
                return Ok(());
            }
            Err(error) if classification.is_explicit() => {
                let message = error.message().to_owned();
                let hint = error.hint().map_or_else(
                    || "write an OCI reference such as 'docker://nginx:latest'".to_owned(),
                    str::to_owned,
                );
                return Err(invalid_with_source("image", message, hint, error));
            }
            Err(_) => {}
        }
    }

    if looks_like_url(&reference) {
        return validate_https_url(&reference);
    }

    let catalog_match = context.catalog.contains_reference(&reference);
    match context.catalog.resolve(&reference, arch.as_str()) {
        Ok(_) => Ok(()),
        Err(error) if catalog_match => {
            let message = error.message().to_owned();
            let hint = error.hint().map_or_else(
                || "choose an image available for this host architecture".to_owned(),
                str::to_owned,
            );
            Err(invalid_with_source("image", message, hint, error))
        }
        Err(error) if looks_like_path(&reference) => Err(invalid_with_source(
            "image",
            format!("local image '{}' does not exist", candidate.display()),
            "correct the image path or choose a catalog reference such as 'ubuntu:24.04'",
            error,
        )),
        Err(error) => {
            let message = error.message().to_owned();
            let hint = error.hint().map_or_else(
                || "choose an image listed by `firestone images ls`".to_owned(),
                str::to_owned,
            );
            Err(invalid_with_source("image", message, hint, error))
        }
    }
}

fn image_path(
    reference: &str,
    context: &ValidationContext<'_>,
    base_dir: &Path,
) -> Result<PathBuf, FirestoneError> {
    context
        .paths
        .resolve_input_path(Path::new(reference), base_dir, "image")
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

fn looks_like_url(reference: &str) -> bool {
    reference.contains("://")
}

fn validate_https_url(reference: &str) -> Result<(), FirestoneError> {
    if reference.chars().any(char::is_whitespace) {
        return Err(invalid(
            "image",
            format!("image URL '{reference}' is malformed"),
            "use a complete https:// URL without whitespace",
        ));
    }

    let syntax_violation = std::cell::Cell::new(false);
    let record_violation = |_| syntax_violation.set(true);
    let parsed = url::Url::options()
        .syntax_violation_callback(Some(&record_violation))
        .parse(reference)
        .map_err(|error| {
            invalid_with_source(
                "image",
                format!("image URL '{reference}' is malformed"),
                "use a complete https:// URL",
                error,
            )
        })?;

    if parsed.scheme() != "https" {
        return Err(invalid(
            "image",
            format!("image URL '{reference}' does not use HTTPS"),
            "use https://; insecure downloads are not supported",
        ));
    }

    if syntax_violation.get()
        || !parsed.has_host()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.authority().contains('@')
        || parsed.fragment().is_some()
    {
        return Err(invalid(
            "image",
            format!("image URL '{reference}' is incomplete or malformed"),
            "use a complete https:// URL with a host and without credentials or a fragment",
        ));
    }

    Ok(())
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
    if let Some(cpus_max) = spec.cpus_max {
        if cpus_max < spec.cpus {
            return Err(invalid(
                "cpus_max",
                format!("cpus_max {cpus_max} is below the boot cpus {}", spec.cpus),
                format!("set cpus_max to {} or more, or remove it", spec.cpus),
            ));
        }
    }
    if spec.memory < ByteSize::MINIMUM_MEMORY {
        return Err(invalid(
            "memory",
            format!("memory {} is below the 128M minimum", spec.memory),
            "set memory to at least '128M'",
        ));
    }
    if let Some(memory_max) = spec.memory_max {
        if memory_max < spec.memory {
            return Err(invalid(
                "memory_max",
                format!(
                    "memory_max {memory_max} is below the boot memory {}",
                    spec.memory
                ),
                format!("set memory_max to at least '{}', or remove it", spec.memory),
            ));
        }
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

pub(super) fn validate_user(user: &str) -> Result<(), FirestoneError> {
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
    crate::network::validate_tap(tap, context.host)
}

fn validate_mounts(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    crate::virtiofs::validate_mount_spec_layout(&spec.mounts)?;
    for (index, mount) in spec.mounts.iter().enumerate() {
        let host_key = format!("mount[{index}].host");
        if !host_path_exists(context.host, &host_key, &mount.host)? {
            return Err(invalid(
                &host_key,
                format!("mount source '{}' does not exist", mount.host.display()),
                "correct the host path or create it before starting the machine",
            ));
        }
    }
    Ok(())
}

fn validate_cloud_init(
    spec: &MachineSpec,
    context: &ValidationContext<'_>,
) -> Result<(), FirestoneError> {
    if spec.cloud_init.user_data.is_some() && spec.cloud_init.user_data_inline.is_some() {
        return Err(invalid(
            "cloud_init.user_data_inline",
            "'cloud_init.user_data' and 'cloud_init.user_data_inline' are both set",
            "keep one user part: clear 'cloud_init.user_data' or 'cloud_init.user_data_inline'",
        ));
    }

    if let Some(inline) = &spec.cloud_init.user_data_inline {
        validate_inline_user_data(inline)?;
    }

    for (index, key) in spec.cloud_init.ssh_authorized_keys.iter().enumerate() {
        validate_inline_authorized_key(index, key)?;
    }

    if let Some(password) = &spec.cloud_init.password {
        validate_password(password)?;
    }

    if let Some(path) = &spec.cloud_init.user_data {
        let contents = read_required_file(
            context.host,
            "cloud_init.user_data",
            path,
            MAX_USER_DATA_BYTES,
            "1 MiB",
            "correct the path and provide UTF-8 user-data of 1 MiB or less",
        )?;
        std::str::from_utf8(&contents).map_err(|error| {
            invalid_with_source(
                "cloud_init.user_data",
                format!("user-data file '{}' is not UTF-8", path.display()),
                "save user-data as UTF-8 without changing its cloud-init header",
                error,
            )
        })?;
        let first_line = contents
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        let first_line = first_line.strip_suffix(b"\r").unwrap_or(first_line);
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
        let contents = read_required_file(
            context.host,
            "cloud_init.network_config",
            path,
            MAX_NETWORK_CONFIG_BYTES,
            "1 MiB",
            "correct the path and provide UTF-8 network-config of 1 MiB or less",
        )?;
        std::str::from_utf8(&contents).map_err(|error| {
            invalid_with_source(
                "cloud_init.network_config",
                format!("network-config file '{}' is not UTF-8", path.display()),
                "save network-config as UTF-8 YAML",
                error,
            )
        })?;
    }

    for (index, path) in spec.cloud_init.ssh_keys.iter().enumerate() {
        let key = format!("cloud_init.ssh_keys[{index}]");
        let contents = read_required_file(
            context.host,
            &key,
            path,
            MAX_SSH_KEY_FILE_BYTES,
            "64 KiB",
            "correct the path and provide an OpenSSH public-key file of 64 KiB or less",
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

/// Applies the §7.2 rules for inline user-data supplied in the spec itself.
fn validate_inline_user_data(inline: &str) -> Result<(), FirestoneError> {
    const KEY: &str = "cloud_init.user_data_inline";
    if inline.len() as u64 > MAX_INLINE_USER_DATA_BYTES {
        return Err(invalid(
            KEY,
            format!("inline user-data is {} bytes", inline.len()),
            "reduce inline user-data to 32 KiB or move it to a 'cloud_init.user_data' file",
        ));
    }
    let first_line = inline.split('\n').next().unwrap_or_default();
    let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
    if first_line != "#cloud-config" && !first_line.starts_with("#!") {
        return Err(invalid(
            KEY,
            "inline user-data has an unsupported first line",
            "start inline user-data with '#cloud-config' or '#!'; set provisioning = false when using a raw shell script",
        ));
    }
    Ok(())
}

/// Validates one inline OpenSSH public key exactly like a file-loaded key.
fn validate_inline_authorized_key(index: usize, key: &str) -> Result<(), FirestoneError> {
    let field = format!("cloud_init.ssh_authorized_keys[{index}]");
    let line = key.trim();
    if line.is_empty() || line.starts_with('#') {
        return Err(invalid(
            &field,
            "inline authorized key is empty or a comment",
            "supply one OpenSSH public key such as 'ssh-ed25519 AAAA… user@host'",
        ));
    }
    if line.lines().count() != 1 {
        return Err(invalid(
            &field,
            "inline authorized key contains more than one line",
            "supply exactly one OpenSSH public key per entry",
        ));
    }
    if line.len() as u64 > MAX_SSH_KEY_FILE_BYTES {
        return Err(invalid(
            &field,
            format!("inline authorized key is {} bytes", line.len()),
            "supply an OpenSSH public key of 64 KiB or less",
        ));
    }
    PublicKey::from_openssh(line).map_err(|error| {
        invalid_with_source(
            &field,
            "inline authorized key is not a valid OpenSSH public key",
            "replace the entry with an OpenSSH public key",
            error,
        )
    })?;
    Ok(())
}

/// Validates the guest password without ever repeating its value.
fn validate_password(password: &str) -> Result<(), FirestoneError> {
    const KEY: &str = "cloud_init.password";
    if password.is_empty() {
        return Err(invalid(
            KEY,
            "password is empty",
            "set a non-empty password or clear 'cloud_init.password'",
        ));
    }
    if password.len() as u64 > MAX_PASSWORD_BYTES {
        return Err(invalid(
            KEY,
            format!("password is {} bytes", password.len()),
            "use a password of 256 bytes or less",
        ));
    }
    if password.chars().any(char::is_control) {
        return Err(invalid(
            KEY,
            "password contains a control character",
            "use a password without newlines, tabs, or other control characters",
        ));
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
        require_executable(
            context.host,
            "vmm.binary",
            path,
            "grant execute access to the VMM binary or remove the override",
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
    if spec
        .vmm
        .config_overlay
        .as_ref()
        .is_some_and(|overlay| !overlay.is_object())
    {
        return Err(invalid(
            "vmm.config_overlay",
            "VMM config overlay must be a JSON object",
            "set vmm.config_overlay to a JSON object or remove it",
        ));
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

fn require_executable(
    host: &dyn ValidationHost,
    key: &str,
    path: &Path,
    hint: impl Into<String>,
) -> Result<(), FirestoneError> {
    match host.path_is_executable(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(invalid(
            key,
            format!("'{}' is not executable by the current user", path.display()),
            hint,
        )),
        Err(error) => Err(invalid_with_source(
            key,
            format!("cannot check execute access for '{}'", path.display()),
            hint,
            error,
        )),
    }
}

fn read_required_file(
    host: &dyn ValidationHost,
    key: &str,
    path: &Path,
    limit: u64,
    limit_label: &str,
    hint: impl Into<String>,
) -> Result<Vec<u8>, FirestoneError> {
    let hint = hint.into();
    match host.read_regular_file(path, limit) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(invalid_with_source(
            key,
            format!("file '{}' does not exist", path.display()),
            hint,
            error,
        )),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(invalid_with_source(
            key,
            format!("'{}' is not a regular file", path.display()),
            hint,
            error,
        )),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(invalid_with_source(
            key,
            format!("file '{}' exceeds {limit_label}", path.display()),
            hint,
            error,
        )),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(invalid_with_source(
            key,
            format!("'{}' is not readable by the current user", path.display()),
            hint,
            error,
        )),
        Err(error) => Err(invalid_with_source(
            key,
            format!("cannot read '{}'", path.display()),
            hint,
            error,
        )),
    }
}
/// Builds one keyed spec error carrying the dotted path as a structured field.
///
/// Every §7.2 validation failure flows through here, so field-addressed
/// surfaces can answer beside the offending input without parsing messages.
fn invalid(key: &str, message: impl Into<String>, hint: impl Into<String>) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("invalid '{key}': {}", message.into()),
    )
    .with_hint(hint)
    .with_field(key.to_owned())
}

fn invalid_with_source(
    key: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FirestoneError {
    invalid(key, message, hint).with_source(source)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        fs, io,
        path::{Path, PathBuf},
    };

    use super::{
        MAX_INLINE_USER_DATA_BYTES, MAX_USER_DATA_BYTES, RealValidationHost, SpecWarning,
        ValidationContext, ValidationHost, read_required_file, validate_machine_spec,
    };
    use crate::{
        Arch, ByteSize, Catalog, CloudInitSpecPatch, ErrorKind, Firmware, MachineSpec,
        MachineSpecPatch, MountSpec, NetMode, PathInputs, Paths, VmmSpecPatch,
    };

    struct FakeHost {
        arch: Arch,
        cpus: usize,
        paths: Paths,
        existing: HashSet<PathBuf>,
        readable: HashSet<PathBuf>,
        executable: HashSet<PathBuf>,
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
                paths: Paths::from_inputs(&PathInputs {
                    current_dir: PathBuf::from("/work"),
                    home_dir: Some(PathBuf::from("/home/test")),
                    firestone_home: Some(PathBuf::from("/firestone")),
                    firestone_config_dir: None,
                    firestone_data_dir: None,
                    firestone_runtime_dir: None,
                    xdg_config_home: None,
                    xdg_data_home: None,
                    xdg_runtime_dir: None,
                    uid: 1000,
                })
                .expect("valid test paths"),
                existing: HashSet::new(),
                readable: HashSet::new(),
                executable: HashSet::new(),
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
            ValidationContext::new(self, &self.paths, Path::new("/machines/dev"), &self.catalog)
        }
    }

    impl ValidationHost for FakeHost {
        fn architecture(&self) -> Result<Arch, crate::FirestoneError> {
            Ok(self.arch)
        }

        fn cpu_count(&self) -> usize {
            self.cpus
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

        fn canonicalize_regular_nofollow(&self, path: &Path) -> io::Result<PathBuf> {
            if self.files.contains_key(path) {
                Ok(path.to_path_buf())
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        }

        fn path_is_executable(&self, path: &Path) -> io::Result<bool> {
            Ok(self.executable.contains(path))
        }

        fn read_regular_file(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
            if let Some(contents) = self.files.get(path) {
                if !self.readable.contains(path) {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
                if contents.len() as u64 > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "file exceeds the read limit",
                    ));
                }
                return Ok(contents.clone());
            }
            if self.existing.contains(path) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a regular file",
                ));
            }
            Err(io::Error::from(io::ErrorKind::NotFound))
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

        patch.resolve_paths(&host.paths, Path::new("/work"))?;

        assert_eq!(
            patch.image.as_ref().map(crate::ImageRef::as_str),
            Some("base.qcow2")
        );
        assert_eq!(
            patch
                .mounts
                .as_ref()
                .and_then(|mounts| mounts.first())
                .map(|mount| mount.host.as_path()),
            Some(Path::new("/work/./src"))
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
    fn patch_image_https_url_is_not_rewritten() -> Result<(), crate::FirestoneError> {
        let host = FakeHost::default();
        let mut patch = MachineSpecPatch {
            image: Some("https://images.example.invalid/base.qcow2".into()),
            ..MachineSpecPatch::default()
        };

        patch.resolve_paths(&host.paths, Path::new("/work"))?;

        assert_eq!(
            patch.image.as_ref().map(crate::ImageRef::as_str),
            Some("https://images.example.invalid/base.qcow2")
        );
        Ok(())
    }

    #[test]
    fn patch_image_existing_url_shaped_path_keeps_origin_until_load()
    -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        let reference = "https://images.example.invalid/base.qcow2";
        let candidate =
            host.paths
                .resolve_input_path(Path::new(reference), Path::new("/work"), "image")?;
        host.add_file(&candidate, Vec::new());
        let mut patch = MachineSpecPatch {
            image: Some(reference.into()),
            ..MachineSpecPatch::default()
        };

        patch.resolve_paths(&host.paths, Path::new("/work"))?;

        assert_eq!(
            patch.image.as_ref().map(crate::ImageRef::as_str),
            Some(reference)
        );
        Ok(())
    }

    #[test]
    fn patch_image_custom_catalog_name_is_not_rewritten() -> Result<(), crate::FirestoneError> {
        let host = FakeHost::default();
        let mut patch = MachineSpecPatch {
            image: Some("custom/os:edge".into()),
            ..MachineSpecPatch::default()
        };

        patch.resolve_paths(&host.paths, Path::new("/work"))?;

        assert_eq!(
            patch.image.as_ref().map(crate::ImageRef::as_str),
            Some("custom/os:edge")
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
            memory: ByteSize::from_mib(127).expect("127 MiB"),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("low memory");
        assert_invalid_key(&error, "memory");
    }

    #[test]
    fn resize_headroom_below_boot_capacity_returns_keyed_errors() {
        let host = FakeHost::default();
        let gib = |value| ByteSize::from_gib(value).expect("test size");

        for (cpus_max, memory_max, key) in [
            (Some(1_u8), None, "cpus_max"),
            (None, Some(gib(1)), "memory_max"),
        ] {
            let mut spec = MachineSpec {
                cpus: 2,
                cpus_max,
                memory: gib(2),
                memory_max,
                ..MachineSpec::default()
            };
            let error =
                validate_machine_spec(&mut spec, &host.context()).expect_err("headroom below boot");
            assert_invalid_key(&error, key);
        }

        for (cpus_max, memory_max) in [
            (None, None),
            (Some(2), Some(gib(2))),
            (Some(8), Some(gib(16))),
        ] {
            let mut spec = MachineSpec {
                cpus: 2,
                cpus_max,
                memory: gib(2),
                memory_max,
                ..MachineSpec::default()
            };
            validate_machine_spec(&mut spec, &host.context()).expect("headroom at or above boot");
        }
    }

    #[test]
    fn disk_below_known_base_size_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            disk: ByteSize::from_gib(4).expect("4 GiB"),
            ..MachineSpec::default()
        };
        let context = host
            .context()
            .with_base_image_virtual_size(ByteSize::from_gib(5).expect("5 GiB"));
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
        let candidate =
            super::image_path("./base.qcow2", &host.context(), Path::new("/machines/dev"))
                .expect("valid relative image path");
        let mut spec = MachineSpec {
            image: "./base.qcow2".into(),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("missing image");
        assert_invalid_key(&error, "image");
        assert!(error.message().contains(&candidate.display().to_string()));
    }

    #[test]
    fn image_missing_absolute_local_path_is_allowed_only_for_complete_pin_context()
    -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "/deleted/base.qcow2".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(
            &mut spec,
            &host.context().with_pinned_image_ref("/deleted/base.qcow2"),
        )?;
        assert_eq!(spec.image.as_str(), "/deleted/base.qcow2");

        host.files
            .insert(PathBuf::from("/deleted/base.qcow2"), Vec::new());
        let mut replaced = MachineSpec {
            image: "/deleted/base.qcow2".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(
            &mut replaced,
            &host.context().with_pinned_image_ref("/deleted/base.qcow2"),
        )?;

        let mut changed = MachineSpec {
            image: "/deleted/other.qcow2".into(),
            ..MachineSpec::default()
        };
        let error = validate_machine_spec(
            &mut changed,
            &host.context().with_pinned_image_ref("/deleted/base.qcow2"),
        )
        .expect_err("a complete pin must not exempt a different missing image");
        assert_invalid_key(&error, "image");
        Ok(())
    }

    #[test]
    fn removed_catalog_reference_is_allowed_only_for_complete_pin_context() {
        let host = FakeHost::default();
        let mut unpinned = MachineSpec {
            image: "removed:1".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(&mut unpinned, &host.context())
            .expect_err("unpinned removed catalog must fail");

        let mut pinned = MachineSpec {
            image: "removed:1".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(
            &mut pinned,
            &host.context().with_pinned_image_ref("removed:1"),
        )
        .expect("complete pin may outlive its catalog entry");

        let mut malformed = MachineSpec {
            image: "https:/broken".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(
            &mut malformed,
            &host.context().with_pinned_image_ref("removed:1"),
        )
        .expect_err("malformed URL must not masquerade as a former catalog pin");
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
        let candidate = {
            let context = host.context();
            super::image_path("./base.qcow2", &context, context.machine_dir)?
        };
        host.add_file(&candidate, Vec::new());
        let mut spec = MachineSpec {
            image: "./base.qcow2".into(),
            ..MachineSpec::default()
        };
        validate_machine_spec(&mut spec, &host.context())?;
        assert_eq!(spec.image.as_str(), candidate.to_string_lossy());
        Ok(())
    }

    #[test]
    fn image_existing_url_shaped_path_is_persisted_as_local() -> Result<(), crate::FirestoneError> {
        let mut host = FakeHost::default();
        let reference = "https://images.example.invalid/base.qcow2";
        let candidate = {
            let context = host.context();
            super::image_path(reference, &context, context.machine_dir)?
        };
        host.add_file(&candidate, Vec::new());
        let mut spec = MachineSpec {
            image: reference.into(),
            ..MachineSpec::default()
        };

        validate_machine_spec(&mut spec, &host.context())?;

        assert_eq!(spec.image.as_str(), candidate.to_string_lossy());
        Ok(())
    }

    #[test]
    fn image_valid_https_url_is_accepted() -> Result<(), crate::FirestoneError> {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "https://images.example.invalid/base.qcow2?build=1".into(),
            ..MachineSpec::default()
        };

        validate_machine_spec(&mut spec, &host.context())?;
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
    fn image_https_policy_violations_return_keyed_error() {
        let host = FakeHost::default();
        for reference in [
            "https://[2001:db8::1/image.qcow2",
            "https://user@example.invalid/image.qcow2",
            " https://example.invalid/image.qcow2",
            "https:///image.qcow2",
            "https://example.invalid/image.qcow2#fragment",
        ] {
            let mut spec = MachineSpec {
                image: reference.into(),
                ..MachineSpec::default()
            };
            let error =
                validate_machine_spec(&mut spec, &host.context()).expect_err("invalid HTTPS URL");
            assert_invalid_key(&error, "image");
        }
    }

    #[test]
    fn image_custom_catalog_name_with_path_character_is_rejected_before_validation() {
        let directory = tempfile::tempdir().expect("temporary catalog directory");
        let catalog_path = directory.path().join("custom.toml");
        fs::write(
            &catalog_path,
            concat!(
                "[[image]]\n",
                "distro = \"custom/os\"\n",
                "version = \"current\"\n",
                "aliases = [\"edge\"]\n",
                "default = true\n",
                "firmware = \"rhf\"\n",
                "format = \"qcow2\"\n\n",
                "[image.arch.x86_64]\n",
                "url = \"https://images.example.invalid/custom.qcow2\"\n",
                "checksum_url = \"https://images.example.invalid/SHA256SUMS\"\n",
                "checksum_alg = \"sha256\"\n",
            ),
        )
        .expect("write custom catalog");
        let error = Catalog::load(
            &directory.path().join("missing-config.toml"),
            &[catalog_path],
        )
        .expect_err("path-shaped catalog component must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
    }
    #[test]
    fn machine_load_catalog_suffix_action_image_remains_catalog_reference()
    -> Result<(), crate::FirestoneError> {
        let catalog = catalog_from_document(
            r#"
[[image]]
distro = "acme.qcow2"
version = "1"
aliases = []
default = true
firmware = "rhf"
format = "qcow2"

[image.arch.x86_64]
url = "https://images.example.invalid/acme.qcow2"
checksum_url = "https://images.example.invalid/SHA256SUMS"
checksum_alg = "sha256"
"#,
        );
        let host = FakeHost {
            catalog,
            ..FakeHost::default()
        };
        let patch = MachineSpecPatch {
            image: Some("acme.qcow2".into()),
            ..MachineSpecPatch::default()
        };

        let loaded = MachineSpec::load(
            "",
            &crate::GlobalConfig::default(),
            &patch,
            Path::new("/work"),
            &host.context(),
        )?;

        assert_eq!(loaded.spec.image.as_str(), "acme.qcow2");
        Ok(())
    }

    #[test]
    fn image_catalog_missing_host_arch_preserves_catalog_error() {
        let catalog = catalog_from_document(
            r#"
[[image]]
distro = "custom-os"
version = "current"
aliases = ["edge"]
default = true
firmware = "rhf"
format = "qcow2"

[image.arch.aarch64]
url = "https://images.example.invalid/custom.qcow2"
checksum_url = "https://images.example.invalid/SHA256SUMS"
checksum_alg = "sha256"
"#,
        );
        let host = FakeHost {
            catalog,
            ..FakeHost::default()
        };
        let mut spec = MachineSpec {
            image: "custom-os:edge".into(),
            ..MachineSpec::default()
        };

        let error = validate_machine_spec(&mut spec, &host.context())
            .expect_err("missing x86_64 catalog source");

        assert_invalid_key(&error, "image");
        assert!(error.message().contains("available architectures: aarch64"));
        assert!(!error.message().contains("local image"));
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
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("firestone catalog"))
        );
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("docker://nginx"))
        );
    }

    #[test]
    fn image_oci_reference_is_normalized_in_place() {
        let host = FakeHost::default();
        for (input, expected) in [
            ("docker://nginx", "docker.io/library/nginx:latest"),
            ("oci://ghcr.io/owner/app:v1", "ghcr.io/owner/app:v1"),
            ("localhost:5000/app", "localhost:5000/app:latest"),
        ] {
            let mut spec = MachineSpec {
                image: input.into(),
                ..MachineSpec::default()
            };
            validate_machine_spec(&mut spec, &host.context())
                .unwrap_or_else(|error| panic!("{input}: {}", error.message()));
            assert_eq!(spec.image.as_str(), expected);
        }
    }

    #[test]
    fn image_explicit_oci_scheme_malformed_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "docker://NGINX".into(),
            ..MachineSpec::default()
        };
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("malformed OCI reference");
        assert_invalid_key(&error, "image");
        assert!(error.message().contains("invalid OCI image reference"));
        assert!(error.hint().is_some());
    }

    #[test]
    fn image_registry_less_namespaced_reference_stays_a_path_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            image: "owner/app".into(),
            ..MachineSpec::default()
        };
        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("missing local image");
        assert_invalid_key(&error, "image");
        assert!(error.message().contains("local image"));
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
    fn cloud_init_both_user_parts_returns_error_naming_both_keys() {
        let mut host = FakeHost::default();
        host.add_file("/machines/dev/user-data", b"#cloud-config\n".to_vec());
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("user-data"));
        spec.cloud_init.user_data_inline = Some("#cloud-config\ntoken: shh\n".to_owned());

        let error = validate_machine_spec(&mut spec, &host.context()).expect_err("both user parts");

        assert_invalid_key(&error, "cloud_init.user_data_inline");
        assert!(error.message().contains("cloud_init.user_data'"));
        assert!(!error.message().contains("token: shh"));
        assert_eq!(error.field(), Some("cloud_init.user_data_inline"));
    }

    #[test]
    fn cloud_init_inline_user_data_matrix_matches_file_rules() {
        let host = FakeHost::default();
        let cases: [(&str, bool); 5] = [
            ("#cloud-config\nruncmd: []\n", true),
            ("#cloud-config\r\nruncmd: []\n", true),
            ("#!/bin/sh\necho ok\n", true),
            ("hostname: dev\n", false),
            ("", false),
        ];

        for (inline, accepted) in cases {
            let mut spec = MachineSpec::default();
            spec.cloud_init.user_data_inline = Some(inline.to_owned());
            let result = validate_machine_spec(&mut spec, &host.context());
            assert_eq!(result.is_ok(), accepted, "inline case {inline:?}");
            if let Err(error) = result {
                assert_invalid_key(&error, "cloud_init.user_data_inline");
            }
        }

        let mut oversized = MachineSpec::default();
        oversized.cloud_init.user_data_inline = Some(format!(
            "#cloud-config\n# {}",
            "p".repeat(MAX_INLINE_USER_DATA_BYTES as usize)
        ));
        let error =
            validate_machine_spec(&mut oversized, &host.context()).expect_err("oversized inline");
        assert_invalid_key(&error, "cloud_init.user_data_inline");
        assert!(error.message().contains("bytes"));
    }

    #[test]
    fn cloud_init_inline_authorized_keys_matrix_returns_indexed_fields() {
        let host = FakeHost::default();
        const VALID: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P test@example";
        let rejected = [
            "",
            "   ",
            "# ssh-ed25519 AAAA comment",
            "ssh-ed25519 not-base64 user@test",
            "not-a-key",
        ];

        let mut accepted = MachineSpec::default();
        accepted.cloud_init.ssh_authorized_keys = vec![VALID.to_owned(), format!("  {VALID}  ")];
        validate_machine_spec(&mut accepted, &host.context()).expect("valid inline keys");

        for entry in rejected {
            let mut spec = MachineSpec::default();
            spec.cloud_init.ssh_authorized_keys = vec![VALID.to_owned(), (*entry).to_owned()];
            let error =
                validate_machine_spec(&mut spec, &host.context()).expect_err("invalid inline key");
            assert_invalid_key(&error, "cloud_init.ssh_authorized_keys[1]");
            assert_eq!(error.field(), Some("cloud_init.ssh_authorized_keys[1]"));
        }
    }

    #[test]
    fn cloud_init_password_matrix_never_repeats_the_value() {
        let host = FakeHost::default();
        let mut accepted = MachineSpec::default();
        accepted.cloud_init.password = Some("correct horse: battery".to_owned());
        validate_machine_spec(&mut accepted, &host.context()).expect("valid password");

        for password in ["", "line\nbreak", "tab\there", &"x".repeat(257)] {
            let mut spec = MachineSpec::default();
            spec.cloud_init.password = Some(password.to_owned());
            let error =
                validate_machine_spec(&mut spec, &host.context()).expect_err("invalid password");
            assert_invalid_key(&error, "cloud_init.password");
            assert_eq!(error.field(), Some("cloud_init.password"));
            if !password.is_empty() {
                assert!(
                    !error.message().contains(password),
                    "password value leaked into {}",
                    error.message()
                );
            }
        }
    }

    #[test]
    fn spec_validation_errors_carry_their_dotted_field_path() {
        let host = FakeHost::default();
        let mut cpus = MachineSpec {
            cpus: 0,
            ..MachineSpec::default()
        };
        let cpus_error = validate_machine_spec(&mut cpus, &host.context()).expect_err("cpus");
        assert_eq!(cpus_error.field(), Some("cpus"));

        let mut mount = MachineSpec {
            mounts: vec![MountSpec {
                host: PathBuf::from("/missing"),
                guest: PathBuf::from("/guest"),
                readonly: false,
                tag: None,
            }],
            ..MachineSpec::default()
        };
        let mount_error = validate_machine_spec(&mut mount, &host.context()).expect_err("mount");
        assert_eq!(mount_error.field(), Some("mount[0].host"));
    }

    #[test]
    fn required_file_directory_returns_keyed_error() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let host = RealValidationHost::new();

        let error = read_required_file(
            &host,
            "cloud_init.user_data",
            directory.path(),
            MAX_USER_DATA_BYTES,
            "1 MiB",
            "choose a regular file",
        )
        .expect_err("directory is not a regular file");

        assert_invalid_key(&error, "cloud_init.user_data");
        assert!(error.message().contains("not a regular file"));
    }

    #[test]
    fn required_file_unreadable_regular_file_returns_keyed_error() {
        let mut host = FakeHost::default();
        let path = PathBuf::from("/machines/dev/user-data");
        host.existing.insert(path.clone());
        host.files.insert(path.clone(), b"#cloud-config\n".to_vec());

        let error = read_required_file(
            &host,
            "cloud_init.user_data",
            &path,
            MAX_USER_DATA_BYTES,
            "1 MiB",
            "make the file readable",
        )
        .expect_err("unreadable regular file");

        assert_invalid_key(&error, "cloud_init.user_data");
        assert!(error.message().contains("not readable"));
    }

    #[test]
    fn required_file_over_limit_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.add_file("/machines/dev/user-data", b"#cloud-config\n".to_vec());
        let path = PathBuf::from("/machines/dev/user-data");

        let error = read_required_file(
            &host,
            "cloud_init.user_data",
            &path,
            4,
            "4 bytes",
            "reduce the file",
        )
        .expect_err("oversized regular file");

        assert_invalid_key(&error, "cloud_init.user_data");
        assert!(error.message().contains("exceeds 4 bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn required_file_fifo_returns_keyed_error_without_blocking() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("user-data.fifo");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("create FIFO");
        let host = RealValidationHost::new();

        let error = read_required_file(
            &host,
            "cloud_init.user_data",
            &path,
            MAX_USER_DATA_BYTES,
            "1 MiB",
            "choose a regular file",
        )
        .expect_err("FIFO is not a regular file");

        assert_invalid_key(&error, "cloud_init.user_data");
        assert!(error.message().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn required_file_character_device_returns_keyed_error() {
        let host = RealValidationHost::new();

        let error = read_required_file(
            &host,
            "cloud_init.user_data",
            Path::new("/dev/null"),
            MAX_USER_DATA_BYTES,
            "1 MiB",
            "choose a regular file",
        )
        .expect_err("character device is not a regular file");

        assert_invalid_key(&error, "cloud_init.user_data");
        assert!(error.message().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn required_file_symlink_to_regular_file_returns_contents() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::write(&target, b"#cloud-config\n").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        let host = RealValidationHost::new();

        let contents = read_required_file(
            &host,
            "cloud_init.user_data",
            &link,
            MAX_USER_DATA_BYTES,
            "1 MiB",
            "choose a regular file",
        )
        .expect("read symlink target");

        assert_eq!(contents, b"#cloud-config\n");
    }

    #[cfg(unix)]
    #[test]
    fn vmm_binary_mode_0644_returns_keyed_error() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = directory.path().join("cloud-hypervisor");
        fs::write(&binary, []).expect("write VMM binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644))
            .expect("set non-executable mode");
        let host = RealValidationHost::new();
        let paths = test_paths(directory.path());
        let catalog = Catalog::built_in().expect("valid built-in catalog");
        let context = ValidationContext::new(&host, &paths, directory.path(), &catalog);
        let mut spec = MachineSpec::default();
        spec.vmm.binary = Some(binary);

        let error = validate_machine_spec(&mut spec, &context).expect_err("non-executable VMM");

        assert_invalid_key(&error, "vmm.binary");
        assert!(error.message().contains("not executable"));
    }

    #[cfg(unix)]
    #[test]
    fn vmm_binary_execute_only_mode_is_accepted() -> Result<(), crate::FirestoneError> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let binary = directory.path().join("cloud-hypervisor");
        fs::write(&binary, []).expect("write VMM binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o111))
            .expect("set executable mode");
        let host = RealValidationHost::new();
        let paths = test_paths(directory.path());
        let catalog = Catalog::built_in()?;
        let context = ValidationContext::new(&host, &paths, directory.path(), &catalog);
        let mut spec = MachineSpec::default();
        spec.vmm.binary = Some(binary);

        validate_machine_spec(&mut spec, &context)?;
        Ok(())
    }

    #[test]
    fn vmm_config_overlay_non_object_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec::default();
        spec.vmm.config_overlay = Some(serde_json::json!(["not", "an", "object"]));

        let error =
            validate_machine_spec(&mut spec, &host.context()).expect_err("non-object overlay");

        assert_invalid_key(&error, "vmm.config_overlay");
    }

    #[test]
    fn firmware_missing_path_returns_keyed_error() {
        let host = FakeHost::default();
        let mut spec = MachineSpec {
            vmm: crate::VmmSpec {
                firmware: Firmware::path("firmware.fd").expect("firmware path"),
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
    fn firmware_directory_path_returns_keyed_error() {
        let mut host = FakeHost::default();
        host.existing
            .insert(PathBuf::from("/machines/dev/firmware"));
        host.readable
            .insert(PathBuf::from("/machines/dev/firmware"));
        let mut spec = MachineSpec {
            vmm: crate::VmmSpec {
                firmware: Firmware::path("firmware").expect("firmware path"),
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

    fn test_paths(base: &Path) -> Paths {
        Paths::from_inputs(&PathInputs {
            current_dir: base.to_path_buf(),
            home_dir: Some(base.to_path_buf()),
            firestone_home: Some(base.join("firestone")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: 1000,
        })
        .expect("valid test paths")
    }

    fn catalog_from_document(document: &str) -> Catalog {
        let directory = tempfile::tempdir().expect("temporary catalog directory");
        let path = directory.path().join("catalog.toml");
        fs::write(&path, document).expect("write catalog document");
        Catalog::load(&directory.path().join("missing.toml"), &[path])
            .expect("load catalog document")
    }
}
