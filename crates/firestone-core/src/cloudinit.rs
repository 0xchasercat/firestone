use std::{
    fs::{self, DirBuilder, File},
    io::{self, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
};

use fatfs::{
    Date, DateTime, FileSystem, FormatVolumeOptions, FsOptions, Time, TimeProvider, format_volume,
};
use minijinja::{Environment, context};
use serde::Serialize;
use sha2::{Digest, Sha256};
use ssh_key::PublicKey;

use crate::{ErrorKind, FirestoneError, MachineSpec, Paths, atomic};

const FIRESTONE_TEMPLATE: &str = include_str!("../../../templates/cloud-init.yaml");
const MIME_BOUNDARY: &str = "===============firestone==";
const VOLUME_LABEL: [u8; 11] = *b"CIDATA     ";
const VOLUME_ID: u32 = 0x4653_0001;
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

/// Renders M1's Firestone-owned cloud-init content without publishing it.
///
/// User data and network configuration enter the product in M3. Rejecting both
/// here prevents an M1 start from silently booting with incomplete provisioning.
pub fn render_cloud_init(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
) -> Result<RenderedCloudInit, FirestoneError> {
    paths.machine_dir(name)?;
    reject_deferred_inputs(spec)?;

    let user_data = if spec.cloud_init.provisioning {
        let firestone_pubkey = read_firestone_public_key(paths)?;
        let user_keys = read_user_public_keys(&spec.cloud_init.ssh_keys)?;
        let mounts = spec
            .mounts
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
            .collect::<Result<Vec<_>, FirestoneError>>()?;
        let firestone_part =
            render_firestone_part(name, &spec.user, &firestone_pubkey, &user_keys, &mounts)?;
        render_multipart(&firestone_part)
    } else {
        Vec::new()
    };

    let instance_id = instance_id(name, &user_data);
    let meta_data = format!(
        "instance-id: {}\nlocal-hostname: {}\n",
        json_string(&instance_id)?,
        json_string(name)?
    )
    .into_bytes();

    Ok(RenderedCloudInit {
        instance_id,
        meta_data,
        user_data,
        network_config: None,
    })
}

/// Renders inspection files and atomically publishes a deterministic CIDATA disk.
pub fn publish_seed(
    paths: &Paths,
    name: &str,
    spec: &MachineSpec,
) -> Result<RenderedCloudInit, FirestoneError> {
    let rendered = render_cloud_init(paths, name, spec)?;
    paths.validate_machine_data_directory(name)?;
    let seed_dir = paths.machine_seed_dir(name)?;
    ensure_seed_directory(&seed_dir)?;
    paths.validate_machine_data_directory(name)?;

    atomic::write(
        &paths.machine_seed_file(name, "meta-data")?,
        &rendered.meta_data,
    )?;
    atomic::write(
        &paths.machine_seed_file(name, "user-data")?,
        &rendered.user_data,
    )?;

    let network_path = paths.machine_seed_file(name, "network-config")?;
    match &rendered.network_config {
        Some(network_config) => atomic::write(&network_path, network_config)?,
        None => remove_optional_file(&network_path)?,
    }

    let seed_image = paths.machine_seed_image(name)?;
    atomic::write_stream(&seed_image, |file| write_seed_image(file, &rendered))?;
    Ok(rendered)
}

fn reject_deferred_inputs(spec: &MachineSpec) -> Result<(), FirestoneError> {
    if spec.cloud_init.user_data.is_some() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            "cloud_init.user_data is not supported during M1 seed rendering",
        )
        .with_hint("remove cloud_init.user_data; user-provided multipart data is enabled in M3"));
    }
    if spec.cloud_init.network_config.is_some() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            "cloud_init.network_config is not supported during M1 seed rendering",
        )
        .with_hint(
            "remove cloud_init.network_config; its instance-id formula and publication are enabled in M3",
        ));
    }
    Ok(())
}

fn read_firestone_public_key(paths: &Paths) -> Result<String, FirestoneError> {
    let path = paths.ssh_public_key();
    paths.validate_ssh_data_directory()?;
    let metadata = fs::symlink_metadata(&path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot read Firestone SSH public key at {}", path.display()),
        )
        .with_hint("run `firestone doctor --fix` to generate the Firestone SSH key")
        .with_source(source)
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "Firestone SSH public key {} is not a regular non-symlink file",
                path.display()
            ),
        )
        .with_hint("run `firestone doctor --fix` to regenerate the Firestone SSH key"));
    }
    let keys = read_public_key_file(&path, "Firestone SSH public key", ErrorKind::Dependency)?;
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
    keys.into_iter().next().ok_or_else(|| {
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

fn read_user_public_keys(paths: &[std::path::PathBuf]) -> Result<Vec<String>, FirestoneError> {
    let mut keys = Vec::new();
    for path in paths {
        let mut file_keys =
            read_public_key_file(path, "cloud_init.ssh_keys entry", ErrorKind::InvalidSpec)?;
        keys.append(&mut file_keys);
    }
    Ok(keys)
}

fn read_public_key_file(
    path: &Path,
    description: &str,
    kind: ErrorKind,
) -> Result<Vec<String>, FirestoneError> {
    let bytes = fs::read(path).map_err(|source| {
        let hint = if kind == ErrorKind::Dependency {
            "run `firestone doctor --fix` to generate the Firestone SSH key"
        } else {
            "correct the path or replace it with a readable OpenSSH public-key file"
        };
        FirestoneError::new(
            kind,
            format!("cannot read {description} at {}", path.display()),
        )
        .with_hint(hint)
        .with_source(source)
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|source| {
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
        PublicKey::from_openssh(line).map_err(|source| {
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
        keys.push(line.to_owned());
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

fn render_firestone_part(
    name: &str,
    user: &str,
    firestone_pubkey: &str,
    user_keys: &[String],
    mounts: &[TemplateMount],
) -> Result<Vec<u8>, FirestoneError> {
    let name = json_string(name)?;
    let firestone_pubkey = json_string(firestone_pubkey)?;
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

fn render_multipart(firestone_part: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(firestone_part.len() + 320);
    bytes.extend_from_slice(
        format!(
            "Content-Type: multipart/mixed; boundary=\"{MIME_BOUNDARY}\"\r\nMIME-Version: 1.0\r\n\r\n--{MIME_BOUNDARY}\r\nContent-Type: text/cloud-config; charset=\"utf-8\"\r\nContent-Disposition: attachment; filename=\"firestone-cloud-config.yaml\"\r\n\r\n"
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(firestone_part);
    bytes.extend_from_slice(format!("--{MIME_BOUNDARY}--\r\n").as_bytes());
    bytes
}

fn instance_id(name: &str, user_data: &[u8]) -> String {
    let digest = Sha256::digest(user_data);
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
                    ErrorKind::InvalidSpec,
                    format!("seed path {} is not a real directory", path.display()),
                )
                .with_hint(
                    "remove the path and retry so Firestone can create the seed directory",
                ));
            }
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(FirestoneError::new(
                    ErrorKind::InvalidSpec,
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
        path::PathBuf,
    };

    use fatfs::{FileSystem, FsOptions};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use crate::{ErrorKind, MachineSpec, MountSpec, PathInputs, Paths};

    use super::{SEED_IMAGE_SIZE, VOLUME_ID, publish_seed, render_cloud_init};

    const FIRESTONE_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P firestone@test\n";
    const USER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN6eVqR0T6lRuT6aGvdMVhZkcNrD1s8g8J3RYfLZBuo5 user@test\n";
    const GOLDEN_MULTIPART: &[u8] = include_bytes!("../testdata/cloud-init.multipart");
    const GOLDEN_SEED_SHA256: &str =
        "2a30a56a100c8c8897b8d7457fa322ed35f0f2c5a6268915e0bfd09db5264b37";

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
                firestone_home: Some(root),
                firestone_config_dir: None,
                firestone_data_dir: None,
                firestone_runtime_dir: None,
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: nix::unistd::getuid().as_raw(),
            })?;
            fs::create_dir_all(paths.machine_dir("demo")?)?;
            fs::create_dir_all(paths.ssh_dir())?;
            if with_key {
                fs::write(paths.ssh_public_key(), FIRESTONE_KEY)?;
            }
            Ok(Self { _temp: temp, paths })
        }
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
    fn multipart_firestone_inputs_matches_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
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

        assert_eq!(rendered.user_data, GOLDEN_MULTIPART);
        assert_eq!(rendered.instance_id, "iid-demo-e3195d705953");
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
    fn seed_publication_rebuild_is_byte_identical_and_matches_golden_hash()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let spec = MachineSpec::default();

        let first_render = publish_seed(&fixture.paths, "demo", &spec)?;
        let first = fs::read(fixture.paths.machine_seed_image("demo")?)?;
        let first_hash = hex_digest(&first);
        let second_render = publish_seed(&fixture.paths, "demo", &spec)?;
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
    fn configured_user_data_returns_stable_invalid_spec_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.user_data = Some(PathBuf::from("/tmp/user-data"));

        let error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("configured user data should fail")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert_eq!(
            error.message(),
            "cloud_init.user_data is not supported during M1 seed rendering"
        );
        assert_eq!(
            error.hint(),
            Some("remove cloud_init.user_data; user-provided multipart data is enabled in M3")
        );
        Ok(())
    }

    #[test]
    fn configured_network_config_returns_stable_invalid_spec_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let mut spec = MachineSpec::default();
        spec.cloud_init.network_config = Some(PathBuf::from("/tmp/network-config"));

        let error = render_cloud_init(&fixture.paths, "demo", &spec)
            .err()
            .ok_or("configured network config should fail")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert_eq!(
            error.message(),
            "cloud_init.network_config is not supported during M1 seed rendering"
        );
        assert_eq!(
            error.hint(),
            Some(
                "remove cloud_init.network_config; its instance-id formula and publication are enabled in M3"
            )
        );
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
                .contains("cannot read Firestone SSH public key")
        );
        assert_eq!(
            error.hint(),
            Some("run `firestone doctor --fix` to generate the Firestone SSH key")
        );
        Ok(())
    }

    fn hex_digest(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>(),
            ["meta-data", "user-data"]
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
        Ok(())
    }
}
