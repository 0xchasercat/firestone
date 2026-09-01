use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use fatfs::{
    Date, DateTime, FileSystem, FormatVolumeOptions, FsOptions, Time, TimeProvider, format_volume,
};
use minijinja::{Environment, context};
use serde::Serialize;
use sha2::{Digest, Sha256};
use ssh_key::PublicKey;

use crate::{
    ErrorKind, FirestoneError, MachineSpec, Paths, atomic,
    bounded::{self, BoundedReadError},
    catalog::SshdPath,
    spec::validate_guest_user,
};

const FIRESTONE_TEMPLATE: &str = include_str!("../../../templates/cloud-init.yaml");
const MIME_BOUNDARY: &str = "===============firestone==";
const VOLUME_LABEL: [u8; 11] = *b"CIDATA     ";
const VOLUME_ID: u32 = 0x4653_0001;
const FIRESTONE_PUBLIC_KEY_MODE: u32 = 0o644;
const MAX_FIRESTONE_PUBLIC_KEY_BYTES: u64 = 16 * 1024;
pub(crate) const MAX_USER_DATA_BYTES: u64 = 1024 * 1024;
/// Inline user-data travels through specs, patches and REST bodies, so it is
/// bounded far below the 1 MiB file limit.
pub(crate) const MAX_INLINE_USER_DATA_BYTES: u64 = 32 * 1024;
pub(crate) const MAX_PASSWORD_BYTES: u64 = 256;
/// Rendered user-data can carry a guest password, so seed artifacts are
/// published owner-read/write only inside the mode-0700 seed directory.
const SEED_FILE_MODE: u32 = 0o600;
pub(crate) const MAX_NETWORK_CONFIG_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_SSH_KEY_FILE_BYTES: u64 = 64 * 1024;
const MAX_RENDERED_SSH_KEYS_BYTES: usize = 256 * 1024;
pub const SEED_IMAGE_SIZE: u64 = 4 * 1024 * 1024;

#[derive(Debug)]
struct FixedTimeProvider;

static FIXED_TIME_PROVIDER: FixedTimeProvider = FixedTimeProvider;

impl TimeProvider for FixedTimeProvider {
    fn get_current_date(&self) -> Date {
        fixed_date_time().date
    }

    fn get_current_date_time(&self) -> DateTime {
        fixed_date_time()
    }
}

fn fixed_date_time() -> DateTime {
    DateTime {
        date: Date {
            year: 1980,
            month: 1,
            day: 1,
        },
        time: Time {
            hour: 0,
            min: 0,
            sec: 0,
            millis: 0,
        },
    }
}

/// Exact NoCloud inputs used to publish one deterministic CIDATA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCloudInit {
    pub instance_id: String,
    pub meta_data: Vec<u8>,
    pub user_data: Vec<u8>,
    pub network_config: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
struct TemplateMount {
    tag: String,
    guest: String,
    readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserDataKind {
    CloudConfig,
    ShellScript,
}

impl UserDataKind {
    const fn content_type(self) -> &'static str {
        match self {
            Self::CloudConfig => "text/cloud-config",
            Self::ShellScript => "text/x-shellscript",
        }
    }

    const fn filename(self) -> &'static str {
        match self {
            Self::CloudConfig => "user-cloud-config.yaml",
            Self::ShellScript => "user-script.sh",
        }
    }
}

#[derive(Debug)]
struct UserDataPart {
    kind: UserDataKind,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct ParsedPublicKey {
    rendered: String,
    parsed: PublicKey,
}

/// Renders Firestone-owned cloud-init content with the default guest sshd path.
pub fn render_cloud_init(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
) -> Result<RenderedCloudInit, FirestoneError> {
    render_cloud_init_from_paths(paths, name, spec, &SshdPath::default())
}

/// Renders Firestone-owned cloud-init content from supplied, validated SSH inputs.
///
/// This API never generates host identity. Callers that own identity generation
/// supply its public key without exposing private-key or user-data contents.
pub fn render_cloud_init_with_guest_ssh(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
    firestone_pubkey: &str,
    sshd_path: &SshdPath,
) -> Result<RenderedCloudInit, FirestoneError> {
    render_cloud_init_inner(paths, name, spec, firestone_pubkey, sshd_path)
}

fn render_cloud_init_from_paths(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
    sshd_path: &SshdPath,
) -> Result<RenderedCloudInit, FirestoneError> {
    let firestone_pubkey = if spec.cloud_init.provisioning {
        read_firestone_public_key(paths)?
    } else {
        String::new()
    };
    render_cloud_init_inner(paths, name, spec, &firestone_pubkey, sshd_path)
}

fn render_cloud_init_inner(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
    firestone_pubkey: &str,
    sshd_path: &SshdPath,
) -> Result<RenderedCloudInit, FirestoneError> {
    let machine_dir = paths.machine_dir(name)?;
    validate_guest_user(&spec.user)?;

    let user_data = match (
        &spec.cloud_init.user_data,
        &spec.cloud_init.user_data_inline,
    ) {
        (Some(_), Some(_)) => {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                "invalid 'cloud_init.user_data_inline': 'cloud_init.user_data' and 'cloud_init.user_data_inline' are both set",
            )
            .with_hint(
                "keep one user part: clear 'cloud_init.user_data' or 'cloud_init.user_data_inline'",
            )
            .with_field("cloud_init.user_data_inline"));
        }
        (Some(path), None) => {
            let path = paths.resolve_input_path(path, &machine_dir, "cloud_init.user_data")?;
            Some(read_user_data_file(&path)?)
        }
        (None, Some(inline)) => Some(read_inline_user_data(inline)?),
        (None, None) => None,
    };
    let network_config = match &spec.cloud_init.network_config {
        Some(path) => {
            let path = paths.resolve_input_path(path, &machine_dir, "cloud_init.network_config")?;
            Some(read_network_config_file(&path)?)
        }
        None => None,
    };
    let user_keys = if spec.cloud_init.provisioning {
        let mut keys = read_user_public_keys(paths, &machine_dir, &spec.cloud_init.ssh_keys)?;
        keys.extend(parse_inline_public_keys(
            &spec.cloud_init.ssh_authorized_keys,
        )?);
        keys
    } else {
        Vec::new()
    };

    render_cloud_init_bytes(
        name,
        spec,
        firestone_pubkey,
        sshd_path,
        user_data,
        network_config,
        user_keys,
    )
}

fn render_cloud_init_bytes(
    name: &str,
    spec: &MachineSpec,
    firestone_pubkey: &str,
    sshd_path: &SshdPath,
    user_data: Option<UserDataPart>,
    network_config: Option<Vec<u8>>,
    user_keys: Vec<ParsedPublicKey>,
) -> Result<RenderedCloudInit, FirestoneError> {
    let firestone_part = if spec.cloud_init.provisioning {
        let firestone_key = validate_supplied_firestone_public_key(firestone_pubkey)?;
        let user_keys = deduplicate_user_keys(&firestone_key.parsed, user_keys);
        let mounts = render_template_mounts(spec)?;
        Some(render_firestone_part(
            name,
            &spec.user,
            &firestone_key.rendered,
            &user_keys,
            &mounts,
            sshd_path,
            spec.cloud_init.password.as_deref(),
            spec.cloud_init.ssh_pwauth,
        )?)
    } else {
        None
    };

    let rendered_user_data = render_multipart(user_data.as_ref(), firestone_part.as_deref());
    let instance_id = instance_id(name, &rendered_user_data, network_config.as_deref());
    let meta_data = format!(
        r#"instance-id: {}
local-hostname: {}
"#,
        json_string(&instance_id)?,
        json_string(name)?
    )
    .into_bytes();

    Ok(RenderedCloudInit {
        instance_id,
        meta_data,
        user_data: rendered_user_data,
        network_config,
    })
}

fn render_template_mounts(spec: &MachineSpec) -> Result<Vec<TemplateMount>, FirestoneError> {
    spec.mounts
        .iter()
        .enumerate()
        .map(|(index, mount)| {
            let guest = mount.guest.to_str().ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!(
                        "mount[{index}].guest '{}' is not UTF-8",
                        mount.guest.display()
                    ),
                )
                .with_hint("use a UTF-8 guest mount path")
            })?;
            Ok(TemplateMount {
                tag: json_string(&mount.effective_tag(index))?,
                guest: json_string(guest)?,
                readonly: mount.readonly,
            })
        })
        .collect()
}

/// Renders inspection files and atomically publishes a deterministic CIDATA disk.
pub fn publish_seed(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
) -> Result<RenderedCloudInit, FirestoneError> {
    publish_seed_with_sshd_path(paths, name, spec, &SshdPath::default())
}

/// Publishes a seed using the sshd path selected by the resolved image catalog.
pub fn publish_seed_with_sshd_path(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
    sshd_path: &SshdPath,
) -> Result<RenderedCloudInit, FirestoneError> {
    let rendered = render_cloud_init_from_paths(paths, name, spec, sshd_path)?;
    publish_rendered_seed(paths, name, rendered)
}

fn publish_rendered_seed(
    paths: &Paths,
    name: &str,
    rendered: RenderedCloudInit,
) -> Result<RenderedCloudInit, FirestoneError> {
    paths.validate_machine_data_directory(name)?;
    let seed_dir = paths.machine_seed_dir(name)?;
    ensure_seed_directory(&seed_dir)?;
    paths.validate_machine_data_directory(name)?;

    atomic::write_with_mode(
        &paths.machine_seed_file(name, "meta-data")?,
        &rendered.meta_data,
        SEED_FILE_MODE,
    )?;
    atomic::write_with_mode(
        &paths.machine_seed_file(name, "user-data")?,
        &rendered.user_data,
        SEED_FILE_MODE,
    )?;

    let network_path = paths.machine_seed_file(name, "network-config")?;
    match &rendered.network_config {
        Some(network_config) => {
            atomic::write_with_mode(&network_path, network_config, SEED_FILE_MODE)?;
        }
        None => remove_optional_file(&network_path)?,
    }

    let seed_image = paths.machine_seed_image(name)?;
    atomic::write_stream_with_mode(&seed_image, SEED_FILE_MODE, |file| {
        write_seed_image(file, &rendered)
    })?;
    Ok(rendered)
}

fn read_user_data_file(path: &Path) -> Result<UserDataPart, FirestoneError> {
    let bytes = read_bounded_user_file(
        path,
        "cloud_init.user_data",
        MAX_USER_DATA_BYTES,
        "1 MiB",
        "correct the path or reduce the user-data file to 1 MiB or less",
    )?;
    parse_user_data(bytes, &format!("file '{}'", path.display()))
}

/// Turns spec-supplied inline user-data into the multipart user part.
///
/// The value never reaches an error message: only its size does.
fn read_inline_user_data(inline: &str) -> Result<UserDataPart, FirestoneError> {
    if inline.len() as u64 > MAX_INLINE_USER_DATA_BYTES {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "cloud_init.user_data_inline is {} bytes and exceeds 32 KiB",
                inline.len()
            ),
        )
        .with_hint("reduce inline user-data to 32 KiB or move it to a 'cloud_init.user_data' file")
        .with_field("cloud_init.user_data_inline"));
    }
    parse_user_data(inline.as_bytes().to_vec(), "inline value")
}

/// Parses inline OpenSSH public keys exactly like file-loaded key lines.
fn parse_inline_public_keys(keys: &[String]) -> Result<Vec<ParsedPublicKey>, FirestoneError> {
    let mut parsed = Vec::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        let field = format!("cloud_init.ssh_authorized_keys[{index}]");
        let line = key.trim();
        if line.is_empty() || line.starts_with('#') || line.lines().count() != 1 {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("{field} is not a single OpenSSH public-key line"),
            )
            .with_hint("supply exactly one OpenSSH public key per entry")
            .with_field(field));
        }
        let public_key = PublicKey::from_openssh(line).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("{field} is not a valid OpenSSH public key"),
            )
            .with_hint("replace the entry with an OpenSSH public key")
            .with_field(field.clone())
            .with_source(source)
        })?;
        parsed.push(ParsedPublicKey {
            rendered: line.to_owned(),
            parsed: public_key,
        });
    }
    Ok(parsed)
}

fn parse_user_data(bytes: Vec<u8>, source_label: &str) -> Result<UserDataPart, FirestoneError> {
    std::str::from_utf8(&bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cloud_init.user_data {source_label} is not UTF-8"),
        )
        .with_hint("save user-data as UTF-8 without changing its cloud-init header")
        .with_source(source)
    })?;
    let first_line = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let first_line = first_line.strip_suffix(b"\r").unwrap_or(first_line);
    let kind = if first_line == b"#cloud-config" {
        UserDataKind::CloudConfig
    } else if first_line.starts_with(b"#!") {
        UserDataKind::ShellScript
    } else {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cloud_init.user_data {source_label} has an unsupported first line"),
        )
        .with_hint("start user-data with '#cloud-config' or a '#!' interpreter line"));
    };
    Ok(UserDataPart { kind, bytes })
}

fn read_network_config_file(path: &Path) -> Result<Vec<u8>, FirestoneError> {
    let bytes = read_bounded_user_file(
        path,
        "cloud_init.network_config",
        MAX_NETWORK_CONFIG_BYTES,
        "1 MiB",
        "correct the path or reduce the network-config file to 1 MiB or less",
    )?;
    std::str::from_utf8(&bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "cloud_init.network_config file '{}' is not UTF-8",
                path.display()
            ),
        )
        .with_hint("save network-config as UTF-8 YAML")
        .with_source(source)
    })?;
    Ok(bytes)
}

fn read_bounded_user_file(
    path: &Path,
    key: &str,
    limit: u64,
    limit_label: &str,
    hint: &'static str,
) -> Result<Vec<u8>, FirestoneError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    let mut file = options.open(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot open {key} file '{}'", path.display()),
        )
        .with_hint(hint)
        .with_source(source)
    })?;
    let metadata = file.metadata().map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot inspect {key} file '{}'", path.display()),
        )
        .with_hint(hint)
        .with_source(source)
    })?;
    if !metadata.is_file() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("{key} path '{}' is not a regular file", path.display()),
        )
        .with_hint(hint));
    }
    match bounded::read_to_end(&mut file, limit) {
        Ok(bytes) => Ok(bytes),
        Err(BoundedReadError::LimitExceeded) => Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("{key} file '{}' exceeds {limit_label}", path.display()),
        )
        .with_hint(hint)),
        Err(BoundedReadError::Io(source)) => Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot read {key} file '{}'", path.display()),
        )
        .with_hint(hint)
        .with_source(source)),
    }
}

fn read_firestone_public_key(paths: &Paths) -> Result<String, FirestoneError> {
    let path = paths.ssh_public_key();
    paths.validate_ssh_data_directory()?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    let mut file = options.open(&path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot open Firestone SSH public key at {}", path.display()),
        )
        .with_hint("run `firestone doctor --fix` to generate the Firestone SSH key")
        .with_source(source)
    })?;
    paths.validate_owned_data_file_handle(
        &path,
        "Firestone SSH public key",
        FIRESTONE_PUBLIC_KEY_MODE,
        &file,
    )?;
    let bytes = match bounded::read_to_end(&mut file, MAX_FIRESTONE_PUBLIC_KEY_BYTES) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::LimitExceeded) => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "Firestone SSH public key '{}' exceeds 16 KiB",
                    path.display()
                ),
            )
            .with_hint("run `firestone doctor --fix` to regenerate the Firestone SSH key"));
        }
        Err(BoundedReadError::Io(source)) => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot read Firestone SSH public key at {}", path.display()),
            )
            .with_hint("run `firestone doctor --fix` to regenerate the Firestone SSH key")
            .with_source(source));
        }
    };
    let mut keys = parse_public_keys(
        &bytes,
        &path,
        "Firestone SSH public key",
        ErrorKind::Dependency,
    )?;
    if keys.len() != 1 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH public-key file {} must contain exactly one key",
                path.display()
            ),
        )
        .with_hint("run `firestone doctor --fix` to regenerate the Firestone SSH key"));
    }
    keys.pop().map(|key| key.rendered).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH public-key file {} contains no key",
                path.display()
            ),
        )
        .with_hint("run `firestone doctor --fix` to regenerate the Firestone SSH key")
    })
}

fn validate_supplied_firestone_public_key(value: &str) -> Result<ParsedPublicKey, FirestoneError> {
    let rendered = value.trim();
    if rendered.is_empty() || rendered.lines().count() != 1 || rendered.starts_with('#') {
        return Err(invalid_supplied_firestone_key());
    }
    let parsed = PublicKey::from_openssh(rendered).map_err(|_| invalid_supplied_firestone_key())?;
    Ok(ParsedPublicKey {
        rendered: rendered.to_owned(),
        parsed,
    })
}

fn invalid_supplied_firestone_key() -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        "supplied Firestone SSH public key must contain exactly one valid OpenSSH key",
    )
    .with_hint("supply one OpenSSH public-key line without surrounding content")
}

fn read_user_public_keys(
    paths: &Paths,
    machine_dir: &Path,
    configured_paths: &[PathBuf],
) -> Result<Vec<ParsedPublicKey>, FirestoneError> {
    let mut total_bytes = 0_usize;
    let mut keys = Vec::new();
    for (index, configured_path) in configured_paths.iter().enumerate() {
        let key = format!("cloud_init.ssh_keys[{index}]");
        let path = paths.resolve_input_path(configured_path, machine_dir, &key)?;
        let bytes = read_bounded_user_file(
            &path,
            &key,
            MAX_SSH_KEY_FILE_BYTES,
            "64 KiB",
            "correct the path or provide an OpenSSH public-key file of 64 KiB or less",
        )?;
        total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                "cloud_init.ssh_keys contents exceed the supported size",
            )
            .with_hint("reduce the configured public-key files to 256 KiB in total")
        })?;
        if total_bytes > MAX_RENDERED_SSH_KEYS_BYTES {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                "cloud_init.ssh_keys contents exceed 256 KiB in total",
            )
            .with_hint("reduce the configured public-key files to 256 KiB in total"));
        }
        keys.extend(parse_public_keys(
            &bytes,
            &path,
            "cloud_init.ssh_keys entry",
            ErrorKind::InvalidSpec,
        )?);
    }
    Ok(keys)
}

fn parse_public_keys(
    bytes: &[u8],
    path: &Path,
    description: &str,
    kind: ErrorKind,
) -> Result<Vec<ParsedPublicKey>, FirestoneError> {
    let text = std::str::from_utf8(bytes).map_err(|source| {
        FirestoneError::new(
            kind,
            format!("{description} at {} is not UTF-8", path.display()),
        )
        .with_hint("replace it with an OpenSSH public-key file")
        .with_source(source)
    })?;

    let mut keys = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let parsed = PublicKey::from_openssh(line).map_err(|source| {
            FirestoneError::new(
                kind,
                format!(
                    "{description} at {} contains an invalid key",
                    path.display()
                ),
            )
            .with_hint("replace the invalid line with an OpenSSH public key")
            .with_source(source)
        })?;
        keys.push(ParsedPublicKey {
            rendered: line.to_owned(),
            parsed,
        });
    }

    if keys.is_empty() {
        return Err(FirestoneError::new(
            kind,
            format!("{description} at {} contains no keys", path.display()),
        )
        .with_hint("add at least one OpenSSH public key"));
    }
    Ok(keys)
}

fn deduplicate_user_keys(firestone_key: &PublicKey, keys: Vec<ParsedPublicKey>) -> Vec<String> {
    let mut unique = Vec::<ParsedPublicKey>::new();
    for key in keys {
        if key.parsed.key_data() == firestone_key.key_data()
            || unique
                .iter()
                .any(|existing| existing.parsed.key_data() == key.parsed.key_data())
        {
            continue;
        }
        unique.push(key);
    }
    unique.into_iter().map(|key| key.rendered).collect()
}

#[allow(clippy::too_many_arguments)]
fn render_firestone_part(
    name: &str,
    user: &str,
    firestone_pubkey: &str,
    user_keys: &[String],
    mounts: &[TemplateMount],
    sshd_path: &SshdPath,
    password: Option<&str>,
    ssh_pwauth: bool,
) -> Result<Vec<u8>, FirestoneError> {
    let name = json_string(name)?;
    let firestone_pubkey = json_string(firestone_pubkey)?;
    let sshd_path = sshd_path.as_str();
    // One pre-quoted scalar keeps a password with YAML metacharacters from
    // changing the document's structure.
    let chpasswd_entry = password
        .map(|password| json_string(&format!("{user}:{password}")))
        .transpose()?;
    let user_keys = user_keys
        .iter()
        .map(|key| json_string(key))
        .collect::<Result<Vec<_>, _>>()?;
    let mut environment = Environment::new();
    environment
        .add_template("firestone-cloud-init", FIRESTONE_TEMPLATE)
        .map_err(template_error)?;
    let template = environment
        .get_template("firestone-cloud-init")
        .map_err(template_error)?;
    let mut rendered = template
        .render(context! {
            name,
            user,
            firestone_pubkey,
            user_keys,
            mounts,
            sshd_path,
            chpasswd_entry,
            ssh_pwauth,
        })
        .map_err(template_error)?
        .into_bytes();
    if !rendered.ends_with(b"\n") {
        rendered.push(b'\n');
    }
    Ok(rendered)
}

fn json_string(value: &str) -> Result<String, FirestoneError> {
    serde_json::to_string(value).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            "cannot quote a value for the Firestone cloud-init template",
        )
        .with_hint("restore the template from the Firestone release")
        .with_source(source)
    })
}

fn template_error(source: minijinja::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        "cannot render the bundled Firestone cloud-init template",
    )
    .with_hint("restore the template from the Firestone release")
    .with_source(source)
}

fn render_multipart(user_part: Option<&UserDataPart>, firestone_part: Option<&[u8]>) -> Vec<u8> {
    if user_part.is_none() && firestone_part.is_none() {
        return Vec::new();
    }

    let body_bytes =
        user_part.map_or(0, |part| part.bytes.len()) + firestone_part.map_or(0, <[u8]>::len);
    let mut bytes = Vec::with_capacity(body_bytes.saturating_add(640));
    bytes.extend_from_slice(b"Content-Type: multipart/mixed; boundary=\"");
    bytes.extend_from_slice(MIME_BOUNDARY.as_bytes());
    bytes.extend_from_slice(b"\"\r\nMIME-Version: 1.0\r\n\r\n");

    if let Some(part) = user_part {
        append_mime_part(
            &mut bytes,
            part.kind.content_type(),
            part.kind.filename(),
            &part.bytes,
        );
    }
    if let Some(part) = firestone_part {
        append_mime_part(
            &mut bytes,
            "text/cloud-config",
            "firestone-cloud-config.yaml",
            part,
        );
    }
    bytes.extend_from_slice(b"--");
    bytes.extend_from_slice(MIME_BOUNDARY.as_bytes());
    bytes.extend_from_slice(b"--\r\n");
    bytes
}

fn append_mime_part(bytes: &mut Vec<u8>, content_type: &str, filename: &str, body: &[u8]) {
    bytes.extend_from_slice(b"--");
    bytes.extend_from_slice(MIME_BOUNDARY.as_bytes());
    bytes.extend_from_slice(b"\r\nContent-Type: ");
    bytes.extend_from_slice(content_type.as_bytes());
    bytes.extend_from_slice(b"; charset=\"utf-8\"\r\nContent-Disposition: attachment; filename=\"");
    bytes.extend_from_slice(filename.as_bytes());
    bytes.extend_from_slice(b"\"\r\n\r\n");
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(b"\r\n");
}

fn instance_id(name: &str, user_data: &[u8], network_config: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    match network_config {
        Some(network_config) => {
            hasher.update(b"firestone-instance-v1\0");
            hasher.update((user_data.len() as u64).to_be_bytes());
            hasher.update(user_data);
            hasher.update((network_config.len() as u64).to_be_bytes());
            hasher.update(network_config);
        }
        None => hasher.update(user_data),
    }
    let digest = hasher.finalize();
    let mut prefix = String::with_capacity(12);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in &digest[..6] {
        prefix.push(char::from(HEX[usize::from(byte >> 4)]));
        prefix.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    format!("iid-{name}-{prefix}")
}

fn ensure_seed_directory(path: &Path) -> Result<(), FirestoneError> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)
                .map_err(|source| seed_io_error("inspect seed directory", path, source))?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("seed path {} is not a real directory", path.display()),
                )
                .with_hint(
                    "remove the path and retry so Firestone can create the seed directory",
                ));
            }
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "seed directory {} is group- or world-writable",
                        path.display()
                    ),
                )
                .with_hint("run chmod 700 on the seed directory and retry"));
            }
            Ok(())
        }
        Err(source) => Err(seed_io_error("create seed directory", path, source)),
    }
}

fn remove_optional_file(path: &Path) -> Result<(), FirestoneError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(seed_io_error("remove stale network-config", path, source)),
    }
}

fn seed_io_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {operation} {}", path.display()),
    )
    .with_hint("check that the machine directory is writable and has free space")
    .with_source(source)
}

fn write_seed_image(file: &mut File, rendered: &RenderedCloudInit) -> io::Result<()> {
    file.set_len(SEED_IMAGE_SIZE)?;
    file.seek(SeekFrom::Start(0))?;
    format_volume(
        &mut *file,
        FormatVolumeOptions::new()
            .bytes_per_sector(512)
            .bytes_per_cluster(512)
            .total_sectors((SEED_IMAGE_SIZE / 512) as u32)
            .volume_id(VOLUME_ID)
            .volume_label(VOLUME_LABEL),
    )?;
    file.seek(SeekFrom::Start(0))?;

    let filesystem = FileSystem::new(
        &mut *file,
        FsOptions::new().time_provider(&FIXED_TIME_PROVIDER),
    )?;
    {
        let root = filesystem.root_dir();
        write_fat_file(&root, "meta-data", &rendered.meta_data)?;
        write_fat_file(&root, "user-data", &rendered.user_data)?;
        if let Some(network_config) = &rendered.network_config {
            write_fat_file(&root, "network-config", network_config)?;
        }
    }
    filesystem.unmount()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn write_fat_file<T>(directory: &fatfs::Dir<'_, T>, name: &str, bytes: &[u8]) -> io::Result<()>
where
    T: fatfs::ReadWriteSeek,
{
    let mut file = directory.create_file(name)?;
    file.truncate()?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Cursor, Read},
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
    };

    use fatfs::{FileSystem, FsOptions};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use crate::{ErrorKind, MachineSpec, MountSpec, PathInputs, Paths, SshdPath};

    use super::{
        MAX_INLINE_USER_DATA_BYTES, MAX_NETWORK_CONFIG_BYTES, MAX_SSH_KEY_FILE_BYTES,
        MAX_USER_DATA_BYTES, SEED_IMAGE_SIZE, VOLUME_ID, ensure_seed_directory, parse_public_keys,
        parse_user_data, publish_seed, publish_seed_with_sshd_path, render_cloud_init,
        render_cloud_init_bytes, render_cloud_init_with_guest_ssh,
    };

    const FIRESTONE_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P firestone@test\n";
    const USER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN6eVqR0T6lRuT6aGvdMVhZkcNrD1s8g8J3RYfLZBuo5 user@test\n";
    const SECOND_USER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN6eVqR0T6lRuT6aGvdMVhZkcNrD1s8g8J3RYfLZBuo4 second@test\n";
    const USER_CLOUD_CONFIG: &str = "#cloud-config\nhostname: \"user: # wins\"\ndisable_root: true\nssh_authorized_keys:\n  - \"user supplied\"\nwrite_files:\n  - path: /etc/user-wins\n    content: \"snowman ☃: # literal\"\nmounts:\n  - [\"user-tag\", \"/user\", \"virtiofs\", \"ro\", \"0\", \"0\"]\nruncmd:\n  - [sh, -c, \"printf user\"]\n";
    const SCRIPT_USER_DATA: &str = "#!/bin/sh\nprintf \"%s\\n\" \"snowman ☃: # literal\"";
    const GOLDEN_MULTIPART: &[u8] = include_bytes!("../testdata/cloud-init.multipart");
    const GOLDEN_USER_MULTIPART: &[u8] = include_bytes!("../testdata/cloud-init-user.multipart");
    const GOLDEN_SCRIPT_MULTIPART: &[u8] =
        include_bytes!("../testdata/cloud-init-script.multipart");
    const GOLDEN_PASSWORD_MULTIPART: &[u8] =
        include_bytes!("../testdata/cloud-init-password.multipart");
    const GOLDEN_SEED_SHA256: &str =
        "d7db86bd93fbcd6ed6d4c10f546ea93503d7770aa0aeb1ef944f815af6b90a36";

    struct Fixture {
        _temp: TempDir,
        paths: Paths,
    }

    impl Fixture {
        fn new(with_key: bool) -> Result<Self, Box<dyn std::error::Error>> {
            let temp = tempfile::tempdir()?;
            let root = fs::canonicalize(temp.path())?;
            let paths = Paths::from_inputs(&PathInputs {
                current_dir: root.clone(),
                home_dir: Some(root.clone()),
                firestone_home: Some(root.clone()),
                firestone_config_dir: None,
                firestone_data_dir: None,
                firestone_runtime_dir: None,
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: nix::unistd::getuid().as_raw(),
            })?;
            let machine_dir = paths.machine_dir("demo")?;
            let ssh_dir = paths.ssh_dir();
            fs::create_dir_all(&machine_dir)?;
            fs::create_dir_all(&ssh_dir)?;
            for directory in [
                root,
                paths.data_dir().to_path_buf(),
                paths.machines_dir(),
                machine_dir,
                ssh_dir,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
            if with_key {
                let public_key = paths.ssh_public_key();
                fs::write(&public_key, FIRESTONE_KEY)?;
                fs::set_permissions(&public_key, fs::Permissions::from_mode(0o644))?;
            }
            Ok(Self { _temp: temp, paths })
        }
    }
    fn layered_spec(key_path: PathBuf) -> MachineSpec {
        MachineSpec {
            user: "ubuntu".to_owned(),
            cloud_init: crate::CloudInitSpec {
                ssh_keys: vec![key_path],
                ..crate::CloudInitSpec::default()
            },
            mounts: vec![
                MountSpec {
                    host: PathBuf::from("/host/code"),
                    guest: PathBuf::from("/work"),
                    readonly: false,
                    tag: None,
                },
                MountSpec {
                    host: PathBuf::from("/host/archive"),
                    guest: PathBuf::from("/archive"),
                    readonly: true,
                    tag: Some("archive".to_owned()),
                },
            ],
            ..MachineSpec::default()
        }
    }

    #[test]
    fn seed_directory_trust_failures_are_dependencies() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let seed = temp.path().join("seed");
        fs::write(&seed, b"not a directory")?;
        let wrong_type = ensure_seed_directory(&seed)
            .err()
            .ok_or("seed file should fail")?;
        assert_eq!(wrong_type.kind(), ErrorKind::Dependency);
        assert!(wrong_type.hint().is_some());

        fs::remove_file(&seed)?;
        fs::create_dir(&seed)?;
        fs::set_permissions(&seed, fs::Permissions::from_mode(0o777))?;
        let insecure = ensure_seed_directory(&seed)
            .err()
            .ok_or("writable seed directory should fail")?;
        assert_eq!(insecure.kind(), ErrorKind::Dependency);
        assert!(insecure.hint().is_some());
        Ok(())
    }

    #[test]
    fn metadata_machine_name_yaml_metacharacters_round_trip_exactly()
    -> Result<(), Box<dyn std::error::Error>> {
        #[derive(serde::Deserialize)]
        struct Metadata {
            #[serde(rename = "instance-id")]
            instance_id: String,
            #[serde(rename = "local-hostname")]
            local_hostname: String,
        }

        let fixture = Fixture::new(true)?;
        let rendered = render_cloud_init(&fixture.paths, "demo: bad", &MachineSpec::default())?;
        let metadata: Metadata = serde_yaml::from_slice(&rendered.meta_data)?;

        assert_eq!(metadata.instance_id, rendered.instance_id);
        assert_eq!(metadata.local_hostname, "demo: bad");
        assert!(std::str::from_utf8(&rendered.meta_data)?.contains("\"demo: bad\""));
        Ok(())
    }

    #[test]
    fn multipart_absent_user_data_matches_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let user_key = fixture.paths.machine_dir("demo")?.join("user.pub");
        fs::write(&user_key, USER_KEY)?;
        let spec = MachineSpec {
            user: "ubuntu".to_owned(),
            cloud_init: crate::CloudInitSpec {
                ssh_keys: vec![user_key],
                ..crate::CloudInitSpec::default()
            },
            mounts: vec![
                MountSpec {
                    host: PathBuf::from("/host/code"),
                    guest: PathBuf::from("/work"),
                    readonly: false,
                    tag: None,
                },
                MountSpec {
                    host: PathBuf::from("/host/archive"),
                    guest: PathBuf::from("/archive"),
                    readonly: true,
                    tag: Some("archive".to_owned()),
                },
            ],
            ..MachineSpec::default()
        };

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;

        let mismatch = rendered
            .user_data
            .iter()
            .zip(GOLDEN_MULTIPART)
            .position(|(actual, expected)| actual != expected);
        assert!(
            rendered.user_data == GOLDEN_MULTIPART,
            "first mismatch at {mismatch:?}; actual length {}, golden length {}",
            rendered.user_data.len(),
            GOLDEN_MULTIPART.len()
        );
        assert_eq!(rendered.instance_id, "iid-demo-06caf6ddb473");
        assert_eq!(
            rendered.meta_data,
            format!(
                "instance-id: \"{}\"\nlocal-hostname: \"demo\"\n",
                rendered.instance_id
            )
            .as_bytes()
        );
        assert!(rendered.network_config.is_none());
        Ok(())
    }
    #[test]
    fn multipart_inline_cloud_config_matches_golden_bytes() -> Result<(), Box<dyn std::error::Error>>
    {
        let spec = layered_spec(PathBuf::from("inline.pub"));
        let user_keys = parse_public_keys(
            USER_KEY.as_bytes(),
            Path::new("inline.pub"),
            "inline key",
            ErrorKind::InvalidSpec,
        )?;
        let user_data =
            parse_user_data(USER_CLOUD_CONFIG.as_bytes().to_vec(), "inline cloud-config")?;

        let rendered = render_cloud_init_bytes(
            "demo",
            &spec,
            FIRESTONE_KEY.trim(),
            &SshdPath::default(),
            Some(user_data),
            None,
            user_keys,
        )?;

        assert_eq!(rendered.user_data, GOLDEN_USER_MULTIPART);
        assert_eq!(rendered.instance_id, "iid-demo-53fd64c9054e");
        Ok(())
    }

    #[test]
    fn multipart_relative_path_cloud_config_matches_inline_and_seed_goldens()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::write(machine_dir.join("user-data.yaml"), USER_CLOUD_CONFIG)?;
        fs::write(machine_dir.join("user.pub"), USER_KEY)?;
        let mut spec = layered_spec(PathBuf::from("user.pub"));
        spec.cloud_init.user_data = Some(PathBuf::from("user-data.yaml"));

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;
        let repeated = render_cloud_init(&fixture.paths, "demo", &spec)?;
        assert_eq!(rendered, repeated);
        assert_eq!(rendered.user_data, GOLDEN_USER_MULTIPART);
        assert_eq!(rendered.instance_id, "iid-demo-53fd64c9054e");

        let published = publish_seed(&fixture.paths, "demo", &spec)?;
        let seed = fs::read(fixture.paths.machine_seed_image("demo")?)?;
        assert_eq!(published, rendered);
        assert_eq!(
            hex_digest(&seed),
            "e4d07c7001f5e68249f4ecac7f6dc857e4d07fdfb3b7e809afc837dd5e36c873"
        );
        verify_seed_filesystem(&seed, &published)?;
        Ok(())
    }

    #[test]
    fn multipart_user_precedes_firestone_merge_directive() -> Result<(), Box<dyn std::error::Error>>
    {
        let user_offset = find_bytes(GOLDEN_USER_MULTIPART, USER_CLOUD_CONFIG.as_bytes())
            .ok_or("missing user cloud-config")?;
        let firestone_offset = find_bytes(
            GOLDEN_USER_MULTIPART,
            b"#cloud-config\nmerge_how: \"list(append)+dict(recurse_dict,recurse_list,no_replace)+str()\"\n",
        )
        .ok_or("missing Firestone merge directive")?;

        assert!(user_offset < firestone_offset);
        assert!(GOLDEN_USER_MULTIPART.starts_with(
            b"Content-Type: multipart/mixed; boundary=\"===============firestone==\"\r\nMIME-Version: 1.0\r\n\r\n"
        ));
        Ok(())
    }

    #[test]
    fn provisioning_false_shellscript_matches_golden_without_identity_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::write(machine_dir.join("user-script.sh"), SCRIPT_USER_DATA)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.provisioning = false;
        spec.cloud_init.user_data = Some(PathBuf::from("user-script.sh"));

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;

        assert_eq!(rendered.user_data, GOLDEN_SCRIPT_MULTIPART);
        assert_eq!(rendered.instance_id, "iid-demo-bb5567ea0d31");
        Ok(())
    }

    #[test]
    fn guest_ssh_users_custom_path_and_identity_are_deterministic()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let spec = MachineSpec {
            user: "ubuntu".to_owned(),
            ..MachineSpec::default()
        };
        let custom_path = SshdPath::new("/usr/libexec/openssh/sshd")?;
        let key = FIRESTONE_KEY.trim();

        let first =
            render_cloud_init_with_guest_ssh(&fixture.paths, "demo", &spec, key, &custom_path)?;
        let unchanged =
            render_cloud_init_with_guest_ssh(&fixture.paths, "demo", &spec, key, &custom_path)?;
        let default_path = render_cloud_init_with_guest_ssh(
            &fixture.paths,
            "demo",
            &spec,
            key,
            &SshdPath::default(),
        )?;

        assert_eq!(first, unchanged);
        assert_ne!(first.instance_id, default_path.instance_id);
        let user_data = std::str::from_utf8(&first.user_data)?;
        let quoted_key = serde_json::to_string(key)?;
        assert!(user_data.contains("ssh_pwauth: false"));
        assert!(user_data.contains("disable_root: false"));
        assert!(user_data.contains("  - default"));
        assert!(user_data.contains("  - name: root"));
        assert_eq!(user_data.matches(&quoted_key).count(), 2);
        assert!(user_data.contains("RuntimeDirectory=sshd"));
        assert!(user_data.contains("RuntimeDirectoryPreserve=yes"));
        assert!(user_data.contains("ExecStart=/usr/libexec/openssh/sshd -i"));
        assert!(!user_data.contains("ExecStart=-/usr/libexec/openssh/sshd -i"));
        Ok(())
    }

    #[test]
    fn native_vsock_presence_deterministically_suppresses_firestone_listener()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let rendered = render_cloud_init_with_guest_ssh(
            &fixture.paths,
            "demo",
            &MachineSpec::default(),
            FIRESTONE_KEY.trim(),
            &SshdPath::default(),
        )?;
        let user_data = std::str::from_utf8(&rendered.user_data)?;
        let condition = user_data
            .lines()
            .find_map(|line| line.trim().strip_prefix("ConditionPathExists=!"))
            .ok_or("missing native-vsock condition")?;
        let firestone_starts = |generated_paths: &[&str]| !generated_paths.contains(&condition);

        assert!(firestone_starts(&[]));
        assert!(!firestone_starts(&[
            "/run/systemd/generator/sshd-vsock.socket"
        ]));
        assert!(user_data.contains("After=sshd-vsock.socket"));
        assert!(user_data.contains("ListenStream=vsock::22"));
        assert!(user_data.contains("Accept=yes"));
        assert!(user_data.contains(
            "systemctl is-active --quiet sshd-vsock.socket || systemctl is-active --quiet firestone-sshd.socket"
        ));
        Ok(())
    }

    #[test]
    fn guest_render_rejects_invalid_user_and_key_without_echoing_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let spec = MachineSpec {
            user: "root;service".to_owned(),
            ..MachineSpec::default()
        };

        let user_error = render_cloud_init_with_guest_ssh(
            &fixture.paths,
            "demo",
            &spec,
            FIRESTONE_KEY.trim(),
            &SshdPath::default(),
        )
        .err()
        .ok_or("invalid user should fail")?;
        assert_eq!(user_error.kind(), ErrorKind::InvalidSpec);

        let spec = MachineSpec::default();
        let invalid_key = "private-looking-but-invalid-key-material";
        let key_error = render_cloud_init_with_guest_ssh(
            &fixture.paths,
            "demo",
            &spec,
            invalid_key,
            &SshdPath::default(),
        )
        .err()
        .ok_or("invalid key should fail")?;
        assert_eq!(key_error.kind(), ErrorKind::InvalidSpec);
        assert!(!key_error.message().contains(invalid_key));
        assert!(
            key_error
                .hint()
                .is_none_or(|hint| !hint.contains(invalid_key))
        );
        Ok(())
    }

    #[test]
    fn rendered_cloud_init_restarts_hvc0_after_installing_dropin()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let rendered = render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())?;
        let user_data = std::str::from_utf8(&rendered.user_data)?;
        let reload = user_data
            .find("systemctl daemon-reload")
            .ok_or("rendered cloud-init did not reload systemd")?;
        let enable = user_data
            .find("systemctl enable serial-getty@hvc0.service")
            .ok_or("rendered cloud-init did not enable hvc0 getty")?;
        let restart = user_data
            .find("systemctl restart serial-getty@hvc0.service")
            .ok_or("rendered cloud-init did not restart hvc0 getty")?;

        assert!(reload < enable && enable < restart);
        Ok(())
    }

    /// Cloud Hypervisor v53 hotplugs vCPUs offline. The udev rule is what makes
    /// `firestone resize --cpus` visible to the guest scheduler without a login.
    #[test]
    fn rendered_cloud_init_auto_onlines_hotplugged_cpus() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new(true)?;
        let rendered = render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())?;
        let user_data = std::str::from_utf8(&rendered.user_data)?;

        let rule = user_data
            .find("- path: /etc/udev/rules.d/80-firestone-hotplug-cpu.rules")
            .ok_or("rendered cloud-init did not write the hotplug-cpu udev rule")?;
        assert!(
            user_data.contains("ACTION==\"add\", SUBSYSTEM==\"cpu\", ATTR{online}=\"1\""),
            "rendered cloud-init did not carry the hotplug-cpu udev rule body"
        );
        let reload = user_data
            .find("udevadm control --reload")
            .ok_or("rendered cloud-init did not reload udev rules")?;
        assert!(
            rule < reload,
            "the rule must be written before udev reloads"
        );
        Ok(())
    }

    #[test]
    fn seed_publication_rebuild_is_byte_identical_and_matches_golden_hash()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let spec = MachineSpec::default();
        let sshd_path = SshdPath::new("/usr/libexec/openssh/sshd")?;

        let first_render = publish_seed_with_sshd_path(&fixture.paths, "demo", &spec, &sshd_path)?;
        let first = fs::read(fixture.paths.machine_seed_image("demo")?)?;
        let first_hash = hex_digest(&first);
        let second_render = publish_seed_with_sshd_path(&fixture.paths, "demo", &spec, &sshd_path)?;
        let second = fs::read(fixture.paths.machine_seed_image("demo")?)?;

        assert_eq!(first_render, second_render);
        assert_eq!(first.len() as u64, SEED_IMAGE_SIZE);
        assert_eq!(first, second);
        assert_eq!(first_hash, GOLDEN_SEED_SHA256);
        assert_eq!(
            fs::read(fixture.paths.machine_seed_file("demo", "meta-data")?)?,
            first_render.meta_data
        );
        assert_eq!(
            fs::read(fixture.paths.machine_seed_file("demo", "user-data")?)?,
            first_render.user_data
        );
        assert!(
            !fixture
                .paths
                .machine_seed_file("demo", "network-config")?
                .exists()
        );

        verify_seed_filesystem(&second, &second_render)?;
        Ok(())
    }

    #[test]
    fn publish_seed_symlinked_machine_directory_preserves_external_sentinel()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let outside = tempfile::tempdir()?;
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"keep")?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::remove_dir(&machine_dir)?;
        symlink(outside.path(), &machine_dir)?;

        let error = publish_seed(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("symlinked machine directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&sentinel)?, b"keep");
        assert!(!outside.path().join("seed.img").exists());
        assert!(!outside.path().join("seed").exists());
        Ok(())
    }

    #[test]
    fn publish_seed_world_writable_machine_directory_refuses_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::set_permissions(&machine_dir, fs::Permissions::from_mode(0o777))?;

        let error = publish_seed(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("world-writable machine directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(!machine_dir.join("seed.img").exists());
        assert!(!machine_dir.join("seed").exists());
        Ok(())
    }

    #[test]
    fn provisioning_false_writes_empty_user_data_without_ssh_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.provisioning = false;

        let rendered = publish_seed(&fixture.paths, "demo", &spec)?;

        assert!(rendered.user_data.is_empty());
        assert_eq!(rendered.instance_id, "iid-demo-e3b0c44298fc");
        assert_eq!(
            fs::read(fixture.paths.machine_seed_file("demo", "user-data")?)?,
            b""
        );
        Ok(())
    }

    #[test]
    fn network_config_path_publishes_exact_bytes_and_changes_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let network_path = machine_dir.join("network.yaml");
        let first_bytes = b"version: 2\nethernets:\n  eth0:\n    dhcp4: true\n";
        let second_bytes = b"version: 2\nethernets:\n  eth0:\n    dhcp4: false\n";
        fs::write(&network_path, first_bytes)?;
        let without_network = render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.network_config = Some(PathBuf::from("network.yaml"));

        let first = publish_seed(&fixture.paths, "demo", &spec)?;
        let repeated = render_cloud_init(&fixture.paths, "demo", &spec)?;
        assert_eq!(first, repeated);
        assert_eq!(
            first.network_config.as_deref(),
            Some(first_bytes.as_slice())
        );
        assert_eq!(first.instance_id, "iid-demo-0639c722e439");
        assert_ne!(first.instance_id, without_network.instance_id);
        assert_eq!(
            fs::read(fixture.paths.machine_seed_file("demo", "network-config")?)?,
            first_bytes
        );

        fs::write(&network_path, second_bytes)?;
        let changed = publish_seed(&fixture.paths, "demo", &spec)?;
        assert_eq!(
            changed.network_config.as_deref(),
            Some(second_bytes.as_slice())
        );
        assert_eq!(changed.instance_id, "iid-demo-7fc6b25ae6fd");
        assert_ne!(changed.instance_id, first.instance_id);
        let seed = fs::read(fixture.paths.machine_seed_image("demo")?)?;
        verify_seed_filesystem(&seed, &changed)?;
        Ok(())
    }

    #[test]
    fn user_data_symlink_to_regular_file_matches_direct_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let target = machine_dir.join("target.yaml");
        let link = machine_dir.join("link.yaml");
        fs::write(&target, USER_CLOUD_CONFIG)?;
        symlink(&target, &link)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("link.yaml"));
        let linked = render_cloud_init(&fixture.paths, "demo", &spec)?;

        spec.cloud_init.user_data = Some(PathBuf::from("target.yaml"));
        let direct = render_cloud_init(&fixture.paths, "demo", &spec)?;

        assert_eq!(linked, direct);
        fs::write(&target, "#cloud-config\nhostname: changed\n")?;
        let changed = render_cloud_init(&fixture.paths, "demo", &spec)?;
        assert_eq!(changed.instance_id, "iid-demo-11bf07aaae1c");
        assert_ne!(changed.instance_id, direct.instance_id);
        Ok(())
    }

    #[test]
    fn user_data_size_and_utf8_errors_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let path = machine_dir.join("user-data.yaml");
        let mut oversized = b"#cloud-config\n".to_vec();
        oversized.resize(MAX_USER_DATA_BYTES as usize + 1, b'x');
        fs::write(&path, oversized)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("user-data.yaml"));

        let size_error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("oversized user-data should fail")?;
        assert_eq!(size_error.kind(), ErrorKind::InvalidSpec);
        assert!(size_error.message().contains("exceeds 1 MiB"));

        fs::write(&path, b"#cloud-config\nvalue: \xff\n")?;
        let utf8_error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("non-UTF-8 user-data should fail")?;
        assert_eq!(utf8_error.kind(), ErrorKind::InvalidSpec);
        assert!(utf8_error.message().contains("is not UTF-8"));
        Ok(())
    }

    #[test]
    fn network_config_size_and_utf8_errors_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let path = machine_dir.join("network.yaml");
        fs::write(&path, vec![b'x'; MAX_NETWORK_CONFIG_BYTES as usize + 1])?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.network_config = Some(PathBuf::from("network.yaml"));

        let size_error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("oversized network-config should fail")?;
        assert_eq!(size_error.kind(), ErrorKind::InvalidSpec);
        assert!(size_error.message().contains("exceeds 1 MiB"));

        fs::write(&path, b"version: \xff\n")?;
        let utf8_error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("non-UTF-8 network-config should fail")?;
        assert_eq!(utf8_error.kind(), ErrorKind::InvalidSpec);
        assert!(utf8_error.message().contains("is not UTF-8"));
        Ok(())
    }

    #[test]
    fn ssh_key_size_and_utf8_errors_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let path = machine_dir.join("user.pub");
        fs::write(&path, vec![b'x'; MAX_SSH_KEY_FILE_BYTES as usize + 1])?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.ssh_keys = vec![PathBuf::from("user.pub")];

        let size_error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("oversized public-key file should fail")?;
        assert_eq!(size_error.kind(), ErrorKind::InvalidSpec);
        assert!(size_error.message().contains("exceeds 64 KiB"));

        fs::write(&path, b"ssh-ed25519 \xff\n")?;
        let utf8_error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("non-UTF-8 public-key file should fail")?;
        assert_eq!(utf8_error.kind(), ErrorKind::InvalidSpec);
        assert!(utf8_error.message().contains("is not UTF-8"));
        Ok(())
    }

    #[test]
    fn ssh_key_deduplication_preserves_first_seen_order_and_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let user_blob = public_key_blob(USER_KEY)?;
        let second_blob = public_key_blob(SECOND_USER_KEY)?;
        let firestone_blob = public_key_blob(FIRESTONE_KEY)?;
        let first_user = format!("ssh-ed25519 {user_blob} user:\" # first\n");
        let first_second = format!("ssh-ed25519 {second_blob} second first\n");
        fs::write(
            machine_dir.join("keys-one.pub"),
            format!("{first_user}ssh-ed25519 {user_blob} duplicate user\n{first_second}"),
        )?;
        fs::write(
            machine_dir.join("keys-two.pub"),
            format!(
                "ssh-ed25519 {second_blob} duplicate second\nssh-ed25519 {firestone_blob} duplicate identity\n"
            ),
        )?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.ssh_keys =
            vec![PathBuf::from("keys-one.pub"), PathBuf::from("keys-two.pub")];

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;
        let text = std::str::from_utf8(&rendered.user_data)?;
        assert_eq!(text.matches(firestone_blob).count(), 2);
        assert_eq!(text.matches(user_blob).count(), 2);
        assert_eq!(text.matches(second_blob).count(), 2);
        assert!(text.contains("user:\\\" # first"));
        assert!(!text.contains("duplicate user"));
        assert!(!text.contains("duplicate second"));
        assert!(!text.contains("duplicate identity"));
        assert!(
            text.find(user_blob).ok_or("missing first user key")?
                < text.find(second_blob).ok_or("missing second user key")?
        );

        fs::write(
            machine_dir.join("keys-clean.pub"),
            format!("{first_user}{first_second}"),
        )?;
        spec.cloud_init.ssh_keys = vec![PathBuf::from("keys-clean.pub")];
        let clean = render_cloud_init(&fixture.paths, "demo", &spec)?;
        assert_eq!(rendered, clean);
        assert_eq!(rendered.instance_id, "iid-demo-6776222a95f6");
        Ok(())
    }

    #[test]
    fn inline_user_data_renders_the_same_part_as_an_identical_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::write(machine_dir.join("user.pub"), USER_KEY)?;
        let mut spec = layered_spec(PathBuf::from("user.pub"));
        spec.cloud_init.user_data_inline = Some(USER_CLOUD_CONFIG.to_owned());

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;

        assert_eq!(rendered.user_data, GOLDEN_USER_MULTIPART);
        assert_eq!(rendered.instance_id, "iid-demo-53fd64c9054e");
        Ok(())
    }

    #[test]
    fn inline_shell_script_user_data_selects_the_shellscript_part()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.provisioning = false;
        spec.cloud_init.user_data_inline = Some(SCRIPT_USER_DATA.to_owned());

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;

        assert_eq!(rendered.user_data, GOLDEN_SCRIPT_MULTIPART);
        assert_eq!(rendered.instance_id, "iid-demo-bb5567ea0d31");
        Ok(())
    }

    #[test]
    fn inline_and_file_user_data_together_names_both_keys_without_contents()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::write(machine_dir.join("user-data.yaml"), USER_CLOUD_CONFIG)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("user-data.yaml"));
        spec.cloud_init.user_data_inline = Some("#cloud-config\nsecret: value\n".to_owned());

        let error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("both user parts should fail")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("cloud_init.user_data"));
        assert!(error.message().contains("cloud_init.user_data_inline"));
        assert!(!error.message().contains("secret: value"));
        assert_eq!(error.field(), Some("cloud_init.user_data_inline"));
        assert!(error.hint().is_some());
        Ok(())
    }

    #[test]
    fn inline_user_data_over_the_limit_reports_size_without_contents()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let mut inline = String::from("#cloud-config\n# ");
        inline.push_str(&"secret".repeat(MAX_INLINE_USER_DATA_BYTES as usize / 6));
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data_inline = Some(inline);

        let error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("oversized inline user-data should fail")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("exceeds 32 KiB"));
        assert!(!error.message().contains("secret"));
        assert_eq!(error.field(), Some("cloud_init.user_data_inline"));
        Ok(())
    }

    #[test]
    fn inline_authorized_key_invalid_entry_is_rejected_without_echoing_material()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.ssh_authorized_keys = vec![
            USER_KEY.trim().to_owned(),
            "ssh-ed25519 AAAAnope".to_owned(),
        ];

        let error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("invalid inline key should fail")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert_eq!(error.field(), Some("cloud_init.ssh_authorized_keys[1]"));
        assert!(!error.message().contains("AAAAnope"));
        assert!(error.hint().is_some());
        Ok(())
    }

    #[test]
    fn inline_and_file_keys_deduplicate_across_both_sources()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        let user_blob = public_key_blob(USER_KEY)?;
        let second_blob = public_key_blob(SECOND_USER_KEY)?;
        let firestone_blob = public_key_blob(FIRESTONE_KEY)?;
        fs::write(
            machine_dir.join("user.pub"),
            format!("ssh-ed25519 {user_blob} from file\n"),
        )?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.ssh_keys = vec![PathBuf::from("user.pub")];
        spec.cloud_init.ssh_authorized_keys = vec![
            format!("ssh-ed25519 {user_blob} duplicate inline"),
            format!("ssh-ed25519 {second_blob} second inline"),
            format!("ssh-ed25519 {firestone_blob} duplicate identity"),
            format!("ssh-ed25519 {second_blob} duplicate second"),
        ];

        let rendered = render_cloud_init(&fixture.paths, "demo", &spec)?;
        let text = std::str::from_utf8(&rendered.user_data)?;

        assert_eq!(text.matches(user_blob).count(), 2);
        assert_eq!(text.matches(second_blob).count(), 2);
        assert_eq!(text.matches(firestone_blob).count(), 2);
        assert!(text.contains("from file"));
        assert!(!text.contains("duplicate inline"));
        assert!(!text.contains("duplicate identity"));
        assert!(!text.contains("duplicate second"));
        assert!(
            text.find(user_blob).ok_or("missing file key")?
                < text.find(second_blob).ok_or("missing inline key")?
        );
        Ok(())
    }

    #[test]
    fn password_renders_chpasswd_and_pwauth_only_when_requested()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let mut spec = MachineSpec {
            user: "ubuntu".to_owned(),
            ..MachineSpec::default()
        };

        let baseline = render_cloud_init(&fixture.paths, "demo", &spec)?;
        assert!(std::str::from_utf8(&baseline.user_data)?.contains("ssh_pwauth: false\n"));
        assert!(!std::str::from_utf8(&baseline.user_data)?.contains("chpasswd"));

        spec.cloud_init.password = Some("s3cret: \"quoted\"".to_owned());
        let with_password = render_cloud_init(&fixture.paths, "demo", &spec)?;
        let text = std::str::from_utf8(&with_password.user_data)?;
        assert!(text.contains("chpasswd:\n  expire: false\n  list:\n"));
        assert!(text.contains("    - \"ubuntu:s3cret: \\\"quoted\\\"\"\n"));
        assert!(text.contains("ssh_pwauth: false\n"));
        assert_ne!(with_password.instance_id, baseline.instance_id);

        spec.cloud_init.ssh_pwauth = true;
        let with_pwauth = render_cloud_init(&fixture.paths, "demo", &spec)?;
        let pwauth_text = std::str::from_utf8(&with_pwauth.user_data)?;
        assert!(pwauth_text.contains("ssh_pwauth: true\n"));
        assert!(pwauth_text.contains("chpasswd:\n"));
        assert_ne!(with_pwauth.instance_id, with_password.instance_id);
        assert_eq!(with_pwauth.user_data, GOLDEN_PASSWORD_MULTIPART);

        let repeated = render_cloud_init(&fixture.paths, "demo", &spec)?;
        assert_eq!(repeated, with_pwauth);
        Ok(())
    }

    #[test]
    fn password_change_reprovisions_through_the_instance_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.password = Some("first".to_owned());
        let first = render_cloud_init(&fixture.paths, "demo", &spec)?;

        spec.cloud_init.password = Some("second".to_owned());
        let second = render_cloud_init(&fixture.paths, "demo", &spec)?;

        assert_ne!(first.instance_id, second.instance_id);
        assert_ne!(first.user_data, second.user_data);
        assert!(
            std::str::from_utf8(&second.meta_data)?.contains(&second.instance_id),
            "meta-data carries the changed instance id"
        );
        Ok(())
    }

    #[test]
    fn published_seed_artifacts_are_owner_only() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::write(machine_dir.join("network-config.yaml"), "version: 2\n")?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.password = Some("s3cret".to_owned());
        spec.cloud_init.network_config = Some(PathBuf::from("network-config.yaml"));

        publish_seed(&fixture.paths, "demo", &spec)?;

        for path in [
            fixture.paths.machine_seed_file("demo", "meta-data")?,
            fixture.paths.machine_seed_file("demo", "user-data")?,
            fixture.paths.machine_seed_file("demo", "network-config")?,
            fixture.paths.machine_seed_image("demo")?,
        ] {
            let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} mode {mode:04o}", path.display());
        }
        Ok(())
    }

    #[test]
    fn instance_id_tracks_rendered_user_key_user_and_mount_bytes_only()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::write(machine_dir.join("user.pub"), USER_KEY)?;

        let baseline = render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())?;
        assert_eq!(baseline.instance_id, "iid-demo-e75d5456fd4c");
        assert_eq!(
            render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())?,
            baseline
        );

        let mut unrelated = MachineSpec {
            cpus: 7,
            ..MachineSpec::default()
        };
        assert_eq!(
            render_cloud_init(&fixture.paths, "demo", &unrelated)?.instance_id,
            baseline.instance_id
        );

        unrelated.user = "ubuntu".to_owned();
        let changed_user = render_cloud_init(&fixture.paths, "demo", &unrelated)?;
        assert_eq!(changed_user.instance_id, "iid-demo-4b98c7ff95cc");
        assert_ne!(changed_user.instance_id, baseline.instance_id);

        let mut key_spec = MachineSpec::default();
        key_spec.cloud_init.ssh_keys = vec![PathBuf::from("user.pub")];
        let changed_key = render_cloud_init(&fixture.paths, "demo", &key_spec)?;
        assert_eq!(changed_key.instance_id, "iid-demo-8872ce46d1f2");
        assert_ne!(changed_key.instance_id, baseline.instance_id);

        let mut mount_spec = MachineSpec {
            mounts: vec![MountSpec {
                host: PathBuf::from("/host/one"),
                guest: PathBuf::from("/work: # \\\"snow\\\""),
                readonly: false,
                tag: None,
            }],
            ..MachineSpec::default()
        };
        let changed_mount = render_cloud_init(&fixture.paths, "demo", &mount_spec)?;
        assert_eq!(changed_mount.instance_id, "iid-demo-d6276c1e119f");
        assert_ne!(changed_mount.instance_id, baseline.instance_id);
        assert!(
            std::str::from_utf8(&changed_mount.user_data)?
                .contains("[\"share0\", \"/work: # \\\\\\\"snow\\\\\\\"\", \"virtiofs\"")
        );

        mount_spec.mounts[0].host = PathBuf::from("/host/two");
        let host_only = render_cloud_init(&fixture.paths, "demo", &mount_spec)?;
        assert_eq!(host_only.instance_id, changed_mount.instance_id);
        mount_spec.mounts[0].readonly = true;
        let readonly = render_cloud_init(&fixture.paths, "demo", &mount_spec)?;
        assert_ne!(readonly.instance_id, changed_mount.instance_id);
        Ok(())
    }

    #[test]
    fn firestone_public_key_wrong_mode_is_rejected_before_render()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        fs::set_permissions(
            fixture.paths.ssh_public_key(),
            fs::Permissions::from_mode(0o600),
        )?;

        let error = render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("wrong public-key mode should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("mode 0644"));
        assert!(!fixture.paths.machine_seed_image("demo")?.exists());
        Ok(())
    }

    #[test]
    fn publish_seed_world_writable_ssh_directory_refuses_key_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        fs::set_permissions(fixture.paths.ssh_dir(), fs::Permissions::from_mode(0o777))?;

        let error = publish_seed(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("world-writable SSH directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(!fixture.paths.machine_seed_image("demo")?.exists());
        Ok(())
    }

    #[test]
    fn publish_seed_symlinked_ssh_directory_preserves_external_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let outside = tempfile::tempdir()?;
        let external_key = outside.path().join("id_ed25519.pub");
        fs::write(&external_key, FIRESTONE_KEY)?;
        fs::remove_file(fixture.paths.ssh_public_key())?;
        fs::remove_dir(fixture.paths.ssh_dir())?;
        symlink(outside.path(), fixture.paths.ssh_dir())?;

        let error = publish_seed(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("symlinked SSH directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&external_key)?, FIRESTONE_KEY.as_bytes());
        assert!(!fixture.paths.machine_seed_image("demo")?.exists());
        Ok(())
    }

    #[test]
    fn publish_seed_symlinked_public_key_preserves_external_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let outside = tempfile::tempdir()?;
        let external_key = outside.path().join("outside.pub");
        fs::write(&external_key, FIRESTONE_KEY)?;
        fs::remove_file(fixture.paths.ssh_public_key())?;
        symlink(&external_key, fixture.paths.ssh_public_key())?;

        let error = publish_seed(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("symlinked Firestone public key should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&external_key)?, FIRESTONE_KEY.as_bytes());
        assert!(!fixture.paths.machine_seed_image("demo")?.exists());
        Ok(())
    }

    #[test]
    fn missing_firestone_key_returns_actionable_dependency_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;

        let error = render_cloud_init(&fixture.paths, "demo", &MachineSpec::default())
            .err()
            .ok_or("missing Firestone key should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error
                .message()
                .contains("cannot open Firestone SSH public key")
        );
        assert_eq!(
            error.hint(),
            Some("run `firestone doctor --fix` to generate the Firestone SSH key")
        );
        Ok(())
    }

    fn public_key_blob(value: &str) -> Result<&str, Box<dyn std::error::Error>> {
        value
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| "public-key fixture has no encoded key".into())
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn hex_digest(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        format!("{digest:x}")
    }

    fn verify_seed_filesystem(
        bytes: &[u8],
        rendered: &super::RenderedCloudInit,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cursor = Cursor::new(bytes.to_vec());
        let filesystem = FileSystem::new(cursor, FsOptions::new())?;
        assert_eq!(filesystem.volume_label(), "CIDATA");
        assert_eq!(
            filesystem.read_volume_label_from_root_dir()?.as_deref(),
            Some("CIDATA")
        );
        assert_eq!(filesystem.volume_id(), VOLUME_ID);

        let root = filesystem.root_dir();
        let entries = root
            .iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| entry.is_file())
            .collect::<Vec<_>>();
        let expected_names = if rendered.network_config.is_some() {
            vec!["meta-data", "user-data", "network-config"]
        } else {
            vec!["meta-data", "user-data"]
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>(),
            expected_names
        );
        for entry in &entries {
            assert_eq!(entry.created(), super::fixed_date_time());
            assert_eq!(entry.modified(), super::fixed_date_time());
            assert_eq!(entry.accessed(), super::fixed_date_time().date);
        }

        let mut meta_data = Vec::new();
        root.open_file("meta-data")?.read_to_end(&mut meta_data)?;
        let mut user_data = Vec::new();
        root.open_file("user-data")?.read_to_end(&mut user_data)?;
        assert_eq!(meta_data, rendered.meta_data);
        assert_eq!(user_data, rendered.user_data);
        if let Some(expected) = &rendered.network_config {
            let mut network_config = Vec::new();
            root.open_file("network-config")?
                .read_to_end(&mut network_config)?;
            assert_eq!(&network_config, expected);
        }
        Ok(())
    }
}
