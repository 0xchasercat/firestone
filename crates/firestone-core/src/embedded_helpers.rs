use std::{
    borrow::Cow,
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

use crate::{DependencyArtifact, DependencyManifest, ErrorKind, FirestoneError, Paths};

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

/// Publishes one pinned `deps.toml` artifact and returns its verified path.
///
/// [`ImageStore`](crate::ImageStore) implements this with the strict HTTPS
/// transport and the locked, no-follow publisher that the firmware and the
/// direct-boot kernel already use. The trait is the seam that keeps this module
/// free of the HTTP client and lets tests publish from local bytes.
pub trait PinnedArtifactInstaller {
    /// Installs `artifact` if it is absent and returns its published path.
    ///
    /// # Errors
    ///
    /// Returns `dependency` when the artifact cannot be downloaded or published,
    /// and `checksum` when the bytes do not match the pinned hash.
    fn install_pinned_artifact(
        &self,
        artifact: &DependencyArtifact,
    ) -> Result<PathBuf, FirestoneError>;
}

/// Returns the verified `firestone-init` payload for rootfs injection (§8.5).
///
/// The payload is resolved in one fixed order (SPEC §10.5, §17.2):
///
/// 1. the payload embedded by a standalone release build, hash-checked against
///    the digest recorded at build time;
/// 2. otherwise the pinned `[dependency.firestone-init]` artifact of
///    `deps.toml`, downloaded once through `installer` and read back only after
///    the publisher has verified its SHA-256.
///
/// The pinned copy lands in `<data>/bin` with mode 0644 because it is guest
/// data, never a host executable: the injection gives it its own 0755 header
/// inside the merged tar. A build with neither an embedded payload nor a pin
/// returns a `dependency` error that names both ways out.
///
/// # Errors
///
/// Returns `dependency` when no payload is embedded and none is pinned or
/// reachable, and `checksum` when either copy fails its hash.
pub fn firestone_init_payload(
    paths: &Paths,
    manifest: &DependencyManifest,
    architecture: &str,
    installer: &dyn PinnedArtifactInstaller,
) -> Result<Cow<'static, [u8]>, FirestoneError> {
    resolve_firestone_init_payload(
        embedded_helper(InternalHelper::FirestoneInit),
        paths,
        manifest,
        architecture,
        installer,
    )
}

/// The resolution order of [`firestone_init_payload`], with the embedded
/// payload supplied by the caller so tests can exercise every branch on a host
/// whose build embedded nothing.
fn resolve_firestone_init_payload(
    embedded: Option<EmbeddedHelper>,
    paths: &Paths,
    manifest: &DependencyManifest,
    architecture: &str,
    installer: &dyn PinnedArtifactInstaller,
) -> Result<Cow<'static, [u8]>, FirestoneError> {
    if let Some(helper) = embedded {
        verify_payload(helper)?;
        return Ok(Cow::Borrowed(helper.bytes()));
    }

    let artifact = manifest.firestone_init(architecture).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "this build carries no embedded firestone-init payload",
        )
        .with_hint(
            "OCI machines need the guest init: use an x86_64 standalone release, or build the \
             `firestone-init` release asset and pin it in deps.toml",
        )
        .with_source(error)
    })?;
    let installed = installer
        .install_pinned_artifact(&artifact)
        .map_err(|error| {
            let kind = error.kind();
            FirestoneError::new(
                kind,
                format!(
                    "cannot obtain the pinned firestone-init {} payload for {architecture}",
                    artifact.version
                ),
            )
            .with_hint(
                "a build without an embedded payload downloads the guest init once, on first \
                 OCI use: retry with network access to the pinned release, or use an x86_64 \
                 standalone release that embeds it",
            )
            .with_source(error)
        })?;
    read_pinned_payload(paths, &installed, &artifact)
}

/// Reads a published pinned payload back without following its final path
/// component, and re-checks its hash before the bytes are used.
fn read_pinned_payload(
    paths: &Paths,
    destination: &Path,
    artifact: &DependencyArtifact,
) -> Result<Cow<'static, [u8]>, FirestoneError> {
    let identity = ArtifactIdentity::pinned(artifact);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
    let mut file = options.open(destination).map_err(|source| {
        artifact_io_error(identity, "open installed file", destination, source)
    })?;
    paths.validate_owned_data_file_handle(
        destination,
        &format!("{} {}", identity.origin.adjective(), identity.dependency),
        identity.mode,
        &file,
    )?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|source| {
        artifact_io_error(identity, "read installed file", destination, source)
    })?;
    let actual = sha256_bytes(&bytes);
    if actual != identity.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "pinned '{}' at {} has checksum {actual}; expected {}",
                identity.dependency,
                destination.display(),
                identity.sha256
            ),
        )
        .with_hint(identity.origin.mismatch_hint()));
    }
    Ok(Cow::Owned(bytes))
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
    use std::{
        cell::Cell, fs, os::unix::fs::PermissionsExt as _, path::PathBuf, sync::Arc, thread,
    };

    use super::{
        DependencyArtifact, EmbeddedHelper, FirestoneError, InternalHelper,
        PinnedArtifactInstaller, install_pinned_artifact_with, materialize,
    };
    use crate::{DependencyManifest, ErrorKind, PathInputs, Paths};

    static TEST_BYTES: &[u8] = b"#!/bin/sh\nexit 0\n";
    const TEST_HELPER: EmbeddedHelper = EmbeddedHelper::new(
        InternalHelper::Passt,
        "test",
        "passt-test",
        "306c6ca7407560340797866e077e053627ad409277d1b9da58106fce4cf717cb",
        TEST_BYTES,
    );

    /// Stand-in for the pinned `firestone-init` asset.
    static INIT_BYTES: &[u8] = b"\x7fELF firestone-init payload\n";
    const INIT_SHA256: &str = "1bdd0e4f2842332f1c7895a67d61877f89dbed5e9130cb610824fc44ab1423b7";
    /// Stand-in for the payload a standalone release embeds, distinct from the
    /// pinned bytes so the winner of the resolution order is unambiguous.
    static EMBEDDED_BYTES: &[u8] = b"\x7fELF embedded firestone-init\n";
    const EMBEDDED_INIT: EmbeddedHelper = EmbeddedHelper::new(
        InternalHelper::FirestoneInit,
        "v0.1.0",
        "firestone-init-v0.1.0",
        "7a602cdceb0b0bb36fc79e86360cac6980a232fc86716372ae29b70906a2e85f",
        EMBEDDED_BYTES,
    );

    /// Publishes fixed bytes through the real locked publisher, standing in for
    /// [`crate::ImageStore`]'s HTTPS download, and counts its calls.
    struct FakeInstaller {
        paths: Paths,
        bytes: &'static [u8],
        calls: Cell<usize>,
    }

    impl FakeInstaller {
        fn new(paths: &Paths, bytes: &'static [u8]) -> Self {
            Self {
                paths: paths.clone(),
                bytes,
                calls: Cell::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    impl PinnedArtifactInstaller for FakeInstaller {
        fn install_pinned_artifact(
            &self,
            artifact: &DependencyArtifact,
        ) -> Result<PathBuf, FirestoneError> {
            self.calls.set(self.calls.get() + 1);
            install_pinned_artifact_with(&self.paths, artifact, |output| {
                output.write_all(self.bytes).map_err(|source| {
                    FirestoneError::new(ErrorKind::Dependency, "cannot write the fake payload")
                        .with_source(source)
                })
            })
        }
    }

    /// A manifest that pins `firestone-init` for x86_64 only.
    fn pinned_init_manifest(sha256: &str) -> Result<DependencyManifest, FirestoneError> {
        DependencyManifest::parse(&format!(
            r#"
manifest_version = 1
[dependency.firestone-init]
version = "v0.1.0"
availability = "binary"
architectures = ["x86_64"]
[dependency.firestone-init.x86_64]
asset = "firestone-init-v0.1.0-x86_64-unknown-linux-musl"
install_name = "firestone-init-v0.1.0"
url = "https://example.invalid/firestone-init-v0.1.0"
sha256 = "{sha256}"
"#
        ))
    }

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

    /// SPEC §10.5/§17.2. `deps.toml` pins the published `firestone-init` release
    /// for x86_64 as mode-0644 guest data, and for that architecture only: an
    /// aarch64 source build reports the dependency instead of downloading an
    /// x86_64 binary.
    #[test]
    fn firestone_init_bundled_pin_covers_x86_64_only() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        let manifest = DependencyManifest::bundled()?;
        let installer = FakeInstaller::new(&paths, INIT_BYTES);

        let artifact = manifest.firestone_init("x86_64")?;
        assert_eq!(artifact.version, crate::PINNED_FIRESTONE_INIT_VERSION);
        assert_eq!(artifact.install_name, "firestone-init-v0.1.0");
        assert_eq!(
            artifact.asset,
            "firestone-init-v0.1.0-x86_64-unknown-linux-musl"
        );
        assert_eq!(
            artifact.sha256,
            "1018c2dceecbf8d761d20ac40a07f28baada0e3cf2c3322af24fe7bb96b67d11"
        );
        assert_eq!(artifact.expected_mode(), 0o644);

        let error = match super::resolve_firestone_init_payload(
            None, &paths, &manifest, "aarch64", &installer,
        ) {
            Err(error) => error,
            Ok(_) => panic!("aarch64 has no pinned firestone-init payload"),
        };
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(
            error.message(),
            "this build carries no embedded firestone-init payload"
        );
        assert_eq!(installer.calls(), 0);
        Ok(())
    }

    /// The embedded payload wins outright: a standalone release never reaches
    /// the network for a payload it already carries.
    #[test]
    fn firestone_init_payload_embedded_wins_without_touching_the_installer()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        let manifest = pinned_init_manifest(INIT_SHA256)?;
        let installer = FakeInstaller::new(&paths, INIT_BYTES);

        let payload = super::resolve_firestone_init_payload(
            Some(EMBEDDED_INIT),
            &paths,
            &manifest,
            "x86_64",
            &installer,
        )?;

        assert_eq!(payload.as_ref(), EMBEDDED_BYTES);
        assert_eq!(installer.calls(), 0);
        Ok(())
    }

    /// With no embedded payload the pinned manifest entry is materialized once,
    /// through the same locked publisher, and read back at mode 0644.
    #[test]
    fn firestone_init_payload_without_an_embedded_payload_installs_the_pinned_artifact()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        let manifest = pinned_init_manifest(INIT_SHA256)?;
        let installer = FakeInstaller::new(&paths, INIT_BYTES);

        let payload =
            super::resolve_firestone_init_payload(None, &paths, &manifest, "x86_64", &installer)?;

        assert_eq!(payload.as_ref(), INIT_BYTES);
        assert_eq!(installer.calls(), 1);
        let installed = paths.binary_file("firestone-init-v0.1.0")?;
        assert_eq!(
            fs::metadata(&installed)?.permissions().mode() & 0o7777,
            0o644
        );
        Ok(())
    }

    /// Neither an embedded payload nor a pin is a clean dependency error that
    /// names both ways out, not a panic and not empty bytes.
    #[test]
    fn firestone_init_payload_without_an_embedded_payload_or_a_pin_reports_the_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        let manifest = DependencyManifest::parse(
            r#"
manifest_version = 1
[dependency.virtiofsd]
version = "v1.14.0"
availability = "binary"
[dependency.virtiofsd.x86_64]
asset = "virtiofsd"
install_name = "virtiofsd-v1.14.0"
url = "https://example.invalid/virtiofsd"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
        )?;
        let installer = FakeInstaller::new(&paths, INIT_BYTES);

        let error = match super::resolve_firestone_init_payload(
            None, &paths, &manifest, "x86_64", &installer,
        ) {
            Err(error) => error,
            Ok(_) => panic!("an unpinned firestone-init payload must fail"),
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(
            error.message(),
            "this build carries no embedded firestone-init payload"
        );
        let hint = error.hint().unwrap_or_default();
        assert!(hint.contains("deps.toml"), "{hint}");
        assert!(hint.contains("standalone release"), "{hint}");
        assert_eq!(installer.calls(), 0);
        Ok(())
    }

    /// A pinned payload whose bytes do not match the manifest hash is refused by
    /// the publisher, so the merged rootfs never sees unverified bytes.
    #[test]
    fn firestone_init_payload_pinned_checksum_mismatch_is_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        let manifest = pinned_init_manifest(INIT_SHA256)?;
        let installer = FakeInstaller::new(&paths, b"not the pinned payload\n");

        let error = match super::resolve_firestone_init_payload(
            None, &paths, &manifest, "x86_64", &installer,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a mismatched pinned payload must fail"),
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        Ok(())
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
