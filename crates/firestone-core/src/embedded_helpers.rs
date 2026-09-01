use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek as _, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use nix::{
    fcntl::{Flock, FlockArg},
    libc::{O_CLOEXEC, O_NOFOLLOW, O_NONBLOCK},
};
use sha2::{Digest as _, Sha256};
use tempfile::Builder as TempBuilder;

use crate::{DependencyArtifact, ErrorKind, FirestoneError, Paths};

const HELPER_LOCK_NAME: &str = ".embedded-helpers.lock";
const EXECUTABLE_MODE: u32 = 0o755;
const LOCK_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy)]
enum ArtifactOrigin {
    Embedded,
    Pinned,
}

impl ArtifactOrigin {
    const fn adjective(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Pinned => "pinned",
        }
    }

    const fn mismatch_hint(self) -> &'static str {
        match self {
            Self::Embedded => "remove the named file and retry with an intact Firestone release",
            Self::Pinned => "remove the named file and retry from a trusted network",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ArtifactIdentity<'a> {
    dependency: &'a str,
    install_name: &'a str,
    sha256: &'a str,
    mode: u32,
    expected_length: Option<u64>,
    origin: ArtifactOrigin,
}

impl ArtifactIdentity<'_> {
    fn embedded(helper: EmbeddedHelper) -> ArtifactIdentity<'static> {
        ArtifactIdentity {
            dependency: helper.kind().dependency(),
            install_name: helper.install_name(),
            sha256: helper.sha256(),
            mode: EXECUTABLE_MODE,
            expected_length: Some(helper.bytes().len() as u64),
            origin: ArtifactOrigin::Embedded,
        }
    }

    fn pinned(artifact: &DependencyArtifact) -> ArtifactIdentity<'_> {
        ArtifactIdentity {
            dependency: &artifact.dependency,
            install_name: &artifact.install_name,
            sha256: &artifact.sha256,
            mode: artifact.expected_mode(),
            expected_length: None,
            origin: ArtifactOrigin::Pinned,
        }
    }
}

/// A helper executable carried inside a standalone Firestone release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalHelper {
    CloudHypervisor,
    Passt,
    QemuImg,
    /// The guest PID 1 injected into a packed OCI rootfs (SPEC §10.5, §17.2).
    ///
    /// Unlike the other three this is a Firestone-owned build artifact, not a
    /// third-party download, and it is never materialized into `<data>/bin`:
    /// its only consumer is the rootfs injection of §8.5.
    FirestoneInit,
}

impl InternalHelper {
    #[must_use]
    pub const fn dependency(self) -> &'static str {
        match self {
            Self::CloudHypervisor => "cloud-hypervisor",
            Self::Passt => "passt",
            Self::QemuImg => "qemu-img",
            Self::FirestoneInit => "firestone-init",
        }
    }

    #[must_use]
    pub const fn system_program(self) -> &'static str {
        self.dependency()
    }
}

/// Immutable metadata and bytes selected by the release build.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedHelper {
    kind: InternalHelper,
    version: &'static str,
    install_name: &'static str,
    sha256: &'static str,
    bytes: &'static [u8],
}

impl EmbeddedHelper {
    pub const fn new(
        kind: InternalHelper,
        version: &'static str,
        install_name: &'static str,
        sha256: &'static str,
        bytes: &'static [u8],
    ) -> Self {
        Self {
            kind,
            version,
            install_name,
            sha256,
            bytes,
        }
    }

    #[must_use]
    pub const fn kind(self) -> InternalHelper {
        self.kind
    }

    #[must_use]
    pub const fn version(self) -> &'static str {
        self.version
    }

    #[must_use]
    pub const fn install_name(self) -> &'static str {
        self.install_name
    }

    #[must_use]
    pub const fn sha256(self) -> &'static str {
        self.sha256
    }

    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        self.bytes
    }
}

include!(concat!(env!("OUT_DIR"), "/embedded_helpers.rs"));

/// Returns the target-selected embedded payload, if this is a standalone build.
#[must_use]
pub const fn embedded_helper(kind: InternalHelper) -> Option<EmbeddedHelper> {
    match kind {
        InternalHelper::CloudHypervisor => BUILD_EMBEDDED_CLOUD_HYPERVISOR,
        InternalHelper::Passt => BUILD_EMBEDDED_PASST,
        InternalHelper::QemuImg => BUILD_EMBEDDED_QEMU_IMG,
        InternalHelper::FirestoneInit => BUILD_EMBEDDED_FIRESTONE_INIT,
    }
}

/// Returns the verified `firestone-init` payload for rootfs injection (§8.5).
///
/// The payload is not published into `<data>/bin` like the other helpers: the
/// pull pipeline writes these bytes straight into the merged tar at
/// `/sbin/firestone-init`, so the only check that applies is the embedded
/// checksum. A build with no payload — every development build until the
/// standalone `firestone-init` release is pinned in `deps.toml` (§17.2) —
/// returns a `dependency` error that names it.
///
/// # Errors
///
/// Returns `dependency` when this build carries no payload, and `checksum` when
/// the embedded bytes do not match the hash recorded at build time.
pub fn firestone_init_payload() -> Result<&'static [u8], FirestoneError> {
    let helper = embedded_helper(InternalHelper::FirestoneInit).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "this build carries no embedded firestone-init payload",
        )
        .with_hint(
            "OCI machines need the guest init: use an x86_64 standalone release, or build the \
             `firestone-init` release asset and pin it in deps.toml",
        )
    })?;
    verify_payload(helper)?;
    Ok(helper.bytes())
}

/// Materializes one embedded helper and returns its stable versioned path.
///
/// Development and compile-only targets without embedded payloads return `None`;
/// callers retain their existing system-program behavior on those targets.
pub fn materialize_embedded_helper(
    paths: &Paths,
    kind: InternalHelper,
) -> Result<Option<PathBuf>, FirestoneError> {
    embedded_helper(kind)
        .map(|helper| materialize(paths, helper).map(Some))
        .unwrap_or(Ok(None))
}

fn materialize(paths: &Paths, helper: EmbeddedHelper) -> Result<PathBuf, FirestoneError> {
    verify_payload(helper)?;
    let identity = ArtifactIdentity::embedded(helper);
    materialize_with(paths, identity, |output| {
        output.write_all(helper.bytes()).map_err(|source| {
            artifact_io_error(identity, "write partial file", &paths.bin_dir(), source)
        })
    })
}

/// Publishes one downloaded manifest artifact through the locked, no-follow
/// path used for embedded helpers.
pub(crate) fn install_pinned_artifact_with(
    paths: &Paths,
    artifact: &DependencyArtifact,
    write_source: impl FnOnce(&mut dyn Write) -> Result<(), FirestoneError>,
) -> Result<PathBuf, FirestoneError> {
    materialize_with(paths, ArtifactIdentity::pinned(artifact), write_source)
}

/// Opens and verifies a published manifest artifact without following its
/// final path component.
pub(crate) fn verified_pinned_artifact(
    paths: &Paths,
    artifact: &DependencyArtifact,
) -> Result<PathBuf, FirestoneError> {
    paths.validate_bin_data_directory()?;
    let identity = ArtifactIdentity::pinned(artifact);
    let destination = paths.binary_file(identity.install_name)?;
    if verify_installed(paths, &destination, identity)? {
        return Ok(destination);
    }
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "pinned '{}' is unavailable at {}",
            identity.dependency,
            destination.display()
        ),
    )
    .with_hint("retry start so Firestone can install the selected pinned firmware"))
}

fn materialize_with(
    paths: &Paths,
    identity: ArtifactIdentity<'_>,
    write_source: impl FnOnce(&mut dyn Write) -> Result<(), FirestoneError>,
) -> Result<PathBuf, FirestoneError> {
    paths.ensure_owned_data_directory(&paths.bin_dir(), "binary directory", true)?;
    paths.validate_bin_data_directory()?;
    let _lock = acquire_helper_lock(paths)?;
    paths.validate_bin_data_directory()?;

    let destination = paths.binary_file(identity.install_name)?;
    if verify_installed(paths, &destination, identity)? {
        return Ok(destination);
    }

    let mut partial = TempBuilder::new()
        .prefix(&format!(".{}.", identity.install_name))
        .suffix(".partial")
        .tempfile_in(paths.bin_dir())
        .map_err(|source| {
            artifact_io_error(identity, "create partial file", &paths.bin_dir(), source)
        })?;
    write_source(partial.as_file_mut())?;
    partial.as_file_mut().flush().map_err(|source| {
        artifact_io_error(identity, "flush partial file", partial.path(), source)
    })?;
    partial
        .as_file()
        .set_permissions(fs::Permissions::from_mode(identity.mode))
        .map_err(|source| {
            artifact_io_error(identity, "set partial mode", partial.path(), source)
        })?;
    partial.as_file().sync_all().map_err(|source| {
        artifact_io_error(identity, "sync partial file", partial.path(), source)
    })?;
    partial
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|source| {
            artifact_io_error(identity, "rewind partial file", partial.path(), source)
        })?;
    let readback = sha256_reader(partial.as_file_mut()).map_err(|source| {
        artifact_io_error(identity, "hash partial file", partial.path(), source)
    })?;
    if readback != identity.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "{} '{}' partial checksum mismatch: expected {}, got {readback}",
                identity.origin.adjective(),
                identity.dependency,
                identity.sha256
            ),
        )
        .with_hint(identity.origin.mismatch_hint()));
    }

    match partial.persist_noclobber(&destination) {
        Ok(file) => file.sync_all().map_err(|source| {
            artifact_io_error(identity, "sync published file", &destination, source)
        })?,
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            drop(error.file);
        }
        Err(error) => {
            return Err(artifact_io_error(
                identity,
                "publish partial file",
                &destination,
                error.error,
            ));
        }
    }
    sync_directory(&paths.bin_dir(), identity)?;
    if !verify_installed(paths, &destination, identity)? {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{} '{}' was not present after publication at {}",
                identity.origin.adjective(),
                identity.dependency,
                destination.display()
            ),
        )
        .with_hint("check the Firestone data directory permissions and free space"));
    }
    Ok(destination)
}

fn verify_payload(helper: EmbeddedHelper) -> Result<(), FirestoneError> {
    let actual = sha256_bytes(helper.bytes());
    if actual == helper.sha256() {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Checksum,
        format!(
            "embedded `{}` checksum mismatch: expected {}, got {actual}",
            helper.kind().dependency(),
            helper.sha256()
        ),
    )
    .with_hint("replace the Firestone executable with an intact signed release"))
}

fn acquire_helper_lock(paths: &Paths) -> Result<Flock<File>, FirestoneError> {
    let lock_path = paths.binary_file(HELPER_LOCK_NAME)?;
    let mut create = OpenOptions::new();
    create
        .read(true)
        .write(true)
        .create_new(true)
        .mode(LOCK_MODE)
        .custom_flags(O_NOFOLLOW | O_CLOEXEC);
    let file = match create.open(&lock_path) {
        Ok(file) => {
            file.set_permissions(fs::Permissions::from_mode(LOCK_MODE))
                .map_err(|source| lock_io_error("set mode", &lock_path, source))?;
            file.sync_all()
                .map_err(|source| lock_io_error("sync", &lock_path, source))?;
            sync_plain_directory(&paths.bin_dir())
                .map_err(|source| lock_io_error("sync parent of", &lock_path, source))?;
            file
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing
                .read(true)
                .write(true)
                .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
            existing
                .open(&lock_path)
                .map_err(|source| lock_io_error("open", &lock_path, source))?
        }
        Err(source) => return Err(lock_io_error("create", &lock_path, source)),
    };
    paths.validate_owned_data_file_handle(&lock_path, "embedded helper lock", LOCK_MODE, &file)?;
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, source)| lock_io_error("acquire", &lock_path, io::Error::from(source)))
}

fn verify_installed(
    paths: &Paths,
    destination: &Path,
    identity: ArtifactIdentity<'_>,
) -> Result<bool, FirestoneError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
    let mut file = match options.open(destination) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(artifact_io_error(
                identity,
                "open installed file",
                destination,
                source,
            ));
        }
    };
    paths.validate_owned_data_file_handle(
        destination,
        &format!("{} {}", identity.origin.adjective(), identity.dependency),
        identity.mode,
        &file,
    )?;
    let metadata = file.metadata().map_err(|source| {
        artifact_io_error(identity, "inspect installed file", destination, source)
    })?;
    if let Some(expected_length) = identity.expected_length {
        if metadata.len() != expected_length {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "{} '{}' at {} has length {}; expected {}",
                    identity.origin.adjective(),
                    identity.dependency,
                    destination.display(),
                    metadata.len(),
                    expected_length
                ),
            )
            .with_hint(identity.origin.mismatch_hint()));
        }
    }
    let actual = sha256_reader(&mut file).map_err(|source| {
        artifact_io_error(identity, "hash installed file", destination, source)
    })?;
    if actual != identity.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "{} '{}' at {} has checksum {actual}; expected {}",
                identity.origin.adjective(),
                identity.dependency,
                destination.display(),
                identity.sha256
            ),
        )
        .with_hint(identity.origin.mismatch_hint()));
    }
    Ok(true)
}

fn sync_directory(path: &Path, identity: ArtifactIdentity<'_>) -> Result<(), FirestoneError> {
    sync_plain_directory(path)
        .map_err(|source| artifact_io_error(identity, "sync binary directory", path, source))
}

fn sync_plain_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_reader(reader: &mut dyn Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn artifact_io_error(
    identity: ArtifactIdentity<'_>,
    operation: &str,
    path: &Path,
    source: io::Error,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "cannot {operation} for {} '{}' at {}",
            identity.origin.adjective(),
            identity.dependency,
            path.display()
        ),
    )
    .with_hint("check the Firestone data directory permissions and free space")
    .with_source(source)
}

fn lock_io_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot {operation} embedded helper lock {}", path.display()),
    )
    .with_hint("check the Firestone binary directory permissions")
    .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, sync::Arc, thread};

    use super::{EmbeddedHelper, InternalHelper, materialize};
    use crate::{ErrorKind, PathInputs, Paths};

    static TEST_BYTES: &[u8] = b"#!/bin/sh\nexit 0\n";
    const TEST_HELPER: EmbeddedHelper = EmbeddedHelper::new(
        InternalHelper::Passt,
        "test",
        "passt-test",
        "306c6ca7407560340797866e077e053627ad409277d1b9da58106fce4cf717cb",
        TEST_BYTES,
    );

    #[test]
    fn internal_helper_names_cover_embedded_vmm_and_sidecars() {
        assert_eq!(
            InternalHelper::CloudHypervisor.dependency(),
            "cloud-hypervisor"
        );
        assert_eq!(
            InternalHelper::CloudHypervisor.system_program(),
            "cloud-hypervisor"
        );
        assert_eq!(InternalHelper::Passt.dependency(), "passt");
        assert_eq!(InternalHelper::QemuImg.dependency(), "qemu-img");
        assert_eq!(InternalHelper::FirestoneInit.dependency(), "firestone-init");
    }

    /// SPEC §10.5/§17.2. Until the standalone `firestone-init` release is built
    /// and pinned, every development build answers the injection feed with one
    /// actionable dependency error rather than an empty or silent payload.
    #[test]
    fn firestone_init_payload_without_an_embedded_release_reports_the_dependency() {
        match super::firestone_init_payload() {
            Ok(bytes) => {
                // A standalone release build does carry the payload; when it
                // does, it must be non-empty and hash-verified (which
                // `firestone_init_payload` already checked).
                assert!(!bytes.is_empty());
            }
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Dependency);
                assert_eq!(
                    error.message(),
                    "this build carries no embedded firestone-init payload"
                );
                let hint = error.hint().unwrap_or_default();
                assert!(hint.contains("deps.toml"), "{hint}");
            }
        }
    }

    #[test]
    fn materialize_absent_helper_publishes_exact_executable()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;

        let installed = materialize(&paths, TEST_HELPER)?;

        assert_eq!(fs::read(&installed)?, TEST_BYTES);
        assert_eq!(
            fs::metadata(&installed)?.permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(materialize(&paths, TEST_HELPER)?, installed);
        Ok(())
    }

    #[test]
    fn materialize_existing_mismatch_refuses_overwrite() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        paths.ensure_owned_data_directory(&paths.bin_dir(), "binary directory", true)?;
        let destination = paths.binary_file(TEST_HELPER.install_name())?;
        fs::write(&destination, b"wrong")?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;

        let error = match materialize(&paths, TEST_HELPER) {
            Err(error) => error,
            Ok(_) => panic!("mismatched helper must fail"),
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert_eq!(fs::read(destination)?, b"wrong");
        Ok(())
    }

    #[test]
    fn materialize_symlink_destination_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        paths.ensure_owned_data_directory(&paths.bin_dir(), "binary directory", true)?;
        let outside = directory.path().join("outside");
        fs::write(&outside, b"outside")?;
        let destination = paths.binary_file(TEST_HELPER.install_name())?;
        symlink(&outside, &destination)?;

        let error = match materialize(&paths, TEST_HELPER) {
            Err(error) => error,
            Ok(_) => panic!("symlink helper must fail"),
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(outside)?, b"outside");
        Ok(())
    }

    #[test]
    fn materialize_concurrent_callers_share_verified_winner()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = Arc::new(test_paths(directory.path())?);
        let workers = (0..8)
            .map(|_| {
                let paths = Arc::clone(&paths);
                thread::spawn(move || materialize(&paths, TEST_HELPER))
            })
            .collect::<Vec<_>>();

        let mut installed = None;
        for worker in workers {
            let path = worker.join().map_err(|_| "helper worker panicked")??;
            if let Some(expected) = &installed {
                assert_eq!(&path, expected);
            } else {
                installed = Some(path);
            }
        }
        assert_eq!(
            fs::read(installed.ok_or("no helper was installed")?)?,
            TEST_BYTES
        );
        Ok(())
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
}
