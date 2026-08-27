use std::{fmt, path::PathBuf, str::FromStr, time::Duration};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// CPU architecture supported by Firestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// Returns the architecture of the current executable's host.
    pub fn current() -> Result<Self, String> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            other => Err(format!("unsupported host architecture '{other}'")),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A machine size stored in bytes and configured in MiB or GiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(u64);

impl ByteSize {
    pub const MIB: u64 = 1024 * 1024;
    pub const GIB: u64 = 1024 * Self::MIB;
    pub(crate) const BUILTIN_MEMORY: Self = Self(2 * Self::GIB);
    pub(crate) const BUILTIN_DISK: Self = Self(20 * Self::GIB);
    pub(crate) const MINIMUM_MEMORY: Self = Self(128 * Self::MIB);

    pub const fn from_mib(value: u64) -> Result<Self, ParseByteSizeError> {
        match value.checked_mul(Self::MIB) {
            Some(bytes) => Ok(Self(bytes)),
            None => Err(ParseByteSizeError),
        }
    }

    pub const fn from_gib(value: u64) -> Result<Self, ParseByteSizeError> {
        match value.checked_mul(Self::GIB) {
            Some(bytes) => Ok(Self(bytes)),
            None => Err(ParseByteSizeError),
        }
    }

    #[must_use]
    pub const fn as_bytes(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn as_mib(self) -> u64 {
        self.0 / Self::MIB
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 % Self::GIB == 0 {
            write!(formatter, "{}G", self.0 / Self::GIB)
        } else {
            write!(formatter, "{}M", self.0 / Self::MIB)
        }
    }
}

impl FromStr for ByteSize {
    type Err = ParseByteSizeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (number, multiplier) = match value.as_bytes().last().copied() {
            Some(b'M' | b'm') => (&value[..value.len() - 1], Self::MIB),
            Some(b'G' | b'g') => (&value[..value.len() - 1], Self::GIB),
            Some(byte) if byte.is_ascii_digit() => (value, Self::MIB),
            _ => return Err(ParseByteSizeError),
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseByteSizeError);
        }
        let amount = number.parse::<u64>().map_err(|_| ParseByteSizeError)?;
        if multiplier == Self::GIB {
            Self::from_gib(amount)
        } else {
            Self::from_mib(amount)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a size such as '512M' or '4G', or an integer number of MiB")]
pub struct ParseByteSizeError;

impl Serialize for ByteSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = ByteSize;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a size string such as '512M' or an integer number of MiB")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                ByteSize::from_mib(value).map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = u64::try_from(value).map_err(|_| E::custom(ParseByteSizeError))?;
                self.visit_u64(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl JsonSchema for ByteSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ByteSize".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                {
                    "type": "string",
                    "pattern": "^[0-9]{1,14}[Mm]?$",
                    "maxLength": 15,
                    "description": "whole MiB; checked against the u64 byte ceiling"
                },
                {
                    "type": "string",
                    "pattern": "^[0-9]{1,11}[Gg]$",
                    "maxLength": 12,
                    "description": "whole GiB; checked against the u64 byte ceiling"
                },
                {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": u64::MAX / ByteSize::MIB,
                    "description": "whole MiB"
                }
            ]
        })
    }
}

/// An image reference before catalog or local-file resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ImageRef {
    fn default() -> Self {
        Self::new("ubuntu:24.04")
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&str> for ImageRef {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ImageRef {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// A six-octet Ethernet MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    #[must_use]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl FromStr for MacAddr {
    type Err = ParseMacAddrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut octets = [0_u8; 6];
        let mut parts = value.split(':');
        for octet in &mut octets {
            let part = parts.next().ok_or(ParseMacAddrError)?;
            if part.len() != 2 {
                return Err(ParseMacAddrError);
            }
            *octet = u8::from_str_radix(part, 16).map_err(|_| ParseMacAddrError)?;
        }
        if parts.next().is_some() {
            return Err(ParseMacAddrError);
        }
        Ok(Self(octets))
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, f] = self.0;
        write!(formatter, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
    }
}

impl Serialize for MacAddr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for MacAddr {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MacAddr".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9A-Fa-f]{2}(:[0-9A-Fa-f]{2}){5}$"
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected six hexadecimal octets such as 52:54:00:9a:1f:c3")]
pub struct ParseMacAddrError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FirmwareValue {
    Auto,
    Rhf,
    Edk2,
    Path(PathBuf),
}

/// Firmware selection for cloud-hypervisor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Firmware(FirmwareValue);

impl Firmware {
    pub const AUTO: Self = Self(FirmwareValue::Auto);
    pub const RHF: Self = Self(FirmwareValue::Rhf);
    pub const EDK2: Self = Self(FirmwareValue::Edk2);

    pub fn path(path: impl Into<PathBuf>) -> Result<Self, ParseFirmwareError> {
        let path = path.into();
        let Some(text) = path.to_str() else {
            return Err(ParseFirmwareError);
        };
        if text.is_empty() || matches!(text, "auto" | "rhf" | "edk2") {
            return Err(ParseFirmwareError);
        }
        Ok(Self(FirmwareValue::Path(path)))
    }

    #[must_use]
    pub fn as_path(&self) -> Option<&std::path::Path> {
        match &self.0 {
            FirmwareValue::Path(path) => Some(path),
            FirmwareValue::Auto | FirmwareValue::Rhf | FirmwareValue::Edk2 => None,
        }
    }

    #[must_use]
    pub fn is_edk2(&self) -> bool {
        matches!(&self.0, FirmwareValue::Edk2)
    }

    /// Returns the architecture-specific edk2 artifact name from §7.2.
    #[must_use]
    pub const fn edk2_file_name(arch: Arch) -> &'static str {
        match arch {
            Arch::X86_64 => "CLOUDHV.fd",
            Arch::Aarch64 => "CLOUDHV_EFI.fd",
        }
    }
}

impl Default for Firmware {
    fn default() -> Self {
        Self::AUTO
    }
}

impl From<crate::CatalogFirmware> for Firmware {
    fn from(firmware: crate::CatalogFirmware) -> Self {
        match firmware {
            crate::CatalogFirmware::Rhf => Self::RHF,
            crate::CatalogFirmware::Edk2 => Self::EDK2,
        }
    }
}

impl fmt::Display for Firmware {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            FirmwareValue::Auto => formatter.write_str("auto"),
            FirmwareValue::Rhf => formatter.write_str("rhf"),
            FirmwareValue::Edk2 => formatter.write_str("edk2"),
            FirmwareValue::Path(path) => write!(formatter, "{}", path.display()),
        }
    }
}

impl FromStr for Firmware {
    type Err = ParseFirmwareError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::AUTO),
            "rhf" => Ok(Self::RHF),
            "edk2" => Ok(Self::EDK2),
            path => Self::path(path),
        }
    }
}

impl Serialize for Firmware {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Firmware {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for Firmware {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Firmware".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type": "string", "minLength": 1})
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "firmware must be 'auto', 'rhf', 'edk2', or a non-empty UTF-8 path; prefix a reserved path with './'"
)]
pub struct ParseFirmwareError;

/// A positive timeout accepted by global configuration and CLI adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HumanDuration(Duration);

impl HumanDuration {
    pub(crate) const DEFAULT_FIRST_BOOT_TIMEOUT: Self = Self(Duration::from_secs(180));
    pub(crate) const DEFAULT_START_TIMEOUT: Self = Self(Duration::from_secs(60));
    pub(crate) const DEFAULT_STOP_TIMEOUT: Self = Self(Duration::from_secs(30));

    pub const fn from_secs(seconds: u64) -> Result<Self, ParseDurationError> {
        if seconds == 0 {
            return Err(ParseDurationError);
        }
        Ok(Self(Duration::from_secs(seconds)))
    }

    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}s", self.0.as_secs())
    }
}

impl FromStr for HumanDuration {
    type Err = ParseDurationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let number = value.strip_suffix('s').ok_or(ParseDurationError)?;
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseDurationError);
        }
        let amount = number.parse::<u64>().map_err(|_| ParseDurationError)?;
        Self::from_secs(amount)
    }
}

impl Serialize for HumanDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for HumanDuration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HumanDuration".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[1-9][0-9]{0,19}s$",
            "maxLength": 21,
            "description": "positive whole seconds; checked against the u64 ceiling"
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a positive duration in seconds such as '60s'")]
pub struct ParseDurationError;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use super::{ByteSize, Firmware, HumanDuration, MacAddr};

    #[test]
    fn byte_size_supported_units_returns_bytes() {
        assert_eq!(ByteSize::from_str("512M"), ByteSize::from_mib(512));
        assert_eq!(ByteSize::from_str("4G"), ByteSize::from_gib(4));
        assert_eq!(ByteSize::from_str("4096"), ByteSize::from_gib(4));
    }

    #[test]
    fn byte_size_maximum_mib_accepts_then_rejects_next_value() {
        let maximum_mib = u64::MAX / ByteSize::MIB;
        let maximum = ByteSize::from_mib(maximum_mib).expect("maximum MiB value");
        let maximum_text = format!("{maximum_mib}M");
        let overflow_text = format!("{}M", maximum_mib + 1);

        assert_eq!(maximum.as_bytes(), maximum_mib * ByteSize::MIB);
        assert_eq!(ByteSize::from_str(&maximum_text), Ok(maximum));
        assert!(ByteSize::from_mib(maximum_mib + 1).is_err());
        assert!(ByteSize::from_str(&overflow_text).is_err());
    }

    #[test]
    fn byte_size_maximum_gib_accepts_then_rejects_next_value() {
        let maximum_gib = u64::MAX / ByteSize::GIB;
        let maximum = ByteSize::from_gib(maximum_gib).expect("maximum GiB value");
        let maximum_text = format!("{maximum_gib}G");
        let overflow_text = format!("{}G", maximum_gib + 1);

        assert_eq!(maximum.as_bytes(), maximum_gib * ByteSize::GIB);
        assert_eq!(ByteSize::from_str(&maximum_text), Ok(maximum));
        assert!(ByteSize::from_gib(maximum_gib + 1).is_err());
        assert!(ByteSize::from_str(&overflow_text).is_err());
    }

    #[test]
    fn byte_size_integer_toml_interprets_mib() -> Result<(), toml::de::Error> {
        #[derive(serde::Deserialize)]
        struct Config {
            value: ByteSize,
        }

        let config: Config = toml::from_str("value = 4096")?;
        assert_eq!(Ok(config.value), ByteSize::from_gib(4));
        Ok(())
    }

    #[test]
    fn byte_size_invalid_text_returns_error() {
        for value in ["", "2GiB", "-1G", "1.5G"] {
            assert!(ByteSize::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn mac_addr_mixed_case_returns_canonical_value() {
        let address = MacAddr::from_str("52:54:00:9A:1f:C3").expect("valid address");
        assert_eq!(address.to_string(), "52:54:00:9a:1f:c3");
    }

    #[test]
    fn mac_addr_wrong_shape_returns_error() {
        for value in [
            "",
            "52:54:00:9a:1f",
            "52:54:00:9a:1f:c3:00",
            "zz:54:00:9a:1f:c3",
        ] {
            assert!(MacAddr::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn firmware_named_and_path_values_round_trip() -> Result<(), toml::de::Error> {
        #[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
        struct Config {
            firmware: Firmware,
        }

        let named: Config = toml::from_str("firmware = \"edk2\"")?;
        assert_eq!(named.firmware, Firmware::EDK2);

        let path: Config = toml::from_str("firmware = \"/opt/CLOUDHV.fd\"")?;
        assert_eq!(
            path.firmware.as_path(),
            Some(std::path::Path::new("/opt/CLOUDHV.fd"))
        );
        let serialized = toml::to_string(&path).expect("serialize firmware path");
        let deserialized: Config = toml::from_str(&serialized)?;
        assert_eq!(deserialized, path);
        Ok(())
    }

    #[test]
    fn firmware_empty_string_returns_parse_error() {
        assert!(Firmware::from_str("").is_err());
        assert!(serde_json::from_str::<Firmware>("\"\"").is_err());
    }

    #[test]
    fn firmware_empty_path_constructor_returns_error() {
        assert!(Firmware::path("").is_err());
    }

    #[test]
    fn firmware_reserved_path_constructor_returns_error() {
        for path in ["auto", "rhf", "edk2"] {
            assert!(Firmware::path(path).is_err(), "accepted {path}");
        }
        assert!(Firmware::path("./auto").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn firmware_non_utf8_path_constructor_returns_error() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        assert!(Firmware::path(OsString::from_vec(vec![0xff])).is_err());
    }

    #[test]
    fn human_duration_seconds_returns_duration() {
        assert_eq!(
            HumanDuration::from_str("60s").map(HumanDuration::get),
            Ok(Duration::from_secs(60))
        );
    }

    #[test]
    fn human_duration_zero_constructor_returns_error() {
        assert!(HumanDuration::from_secs(0).is_err());
    }

    #[test]
    fn human_duration_maximum_seconds_round_trips() -> Result<(), serde_json::Error> {
        let duration = HumanDuration::from_secs(u64::MAX).expect("maximum duration in seconds");
        let serialized = serde_json::to_string(&duration)?;
        let deserialized: HumanDuration = serde_json::from_str(&serialized)?;

        assert_eq!(duration.get(), Duration::from_secs(u64::MAX));
        assert_eq!(serialized, format!("\"{}s\"", u64::MAX));
        assert_eq!(deserialized, duration);
        Ok(())
    }

    #[test]
    fn human_duration_zero_or_missing_unit_returns_error() {
        for value in ["", "60", "0s", "-1s", "1.5s", "500ms", "1m", "1h"] {
            assert!(HumanDuration::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn firmware_edk2_architecture_returns_documented_artifact() {
        assert_eq!(Firmware::edk2_file_name(super::Arch::X86_64), "CLOUDHV.fd");
        assert_eq!(
            Firmware::edk2_file_name(super::Arch::Aarch64),
            "CLOUDHV_EFI.fd"
        );
    }

    #[test]
    fn firmware_catalog_value_converts_to_machine_value() {
        assert_eq!(Firmware::from(crate::CatalogFirmware::Rhf), Firmware::RHF);
        assert_eq!(Firmware::from(crate::CatalogFirmware::Edk2), Firmware::EDK2);
    }

    #[test]
    fn value_schemas_expose_validation_grammars() -> Result<(), serde_json::Error> {
        let byte_size = serde_json::to_value(schemars::schema_for!(ByteSize))?;
        let mac = serde_json::to_value(schemars::schema_for!(MacAddr))?;
        let duration = serde_json::to_value(schemars::schema_for!(HumanDuration))?;
        let firmware = serde_json::to_value(schemars::schema_for!(Firmware))?;

        assert!(byte_size["oneOf"].is_array());
        assert_eq!(byte_size["oneOf"][0]["maxLength"], 15);
        assert_eq!(byte_size["oneOf"][1]["maxLength"], 12);
        assert_eq!(byte_size["oneOf"][2]["maximum"], u64::MAX / ByteSize::MIB);
        assert_eq!(mac["pattern"], "^[0-9A-Fa-f]{2}(:[0-9A-Fa-f]{2}){5}$");
        assert_eq!(duration["maxLength"], 21);
        assert_eq!(firmware["minLength"], 1);
        Ok(())
    }
}
