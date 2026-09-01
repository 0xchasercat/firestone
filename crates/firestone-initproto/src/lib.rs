//! The `firestone-init` configuration document and its magic-framed container.
//!
//! SPEC §10.5 defines one byte layout that two programs must agree on: the host
//! writes `machines/<name>/config.img` and the guest PID 1 reads `/dev/vdb`.
//! Both sides link this crate so the frame constants, the JSON field set, and
//! the refusal rules cannot drift apart. Nothing here touches the filesystem,
//! so both a host build and a `x86_64-unknown-linux-musl` guest build compile
//! the identical code.

#![forbid(unsafe_code)]

use std::fmt;

use serde::{Deserialize, Serialize};

/// ASCII magic at offset 0 of the config disk (SPEC §10.5).
pub const CONFIG_MAGIC: [u8; 8] = *b"FSTNINIT";
/// Frame format version written by this release.
pub const CONFIG_FORMAT_VERSION: u32 = 1;
/// Magic, version and length together.
pub const CONFIG_HEADER_LEN: usize = 16;
/// Largest JSON document the frame may declare.
pub const MAX_CONFIG_JSON_BYTES: u32 = 65_536;
/// The config disk is padded with zeroes to a multiple of this many bytes.
pub const CONFIG_DISK_ALIGNMENT: u64 = 4096;

/// Guest network mode selected by the machine spec (SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitNetwork {
    /// Run the bounded one-shot DHCP client on `eth0`.
    Dhcp,
    /// Configure nothing beyond loopback.
    None,
}

impl InitNetwork {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dhcp => "dhcp",
            Self::None => "none",
        }
    }
}

/// The complete config document of SPEC §10.5.
///
/// Every key is required. `workdir` and `user` are explicit `null` when the
/// image does not set them; `entrypoint`, `cmd` and `env` may be empty arrays.
/// Field order here is the serialized key order, which is what makes the
/// published disk bytes deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitConfig {
    pub hostname: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub network: InitNetwork,
    pub disk_size_bytes: u64,
}

/// Every way a config frame can be rejected.
///
/// `firestone-init` prints the [`fmt::Display`] rendering to the console before
/// powering off, so each variant names what was wrong without echoing document
/// bytes back at the operator.
#[derive(Debug)]
pub enum FrameError {
    /// The device held fewer bytes than one header.
    TooShort { available: usize },
    /// Offset 0 is not `FSTNINIT`.
    BadMagic,
    /// The frame declares a format version this build does not implement.
    UnsupportedVersion { found: u32 },
    /// The declared JSON length is above [`MAX_CONFIG_JSON_BYTES`].
    LengthTooLarge { declared: u32 },
    /// The device held fewer bytes than the declared JSON length.
    Truncated { declared: u32, available: usize },
    /// The framed bytes are not the documented JSON document.
    Json(serde_json::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { available } => write!(
                formatter,
                "config disk holds {available} bytes; a frame header needs {CONFIG_HEADER_LEN}"
            ),
            Self::BadMagic => write!(
                formatter,
                "config disk does not start with the {} magic",
                magic_str()
            ),
            Self::UnsupportedVersion { found } => write!(
                formatter,
                "config disk declares format version {found}; this build reads version {CONFIG_FORMAT_VERSION}"
            ),
            Self::LengthTooLarge { declared } => write!(
                formatter,
                "config disk declares {declared} JSON bytes; the limit is {MAX_CONFIG_JSON_BYTES}"
            ),
            Self::Truncated {
                declared,
                available,
            } => write!(
                formatter,
                "config disk declares {declared} JSON bytes but holds {available}"
            ),
            Self::Json(source) => write!(formatter, "config document is invalid: {source}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(source) => Some(source),
            _ => None,
        }
    }
}

/// Every way a config document can refuse to be framed.
#[derive(Debug)]
pub enum EncodeError {
    /// The serialized document is above [`MAX_CONFIG_JSON_BYTES`].
    TooLarge { len: usize },
    /// The document could not be serialized at all.
    Json(serde_json::Error),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len } => write!(
                formatter,
                "config document is {len} bytes; the limit is {MAX_CONFIG_JSON_BYTES}"
            ),
            Self::Json(source) => write!(formatter, "cannot serialize config document: {source}"),
        }
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(source) => Some(source),
            Self::TooLarge { .. } => None,
        }
    }
}

fn magic_str() -> &'static str {
    // The magic is ASCII by construction, so this never allocates a fallback.
    std::str::from_utf8(&CONFIG_MAGIC).unwrap_or("FSTNINIT")
}

/// Serializes one config document into its magic-framed bytes, header included.
///
/// The result is exactly `CONFIG_HEADER_LEN + json.len()` bytes; padding the
/// disk out to [`CONFIG_DISK_ALIGNMENT`] is the writer's job.
pub fn encode_frame(config: &InitConfig) -> Result<Vec<u8>, EncodeError> {
    let json = serde_json::to_vec(config).map_err(EncodeError::Json)?;
    let len = u32::try_from(json.len()).map_err(|_| EncodeError::TooLarge { len: json.len() })?;
    if len > MAX_CONFIG_JSON_BYTES {
        return Err(EncodeError::TooLarge { len: json.len() });
    }
    let mut framed = Vec::with_capacity(CONFIG_HEADER_LEN + json.len());
    framed.extend_from_slice(&CONFIG_MAGIC);
    framed.extend_from_slice(&CONFIG_FORMAT_VERSION.to_le_bytes());
    framed.extend_from_slice(&len.to_le_bytes());
    framed.extend_from_slice(&json);
    Ok(framed)
}

/// Reads one config document out of the raw bytes of the config disk.
///
/// Trailing zero padding is ignored: only the declared length is parsed.
pub fn decode_frame(bytes: &[u8]) -> Result<InitConfig, FrameError> {
    if bytes.len() < CONFIG_HEADER_LEN {
        return Err(FrameError::TooShort {
            available: bytes.len(),
        });
    }
    let (header, body) = bytes.split_at(CONFIG_HEADER_LEN);
    if header[..8] != CONFIG_MAGIC {
        return Err(FrameError::BadMagic);
    }
    let version = read_u32_le(&header[8..12]);
    if version != CONFIG_FORMAT_VERSION {
        return Err(FrameError::UnsupportedVersion { found: version });
    }
    let declared = read_u32_le(&header[12..16]);
    if declared > MAX_CONFIG_JSON_BYTES {
        return Err(FrameError::LengthTooLarge { declared });
    }
    let length = declared as usize;
    if body.len() < length {
        return Err(FrameError::Truncated {
            declared,
            available: body.len(),
        });
    }
    serde_json::from_slice(&body[..length]).map_err(FrameError::Json)
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    let mut value = [0_u8; 4];
    let len = value.len().min(bytes.len());
    value[..len].copy_from_slice(&bytes[..len]);
    u32::from_le_bytes(value)
}

/// Merges an image's environment with per-machine overrides, deterministically.
///
/// The image order is preserved and an override replaces the value in place, so
/// the same inputs always produce the same `env` array and therefore the same
/// config-disk bytes. Entries without `=` are keys with an empty value. A later
/// duplicate inside one list wins, exactly as `execve` semantics imply.
#[must_use]
pub fn merge_env(image: &[String], overrides: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(image.len() + overrides.len());
    for entry in image.iter().chain(overrides.iter()) {
        let key = env_key(entry);
        match merged.iter_mut().find(|existing| env_key(existing) == key) {
            Some(existing) => existing.clone_from(entry),
            None => merged.push(entry.clone()),
        }
    }
    merged
}

/// The variable name of one `KEY=VALUE` entry, or the whole entry when bare.
#[must_use]
pub fn env_key(entry: &str) -> &str {
    match entry.find('=') {
        Some(index) => &entry[..index],
        None => entry,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIG_FORMAT_VERSION, CONFIG_HEADER_LEN, CONFIG_MAGIC, EncodeError, FrameError,
        InitConfig, InitNetwork, MAX_CONFIG_JSON_BYTES, decode_frame, encode_frame, merge_env,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample() -> InitConfig {
        InitConfig {
            hostname: "app".to_owned(),
            entrypoint: vec!["/docker-entrypoint.sh".to_owned()],
            cmd: vec![
                "nginx".to_owned(),
                "-g".to_owned(),
                "daemon off;".to_owned(),
            ],
            env: vec!["PATH=/usr/bin:/bin".to_owned()],
            workdir: Some("/".to_owned()),
            user: Some("root".to_owned()),
            network: InitNetwork::Dhcp,
            disk_size_bytes: 21_474_836_480,
        }
    }

    #[test]
    fn encode_frame_writes_the_documented_header() -> TestResult {
        let framed = encode_frame(&sample())?;

        assert_eq!(&framed[..8], &CONFIG_MAGIC);
        assert_eq!(&framed[8..12], &CONFIG_FORMAT_VERSION.to_le_bytes());
        let declared = u32::from_le_bytes([framed[12], framed[13], framed[14], framed[15]]);
        assert_eq!(declared as usize, framed.len() - CONFIG_HEADER_LEN);
        assert_eq!(
            std::str::from_utf8(&framed[CONFIG_HEADER_LEN..])?,
            r#"{"hostname":"app","entrypoint":["/docker-entrypoint.sh"],"cmd":["nginx","-g","daemon off;"],"env":["PATH=/usr/bin:/bin"],"workdir":"/","user":"root","network":"dhcp","disk_size_bytes":21474836480}"#
        );
        Ok(())
    }

    #[test]
    fn frame_round_trip_preserves_every_field() -> TestResult {
        let config = sample();

        let decoded = decode_frame(&encode_frame(&config)?)?;

        assert_eq!(decoded, config);
        Ok(())
    }

    #[test]
    fn frame_round_trip_ignores_trailing_zero_padding() -> TestResult {
        let config = sample();
        let mut framed = encode_frame(&config)?;
        framed.resize(4096, 0);

        assert_eq!(decode_frame(&framed)?, config);
        Ok(())
    }

    #[test]
    fn frame_encoding_is_byte_stable_for_equal_inputs() -> TestResult {
        assert_eq!(encode_frame(&sample())?, encode_frame(&sample())?);
        Ok(())
    }

    #[test]
    fn decode_frame_short_header_reports_available_bytes() {
        let error = decode_frame(&[0_u8; 4]);

        assert!(matches!(error, Err(FrameError::TooShort { available: 4 })));
    }

    #[test]
    fn decode_frame_bad_magic_is_refused() -> TestResult {
        let mut framed = encode_frame(&sample())?;
        framed[0] = b'X';

        assert!(matches!(decode_frame(&framed), Err(FrameError::BadMagic)));
        Ok(())
    }

    #[test]
    fn decode_frame_unknown_version_names_the_version() -> TestResult {
        let mut framed = encode_frame(&sample())?;
        framed[8..12].copy_from_slice(&7_u32.to_le_bytes());

        assert!(matches!(
            decode_frame(&framed),
            Err(FrameError::UnsupportedVersion { found: 7 })
        ));
        Ok(())
    }

    #[test]
    fn decode_frame_oversize_length_is_refused_before_reading() -> TestResult {
        let mut framed = encode_frame(&sample())?;
        framed[12..16].copy_from_slice(&(MAX_CONFIG_JSON_BYTES + 1).to_le_bytes());

        assert!(matches!(
            decode_frame(&framed),
            Err(FrameError::LengthTooLarge { .. })
        ));
        Ok(())
    }

    #[test]
    fn decode_frame_truncated_body_reports_both_lengths() -> TestResult {
        let mut framed = encode_frame(&sample())?;
        framed.truncate(framed.len() - 3);

        match decode_frame(&framed) {
            Err(FrameError::Truncated {
                declared,
                available,
            }) => assert_eq!(usize::try_from(declared)?, available + 3),
            other => return Err(format!("expected a truncation error, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn decode_frame_unknown_json_key_is_refused() -> TestResult {
        let json = br#"{"hostname":"a","entrypoint":[],"cmd":[],"env":[],"workdir":null,"user":null,"network":"none","disk_size_bytes":1,"extra":true}"#;
        let mut framed = Vec::from(CONFIG_MAGIC);
        framed.extend_from_slice(&CONFIG_FORMAT_VERSION.to_le_bytes());
        framed.extend_from_slice(&u32::try_from(json.len())?.to_le_bytes());
        framed.extend_from_slice(json);

        assert!(matches!(decode_frame(&framed), Err(FrameError::Json(_))));
        Ok(())
    }

    #[test]
    fn decode_frame_missing_json_key_is_refused() -> TestResult {
        let json = br#"{"hostname":"a","entrypoint":[],"cmd":[],"env":[],"workdir":null,"user":null,"network":"none"}"#;
        let mut framed = Vec::from(CONFIG_MAGIC);
        framed.extend_from_slice(&CONFIG_FORMAT_VERSION.to_le_bytes());
        framed.extend_from_slice(&u32::try_from(json.len())?.to_le_bytes());
        framed.extend_from_slice(json);

        assert!(matches!(decode_frame(&framed), Err(FrameError::Json(_))));
        Ok(())
    }

    #[test]
    fn encode_frame_oversize_document_is_refused() {
        let mut config = sample();
        config.env = vec!["A=".to_owned() + &"x".repeat(MAX_CONFIG_JSON_BYTES as usize)];

        assert!(matches!(
            encode_frame(&config),
            Err(EncodeError::TooLarge { .. })
        ));
    }

    #[test]
    fn merge_env_override_replaces_in_place_and_appends_new_keys() {
        let image = vec![
            "PATH=/usr/bin".to_owned(),
            "LANG=C".to_owned(),
            "BARE".to_owned(),
        ];
        let overrides = vec!["LANG=en_US.UTF-8".to_owned(), "TZ=UTC".to_owned()];

        let merged = merge_env(&image, &overrides);

        assert_eq!(
            merged,
            vec![
                "PATH=/usr/bin".to_owned(),
                "LANG=en_US.UTF-8".to_owned(),
                "BARE".to_owned(),
                "TZ=UTC".to_owned(),
            ]
        );
    }

    #[test]
    fn merge_env_duplicate_image_key_keeps_one_entry_at_its_first_position() {
        let image = vec!["A=1".to_owned(), "B=1".to_owned(), "A=2".to_owned()];

        assert_eq!(
            merge_env(&image, &[]),
            vec!["A=2".to_owned(), "B=1".to_owned()]
        );
    }

    #[test]
    fn merge_env_empty_inputs_produce_an_empty_environment() {
        assert!(merge_env(&[], &[]).is_empty());
    }
}
