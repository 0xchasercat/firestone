//! Reading the config disk that Cloud Hypervisor attaches as `/dev/vdb`.
//!
//! SPEC §10.5. The frame format itself lives in `firestone-initproto` so the
//! host writer and this reader cannot drift; this module only bounds the read
//! and turns a failure into one console-printable sentence.

use std::{
    fmt,
    fs::File,
    io::{self, Read},
    path::Path,
};

use firestone_initproto::{
    CONFIG_HEADER_LEN, FrameError, InitConfig, MAX_CONFIG_JSON_BYTES, decode_frame,
};

/// The virtio-blk device the config disk is attached to (SPEC §9.2 `disks[1]`).
pub const CONFIG_DEVICE: &str = "/dev/vdb";

/// Why the config disk could not be turned into a config document.
#[derive(Debug)]
pub enum ConfigError {
    Io { path: String, source: io::Error },
    Frame(FrameError),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "cannot read {path}: {source}"),
            Self::Frame(source) => write!(formatter, "{source}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Frame(source) => Some(source),
        }
    }
}

/// Reads and decodes the config document from one raw device or file.
///
/// At most one header plus the documented JSON cap is read, so a device whose
/// declared length is a lie can never make PID 1 allocate without bound.
pub fn read_config(path: &Path) -> Result<InitConfig, ConfigError> {
    let limit = CONFIG_HEADER_LEN as u64 + u64::from(MAX_CONFIG_JSON_BYTES);
    let file = File::open(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(CONFIG_HEADER_LEN);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
    decode_frame(&bytes).map_err(ConfigError::Frame)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use firestone_initproto::{InitConfig, InitNetwork, encode_frame};

    use super::{ConfigError, read_config};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct Scratch {
        directory: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Result<Self, std::io::Error> {
            let directory =
                std::env::temp_dir().join(format!("firestone-init-{label}-{}", std::process::id()));
            fs::create_dir_all(&directory)?;
            Ok(Self { directory })
        }

        fn file(&self, name: &str) -> PathBuf {
            self.directory.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn sample() -> InitConfig {
        InitConfig {
            hostname: "demo".to_owned(),
            entrypoint: vec!["/bin/sh".to_owned()],
            cmd: Vec::new(),
            env: vec!["PATH=/bin".to_owned()],
            workdir: None,
            user: None,
            network: InitNetwork::None,
            disk_size_bytes: 4096,
        }
    }

    #[test]
    fn read_config_padded_disk_returns_the_document() -> TestResult {
        let scratch = Scratch::new("padded")?;
        let path = scratch.file("config.img");
        let mut bytes = encode_frame(&sample())?;
        bytes.resize(4096, 0);
        fs::write(&path, &bytes)?;

        assert_eq!(read_config(&path)?, sample());
        Ok(())
    }

    #[test]
    fn read_config_missing_device_reports_the_path() -> TestResult {
        let scratch = Scratch::new("missing")?;

        match read_config(&scratch.file("absent.img")) {
            Err(ConfigError::Io { path, .. }) => assert!(path.ends_with("absent.img")),
            other => return Err(format!("expected an io error, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn read_config_wrong_magic_reports_a_frame_error() -> TestResult {
        let scratch = Scratch::new("magic")?;
        let path = scratch.file("config.img");
        fs::write(&path, vec![0_u8; 4096])?;

        match read_config(&path) {
            Err(ConfigError::Frame(_)) => Ok(()),
            other => Err(format!("expected a frame error, got {other:?}").into()),
        }
    }
}
