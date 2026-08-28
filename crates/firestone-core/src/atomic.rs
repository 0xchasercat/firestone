use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{ErrorKind, FirestoneError};

/// Writes JSON through a sibling temporary file and durably replaces `path`.
///
/// Callers must follow the ownership rule for the file they are writing. This
/// function serializes before touching the known temporary file, so a
/// serialization failure leaves both the destination and any prior temp file
/// unchanged.
pub fn write_json<T>(path: &Path, value: &T) -> Result<(), FirestoneError>
where
    T: Serialize + ?Sized,
{
    let bytes = serialize_json(path, value)?;
    write(path, &bytes)
}

/// Writes JSON atomically while creating the published file with the supplied mode.
pub fn write_json_with_mode<T>(path: &Path, value: &T, mode: u32) -> Result<(), FirestoneError>
where
    T: Serialize + ?Sized,
{
    let bytes = serialize_json(path, value)?;
    write_with_mode(path, &bytes, mode)
}

fn serialize_json<T>(path: &Path, value: &T) -> Result<Vec<u8>, FirestoneError>
where
    T: Serialize + ?Sized,
{
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot serialize JSON for {}", path.display()),
        )
        .with_hint("the existing file was left unchanged")
        .with_source(error)
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes all bytes through a sibling temporary file, fsyncs, renames, and fsyncs the
/// parent directory.
pub fn write(path: &Path, bytes: &[u8]) -> Result<(), FirestoneError> {
    write_with(path, bytes, |file, contents| file.write_all(contents))
}

/// Writes bytes atomically while creating the published file with the supplied mode.
pub fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<(), FirestoneError> {
    write_with_options(path, bytes, Some(mode), false, |file, contents| {
        file.write_all(contents)
    })
}

/// Streams bytes through a sibling temporary file and durably replaces `path`.
///
/// The callback writes directly to the temporary file, so fixed-size artifacts
/// do not need a second in-memory copy before publication.
pub fn write_stream<F>(path: &Path, write_bytes: F) -> Result<(), FirestoneError>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    write_with_options(path, &[], None, true, |file, _contents| write_bytes(file))
}
fn write_with<F>(path: &Path, bytes: &[u8], write_bytes: F) -> Result<(), FirestoneError>
where
    F: FnOnce(&mut File, &[u8]) -> io::Result<()>,
{
    write_with_options(path, bytes, None, false, write_bytes)
}

fn write_with_options<F>(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    readable: bool,
    write_bytes: F,
) -> Result<(), FirestoneError>
where
    F: FnOnce(&mut File, &[u8]) -> io::Result<()>,
{
    if matches!(mode, Some(value) if value & !0o7777 != 0) {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "invalid atomic file mode {value:04o}",
                value = mode.unwrap_or_default()
            ),
        ));
    }
    let parent = parent_directory(path)?;
    let temp_path = temp_path(path)?;
    let parent_file =
        File::open(parent).map_err(|error| io_failure("open parent directory", parent, error))?;

    remove_known_temp(&temp_path)?;

    let mut options = OpenOptions::new();
    options.read(readable).write(true).create_new(true);
    if let Some(mode) = mode {
        options.mode(mode);
    }
    let mut temp = options
        .open(&temp_path)
        .map_err(|error| io_failure("create temporary file", &temp_path, error))?;
    if let Some(mode) = mode {
        temp.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| fail_before_rename("set temporary file mode", &temp_path, error))?;
    }

    if let Err(error) = write_bytes(&mut temp, bytes) {
        drop(temp);
        return Err(fail_before_rename(
            "write temporary file",
            &temp_path,
            error,
        ));
    }

    if let Err(error) = temp.sync_all() {
        drop(temp);
        return Err(fail_before_rename(
            "fsync temporary file",
            &temp_path,
            error,
        ));
    }
    drop(temp);

    fs::rename(&temp_path, path)
        .map_err(|error| fail_before_rename("rename temporary file", &temp_path, error))?;

    parent_file
        .sync_all()
        .map_err(|error| io_failure("fsync parent directory", parent, error))?;

    Ok(())
}

fn parent_directory(path: &Path) -> Result<&Path, FirestoneError> {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(parent) => Ok(parent),
        None => Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot determine parent directory for {}", path.display()),
        )
        .with_hint("use a file path with a parent directory")),
    }
}

fn temp_path(path: &Path) -> Result<PathBuf, FirestoneError> {
    let file_name = path.file_name().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot determine file name for {}", path.display()),
        )
        .with_hint("use a path to a file, not a directory")
    })?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".tmp");
    Ok(path.with_file_name(temp_name))
}

fn remove_known_temp(temp_path: &Path) -> Result<(), FirestoneError> {
    match fs::remove_file(temp_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_failure("remove stale temporary file", temp_path, error)),
    }
}

fn fail_before_rename(
    operation: &'static str,
    temp_path: &Path,
    error: io::Error,
) -> FirestoneError {
    let cleanup = match fs::remove_file(temp_path) {
        Ok(()) => None,
        Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => None,
        Err(cleanup_error) => Some(cleanup_error),
    };

    let mut failure = io_failure(operation, temp_path, error);
    if let Some(cleanup_error) = cleanup {
        failure = FirestoneError::new(
            ErrorKind::Generic,
            format!(
                "{}; cannot remove temporary file {}: {cleanup_error}",
                failure.message(),
                temp_path.display()
            ),
        )
        .with_hint("the destination was left unchanged; remove the named temp file and retry");
    }
    failure
}

fn io_failure(operation: &'static str, path: &Path, error: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {operation} {}", path.display()),
    )
    .with_hint("check that the directory is writable and has free space")
    .with_source(error)
}

#[cfg(test)]
mod tests {
    use std::{fs, io};

    use serde::ser::{Error as _, Serializer};

    use super::{temp_path, write, write_json, write_with};

    struct SerializationFailure;

    impl serde::Serialize for SerializationFailure {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("injected serialization failure"))
        }
    }

    #[test]
    fn write_existing_file_replaces_contents_and_removes_temp()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("state.json");
        fs::write(&target, b"old")?;

        write(&target, b"new")?;

        assert_eq!(fs::read(&target)?, b"new");
        assert!(!temp_path(&target)?.exists());
        Ok(())
    }

    #[test]
    fn write_stale_temp_replaces_only_known_temp() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("state.json");
        let temp = temp_path(&target)?;
        let unrelated = directory.path().join("state.json.tmp.backup");
        fs::write(&temp, b"stale")?;
        fs::write(&unrelated, b"keep")?;

        write(&target, b"new")?;

        assert!(!temp.exists());
        assert_eq!(fs::read(unrelated)?, b"keep");
        Ok(())
    }

    #[test]
    fn write_partial_failure_preserves_destination_and_removes_temp()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("state.json");
        fs::write(&target, b"old")?;

        let result = write_with(&target, b"new", |file, contents| {
            use std::io::Write as _;
            file.write_all(&contents[..1])?;
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "injected write failure",
            ))
        });
        let error = match result {
            Err(error) => error,
            Ok(()) => panic!("the injected write must fail"),
        };

        assert!(error.message().contains("write temporary file"));
        assert_eq!(fs::read(&target)?, b"old");
        assert!(!temp_path(&target)?.exists());
        Ok(())
    }

    #[test]
    fn write_json_serialization_failure_preserves_destination_and_temp()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("state.json");
        let temp = temp_path(&target)?;
        fs::write(&target, b"old")?;
        fs::write(&temp, b"prior temp")?;

        let error = match write_json(&target, &SerializationFailure) {
            Err(error) => error,
            Ok(()) => panic!("the injected serialization must fail"),
        };

        assert!(error.message().contains("serialize JSON"));
        assert_eq!(fs::read(&target)?, b"old");
        assert_eq!(fs::read(&temp)?, b"prior temp");
        Ok(())
    }
}
