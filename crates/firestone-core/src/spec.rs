//! Machine configuration shared by the CLI, files on disk, and the REST API.

mod port_forward;
mod validation;
mod value;

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{ErrorKind, FirestoneError};

pub use port_forward::{ParsePortForwardError, PortForward, PortRange, Protocol};
pub use validation::{
    RealValidationHost, SpecWarning, ValidationContext, ValidationHost, validate_machine_spec,
};
pub use value::{
    Arch, ByteSize, Firmware, HumanDuration, ImageRef, MacAddr, ParseByteSizeError,
    ParseDurationError, ParseFirmwareError, ParseMacAddrError,
};

/// Desired state of a machine. The same type is TOML on disk and JSON over REST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MachineSpec {
    pub image: ImageRef,
    pub arch: Option<Arch>,
    pub cpus: u8,
    pub memory: ByteSize,
    pub disk: ByteSize,
    pub user: String,
    pub network: NetworkSpec,
    #[serde(rename = "mount")]
    pub mounts: Vec<MountSpec>,
    pub cloud_init: CloudInitSpec,
    pub vmm: VmmSpec,
}

impl Default for MachineSpec {
    fn default() -> Self {
        Self {
            image: ImageRef::default(),
            arch: None,
            cpus: 2,
            memory: ByteSize::from_gib(2),
            disk: ByteSize::from_gib(20),
            user: "root".to_owned(),
            network: NetworkSpec::default(),
            mounts: Vec::new(),
            cloud_init: CloudInitSpec::default(),
            vmm: VmmSpec::default(),
        }
    }
}

impl MachineSpec {
    /// Parses an optional-key machine TOML document over built-in defaults.
    pub fn from_toml(input: &str) -> Result<Self, FirestoneError> {
        let patch = MachineSpecPatch::from_toml(input)?;
        Ok(Self::from_layers(
            &MachineSpecPatch::default(),
            &patch,
            &MachineSpecPatch::default(),
        ))
    }

    /// Applies all configuration layers in their normative order.
    #[must_use]
    pub fn from_layers(
        global_defaults: &MachineSpecPatch,
        machine: &MachineSpecPatch,
        patch: &MachineSpecPatch,
    ) -> Self {
        let mut spec = Self::default();
        global_defaults.apply_to(&mut spec);
        machine.apply_to(&mut spec);
        patch.apply_to(&mut spec);
        spec
    }

    /// Parses, layers, expands paths, and validates a machine TOML document.
    pub fn load(
        input: &str,
        global: &GlobalConfig,
        patch: &MachineSpecPatch,
        context: &ValidationContext<'_>,
    ) -> Result<LoadedMachineSpec, FirestoneError> {
        let machine = MachineSpecPatch::from_toml(input)?;
        let mut spec = Self::from_layers(&global.defaults, &machine, patch);
        let warnings = validate_machine_spec(&mut spec, context)?;
        Ok(LoadedMachineSpec { spec, warnings })
    }
}

/// Validated output from deterministic spec loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMachineSpec {
    pub spec: MachineSpec,
    pub warnings: Vec<SpecWarning>,
}

/// Network configuration for one machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSpec {
    pub mode: NetMode,
    pub forward: Vec<PortForward>,
    pub tap: Option<String>,
    pub mac: Option<MacAddr>,
}

/// Network backend selected for a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetMode {
    #[default]
    Passt,
    Tap,
    None,
}

/// One host directory made available inside the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    pub host: PathBuf,
    pub guest: PathBuf,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub tag: Option<String>,
}

impl MountSpec {
    #[must_use]
    pub fn effective_tag(&self, index: usize) -> String {
        match &self.tag {
            Some(tag) => tag.clone(),
            None => format!("share{index}"),
        }
    }
}

/// Cloud-init inputs and Firestone provisioning policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudInitSpec {
    pub user_data: Option<PathBuf>,
    pub network_config: Option<PathBuf>,
    pub ssh_keys: Vec<PathBuf>,
    pub provisioning: bool,
}

impl Default for CloudInitSpec {
    fn default() -> Self {
        Self {
            user_data: None,
            network_config: None,
            ssh_keys: Vec::new(),
            provisioning: true,
        }
    }
}

/// VMM-specific overrides retained as the documented escape hatch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VmmSpec {
    pub binary: Option<PathBuf>,
    pub firmware: Firmware,
    pub extra_args: Vec<String>,
    pub config_overlay: Option<serde_json::Value>,
}

/// Sparse machine update used by defaults, machine files, CLI flags, and REST PATCH.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MachineSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<Arch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<ByteSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<ByteSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkSpecPatch>,
    #[serde(rename = "mount", skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<MountSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<CloudInitSpecPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vmm: Option<VmmSpecPatch>,
}

impl MachineSpecPatch {
    /// Parses a machine layer while preserving which keys were absent.
    pub fn from_toml(input: &str) -> Result<Self, FirestoneError> {
        validate_known_keys(input, TomlSchema::Machine, "firestone.toml")?;
        deserialize_toml(input, "firestone.toml")
    }

    /// Applies replacements and appends vector leaves to an effective spec.
    pub fn apply_to(&self, spec: &mut MachineSpec) {
        if let Some(image) = &self.image {
            spec.image = image.clone();
        }
        if let Some(arch) = self.arch {
            spec.arch = Some(arch);
        }
        if let Some(cpus) = self.cpus {
            spec.cpus = cpus;
        }
        if let Some(memory) = self.memory {
            spec.memory = memory;
        }
        if let Some(disk) = self.disk {
            spec.disk = disk;
        }
        if let Some(user) = &self.user {
            spec.user.clone_from(user);
        }
        if let Some(network) = &self.network {
            network.apply_to(&mut spec.network);
        }
        if let Some(mounts) = &self.mounts {
            spec.mounts.extend(mounts.iter().cloned());
        }
        if let Some(cloud_init) = &self.cloud_init {
            cloud_init.apply_to(&mut spec.cloud_init);
        }
        if let Some(vmm) = &self.vmm {
            vmm.apply_to(&mut spec.vmm);
        }
    }

    /// Anchors relative paths in one layer before it is merged with other sources.
    ///
    /// CLI adapters call this with the invocation working directory. Machine-file
    /// loading anchors its own layer against the machine directory during validation.
    pub fn resolve_paths(
        &mut self,
        host: &dyn ValidationHost,
        base_dir: &Path,
    ) -> Result<(), FirestoneError> {
        validation::resolve_patch_paths(self, host, base_dir)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward: Option<Vec<PortForward>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<MacAddr>,
}

impl NetworkSpecPatch {
    fn apply_to(&self, spec: &mut NetworkSpec) {
        if let Some(mode) = self.mode {
            spec.mode = mode;
        }
        if let Some(forward) = &self.forward {
            spec.forward.extend(forward.iter().cloned());
        }
        if let Some(tap) = &self.tap {
            spec.tap = Some(tap.clone());
        }
        if let Some(mac) = self.mac {
            spec.mac = Some(mac);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudInitSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_config: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_keys: Option<Vec<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisioning: Option<bool>,
}

impl CloudInitSpecPatch {
    fn apply_to(&self, spec: &mut CloudInitSpec) {
        if let Some(user_data) = &self.user_data {
            spec.user_data = Some(user_data.clone());
        }
        if let Some(network_config) = &self.network_config {
            spec.network_config = Some(network_config.clone());
        }
        if let Some(ssh_keys) = &self.ssh_keys {
            spec.ssh_keys.extend(ssh_keys.iter().cloned());
        }
        if let Some(provisioning) = self.provisioning {
            spec.provisioning = provisioning;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VmmSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<Firmware>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_overlay: Option<serde_json::Value>,
}

impl VmmSpecPatch {
    fn apply_to(&self, spec: &mut VmmSpec) {
        if let Some(binary) = &self.binary {
            spec.binary = Some(binary.clone());
        }
        if let Some(firmware) = &self.firmware {
            spec.firmware = firmware.clone();
        }
        if let Some(extra_args) = &self.extra_args {
            spec.extra_args.extend(extra_args.iter().cloned());
        }
        if let Some(config_overlay) = &self.config_overlay {
            spec.config_overlay = Some(config_overlay.clone());
        }
    }
}

/// Process-wide configuration from `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalConfig {
    pub defaults: MachineSpecPatch,
    pub start: StartConfig,
    pub stop: StopConfig,
    pub ui: UiConfig,
    pub images: ImagesConfig,
}

impl GlobalConfig {
    pub fn from_toml(input: &str) -> Result<Self, FirestoneError> {
        validate_known_keys(input, TomlSchema::Global, "config.toml")?;
        deserialize_toml(input, "config.toml")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StartConfig {
    pub timeout_first_boot: HumanDuration,
    pub timeout: HumanDuration,
}

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            timeout_first_boot: HumanDuration::from_secs(180),
            timeout: HumanDuration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StopConfig {
    pub timeout: HumanDuration,
}

impl Default for StopConfig {
    fn default() -> Self {
        Self {
            timeout: HumanDuration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub color: ColorMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesConfig {
    pub catalog: Vec<PathBuf>,
}

/// How one patch leaf combines with an earlier configuration layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchMerge {
    Replace,
    Append,
}

/// Clap-free metadata used by the CLI crate to verify its flag projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecFieldMetadata {
    pub key: &'static str,
    pub long: &'static str,
    pub short: Option<char>,
    pub merge: PatchMerge,
    pub composite: bool,
}

/// Complete CLI-facing field inventory from §7.4.
pub const SPEC_FIELD_METADATA: &[SpecFieldMetadata] = &[
    field("image", "image", None, PatchMerge::Replace, false),
    field("arch", "arch", None, PatchMerge::Replace, false),
    field("cpus", "cpus", None, PatchMerge::Replace, false),
    field("memory", "memory", None, PatchMerge::Replace, false),
    field("disk", "disk", None, PatchMerge::Replace, false),
    field("user", "user", None, PatchMerge::Replace, false),
    field("network.mode", "net", None, PatchMerge::Replace, false),
    field(
        "network.forward",
        "forward",
        Some('p'),
        PatchMerge::Append,
        false,
    ),
    field("network.tap", "tap", None, PatchMerge::Replace, false),
    field(
        "network.mac",
        "network-mac",
        None,
        PatchMerge::Replace,
        false,
    ),
    field("mount", "mount", None, PatchMerge::Append, true),
    field(
        "cloud_init.user_data",
        "user-data",
        None,
        PatchMerge::Replace,
        false,
    ),
    field(
        "cloud_init.network_config",
        "cloud-init-network-config",
        None,
        PatchMerge::Replace,
        false,
    ),
    field(
        "cloud_init.ssh_keys",
        "ssh-key",
        None,
        PatchMerge::Append,
        false,
    ),
    field(
        "cloud_init.provisioning",
        "no-provisioning",
        None,
        PatchMerge::Replace,
        false,
    ),
    field("vmm.binary", "vmm-binary", None, PatchMerge::Replace, false),
    field(
        "vmm.firmware",
        "vmm-firmware",
        None,
        PatchMerge::Replace,
        false,
    ),
    field("vmm.extra_args", "vmm-arg", None, PatchMerge::Append, false),
    field(
        "vmm.config_overlay",
        "vmm-config",
        None,
        PatchMerge::Replace,
        false,
    ),
];

const fn field(
    key: &'static str,
    long: &'static str,
    short: Option<char>,
    merge: PatchMerge,
    composite: bool,
) -> SpecFieldMetadata {
    SpecFieldMetadata {
        key,
        long,
        short,
        merge,
        composite,
    }
}

fn deserialize_toml<T>(input: &str, file: &str) -> Result<T, FirestoneError>
where
    T: DeserializeOwned,
{
    let deserializer = toml::de::Deserializer::new(input);
    serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let path = error.path().to_string();
        let path = if path.is_empty() { "<root>" } else { &path };
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("invalid value for '{path}' in {file}: {}", error.inner()),
        )
        .with_hint(format!("correct '{path}' in {file}"))
    })
}

#[derive(Clone, Copy)]
enum TomlSchema {
    Machine,
    Global,
}

#[derive(Clone, Copy)]
enum TableSchema {
    Machine,
    Network,
    Mount,
    CloudInit,
    Vmm,
    Global,
    Start,
    Stop,
    Ui,
    Images,
}

impl TableSchema {
    const fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Machine => &[
                "image",
                "arch",
                "cpus",
                "memory",
                "disk",
                "user",
                "network",
                "mount",
                "cloud_init",
                "vmm",
            ],
            Self::Network => &["mode", "forward", "tap", "mac"],
            Self::Mount => &["host", "guest", "readonly", "tag"],
            Self::CloudInit => &["user_data", "network_config", "ssh_keys", "provisioning"],
            Self::Vmm => &["binary", "firmware", "extra_args", "config_overlay"],
            Self::Global => &["defaults", "start", "stop", "ui", "images"],
            Self::Start => &["timeout_first_boot", "timeout"],
            Self::Stop => &["timeout"],
            Self::Ui => &["color"],
            Self::Images => &["catalog"],
        }
    }

    fn child(self, key: &str) -> Option<Self> {
        match (self, key) {
            (Self::Machine, "network") => Some(Self::Network),
            (Self::Machine, "mount") => Some(Self::Mount),
            (Self::Machine, "cloud_init") => Some(Self::CloudInit),
            (Self::Machine, "vmm") => Some(Self::Vmm),
            (Self::Global, "defaults") => Some(Self::Machine),
            (Self::Global, "start") => Some(Self::Start),
            (Self::Global, "stop") => Some(Self::Stop),
            (Self::Global, "ui") => Some(Self::Ui),
            (Self::Global, "images") => Some(Self::Images),
            _ => None,
        }
    }
}

fn validate_known_keys(input: &str, schema: TomlSchema, file: &str) -> Result<(), FirestoneError> {
    let value = input.parse::<toml::Value>().map_err(|error| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot parse {file}: {error}"),
        )
        .with_hint(format!("fix the TOML syntax in {file}"))
    })?;
    let root = match schema {
        TomlSchema::Machine => TableSchema::Machine,
        TomlSchema::Global => TableSchema::Global,
    };
    validate_table_keys(&value, root, "", file)
}

fn validate_table_keys(
    value: &toml::Value,
    schema: TableSchema,
    prefix: &str,
    file: &str,
) -> Result<(), FirestoneError> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for (key, child_value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if !schema.keys().contains(&key.as_str()) {
            let suggestion = closest_key(key, schema.keys()).map(|candidate| {
                if prefix.is_empty() {
                    candidate.to_owned()
                } else {
                    format!("{prefix}.{candidate}")
                }
            });
            let error = FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("unknown key '{path}' in {file}"),
            );
            return Err(match suggestion {
                Some(suggestion) => error.with_hint(format!("did you mean '{suggestion}'?")),
                None => {
                    error.with_hint(format!("remove '{path}' or use a key documented in {file}"))
                }
            });
        }
        if let Some(child_schema) = schema.child(key) {
            match child_value {
                toml::Value::Array(values) if matches!(child_schema, TableSchema::Mount) => {
                    for (index, value) in values.iter().enumerate() {
                        validate_table_keys(
                            value,
                            child_schema,
                            &format!("{path}[{index}]"),
                            file,
                        )?;
                    }
                }
                _ => validate_table_keys(child_value, child_schema, &path, file)?,
            }
        }
    }
    Ok(())
}

fn closest_key<'a>(unknown: &str, candidates: &'a [&str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .map(|candidate| (candidate, edit_distance(unknown, candidate)))
        .filter(|(_, distance)| *distance <= 3)
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| candidate)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous: Vec<usize> = (0..=right.chars().count()).collect();
    let mut current = vec![0; previous.len()];
    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.chars().enumerate() {
            current[right_index + 1] = if left_char == right_char {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(current[right_index])
                    .min(previous[right_index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.chars().count()]
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use serde_json::{Value, json};

    use super::{
        Arch, ByteSize, CloudInitSpec, CloudInitSpecPatch, ColorMode, Firmware, GlobalConfig,
        ImageRef, MachineSpec, MachineSpecPatch, MountSpec, NetMode, NetworkSpec, NetworkSpecPatch,
        PatchMerge, SPEC_FIELD_METADATA, VmmSpec, VmmSpecPatch,
    };
    use crate::ErrorKind;

    #[test]
    fn machine_spec_defaults_match_documented_values() {
        let spec = MachineSpec::default();
        assert_eq!(spec.image, ImageRef::from("ubuntu:24.04"));
        assert_eq!(spec.arch, None);
        assert_eq!(spec.cpus, 2);
        assert_eq!(spec.memory, ByteSize::from_gib(2));
        assert_eq!(spec.disk, ByteSize::from_gib(20));
        assert_eq!(spec.user, "root");
        assert_eq!(spec.network.mode, NetMode::Passt);
        assert!(spec.network.forward.is_empty());
        assert!(spec.mounts.is_empty());
        assert!(spec.cloud_init.provisioning);
        assert_eq!(spec.vmm.firmware, Firmware::AUTO);
    }

    #[test]
    fn machine_toml_missing_keys_uses_defaults() -> Result<(), crate::FirestoneError> {
        let spec = MachineSpec::from_toml("cpus = 4")?;
        assert_eq!(spec.cpus, 4);
        assert_eq!(spec.memory, ByteSize::from_gib(2));
        assert_eq!(spec.network, NetworkSpec::default());
        Ok(())
    }

    #[test]
    fn machine_spec_toml_round_trip_preserves_values() -> Result<(), Box<dyn std::error::Error>> {
        let spec = populated_spec();
        let encoded = toml::to_string_pretty(&spec)?;
        let decoded = MachineSpec::from_toml(&encoded)?;
        assert_eq!(decoded, spec);
        Ok(())
    }

    #[test]
    fn machine_toml_unknown_root_key_suggests_match() {
        let error = MachineSpec::from_toml("memeory = \"2G\"").expect_err("unknown key");
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("memeory"));
        assert_eq!(error.hint(), Some("did you mean 'memory'?"));
    }

    #[test]
    fn machine_toml_unknown_nested_key_reports_full_path() {
        let error = MachineSpec::from_toml("[network]\nmdoe = \"passt\"").expect_err("unknown key");
        assert!(error.message().contains("network.mdoe"));
        assert_eq!(error.hint(), Some("did you mean 'network.mode'?"));
    }

    #[test]
    fn machine_toml_unknown_second_mount_key_reports_array_index() {
        let input = r#"
[[mount]]
host = "/first"
guest = "/first"

[[mount]]
host = "/second"
guest = "/second"
readnoly = true
"#;
        let error = MachineSpec::from_toml(input).expect_err("unknown mount key");
        assert!(error.message().contains("mount[1].readnoly"));
        assert_eq!(error.hint(), Some("did you mean 'mount[1].readonly'?"));
    }

    #[test]
    fn machine_toml_invalid_leaf_reports_key_path() {
        let error =
            MachineSpec::from_toml("[network]\nforward = [\"0:22\"]").expect_err("invalid port");
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error.message().contains("network.forward[0]"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn machine_toml_cpu_count_above_u8_reports_key_path() {
        let error = MachineSpec::from_toml("cpus = 256").expect_err("u8 overflow");
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("cpus"), "{}", error.message());
        assert!(error.hint().is_some());
    }

    #[test]
    fn layering_all_layers_applies_in_order_and_appends_vectors() {
        let global = MachineSpecPatch {
            cpus: Some(3),
            network: Some(NetworkSpecPatch {
                forward: Some(vec!["8000:80".parse().expect("valid forward")]),
                ..NetworkSpecPatch::default()
            }),
            mounts: Some(vec![mount("global", "/global")]),
            cloud_init: Some(CloudInitSpecPatch {
                ssh_keys: Some(vec![PathBuf::from("global.pub")]),
                ..CloudInitSpecPatch::default()
            }),
            vmm: Some(VmmSpecPatch {
                extra_args: Some(vec!["--global".to_owned()]),
                ..VmmSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };
        let machine = MachineSpecPatch {
            cpus: Some(4),
            network: Some(NetworkSpecPatch {
                forward: Some(vec!["8500:85".parse().expect("valid forward")]),
                ..NetworkSpecPatch::default()
            }),
            mounts: Some(vec![mount("machine", "/machine")]),
            cloud_init: Some(CloudInitSpecPatch {
                ssh_keys: Some(vec![PathBuf::from("machine.pub")]),
                ..CloudInitSpecPatch::default()
            }),
            vmm: Some(VmmSpecPatch {
                extra_args: Some(vec!["--machine".to_owned()]),
                ..VmmSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };
        let cli = MachineSpecPatch {
            cpus: Some(6),
            network: Some(NetworkSpecPatch {
                forward: Some(vec!["9000:90".parse().expect("valid forward")]),
                ..NetworkSpecPatch::default()
            }),
            mounts: Some(vec![mount("cli", "/cli")]),
            cloud_init: Some(CloudInitSpecPatch {
                ssh_keys: Some(vec![PathBuf::from("cli.pub")]),
                ..CloudInitSpecPatch::default()
            }),
            vmm: Some(VmmSpecPatch {
                extra_args: Some(vec!["--cli".to_owned()]),
                ..VmmSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };

        let spec = MachineSpec::from_layers(&global, &machine, &cli);
        assert_eq!(spec.cpus, 6);
        assert_eq!(
            spec.network.forward,
            [
                "8000:80".parse().expect("valid forward"),
                "8500:85".parse().expect("valid forward"),
                "9000:90".parse().expect("valid forward")
            ]
        );
        assert_eq!(
            spec.mounts
                .iter()
                .map(|mount| mount.host.as_path())
                .collect::<Vec<_>>(),
            [
                std::path::Path::new("global"),
                std::path::Path::new("machine"),
                std::path::Path::new("cli")
            ]
        );
        assert_eq!(
            spec.cloud_init.ssh_keys,
            [
                PathBuf::from("global.pub"),
                PathBuf::from("machine.pub"),
                PathBuf::from("cli.pub")
            ]
        );
        assert_eq!(spec.vmm.extra_args, ["--global", "--machine", "--cli"]);
    }

    #[test]
    fn machine_load_composes_layers_paths_validation_and_warnings()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        std::fs::create_dir(temporary.path().join("share"))?;
        let global = GlobalConfig {
            defaults: MachineSpecPatch {
                user: Some("global".to_owned()),
                ..MachineSpecPatch::default()
            },
            ..GlobalConfig::default()
        };
        let patch = MachineSpecPatch {
            user: Some("cli".to_owned()),
            ..MachineSpecPatch::default()
        };
        let input = r#"
cpus = 1
user = "machine"

[[mount]]
host = "share"
guest = "/share"
"#;
        let host = super::RealValidationHost::new();
        let catalog = crate::Catalog::built_in()?;
        let context = super::ValidationContext::new(&host, temporary.path(), &catalog);

        let loaded = MachineSpec::load(input, &global, &patch, &context)?;

        assert_eq!(loaded.spec.cpus, 1);
        assert_eq!(loaded.spec.user, "cli");
        assert_eq!(loaded.spec.mounts[0].host, temporary.path().join("share"));
        assert!(loaded.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn global_config_missing_keys_uses_documented_defaults() -> Result<(), crate::FirestoneError> {
        let config = GlobalConfig::from_toml("")?;
        assert_eq!(config, GlobalConfig::default());
        assert_eq!(config.start.timeout_first_boot.get().as_secs(), 180);
        assert_eq!(config.start.timeout.get().as_secs(), 60);
        assert_eq!(config.stop.timeout.get().as_secs(), 30);
        assert_eq!(config.ui.color, ColorMode::Auto);
        Ok(())
    }

    #[test]
    fn global_config_unknown_defaults_key_reports_full_path() {
        let error =
            GlobalConfig::from_toml("[defaults]\nmemeory = \"4G\"").expect_err("unknown key");
        assert!(error.message().contains("defaults.memeory"));
        assert_eq!(error.hint(), Some("did you mean 'defaults.memory'?"));
    }

    #[test]
    fn global_config_invalid_duration_reports_key_path() {
        let error =
            GlobalConfig::from_toml("[start]\ntimeout = \"0s\"").expect_err("invalid duration");
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error.message().contains("start.timeout"),
            "{}",
            error.message()
        );
        assert!(error.hint().is_some());
    }

    #[test]
    fn fully_populated_spec_and_patch_recursive_keys_match() -> Result<(), serde_json::Error> {
        let spec = serde_json::to_value(populated_spec())?;
        let patch = serde_json::to_value(populated_patch())?;
        assert_eq!(recursive_keys(&spec), recursive_keys(&patch));
        Ok(())
    }

    #[test]
    fn fully_populated_spec_and_patch_json_round_trip() -> Result<(), serde_json::Error> {
        let spec = populated_spec();
        let patch = populated_patch();
        let spec_encoded = serde_json::to_vec(&spec)?;
        let patch_encoded = serde_json::to_vec(&patch)?;

        assert_eq!(serde_json::from_slice::<MachineSpec>(&spec_encoded)?, spec);
        assert_eq!(
            serde_json::from_slice::<MachineSpecPatch>(&patch_encoded)?,
            patch
        );
        Ok(())
    }

    #[test]
    fn machine_spec_schema_denies_unknown_keys_and_uses_mount_name() -> Result<(), serde_json::Error>
    {
        let schema = serde_json::to_value(schemars::schema_for!(MachineSpec))?;
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("mount").is_some());
        assert!(schema["properties"].get("mounts").is_none());
        Ok(())
    }

    #[test]
    fn field_metadata_covers_cli_projection_and_append_leaves() {
        let keys: BTreeSet<_> = SPEC_FIELD_METADATA.iter().map(|field| field.key).collect();
        let expected = BTreeSet::from([
            "image",
            "arch",
            "cpus",
            "memory",
            "disk",
            "user",
            "network.mode",
            "network.forward",
            "network.tap",
            "network.mac",
            "mount",
            "cloud_init.user_data",
            "cloud_init.network_config",
            "cloud_init.ssh_keys",
            "cloud_init.provisioning",
            "vmm.binary",
            "vmm.firmware",
            "vmm.extra_args",
            "vmm.config_overlay",
        ]);
        assert_eq!(keys, expected);
        assert_eq!(
            SPEC_FIELD_METADATA
                .iter()
                .filter(|field| field.merge == PatchMerge::Append)
                .map(|field| field.key)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "network.forward",
                "mount",
                "cloud_init.ssh_keys",
                "vmm.extra_args"
            ])
        );
    }

    fn populated_spec() -> MachineSpec {
        MachineSpec {
            image: ImageRef::from("ubuntu:24.04"),
            arch: Some(Arch::X86_64),
            cpus: 4,
            memory: ByteSize::from_gib(8),
            disk: ByteSize::from_gib(40),
            user: "developer".to_owned(),
            network: NetworkSpec {
                mode: NetMode::Tap,
                forward: vec!["tcp:127.0.0.1:8080:80".parse().expect("valid forward")],
                tap: Some("tap0".to_owned()),
                mac: Some("52:54:00:9a:1f:c3".parse().expect("valid mac")),
            },
            mounts: vec![MountSpec {
                host: PathBuf::from("/home/developer/code"),
                guest: PathBuf::from("/code"),
                readonly: true,
                tag: Some("code".to_owned()),
            }],
            cloud_init: CloudInitSpec {
                user_data: Some(PathBuf::from("/tmp/user-data")),
                network_config: Some(PathBuf::from("/tmp/network-config")),
                ssh_keys: vec![PathBuf::from("/tmp/id.pub")],
                provisioning: false,
            },
            vmm: VmmSpec {
                binary: Some(PathBuf::from("/opt/cloud-hypervisor")),
                firmware: Firmware::path("/opt/CLOUDHV.fd"),
                extra_args: vec!["--verbose".to_owned()],
                config_overlay: Some(json!({"memory": {"shared": true}})),
            },
        }
    }

    fn mount(host: &str, guest: &str) -> MountSpec {
        MountSpec {
            host: PathBuf::from(host),
            guest: PathBuf::from(guest),
            readonly: false,
            tag: None,
        }
    }

    fn populated_patch() -> MachineSpecPatch {
        let spec = populated_spec();
        MachineSpecPatch {
            image: Some(spec.image),
            arch: spec.arch,
            cpus: Some(spec.cpus),
            memory: Some(spec.memory),
            disk: Some(spec.disk),
            user: Some(spec.user),
            network: Some(NetworkSpecPatch {
                mode: Some(spec.network.mode),
                forward: Some(spec.network.forward),
                tap: spec.network.tap,
                mac: spec.network.mac,
            }),
            mounts: Some(spec.mounts),
            cloud_init: Some(CloudInitSpecPatch {
                user_data: spec.cloud_init.user_data,
                network_config: spec.cloud_init.network_config,
                ssh_keys: Some(spec.cloud_init.ssh_keys),
                provisioning: Some(spec.cloud_init.provisioning),
            }),
            vmm: Some(VmmSpecPatch {
                binary: spec.vmm.binary,
                firmware: Some(spec.vmm.firmware),
                extra_args: Some(spec.vmm.extra_args),
                config_overlay: spec.vmm.config_overlay,
            }),
        }
    }

    fn recursive_keys(value: &Value) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        collect_keys(value, "", &mut keys);
        keys
    }

    fn collect_keys(value: &Value, prefix: &str, keys: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    keys.insert(path.clone());
                    collect_keys(value, &path, keys);
                }
            }
            Value::Array(values) => {
                for value in values {
                    collect_keys(value, prefix, keys);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
}
