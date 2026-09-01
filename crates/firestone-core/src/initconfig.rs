//! The `firestone-init` config disk (SPEC §10.5).
//!
//! An OCI machine has no cloud-init seed. The `disks[1]` slot of §9.2 carries
//! `machines/<name>/config.img` instead: a magic-framed, length-prefixed JSON
//! document that the guest's PID 1 reads from `/dev/vdb`. The frame format and
//! the document itself live in `firestone-initproto`, which both this writer and
//! the guest reader link, so the two cannot drift.
//!
//! The bytes are a pure function of their inputs: the same machine spec and the
//! same image runtime configuration always produce the same disk, which is what
//! lets `start` rewrite it only when it changed, exactly as step 4 of §9.3
//! rewrites `seed.img`.

use std::{
    fs::DirBuilder,
    io,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
};

use firestone_initproto::{
    CONFIG_DISK_ALIGNMENT, InitConfig, InitNetwork, encode_frame, merge_env,
};
use sha2::{Digest as _, Sha256};

use crate::{
    ErrorKind, FirestoneError, MachineSpec, Paths, atomic, oci::layers::OciImageConfig,
    spec::NetMode,
};

/// The config document and disk are owner-only, like every seed artifact.
const CONFIG_FILE_MODE: u32 = 0o600;
const CONFIG_DIR_MODE: u32 = 0o700;

/// What one published config disk produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedInitConfig {
    /// The exact document written into the disk.
    pub config: InitConfig,
    /// The machine identity these bytes imply (SPEC §10.4/§10.5).
    pub identity: String,
    /// Size of the published raw image in bytes.
    pub size: u64,
}

/// Builds the config document for one OCI machine.
///
/// v0.2 has no `firestone.toml` keys for `entrypoint`, `cmd`, `env`, `workdir`
/// or `user`, so those come from the image; `hostname`, `network` and
/// `disk_size_bytes` come from the machine spec. `env_overrides` is the seam the
/// per-machine environment will use when §7.1 grows one: it merges over the
/// image's list in place, deterministically.
#[must_use]
pub fn render_init_config(
    name: &str,
    spec: &MachineSpec,
    image: &OciImageConfig,
    env_overrides: &[String],
) -> InitConfig {
    InitConfig {
        hostname: name.to_owned(),
        entrypoint: image.entrypoint.clone(),
        cmd: image.cmd.clone(),
        env: merge_env(&image.env, env_overrides),
        workdir: image.working_dir.clone(),
        user: image.user.clone(),
        network: match spec.network.mode {
            NetMode::None => InitNetwork::None,
            NetMode::Passt | NetMode::Tap => InitNetwork::Dhcp,
        },
        disk_size_bytes: spec.disk.as_bytes(),
    }
}

/// Renders the raw bytes of the config disk.
///
/// The frame is followed by zeroes up to a [`CONFIG_DISK_ALIGNMENT`] multiple,
/// so the image is a whole number of 4 KiB blocks and Cloud Hypervisor's raw
/// backend is happy with it.
///
/// # Errors
///
/// Returns `invalid_spec` when the document does not serialize or exceeds the
/// 64 KiB cap of §10.5.
pub fn build_config_disk(config: &InitConfig) -> Result<Vec<u8>, FirestoneError> {
    let mut bytes = encode_frame(config).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot build the firestone-init config disk: {source}"),
        )
        .with_hint("reduce the image entrypoint, cmd and environment")
    })?;
    let alignment = usize::try_from(CONFIG_DISK_ALIGNMENT).unwrap_or(4096);
    let padded = bytes.len().div_ceil(alignment) * alignment;
    bytes.resize(padded.max(alignment), 0);
    Ok(bytes)
}

/// The instance identity implied by one config document.
///
/// §10.5: the machine's identity digest covers the config-disk bytes the same
/// way §10.4's covers the seed bytes, so the rendering shares §10.4's shape —
/// `iid-<name>-<first six digest bytes>` — over a domain-separated hash of the
/// framed document.
#[must_use]
pub fn config_disk_identity(name: &str, framed: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"firestone-init-config-v1\0");
    hasher.update((framed.len() as u64).to_be_bytes());
    hasher.update(framed);
    let digest = hasher.finalize();
    let mut prefix = String::with_capacity(12);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in &digest[..6] {
        prefix.push(char::from(HEX[usize::from(byte >> 4)]));
        prefix.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    format!("iid-{name}-{prefix}")
}

/// Writes `config/config.json` for inspection and publishes `config.img`.
///
/// Both are written atomically with mode 0600, exactly like the seed artifacts
/// of §10.5, and the machine's data directory is revalidated first.
///
/// # Errors
///
/// Returns a filesystem or `invalid_spec` error when the document cannot be
/// framed or the machine directory cannot be written.
pub fn publish_init_config(
    paths: &Paths,
    name: &str,
    config: &InitConfig,
) -> Result<PublishedInitConfig, FirestoneError> {
    let framed = build_config_disk(config)?;
    let document = inspection_document(config)?;

    paths.validate_machine_data_directory(name)?;
    let directory = paths.machine_config_dir(name)?;
    ensure_config_directory(&directory)?;
    paths.validate_machine_data_directory(name)?;

    atomic::write_with_mode(
        &paths.machine_config_file(name, "config.json")?,
        &document,
        CONFIG_FILE_MODE,
    )?;
    atomic::write_with_mode(
        &paths.machine_config_image(name)?,
        &framed,
        CONFIG_FILE_MODE,
    )?;

    Ok(PublishedInitConfig {
        identity: config_disk_identity(name, &framed),
        size: framed.len() as u64,
        config: config.clone(),
    })
}

fn inspection_document(config: &InitConfig) -> Result<Vec<u8>, FirestoneError> {
    let mut document = serde_json::to_vec_pretty(config).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            "cannot render the firestone-init config document",
        )
        .with_hint("use UTF-8 values in the image runtime configuration")
        .with_source(source)
    })?;
    document.push(b'\n');
    Ok(document)
}

fn ensure_config_directory(path: &Path) -> Result<(), FirestoneError> {
    let mut builder = DirBuilder::new();
    builder.mode(CONFIG_DIR_MODE);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot create config directory {}", path.display()),
        )
        .with_hint("check the machine directory permissions")
        .with_source(source)),
    }
}

/// The published disk path, for callers that only need to name it.
///
/// # Errors
///
/// Returns `invalid_spec` when the machine name is not a valid path component.
pub fn config_disk_path(paths: &Paths, name: &str) -> Result<PathBuf, FirestoneError> {
    paths.machine_config_image(name)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use firestone_initproto::{CONFIG_HEADER_LEN, CONFIG_MAGIC, InitNetwork, decode_frame};

    use super::{build_config_disk, config_disk_identity, publish_init_config, render_init_config};
    use crate::{
        ByteSize, MachineSpec, PathInputs, Paths, oci::layers::OciImageConfig, spec::NetMode,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn image() -> OciImageConfig {
        OciImageConfig {
            env: vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
            ],
            entrypoint: vec!["/docker-entrypoint.sh".to_owned()],
            cmd: vec![
                "nginx".to_owned(),
                "-g".to_owned(),
                "daemon off;".to_owned(),
            ],
            working_dir: Some("/".to_owned()),
            user: Some("root".to_owned()),
        }
    }

    const fn byte_size(gib: u64) -> ByteSize {
        match ByteSize::from_gib(gib) {
            Ok(size) => size,
            Err(_) => panic!("the test sizes fit a u64"),
        }
    }

    fn spec() -> MachineSpec {
        MachineSpec {
            disk: byte_size(20),
            ..MachineSpec::default()
        }
    }

    fn test_paths(root: &std::path::Path) -> Result<Paths, Box<dyn std::error::Error>> {
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let root = fs::canonicalize(root)?;
        Ok(Paths::from_inputs(&PathInputs {
            current_dir: root.clone(),
            home_dir: Some(root.clone()),
            firestone_home: Some(root.join("home")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: nix::unistd::getuid().as_raw(),
        })?)
    }

    #[test]
    fn render_init_config_takes_runtime_values_from_the_image() {
        let config = render_init_config("app", &spec(), &image(), &[]);

        assert_eq!(config.hostname, "app");
        assert_eq!(config.entrypoint, vec!["/docker-entrypoint.sh".to_owned()]);
        assert_eq!(config.cmd.len(), 3);
        assert_eq!(config.workdir.as_deref(), Some("/"));
        assert_eq!(config.user.as_deref(), Some("root"));
        assert_eq!(config.disk_size_bytes, 20 * 1024 * 1024 * 1024);
    }

    #[test]
    fn render_init_config_maps_the_network_mode() {
        let mut disabled = spec();
        disabled.network.mode = NetMode::None;
        let mut tap = spec();
        tap.network.mode = NetMode::Tap;

        assert_eq!(
            render_init_config("a", &disabled, &image(), &[]).network,
            InitNetwork::None
        );
        assert_eq!(
            render_init_config("a", &tap, &image(), &[]).network,
            InitNetwork::Dhcp
        );
        assert_eq!(
            render_init_config("a", &spec(), &image(), &[]).network,
            InitNetwork::Dhcp
        );
    }

    #[test]
    fn render_init_config_env_override_replaces_the_image_value() {
        let config = render_init_config("a", &spec(), &image(), &["PATH=/bin".to_owned()]);

        assert_eq!(config.env, vec!["PATH=/bin".to_owned()]);
    }

    #[test]
    fn build_config_disk_is_byte_identical_for_equal_inputs() -> TestResult {
        let config = render_init_config("app", &spec(), &image(), &[]);

        assert_eq!(build_config_disk(&config)?, build_config_disk(&config)?);
        Ok(())
    }

    #[test]
    fn build_config_disk_frames_and_pads_to_four_kib() -> TestResult {
        let config = render_init_config("app", &spec(), &image(), &[]);

        let bytes = build_config_disk(&config)?;

        assert_eq!(bytes.len() % 4096, 0);
        assert!(bytes.len() >= 4096);
        assert_eq!(&bytes[..8], &CONFIG_MAGIC);
        assert_eq!(decode_frame(&bytes)?, config);
        let declared = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        assert!(
            bytes[CONFIG_HEADER_LEN + declared..]
                .iter()
                .all(|byte| *byte == 0)
        );
        Ok(())
    }

    #[test]
    fn build_config_disk_different_spec_changes_the_bytes() -> TestResult {
        let first = build_config_disk(&render_init_config("app", &spec(), &image(), &[]))?;
        let mut larger = spec();
        larger.disk = byte_size(40);
        let second = build_config_disk(&render_init_config("app", &larger, &image(), &[]))?;

        assert_ne!(first, second);
        Ok(())
    }

    #[test]
    fn config_disk_identity_tracks_the_framed_bytes() -> TestResult {
        let first = build_config_disk(&render_init_config("app", &spec(), &image(), &[]))?;
        let mut other = image();
        other.cmd = vec!["nginx".to_owned()];
        let second = build_config_disk(&render_init_config("app", &spec(), &other, &[]))?;

        assert_eq!(
            config_disk_identity("app", &first),
            config_disk_identity("app", &first)
        );
        assert_ne!(
            config_disk_identity("app", &first),
            config_disk_identity("app", &second)
        );
        assert!(config_disk_identity("app", &first).starts_with("iid-app-"));
        Ok(())
    }

    #[test]
    fn publish_init_config_writes_owner_only_artifacts() -> TestResult {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        let machine = paths.machine_dir("app")?;
        fs::create_dir_all(&machine)?;
        fs::set_permissions(&machine, fs::Permissions::from_mode(0o700))?;
        let config = render_init_config("app", &spec(), &image(), &[]);

        let published = publish_init_config(&paths, "app", &config)?;

        let image_path = paths.machine_config_image("app")?;
        let document = paths.machine_config_file("app", "config.json")?;
        assert_eq!(
            fs::metadata(&image_path)?.permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(&document)?.permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(paths.machine_config_dir("app")?)?
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(fs::read(&image_path)?.len() as u64, published.size);
        assert_eq!(decode_frame(&fs::read(&image_path)?)?, config);
        assert!(String::from_utf8(fs::read(&document)?)?.ends_with("\n"));

        // Republishing the same document produces the same bytes and identity.
        let again = publish_init_config(&paths, "app", &config)?;
        assert_eq!(again, published);
        Ok(())
    }
}
