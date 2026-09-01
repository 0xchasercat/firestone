//! Immutable machine snapshots (SPEC section 23).
//!
//! A snapshot is a directory under `machines/<name>/snapshots/<snapshot>/`
//! holding the metadata document, a qcow2 overlay copy sharing the machine's
//! base image, byte copies of `firestone.toml` and the published
//! `vmconfig.json`, and, for a warm snapshot, the Cloud Hypervisor `vmstate/`
//! directory. Every snapshot is built inside a `.partial-<snapshot>` directory
//! and published with one rename, so a partially written snapshot is never
//! listed or restored.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{ErrorKind, FirestoneError};

/// Mode of every file Firestone publishes inside a snapshot directory.
pub const SNAPSHOT_FILE_MODE: u32 = 0o600;
/// Mode of every directory Firestone publishes inside a snapshot directory.
pub const SNAPSHOT_DIR_MODE: u32 = 0o700;
/// Schema version of `metadata.json` and `restore-request.json`.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
/// Longest snapshot identifier Firestone accepts.
pub const MAX_SNAPSHOT_NAME_BYTES: usize = 64;
/// Largest `metadata.json` or `restore-request.json` Firestone reads.
pub const MAX_SNAPSHOT_DOCUMENT_BYTES: u64 = 64 * 1024;
/// Block size used by the sparse copy.
const SPARSE_BLOCK_BYTES: usize = 128 * 1024;

/// Whether a snapshot captured guest memory as well as the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// Disk and spec only, taken while the machine was stopped or created.
    Cold,
    /// Disk, spec and Cloud Hypervisor VM state, taken while the machine ran.
    Warm,
}

impl SnapshotKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
    }
}

impl std::fmt::Display for SnapshotKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// `metadata.json` published inside every snapshot directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMetadata {
    pub schema_version: u32,
    pub kind: SnapshotKind,
    pub created_at: String,
    /// Stored image the copied overlay backs onto, when the machine has one.
    pub image_id: Option<String>,
    pub firestone_version: String,
    pub disk_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

impl SnapshotMetadata {
    /// Rejects a document Firestone did not write or can no longer read.
    pub fn validate(&self, path: &Path) -> Result<(), FirestoneError> {
        if self.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "snapshot metadata '{}' has schema version {}; this Firestone writes {SNAPSHOT_SCHEMA_VERSION}",
                    path.display(),
                    self.schema_version
                ),
            )
            .with_hint("remove the snapshot or use the Firestone release that wrote it"));
        }
        if self.created_at.is_empty() || self.firestone_version.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("snapshot metadata '{}' is incomplete", path.display()),
            )
            .with_hint("remove the snapshot directory and take a new snapshot"));
        }
        match (self.kind, self.memory_bytes) {
            (SnapshotKind::Warm, Some(_)) | (SnapshotKind::Cold, None) => Ok(()),
            (SnapshotKind::Warm, None) => Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "warm snapshot metadata '{}' has no memory_bytes",
                    path.display()
                ),
            )
            .with_hint("remove the snapshot directory and take a new snapshot")),
            (SnapshotKind::Cold, Some(_)) => Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "cold snapshot metadata '{}' records memory_bytes",
                    path.display()
                ),
            )
            .with_hint("remove the snapshot directory and take a new snapshot")),
        }
    }
}

/// `machines/<name>/restore-request.json`: the marker the shim consumes to turn
/// one launch into a `vm.restore` instead of `vm.create` plus `vm.boot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub schema_version: u32,
    pub snapshot: String,
    pub snapshot_dir: PathBuf,
    pub vmstate_dir: PathBuf,
    /// SHA-256 of the snapshot's `vmconfig.json`, which the launch republishes
    /// from the restored spec and compares byte for byte.
    pub vmconfig_sha256: String,
    pub created_at: String,
}

impl RestoreRequest {
    /// Rejects a marker this Firestone cannot honor.
    pub fn validate(&self, path: &Path) -> Result<(), FirestoneError> {
        if self.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "restore marker '{}' has schema version {}; this Firestone writes {SNAPSHOT_SCHEMA_VERSION}",
                    path.display(),
                    self.schema_version
                ),
            )
            .with_hint("remove the restore marker and retry the restore"));
        }
        validate_snapshot_name(&self.snapshot)?;
        if !self.snapshot_dir.is_absolute() || !self.vmstate_dir.is_absolute() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("restore marker '{}' has a relative path", path.display()),
            )
            .with_hint("remove the restore marker and retry the restore"));
        }
        if self.vmconfig_sha256.len() != 64
            || !self
                .vmconfig_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "restore marker '{}' has an invalid vmconfig digest",
                    path.display()
                ),
            )
            .with_hint("remove the restore marker and retry the restore"));
        }
        Ok(())
    }
}

/// Returns the deterministic automatic identifier `snap-<yyyymmdd>-<hhmmss>`.
///
/// The timestamp is UTC, taken from the RFC 3339 rendering Firestone already
/// persists everywhere else, so two Firestone builds name the same instant the
/// same way.
#[must_use]
pub fn auto_snapshot_name(now: jiff::Timestamp) -> String {
    let rendered = now.to_string();
    let digits = rendered
        .chars()
        .filter(char::is_ascii_digit)
        .take(14)
        .collect::<String>();
    match (digits.get(..8), digits.get(8..14)) {
        (Some(date), Some(time)) => format!("snap-{date}-{time}"),
        _ => "snap-00000000-000000".to_owned(),
    }
}

/// Accepts one snapshot identifier: 1 to 64 bytes of `[A-Za-z0-9._-]` that
/// neither starts with `.` or `-` nor is `.` or `..`.
pub fn validate_snapshot_name(snapshot: &str) -> Result<(), FirestoneError> {
    let valid = !snapshot.is_empty()
        && snapshot.len() <= MAX_SNAPSHOT_NAME_BYTES
        && !snapshot.starts_with('.')
        && !snapshot.starts_with('-')
        && snapshot
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("snapshot name {snapshot:?} is not a valid snapshot identifier"),
    )
    .with_hint(format!(
        "use 1 to {MAX_SNAPSHOT_NAME_BYTES} characters from A-Z a-z 0-9 . _ - that do not start with '.' or '-'"
    )))
}

/// Byte counts observed by one sparse copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseCopy {
    /// Length of the destination file, holes included.
    pub apparent_bytes: u64,
    /// Bytes actually written; all-zero blocks are skipped with a seek.
    pub written_bytes: u64,
}

/// Copies one regular file while preserving its holes.
///
/// Cloud Hypervisor writes `memory-ranges` with an apparent size equal to the
/// guest's RAM but leaves most of it unallocated, and a machine overlay is
/// sparse for the same reason. A plain `std::fs::copy` would materialize every
/// hole, so each `SPARSE_BLOCK_BYTES` block that is entirely zero is skipped
/// with a seek instead of written, and the destination is truncated to the
/// source length so a trailing hole survives too.
pub fn sparse_copy_file(
    source: &Path,
    destination: &Path,
    mode: u32,
) -> Result<SparseCopy, FirestoneError> {
    let mut reader = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(source)
        .map_err(|error| snapshot_io_error("open", source, error))?;
    let metadata = reader
        .metadata()
        .map_err(|error| snapshot_io_error("inspect", source, error))?;
    if !metadata.is_file() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("'{}' is not a regular file", source.display()),
        )
        .with_hint("restore the owned machine file and retry"));
    }
    let apparent_bytes = metadata.len();

    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(destination)
        .map_err(|error| snapshot_io_error("create", destination, error))?;

    let mut buffer = vec![0_u8; SPARSE_BLOCK_BYTES];
    let mut offset = 0_u64;
    let mut written_bytes = 0_u64;
    loop {
        let read = read_block(&mut reader, &mut buffer)
            .map_err(|error| snapshot_io_error("read", source, error))?;
        if read == 0 {
            break;
        }
        let block = buffer.get(..read).unwrap_or_default();
        if block.iter().any(|byte| *byte != 0) {
            writer
                .seek(SeekFrom::Start(offset))
                .map_err(|error| snapshot_io_error("seek", destination, error))?;
            writer
                .write_all(block)
                .map_err(|error| snapshot_io_error("write", destination, error))?;
            written_bytes = written_bytes.saturating_add(read as u64);
        }
        offset = offset.saturating_add(read as u64);
    }
    writer
        .set_len(apparent_bytes)
        .map_err(|error| snapshot_io_error("truncate", destination, error))?;
    writer
        .sync_all()
        .map_err(|error| snapshot_io_error("sync", destination, error))?;
    Ok(SparseCopy {
        apparent_bytes,
        written_bytes,
    })
}

fn read_block(reader: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let slice = buffer.get_mut(filled..).unwrap_or_default();
        match reader.read(slice) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Creates one owner-only directory, failing when it already exists.
pub fn create_snapshot_directory(path: &Path) -> Result<(), FirestoneError> {
    fs::DirBuilder::new()
        .recursive(false)
        .mode(SNAPSHOT_DIR_MODE)
        .create(path)
        .map_err(|error| snapshot_io_error("create", path, error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(SNAPSHOT_DIR_MODE))
        .map_err(|error| snapshot_io_error("set mode 0700 on", path, error))
}

/// Creates one owner-only directory, accepting an existing owned directory.
pub fn ensure_snapshot_directory(path: &Path) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("snapshot directory '{}' is a symbolic link", path.display()),
        )
        .with_hint("move the symlink aside and retry")),
        Ok(metadata) if metadata.is_dir() => {
            fs::set_permissions(path, fs::Permissions::from_mode(SNAPSHOT_DIR_MODE))
                .map_err(|error| snapshot_io_error("set mode 0700 on", path, error))
        }
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("snapshot directory '{}' is not a directory", path.display()),
        )
        .with_hint("move the conflicting path aside and retry")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_snapshot_directory(path),
        Err(error) => Err(snapshot_io_error("inspect", path, error)),
    }
}

/// Reads and validates one snapshot metadata document.
pub fn read_snapshot_metadata(path: &Path) -> Result<SnapshotMetadata, FirestoneError> {
    let bytes = read_snapshot_document(path, "snapshot metadata")?;
    let metadata: SnapshotMetadata = serde_json::from_slice(&bytes).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("snapshot metadata '{}' is not valid JSON", path.display()),
        )
        .with_hint("remove the snapshot directory and take a new snapshot")
        .with_source(error)
    })?;
    metadata.validate(path)?;
    Ok(metadata)
}

/// Reads and validates one restore marker.
pub fn read_restore_request(path: &Path) -> Result<RestoreRequest, FirestoneError> {
    let bytes = read_snapshot_document(path, "restore marker")?;
    let request: RestoreRequest = serde_json::from_slice(&bytes).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("restore marker '{}' is not valid JSON", path.display()),
        )
        .with_hint("remove the restore marker and retry the restore")
        .with_source(error)
    })?;
    request.validate(path)?;
    Ok(request)
}

fn read_snapshot_document(path: &Path, label: &str) -> Result<Vec<u8>, FirestoneError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            let kind = if error.kind() == io::ErrorKind::NotFound {
                ErrorKind::NotFound
            } else {
                ErrorKind::Dependency
            };
            FirestoneError::new(kind, format!("cannot open {label} '{}'", path.display()))
                .with_hint("check the machine directory permissions")
                .with_source(error)
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| snapshot_io_error("inspect", path, error))?;
    if !metadata.is_file() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} '{}' is not a regular file", path.display()),
        )
        .with_hint("move the conflicting path aside and retry"));
    }
    if metadata.len() > MAX_SNAPSHOT_DOCUMENT_BYTES {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "{label} '{}' exceeds the {MAX_SNAPSHOT_DOCUMENT_BYTES}-byte limit",
                path.display()
            ),
        )
        .with_hint("remove the oversized document and retry"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| snapshot_io_error("read", path, error))?;
    Ok(bytes)
}

/// Formats one absolute path as the `file://` URL Cloud Hypervisor v53 expects
/// in `vm.snapshot` and `vm.restore` bodies.
pub fn snapshot_file_url(directory: &Path) -> Result<String, FirestoneError> {
    if !directory.is_absolute() {
        return Err(FirestoneError::new(
            ErrorKind::Generic,
            format!(
                "VM state directory '{}' is not absolute",
                directory.display()
            ),
        )
        .with_hint("report this Firestone path bug"));
    }
    let text = directory.to_str().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("VM state directory '{}' is not UTF-8", directory.display()),
        )
        .with_hint("use a UTF-8 Firestone data directory")
    })?;
    if text.chars().any(char::is_control) {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            "VM state directory contains control characters",
        )
        .with_hint("use a Firestone data directory without control characters"));
    }
    Ok(format!("file://{text}"))
}

/// Lowercase hexadecimal SHA-256 of one snapshot document.
///
/// The restore marker pins the snapshot's `vmconfig.json` with this digest, and
/// the shim recomputes it before it hands the VM state to Cloud Hypervisor.
#[must_use]
pub fn snapshot_document_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// Bytes still available to this user on the filesystem holding `path`.
pub fn available_bytes(path: &Path) -> Result<u64, FirestoneError> {
    let stats = nix::sys::statvfs::statvfs(path).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot measure free space on '{}'", path.display()),
        )
        .with_hint("check the Firestone data directory permissions")
        .with_source(std::io::Error::from(error))
    })?;
    // statvfs widths differ between the supported targets, so both counts are
    // widened through one generic conversion rather than a per-target cast.
    Ok(widen(stats.fragment_size()).saturating_mul(widen(stats.blocks_available())))
}

fn widen<T: Into<u64>>(value: T) -> u64 {
    value.into()
}

/// Bytes one file actually occupies on disk, holes excluded.
pub fn allocated_bytes(path: &Path) -> Result<u64, FirestoneError> {
    use std::os::unix::fs::MetadataExt as _;

    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.blocks().saturating_mul(512)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(snapshot_io_error("inspect", path, error)),
    }
}

fn snapshot_io_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {operation} '{}'", path.display()),
    )
    .with_hint("check the Firestone data directory permissions")
    .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::{
        SnapshotKind, SnapshotMetadata, auto_snapshot_name, snapshot_file_url, sparse_copy_file,
        validate_snapshot_name,
    };
    use crate::ErrorKind;

    #[test]
    fn auto_snapshot_name_utc_instant_is_the_stamped_identifier()
    -> Result<(), Box<dyn std::error::Error>> {
        let stamp: jiff::Timestamp = "2026-09-02T12:34:56Z".parse()?;
        assert_eq!(auto_snapshot_name(stamp), "snap-20260902-123456");

        let fractional: jiff::Timestamp = "2026-01-05T00:00:01.500Z".parse()?;
        assert_eq!(auto_snapshot_name(fractional), "snap-20260105-000001");
        Ok(())
    }

    #[test]
    fn validate_snapshot_name_rejects_traversal_and_hidden_names() {
        for accepted in ["snap-20260902-123456", "before_upgrade", "a", "v1.2-rc3"] {
            assert!(
                validate_snapshot_name(accepted).is_ok(),
                "rejected {accepted}"
            );
        }
        for rejected in [
            "",
            ".",
            "..",
            ".hidden",
            "-leading",
            "with/slash",
            "with space",
            "new\nline",
            &"x".repeat(65),
        ] {
            let error = validate_snapshot_name(rejected).err();
            assert_eq!(
                error.as_ref().map(crate::FirestoneError::kind),
                Some(ErrorKind::InvalidSpec),
                "accepted {rejected:?}"
            );
        }
    }

    #[test]
    fn sparse_copy_file_holey_source_skips_zero_blocks_and_matches_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("memory-ranges");
        let destination = directory.path().join("copy");

        let mut file = std::fs::File::create(&source)?;
        file.write_all(&[7_u8; 4096])?;
        file.set_len(4 * 1024 * 1024)?;
        file.sync_all()?;
        drop(file);

        let copy = sparse_copy_file(&source, &destination, 0o600)?;
        assert_eq!(copy.apparent_bytes, 4 * 1024 * 1024);
        assert!(
            copy.written_bytes < copy.apparent_bytes,
            "{copy:?} materialized the hole"
        );
        assert_eq!(std::fs::read(&source)?, std::fs::read(&destination)?);
        Ok(())
    }

    #[test]
    fn sparse_copy_file_existing_destination_is_refused() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("disk.qcow2");
        let destination = directory.path().join("taken");
        std::fs::write(&source, b"content")?;
        std::fs::write(&destination, b"existing")?;

        assert!(sparse_copy_file(&source, &destination, 0o600).is_err());
        assert_eq!(std::fs::read(&destination)?, b"existing");
        Ok(())
    }

    #[test]
    fn snapshot_metadata_kind_and_memory_must_agree() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::path::Path::new("/tmp/metadata.json");
        let cold = SnapshotMetadata {
            schema_version: super::SNAPSHOT_SCHEMA_VERSION,
            kind: SnapshotKind::Cold,
            created_at: "2026-09-02T12:34:56Z".to_owned(),
            image_id: Some("image-1".to_owned()),
            firestone_version: "0.1.4".to_owned(),
            disk_bytes: 10,
            memory_bytes: None,
        };
        cold.validate(path)?;

        let warm = SnapshotMetadata {
            kind: SnapshotKind::Warm,
            memory_bytes: Some(2048),
            ..cold.clone()
        };
        warm.validate(path)?;

        let inconsistent = SnapshotMetadata {
            kind: SnapshotKind::Warm,
            memory_bytes: None,
            ..cold.clone()
        };
        assert_eq!(
            inconsistent.validate(path).err().map(|error| error.kind()),
            Some(ErrorKind::Conflict)
        );

        let future = SnapshotMetadata {
            schema_version: super::SNAPSHOT_SCHEMA_VERSION + 1,
            ..cold
        };
        assert_eq!(
            future.validate(path).err().map(|error| error.kind()),
            Some(ErrorKind::Conflict)
        );
        Ok(())
    }

    #[test]
    fn snapshot_file_url_requires_an_absolute_utf8_path() {
        assert_eq!(
            snapshot_file_url(std::path::Path::new("/data/vmstate")).ok(),
            Some("file:///data/vmstate".to_owned())
        );
        assert!(snapshot_file_url(std::path::Path::new("vmstate")).is_err());
    }
}
