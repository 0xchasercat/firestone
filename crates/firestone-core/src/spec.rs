//! Machine configuration shared by the CLI, files on disk, and the REST API.

mod port_forward;
mod validation;
mod value;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};

use crate::{ErrorKind, FirestoneError, Paths};

pub use port_forward::{ParsePortForwardError, PortForward, PortRange, Protocol};
pub use validation::{
    RealValidationHost, SpecWarning, ValidationContext, ValidationHost, validate_machine_spec,
};
pub use value::{
    Arch, ByteSize, Firmware, HumanDuration, ImageRef, MacAddr, ParseByteSizeError,
    ParseDurationError, ParseFirmwareError, ParseMacAddrError,
};

/// Validates a guest login name before it is passed to cloud-init or OpenSSH.
pub fn validate_guest_user(user: &str) -> Result<(), FirestoneError> {
    validation::validate_user(user)
}

/// Desired state of a machine. The same type is TOML on disk and JSON over REST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MachineSpec {
    pub image: ImageRef,
    pub arch: Option<Arch>,
    pub cpus: u8,
    pub cpus_max: Option<u8>,
    pub memory: ByteSize,
    pub memory_max: Option<ByteSize>,
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
            cpus_max: None,
            memory: ByteSize::BUILTIN_MEMORY,
            memory_max: None,
            disk: ByteSize::BUILTIN_DISK,
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
        Self::from_layers(
            &MachineSpecPatch::default(),
            &patch,
            &MachineSpecPatch::default(),
        )
    }

    /// Applies all configuration layers in their normative order.
    pub fn from_layers(
        global_defaults: &MachineSpecPatch,
        machine: &MachineSpecPatch,
        patch: &MachineSpecPatch,
    ) -> Result<Self, FirestoneError> {
        let mut spec = Self::default();
        global_defaults.apply_to_with_vectors(&mut spec, VectorMerge::Append)?;
        machine.apply_to_with_vectors(&mut spec, VectorMerge::Replace)?;
        patch.apply_to_with_vectors(&mut spec, VectorMerge::Append)?;
        Ok(spec)
    }

    /// Serializes a complete effective spec for `firestone.toml` persistence.
    ///
    /// The persisted machine layer sets every concrete value and records clear
    /// markers for absent optional values. Loading it over the same global
    /// defaults therefore reproduces this spec exactly.
    pub fn to_toml(&self) -> Result<String, FirestoneError> {
        MachineSpecPatch::from(self).to_toml()
    }

    /// Converts a complete effective spec into its lossless machine-file layer.
    #[must_use]
    pub fn to_persisted_patch(&self) -> MachineSpecPatch {
        MachineSpecPatch::from(self)
    }

    /// Parses, layers, expands paths, and validates a machine TOML document.
    pub fn load(
        input: &str,
        global: &GlobalConfig,
        patch: &MachineSpecPatch,
        patch_base_dir: &Path,
        context: &ValidationContext<'_>,
    ) -> Result<LoadedMachineSpec, FirestoneError> {
        let machine = MachineSpecPatch::from_toml(input)?;
        let mut resolved_patch = patch.clone();
        resolved_patch.resolve_paths(context.paths, patch_base_dir)?;
        let mut spec = Self::from_layers(&global.defaults, &machine, &resolved_patch)?;
        let image_base_dir = if resolved_patch.image.is_some() {
            patch_base_dir
        } else {
            context.machine_dir
        };
        let warnings =
            validation::validate_machine_spec_with_image_base(&mut spec, context, image_base_dir)?;
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudInitSpec {
    pub user_data: Option<PathBuf>,
    pub user_data_inline: Option<String>,
    pub network_config: Option<PathBuf>,
    pub ssh_keys: Vec<PathBuf>,
    pub ssh_authorized_keys: Vec<String>,
    pub password: Option<String>,
    pub ssh_pwauth: bool,
    pub provisioning: bool,
}

impl Default for CloudInitSpec {
    fn default() -> Self {
        Self {
            user_data: None,
            user_data_inline: None,
            network_config: None,
            ssh_keys: Vec::new(),
            ssh_authorized_keys: Vec::new(),
            password: None,
            ssh_pwauth: false,
            provisioning: true,
        }
    }
}

/// Placeholder printed instead of guest-secret or cloud-init contents.
pub(crate) const REDACTED: &str = "<redacted>";

fn redacted(value: Option<&impl AsRef<str>>) -> Option<&'static str> {
    value.map(|_| REDACTED)
}

/// Redacts inline user-data and the guest password in every debug rendering.
///
/// `MachineSpec` derives `Debug`, so anything that formats a spec — a panic
/// message, a trace line, a test assertion — reaches this implementation.
impl std::fmt::Debug for CloudInitSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudInitSpec")
            .field("user_data", &self.user_data)
            .field(
                "user_data_inline",
                &redacted(self.user_data_inline.as_ref()),
            )
            .field("network_config", &self.network_config)
            .field("ssh_keys", &self.ssh_keys)
            .field("ssh_authorized_keys", &self.ssh_authorized_keys)
            .field("password", &redacted(self.password.as_ref()))
            .field("ssh_pwauth", &self.ssh_pwauth)
            .field("provisioning", &self.provisioning)
            .finish()
    }
}

/// VMM-specific overrides retained as the documented escape hatch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VmmSpec {
    pub binary: Option<PathBuf>,
    pub firmware: Firmware,
    pub extra_args: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_overlay")]
    #[schemars(with = "Option<std::collections::BTreeMap<String, serde_json::Value>>")]
    pub config_overlay: Option<serde_json::Value>,
}

/// A machine-spec value that a sparse layer may explicitly clear.
///
/// Strings use the same serialized paths in TOML, JSON, and CLI patches.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum SpecClear {
    #[serde(rename = "arch")]
    Arch,
    #[serde(rename = "network.forward")]
    NetworkForward,
    #[serde(rename = "network.tap")]
    NetworkTap,
    #[serde(rename = "network.mac")]
    NetworkMac,
    #[serde(rename = "mount")]
    Mount,
    #[serde(rename = "cloud_init.user_data")]
    CloudInitUserData,
    #[serde(rename = "cloud_init.user_data_inline")]
    CloudInitUserDataInline,
    #[serde(rename = "cloud_init.network_config")]
    CloudInitNetworkConfig,
    #[serde(rename = "cloud_init.ssh_keys")]
    CloudInitSshKeys,
    #[serde(rename = "cloud_init.ssh_authorized_keys")]
    CloudInitSshAuthorizedKeys,
    #[serde(rename = "cloud_init.password")]
    CloudInitPassword,
    #[serde(rename = "vmm.binary")]
    VmmBinary,
    #[serde(rename = "vmm.extra_args")]
    VmmExtraArgs,
    #[serde(rename = "vmm.config_overlay")]
    VmmConfigOverlay,
    #[serde(rename = "cpus_max")]
    CpusMax,
    #[serde(rename = "memory_max")]
    MemoryMax,
}

impl SpecClear {
    pub const ALL: &'static [Self] = &[
        Self::Arch,
        Self::NetworkForward,
        Self::NetworkTap,
        Self::NetworkMac,
        Self::Mount,
        Self::CloudInitUserData,
        Self::CloudInitUserDataInline,
        Self::CloudInitNetworkConfig,
        Self::CloudInitSshKeys,
        Self::CloudInitSshAuthorizedKeys,
        Self::CloudInitPassword,
        Self::VmmBinary,
        Self::VmmExtraArgs,
        Self::VmmConfigOverlay,
        Self::CpusMax,
        Self::MemoryMax,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Arch => "arch",
            Self::NetworkForward => "network.forward",
            Self::NetworkTap => "network.tap",
            Self::NetworkMac => "network.mac",
            Self::Mount => "mount",
            Self::CloudInitUserData => "cloud_init.user_data",
            Self::CloudInitUserDataInline => "cloud_init.user_data_inline",
            Self::CloudInitNetworkConfig => "cloud_init.network_config",
            Self::CloudInitSshKeys => "cloud_init.ssh_keys",
            Self::CloudInitSshAuthorizedKeys => "cloud_init.ssh_authorized_keys",
            Self::CloudInitPassword => "cloud_init.password",
            Self::VmmBinary => "vmm.binary",
            Self::VmmExtraArgs => "vmm.extra_args",
            Self::VmmConfigOverlay => "vmm.config_overlay",
            Self::CpusMax => "cpus_max",
            Self::MemoryMax => "memory_max",
        }
    }
}

impl std::fmt::Display for SpecClear {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for SpecClear {
    type Err = ParseSpecClearError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|field| field.as_str() == value)
            .ok_or_else(|| ParseSpecClearError {
                value: value.to_owned(),
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown clear field '{value}'")]
pub struct ParseSpecClearError {
    value: String,
}

/// Sparse machine update used by defaults, machine files, CLI flags, and REST PATCH.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MachineSpecPatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<SpecClear>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<Arch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus_max: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<ByteSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<ByteSize>,
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

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MachineSpecPatchWire {
    clear: Vec<SpecClear>,
    image: Option<ImageRef>,
    arch: Option<Arch>,
    cpus: Option<u8>,
    cpus_max: Option<u8>,
    memory: Option<ByteSize>,
    memory_max: Option<ByteSize>,
    disk: Option<ByteSize>,
    user: Option<String>,
    network: Option<NetworkSpecPatch>,
    #[serde(rename = "mount")]
    mounts: Option<Vec<MountSpec>>,
    cloud_init: Option<CloudInitSpecPatch>,
    vmm: Option<VmmSpecPatch>,
}

impl<'de> Deserialize<'de> for MachineSpecPatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MachineSpecPatchWire::deserialize(deserializer)?;
        let patch = Self {
            clear: wire.clear,
            image: wire.image,
            arch: wire.arch,
            cpus: wire.cpus,
            cpus_max: wire.cpus_max,
            memory: wire.memory,
            memory_max: wire.memory_max,
            disk: wire.disk,
            user: wire.user,
            network: wire.network,
            mounts: wire.mounts,
            cloud_init: wire.cloud_init,
            vmm: wire.vmm,
        };
        patch.validate().map_err(serde::de::Error::custom)?;
        Ok(patch)
    }
}

impl MachineSpecPatch {
    /// Parses a machine layer while preserving which keys were absent.
    pub fn from_toml(input: &str) -> Result<Self, FirestoneError> {
        validate_known_keys(input, TomlSchema::Machine, "firestone.toml")?;
        deserialize_patch_toml(input, "firestone.toml", &["vmm", "config_overlay"])
    }

    /// Serializes a sparse layer to its canonical TOML representation.
    ///
    /// `vmm.config_overlay` is JSON text in TOML so RFC 7396 nested nulls
    /// survive. Its JSON representation remains an object.
    pub fn to_toml(&self) -> Result<String, FirestoneError> {
        self.validate()?;
        let mut value = self.clone();
        let overlay = value.vmm.as_mut().and_then(|vmm| vmm.config_overlay.take());
        serialize_toml_with_overlay(
            &value,
            overlay.as_ref(),
            &["vmm", "config_overlay"],
            "firestone.toml",
        )
    }

    /// Applies a CLI or REST patch to an effective spec.
    pub fn apply_to(&self, spec: &mut MachineSpec) -> Result<(), FirestoneError> {
        self.apply_to_with_vectors(spec, VectorMerge::Append)
    }

    fn apply_to_with_vectors(
        &self,
        spec: &mut MachineSpec,
        vector_merge: VectorMerge,
    ) -> Result<(), FirestoneError> {
        self.validate()?;
        self.apply_clears(spec);
        if let Some(image) = &self.image {
            spec.image = image.clone();
        }
        if let Some(arch) = self.arch {
            spec.arch = Some(arch);
        }
        if let Some(cpus) = self.cpus {
            spec.cpus = cpus;
        }
        if let Some(cpus_max) = self.cpus_max {
            spec.cpus_max = Some(cpus_max);
        }
        if let Some(memory) = self.memory {
            spec.memory = memory;
        }
        if let Some(memory_max) = self.memory_max {
            spec.memory_max = Some(memory_max);
        }
        if let Some(disk) = self.disk {
            spec.disk = disk;
        }
        if let Some(user) = &self.user {
            spec.user.clone_from(user);
        }
        if let Some(network) = &self.network {
            network.apply_to(&mut spec.network, vector_merge);
        }
        if let Some(mounts) = &self.mounts {
            merge_vector(&mut spec.mounts, mounts, vector_merge);
        }
        if let Some(cloud_init) = &self.cloud_init {
            cloud_init.apply_to(&mut spec.cloud_init, vector_merge);
        }
        if let Some(vmm) = &self.vmm {
            vmm.apply_to(&mut spec.vmm, vector_merge);
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), FirestoneError> {
        if let Some(overlay) = self
            .vmm
            .as_ref()
            .and_then(|vmm| vmm.config_overlay.as_ref())
        {
            validate_overlay(overlay, "vmm.config_overlay")?;
        }
        let mut seen = BTreeSet::new();
        for clear in &self.clear {
            if !seen.insert(*clear) {
                return Err(FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!("clear path '{}' appears more than once", clear.as_str()),
                )
                .with_hint(format!("keep one '{}' entry in clear", clear.as_str())));
            }
            if self.sets(*clear) {
                return Err(FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!(
                        "patch sets and clears '{}' in the same layer",
                        clear.as_str()
                    ),
                )
                .with_hint(format!(
                    "remove '{}' from clear or omit its value",
                    clear.as_str()
                )));
            }
        }
        Ok(())
    }

    fn sets(&self, clear: SpecClear) -> bool {
        match clear {
            SpecClear::Arch => self.arch.is_some(),
            SpecClear::NetworkForward => self
                .network
                .as_ref()
                .is_some_and(|network| network.forward.is_some()),
            SpecClear::NetworkTap => self
                .network
                .as_ref()
                .is_some_and(|network| network.tap.is_some()),
            SpecClear::NetworkMac => self
                .network
                .as_ref()
                .is_some_and(|network| network.mac.is_some()),
            SpecClear::Mount => self.mounts.is_some(),
            SpecClear::CloudInitUserData => self
                .cloud_init
                .as_ref()
                .is_some_and(|cloud_init| cloud_init.user_data.is_some()),
            SpecClear::CloudInitUserDataInline => self
                .cloud_init
                .as_ref()
                .is_some_and(|cloud_init| cloud_init.user_data_inline.is_some()),
            SpecClear::CloudInitSshAuthorizedKeys => self
                .cloud_init
                .as_ref()
                .is_some_and(|cloud_init| cloud_init.ssh_authorized_keys.is_some()),
            SpecClear::CloudInitPassword => self
                .cloud_init
                .as_ref()
                .is_some_and(|cloud_init| cloud_init.password.is_some()),
            SpecClear::CloudInitNetworkConfig => self
                .cloud_init
                .as_ref()
                .is_some_and(|cloud_init| cloud_init.network_config.is_some()),
            SpecClear::CloudInitSshKeys => self
                .cloud_init
                .as_ref()
                .is_some_and(|cloud_init| cloud_init.ssh_keys.is_some()),
            SpecClear::VmmBinary => self.vmm.as_ref().is_some_and(|vmm| vmm.binary.is_some()),
            SpecClear::VmmExtraArgs => self
                .vmm
                .as_ref()
                .is_some_and(|vmm| vmm.extra_args.is_some()),
            SpecClear::VmmConfigOverlay => self
                .vmm
                .as_ref()
                .is_some_and(|vmm| vmm.config_overlay.is_some()),
            SpecClear::CpusMax => self.cpus_max.is_some(),
            SpecClear::MemoryMax => self.memory_max.is_some(),
        }
    }

    fn apply_clears(&self, spec: &mut MachineSpec) {
        for clear in &self.clear {
            match clear {
                SpecClear::Arch => spec.arch = None,
                SpecClear::NetworkForward => spec.network.forward.clear(),
                SpecClear::NetworkTap => spec.network.tap = None,
                SpecClear::NetworkMac => spec.network.mac = None,
                SpecClear::Mount => spec.mounts.clear(),
                SpecClear::CloudInitUserData => spec.cloud_init.user_data = None,
                SpecClear::CloudInitUserDataInline => spec.cloud_init.user_data_inline = None,
                SpecClear::CloudInitNetworkConfig => spec.cloud_init.network_config = None,
                SpecClear::CloudInitSshKeys => spec.cloud_init.ssh_keys.clear(),
                SpecClear::CloudInitSshAuthorizedKeys => {
                    spec.cloud_init.ssh_authorized_keys.clear();
                }
                SpecClear::CloudInitPassword => spec.cloud_init.password = None,
                SpecClear::VmmBinary => spec.vmm.binary = None,
                SpecClear::VmmExtraArgs => spec.vmm.extra_args.clear(),
                SpecClear::VmmConfigOverlay => spec.vmm.config_overlay = None,
                SpecClear::CpusMax => spec.cpus_max = None,
                SpecClear::MemoryMax => spec.memory_max = None,
            }
        }
    }

    /// Anchors relative path-valued fields in one patch against its source directory.
    ///
    /// [`MachineSpec::load`] calls this on a clone of an action patch before
    /// layering. Adapters pass the unresolved patch and its base directory to
    /// the loader instead of mutating path provenance themselves. Image
    /// references retain their source base until catalog, URL, or local-file
    /// classification during validation.
    pub fn resolve_paths(&mut self, paths: &Paths, base_dir: &Path) -> Result<(), FirestoneError> {
        validation::resolve_patch_paths(self, paths, base_dir)
    }
}

impl From<&MachineSpec> for MachineSpecPatch {
    fn from(spec: &MachineSpec) -> Self {
        let mut clear = Vec::new();
        if spec.arch.is_none() {
            clear.push(SpecClear::Arch);
        }
        if spec.network.tap.is_none() {
            clear.push(SpecClear::NetworkTap);
        }
        if spec.network.mac.is_none() {
            clear.push(SpecClear::NetworkMac);
        }
        if spec.cloud_init.user_data.is_none() {
            clear.push(SpecClear::CloudInitUserData);
        }
        if spec.cloud_init.user_data_inline.is_none() {
            clear.push(SpecClear::CloudInitUserDataInline);
        }
        if spec.cloud_init.network_config.is_none() {
            clear.push(SpecClear::CloudInitNetworkConfig);
        }
        if spec.cloud_init.password.is_none() {
            clear.push(SpecClear::CloudInitPassword);
        }
        if spec.vmm.binary.is_none() {
            clear.push(SpecClear::VmmBinary);
        }
        if spec.vmm.config_overlay.is_none() {
            clear.push(SpecClear::VmmConfigOverlay);
        }
        if spec.cpus_max.is_none() {
            clear.push(SpecClear::CpusMax);
        }
        if spec.memory_max.is_none() {
            clear.push(SpecClear::MemoryMax);
        }

        Self {
            clear,
            image: Some(spec.image.clone()),
            arch: spec.arch,
            cpus: Some(spec.cpus),
            cpus_max: spec.cpus_max,
            memory: Some(spec.memory),
            memory_max: spec.memory_max,
            disk: Some(spec.disk),
            user: Some(spec.user.clone()),
            network: Some(NetworkSpecPatch {
                mode: Some(spec.network.mode),
                forward: Some(spec.network.forward.clone()),
                tap: spec.network.tap.clone(),
                mac: spec.network.mac,
            }),
            mounts: Some(spec.mounts.clone()),
            cloud_init: Some(CloudInitSpecPatch {
                user_data: spec.cloud_init.user_data.clone(),
                user_data_inline: spec.cloud_init.user_data_inline.clone(),
                network_config: spec.cloud_init.network_config.clone(),
                ssh_keys: Some(spec.cloud_init.ssh_keys.clone()),
                ssh_authorized_keys: Some(spec.cloud_init.ssh_authorized_keys.clone()),
                password: spec.cloud_init.password.clone(),
                ssh_pwauth: Some(spec.cloud_init.ssh_pwauth),
                provisioning: Some(spec.cloud_init.provisioning),
            }),
            vmm: Some(VmmSpecPatch {
                binary: spec.vmm.binary.clone(),
                firmware: Some(spec.vmm.firmware.clone()),
                extra_args: Some(spec.vmm.extra_args.clone()),
                config_overlay: spec.vmm.config_overlay.clone(),
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum VectorMerge {
    Append,
    Replace,
}

fn merge_vector<T: Clone>(target: &mut Vec<T>, values: &[T], merge: VectorMerge) {
    if matches!(merge, VectorMerge::Replace) {
        target.clear();
    }
    target.extend_from_slice(values);
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
    fn apply_to(&self, spec: &mut NetworkSpec, vector_merge: VectorMerge) {
        if let Some(mode) = self.mode {
            spec.mode = mode;
        }
        if let Some(forward) = &self.forward {
            merge_vector(&mut spec.forward, forward, vector_merge);
        }
        if let Some(tap) = &self.tap {
            spec.tap = Some(tap.clone());
        }
        if let Some(mac) = self.mac {
            spec.mac = Some(mac);
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudInitSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data_inline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_config: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_keys: Option<Vec<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_authorized_keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_pwauth: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisioning: Option<bool>,
}

/// Redacts the same leaves [`CloudInitSpec`]'s implementation redacts.
impl std::fmt::Debug for CloudInitSpecPatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudInitSpecPatch")
            .field("user_data", &self.user_data)
            .field(
                "user_data_inline",
                &redacted(self.user_data_inline.as_ref()),
            )
            .field("network_config", &self.network_config)
            .field("ssh_keys", &self.ssh_keys)
            .field("ssh_authorized_keys", &self.ssh_authorized_keys)
            .field("password", &redacted(self.password.as_ref()))
            .field("ssh_pwauth", &self.ssh_pwauth)
            .field("provisioning", &self.provisioning)
            .finish()
    }
}

impl CloudInitSpecPatch {
    fn apply_to(&self, spec: &mut CloudInitSpec, vector_merge: VectorMerge) {
        if let Some(user_data) = &self.user_data {
            spec.user_data = Some(user_data.clone());
        }
        if let Some(user_data_inline) = &self.user_data_inline {
            spec.user_data_inline = Some(user_data_inline.clone());
        }
        if let Some(network_config) = &self.network_config {
            spec.network_config = Some(network_config.clone());
        }
        if let Some(ssh_keys) = &self.ssh_keys {
            merge_vector(&mut spec.ssh_keys, ssh_keys, vector_merge);
        }
        if let Some(ssh_authorized_keys) = &self.ssh_authorized_keys {
            merge_vector(
                &mut spec.ssh_authorized_keys,
                ssh_authorized_keys,
                vector_merge,
            );
        }
        if let Some(password) = &self.password {
            spec.password = Some(password.clone());
        }
        if let Some(ssh_pwauth) = self.ssh_pwauth {
            spec.ssh_pwauth = ssh_pwauth;
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_overlay"
    )]
    #[schemars(with = "Option<std::collections::BTreeMap<String, serde_json::Value>>")]
    pub config_overlay: Option<serde_json::Value>,
}

impl VmmSpecPatch {
    fn apply_to(&self, spec: &mut VmmSpec, vector_merge: VectorMerge) {
        if let Some(binary) = &self.binary {
            spec.binary = Some(binary.clone());
        }
        if let Some(firmware) = &self.firmware {
            spec.firmware = firmware.clone();
        }
        if let Some(extra_args) = &self.extra_args {
            merge_vector(&mut spec.extra_args, extra_args, vector_merge);
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
        let config = deserialize_global_toml(input, "config.toml")?;
        config.images.validate()?;
        Ok(config)
    }

    /// Serializes global configuration without losing nested overlay nulls.
    pub fn to_toml(&self) -> Result<String, FirestoneError> {
        self.defaults.validate()?;
        let mut value = self.clone();
        let overlay = value
            .defaults
            .vmm
            .as_mut()
            .and_then(|vmm| vmm.config_overlay.take());
        serialize_toml_with_overlay(
            &value,
            overlay.as_ref(),
            &["defaults", "vmm", "config_overlay"],
            "config.toml",
        )
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
            timeout_first_boot: HumanDuration::DEFAULT_FIRST_BOOT_TIMEOUT,
            timeout: HumanDuration::DEFAULT_START_TIMEOUT,
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
            timeout: HumanDuration::DEFAULT_STOP_TIMEOUT,
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
    /// Registries reachable over plain HTTP, as `host` or `host:port` entries.
    pub insecure_registries: Vec<String>,
}

impl ImagesConfig {
    /// Rejects `insecure_registries` entries that carry a scheme or a path.
    pub fn validate(&self) -> Result<(), FirestoneError> {
        for entry in &self.insecure_registries {
            crate::oci::validate_registry_host(entry).map_err(|error| {
                FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!("images.insecure_registries: {}", error.message()),
                )
                .with_hint(
                    error
                        .hint()
                        .unwrap_or("write a bare 'host' or 'host:port' entry")
                        .to_owned(),
                )
            })?;
        }
        Ok(())
    }
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
    field("cpus_max", "cpus-max", None, PatchMerge::Replace, false),
    field("memory", "memory", None, PatchMerge::Replace, false),
    field("memory_max", "memory-max", None, PatchMerge::Replace, false),
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
        "cloud_init.user_data_inline",
        "user-data-inline",
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
        "cloud_init.ssh_authorized_keys",
        "ssh-authorized-key",
        None,
        PatchMerge::Append,
        false,
    ),
    field(
        "cloud_init.password",
        "password-file",
        None,
        PatchMerge::Replace,
        false,
    ),
    field(
        "cloud_init.ssh_pwauth",
        "ssh-pwauth",
        None,
        PatchMerge::Replace,
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

fn deserialize_patch_toml(
    input: &str,
    file: &str,
    overlay_path: &[&str],
) -> Result<MachineSpecPatch, FirestoneError> {
    let mut value = parse_toml(input, file)?;
    let overlay = take_toml_path(&mut value, overlay_path)
        .map(|value| decode_toml_overlay(value, file, overlay_path))
        .transpose()?;
    let input_without_overlay = encode_toml_value(&value, file)?;
    let mut patch: MachineSpecPatch = deserialize_toml(&input_without_overlay, file)?;
    if let Some(overlay) = overlay {
        patch
            .vmm
            .get_or_insert_with(VmmSpecPatch::default)
            .config_overlay = Some(overlay);
    }
    patch.validate()?;
    Ok(patch)
}

fn deserialize_global_toml(input: &str, file: &str) -> Result<GlobalConfig, FirestoneError> {
    const OVERLAY_PATH: &[&str] = &["defaults", "vmm", "config_overlay"];
    let mut value = parse_toml(input, file)?;
    let overlay = take_toml_path(&mut value, OVERLAY_PATH)
        .map(|value| decode_toml_overlay(value, file, OVERLAY_PATH))
        .transpose()?;
    let input_without_overlay = encode_toml_value(&value, file)?;
    let mut global: GlobalConfig = deserialize_toml(&input_without_overlay, file)?;
    if let Some(overlay) = overlay {
        global
            .defaults
            .vmm
            .get_or_insert_with(VmmSpecPatch::default)
            .config_overlay = Some(overlay);
    }
    global.defaults.validate()?;
    Ok(global)
}

fn serialize_toml_with_overlay<T: Serialize>(
    value: &T,
    overlay: Option<&serde_json::Value>,
    overlay_path: &[&str],
    file: &str,
) -> Result<String, FirestoneError> {
    let mut value = toml::Value::try_from(value)
        .map_err(|error| serialization_error(file, "cannot encode values as TOML", error))?;
    if let Some(overlay) = overlay {
        validate_overlay(overlay, &overlay_path.join("."))?;
        let text = serde_json::to_string(overlay).map_err(|error| {
            serialization_error(file, "cannot encode vmm.config_overlay as JSON", error)
        })?;
        insert_toml_path(&mut value, overlay_path, toml::Value::String(text), file)?;
    }
    encode_toml_value(&value, file)
}

fn parse_toml(input: &str, file: &str) -> Result<toml::Value, FirestoneError> {
    input.parse::<toml::Value>().map_err(|error| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot parse {file}: {error}"),
        )
        .with_hint(format!("fix the TOML syntax in {file}"))
        .with_source(error)
    })
}

fn encode_toml_value(value: &toml::Value, file: &str) -> Result<String, FirestoneError> {
    toml::to_string_pretty(value)
        .map_err(|error| serialization_error(file, "cannot encode TOML document", error))
}

fn serialization_error(
    file: &str,
    message: &str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FirestoneError {
    FirestoneError::new(ErrorKind::Generic, format!("{message} for {file}"))
        .with_hint("report this Firestone serialization error")
        .with_source(source)
}

fn take_toml_path(value: &mut toml::Value, path: &[&str]) -> Option<toml::Value> {
    let (leaf, parents) = path.split_last()?;
    let mut current = value;
    for parent in parents {
        current = current.as_table_mut()?.get_mut(*parent)?;
    }
    current.as_table_mut()?.remove(*leaf)
}

fn insert_toml_path(
    value: &mut toml::Value,
    path: &[&str],
    inserted: toml::Value,
    file: &str,
) -> Result<(), FirestoneError> {
    let Some((leaf, parents)) = path.split_last() else {
        return Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot encode an empty TOML key path for {file}"),
        )
        .with_hint("report this Firestone serialization error"));
    };
    let mut current = value;
    for parent in parents {
        let table = current.as_table_mut().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot encode '{}' as a TOML table in {file}", parent),
            )
            .with_hint("report this Firestone serialization error")
        })?;
        current = table
            .entry((*parent).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    let table = current.as_table_mut().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot encode '{}' as a TOML table in {file}", leaf),
        )
        .with_hint("report this Firestone serialization error")
    })?;
    table.insert((*leaf).to_owned(), inserted);
    Ok(())
}

fn decode_toml_overlay(
    value: toml::Value,
    file: &str,
    path: &[&str],
) -> Result<serde_json::Value, FirestoneError> {
    let key = path.join(".");
    let overlay = match value {
        toml::Value::String(text) => serde_json::from_str(&text).map_err(|error| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("invalid JSON text for '{key}' in {file}: {error}"),
            )
            .with_hint(format!("set '{key}' to a JSON object"))
            .with_source(error)
        })?,
        _ => {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("'{key}' in {file} must be canonical JSON text"),
            )
            .with_hint(format!("set '{key}' to JSON text such as '{{}}'")));
        }
    };
    validate_overlay(&overlay, &key)?;
    Ok(overlay)
}

fn validate_overlay(overlay: &serde_json::Value, key: &str) -> Result<(), FirestoneError> {
    if overlay.is_object() {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("{key} must be a JSON object"),
    )
    .with_hint(format!("set {key} to a JSON object such as {{}}")))
}

fn deserialize_optional_overlay<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let overlay = Option::<serde_json::Value>::deserialize(deserializer)?;
    if let Some(overlay) = &overlay {
        validate_overlay(overlay, "vmm.config_overlay").map_err(serde::de::Error::custom)?;
    }
    Ok(overlay)
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
                "clear",
                "image",
                "arch",
                "cpus",
                "cpus_max",
                "memory",
                "memory_max",
                "disk",
                "user",
                "network",
                "mount",
                "cloud_init",
                "vmm",
            ],
            Self::Network => &["mode", "forward", "tap", "mac"],
            Self::Mount => &["host", "guest", "readonly", "tag"],
            Self::CloudInit => &[
                "user_data",
                "user_data_inline",
                "network_config",
                "ssh_keys",
                "ssh_authorized_keys",
                "password",
                "ssh_pwauth",
                "provisioning",
            ],
            Self::Vmm => &["binary", "firmware", "extra_args", "config_overlay"],
            Self::Global => &["defaults", "start", "stop", "ui", "images"],
            Self::Start => &["timeout_first_boot", "timeout"],
            Self::Stop => &["timeout"],
            Self::Ui => &["color"],
            Self::Images => &["catalog", "insecure_registries"],
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
        PatchMerge, SPEC_FIELD_METADATA, SpecClear, VmmSpec, VmmSpecPatch,
    };
    use crate::{ErrorKind, Paths};

    #[test]
    fn machine_spec_defaults_match_documented_values() {
        let spec = MachineSpec::default();
        assert_eq!(spec.image, ImageRef::from("ubuntu:24.04"));
        assert_eq!(spec.arch, None);
        assert_eq!(spec.cpus, 2);
        assert_eq!(spec.memory, gib(2));
        assert_eq!(spec.disk, gib(20));
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
        assert_eq!(spec.memory, gib(2));
        assert_eq!(spec.network, NetworkSpec::default());
        Ok(())
    }

    #[test]
    fn machine_spec_toml_round_trip_preserves_values() -> Result<(), Box<dyn std::error::Error>> {
        let spec = populated_spec();
        let encoded = spec.to_toml()?;
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
    fn layering_machine_vectors_replace_and_action_vectors_append()
    -> Result<(), crate::FirestoneError> {
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

        let spec = MachineSpec::from_layers(&global, &machine, &cli)?;
        assert_eq!(spec.cpus, 6);
        assert_eq!(
            spec.network.forward,
            [
                "8500:85".parse().expect("valid forward"),
                "9000:90".parse().expect("valid forward")
            ]
        );
        assert_eq!(
            spec.mounts
                .iter()
                .map(|mount| mount.host.as_path())
                .collect::<Vec<_>>(),
            [std::path::Path::new("machine"), std::path::Path::new("cli")]
        );
        assert_eq!(
            spec.cloud_init.ssh_keys,
            [PathBuf::from("machine.pub"), PathBuf::from("cli.pub")]
        );
        assert_eq!(spec.vmm.extra_args, ["--machine", "--cli"]);
        Ok(())
    }

    #[test]
    fn effective_spec_persist_reload_does_not_duplicate_vectors()
    -> Result<(), Box<dyn std::error::Error>> {
        let global = vector_patch("global", "8000:80", "global.pub", "--global");
        let machine = vector_patch("machine", "8500:85", "machine.pub", "--machine");
        let action = vector_patch("action", "9000:90", "action.pub", "--action");
        let effective = MachineSpec::from_layers(&global, &machine, &action)?;

        let persisted = effective.to_toml()?;
        let persisted_machine = MachineSpecPatch::from_toml(&persisted)?;
        let reloaded =
            MachineSpec::from_layers(&global, &persisted_machine, &MachineSpecPatch::default())?;

        assert_eq!(reloaded, effective);
        assert_eq!(reloaded.network.forward.len(), 2);
        assert_eq!(reloaded.mounts.len(), 2);
        assert_eq!(reloaded.cloud_init.ssh_keys.len(), 2);
        assert_eq!(reloaded.vmm.extra_args.len(), 2);
        Ok(())
    }

    #[test]
    fn machine_load_composes_layers_paths_validation_and_warnings()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let machine_dir = temporary.path().join("machine");
        let patch_dir = temporary.path().join("action");
        std::fs::create_dir(&machine_dir)?;
        std::fs::create_dir(&patch_dir)?;
        std::fs::create_dir(machine_dir.join("machine-share"))?;
        std::fs::create_dir(patch_dir.join("action-share"))?;
        let global = GlobalConfig {
            defaults: MachineSpecPatch {
                user: Some("global".to_owned()),
                ..MachineSpecPatch::default()
            },
            ..GlobalConfig::default()
        };
        let patch = MachineSpecPatch {
            user: Some("cli".to_owned()),
            mounts: Some(vec![mount("action-share", "/action")]),
            ..MachineSpecPatch::default()
        };
        let input = r#"
cpus = 1
user = "machine"

[[mount]]
host = "machine-share"
guest = "/machine"
"#;
        let host = super::RealValidationHost::new();
        let catalog = crate::Catalog::built_in()?;
        let paths = Paths::from_process()?;
        let context = super::ValidationContext::new(&host, &paths, &machine_dir, &catalog);

        let loaded = MachineSpec::load(input, &global, &patch, &patch_dir, &context)?;

        assert_eq!(loaded.spec.cpus, 1);
        assert_eq!(loaded.spec.user, "cli");
        assert_eq!(
            loaded.spec.mounts[0].host,
            machine_dir.join("machine-share")
        );
        assert_eq!(loaded.spec.mounts[1].host, patch_dir.join("action-share"));
        assert!(loaded.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn machine_load_missing_extensionless_action_image_uses_action_base()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let machine_dir = temporary.path().join("machine");
        let action_dir = temporary.path().join("action");
        std::fs::create_dir_all(machine_dir.join("images"))?;
        std::fs::create_dir(&action_dir)?;
        std::fs::write(machine_dir.join("images/base"), b"machine lookalike")?;
        let patch = MachineSpecPatch {
            image: Some(ImageRef::from("images/base")),
            ..MachineSpecPatch::default()
        };
        let global = GlobalConfig::default();
        let host = super::RealValidationHost::new();
        let catalog = crate::Catalog::built_in()?;
        let paths = Paths::from_process()?;
        let context = super::ValidationContext::new(&host, &paths, &machine_dir, &catalog);

        let error = MachineSpec::load("", &global, &patch, &action_dir, &context)
            .expect_err("missing action-relative image");

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error
                .message()
                .contains(&action_dir.join("images/base").display().to_string())
        );
        assert!(
            !error
                .message()
                .contains(&machine_dir.join("images/base").display().to_string())
        );
        Ok(())
    }

    #[test]
    fn global_config_missing_keys_uses_documented_defaults() -> Result<(), crate::FirestoneError> {
        let config = GlobalConfig::from_toml("")?;
        assert_eq!(config, GlobalConfig::default());
        assert_eq!(config.start.timeout_first_boot.get().as_secs(), 300);
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
    fn clear_unknown_path_rejected_by_toml_and_json() {
        let toml_error = MachineSpecPatch::from_toml("clear = [\"network.unknown\"]")
            .expect_err("unknown clear");
        let json_error = serde_json::from_value::<MachineSpecPatch>(json!({
            "clear": ["network.unknown"]
        }))
        .expect_err("unknown clear");

        assert_eq!(toml_error.kind(), ErrorKind::InvalidSpec);
        assert!(toml_error.message().contains("clear[0]"));
        assert!(json_error.to_string().contains("network.unknown"));
    }

    #[test]
    fn clear_set_conflict_rejected_by_all_entry_points() {
        let toml_error = MachineSpecPatch::from_toml("clear = [\"arch\"]\narch = \"x86_64\"")
            .expect_err("conflicting TOML patch");
        let json_error = serde_json::from_value::<MachineSpecPatch>(json!({
            "clear": ["arch"],
            "arch": "x86_64"
        }))
        .expect_err("conflicting JSON patch");
        let programmatic = MachineSpecPatch {
            clear: vec![SpecClear::Arch],
            arch: Some(Arch::X86_64),
            ..MachineSpecPatch::default()
        };
        let apply_error = programmatic
            .apply_to(&mut MachineSpec::default())
            .expect_err("conflicting programmatic patch");

        assert_eq!(toml_error.kind(), ErrorKind::InvalidSpec);
        assert!(toml_error.message().contains("sets and clears 'arch'"));
        assert!(json_error.to_string().contains("sets and clears 'arch'"));
        assert_eq!(apply_error.kind(), ErrorKind::InvalidSpec);
    }

    #[test]
    fn clear_all_supported_paths_removes_lower_layer_values() -> Result<(), crate::FirestoneError> {
        let global = populated_patch();
        let machine = MachineSpecPatch {
            clear: SpecClear::ALL.to_vec(),
            ..MachineSpecPatch::default()
        };

        let spec = MachineSpec::from_layers(&global, &machine, &MachineSpecPatch::default())?;

        assert_eq!(spec.arch, None);
        assert!(spec.network.forward.is_empty());
        assert_eq!(spec.network.tap, None);
        assert_eq!(spec.network.mac, None);
        assert!(spec.mounts.is_empty());
        assert_eq!(spec.cloud_init.user_data, None);
        assert_eq!(spec.cloud_init.network_config, None);
        assert!(spec.cloud_init.ssh_keys.is_empty());
        assert_eq!(spec.vmm.binary, None);
        assert!(spec.vmm.extra_args.is_empty());
        assert_eq!(spec.vmm.config_overlay, None);
        Ok(())
    }

    #[test]
    fn full_spec_persistence_clears_optional_global_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected = MachineSpec::default();
        let persisted = expected.to_toml()?;
        let machine = MachineSpecPatch::from_toml(&persisted)?;
        let reloaded =
            MachineSpec::from_layers(&populated_patch(), &machine, &MachineSpecPatch::default())?;

        assert_eq!(reloaded, expected);
        assert!(persisted.contains("clear = ["));
        assert!(machine.clear.contains(&SpecClear::Arch));
        assert!(machine.clear.contains(&SpecClear::NetworkTap));
        assert!(machine.clear.contains(&SpecClear::VmmConfigOverlay));
        Ok(())
    }

    #[test]
    fn config_overlay_nested_null_round_trips_json_and_toml()
    -> Result<(), Box<dyn std::error::Error>> {
        let patch = MachineSpecPatch {
            vmm: Some(VmmSpecPatch {
                config_overlay: Some(json!({
                    "memory": {"shared": null},
                    "serial": null
                })),
                ..VmmSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };

        let json_value = serde_json::to_value(&patch)?;
        assert!(json_value["vmm"]["config_overlay"].is_object());
        assert!(json_value["vmm"]["config_overlay"]["serial"].is_null());
        assert_eq!(
            serde_json::from_value::<MachineSpecPatch>(json_value)?,
            patch
        );

        let toml = patch.to_toml()?;
        let toml_value = toml.parse::<toml::Value>()?;
        assert!(toml_value["vmm"]["config_overlay"].is_str());
        let decoded = MachineSpecPatch::from_toml(&toml)?;
        assert_eq!(decoded, patch);
        assert_eq!(decoded.to_toml()?, toml);
        Ok(())
    }

    #[test]
    fn global_config_overlay_nested_null_round_trips_toml() -> Result<(), Box<dyn std::error::Error>>
    {
        let config = GlobalConfig {
            defaults: MachineSpecPatch {
                vmm: Some(VmmSpecPatch {
                    config_overlay: Some(json!({"cpu": {"max_phys_bits": null}})),
                    ..VmmSpecPatch::default()
                }),
                ..MachineSpecPatch::default()
            },
            ..GlobalConfig::default()
        };

        let toml = config.to_toml()?;
        assert_eq!(GlobalConfig::from_toml(&toml)?, config);
        Ok(())
    }

    #[test]
    fn config_overlay_toml_object_syntax_is_rejected() {
        let error = MachineSpecPatch::from_toml(
            r#"
[vmm]
config_overlay = { memory = { shared = true } }
"#,
        )
        .expect_err("noncanonical TOML overlay");

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("vmm.config_overlay"));
        assert!(error.message().contains("canonical JSON text"));
    }

    #[test]
    fn global_config_overlay_error_reports_full_key_path() {
        let error = GlobalConfig::from_toml(
            r#"
[defaults.vmm]
config_overlay = "[]"
"#,
        )
        .expect_err("non-object global overlay");

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("defaults.vmm.config_overlay"));
    }

    #[test]
    fn config_overlay_non_object_rejected_by_full_and_patch_json() {
        let full = serde_json::to_value(MachineSpec::default()).expect("serialize full spec");
        let mut full = full.as_object().cloned().expect("full spec object");
        full.get_mut("vmm")
            .and_then(Value::as_object_mut)
            .expect("VMM object")
            .insert("config_overlay".to_owned(), json!(["not", "an", "object"]));

        assert!(serde_json::from_value::<MachineSpec>(Value::Object(full)).is_err());
        assert!(
            serde_json::from_value::<MachineSpecPatch>(json!({
                "vmm": {"config_overlay": "{}"}
            }))
            .is_err()
        );
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
    fn field_metadata_matches_machine_spec_schema_leaves() -> Result<(), serde_json::Error> {
        let schema = serde_json::to_value(schemars::schema_for!(MachineSpec))?;
        let leaf_paths = schema_leaf_paths(&schema);
        let vector_paths = array_leaf_paths(&serde_json::to_value(populated_spec())?);
        let mut expected = leaf_paths
            .iter()
            .map(|key| expected_metadata_row(key, vector_paths.contains(key)))
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));

        let mut actual = SPEC_FIELD_METADATA
            .iter()
            .map(|field| {
                (
                    field.key.to_owned(),
                    field.long.to_owned(),
                    field.short,
                    field.merge,
                    field.composite,
                )
            })
            .collect::<Vec<_>>();
        actual.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(actual, expected);
        assert_eq!(
            SPEC_FIELD_METADATA
                .iter()
                .map(|field| field.key)
                .collect::<BTreeSet<_>>()
                .len(),
            SPEC_FIELD_METADATA.len()
        );
        assert_eq!(
            SPEC_FIELD_METADATA
                .iter()
                .map(|field| field.long)
                .collect::<BTreeSet<_>>()
                .len(),
            SPEC_FIELD_METADATA.len()
        );
        let short_count = SPEC_FIELD_METADATA
            .iter()
            .filter_map(|field| field.short)
            .count();
        assert_eq!(
            SPEC_FIELD_METADATA
                .iter()
                .filter_map(|field| field.short)
                .collect::<BTreeSet<_>>()
                .len(),
            short_count
        );
        Ok(())
    }

    #[test]
    fn clear_paths_match_optional_and_append_spec_leaves() -> Result<(), serde_json::Error> {
        let default = serde_json::to_value(MachineSpec::default())?;
        let mut expected = null_leaf_paths(&default);
        expected.extend(
            SPEC_FIELD_METADATA
                .iter()
                .filter(|field| field.merge == PatchMerge::Append)
                .map(|field| field.key.to_owned()),
        );
        let actual = SpecClear::ALL
            .iter()
            .map(|clear| clear.as_str().to_owned())
            .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn clear_paths_round_trip_json_toml_and_schema() -> Result<(), Box<dyn std::error::Error>> {
        let patch = MachineSpecPatch {
            clear: SpecClear::ALL.to_vec(),
            ..MachineSpecPatch::default()
        };
        let json = serde_json::to_vec(&patch)?;
        let toml = patch.to_toml()?;
        assert_eq!(serde_json::from_slice::<MachineSpecPatch>(&json)?, patch);
        assert_eq!(MachineSpecPatch::from_toml(&toml)?, patch);

        let schema = serde_json::to_value(schemars::schema_for!(SpecClear))?;
        let schema_values = schema["enum"]
            .as_array()
            .expect("clear enum schema")
            .iter()
            .map(|value| value.as_str().expect("clear path string").to_owned())
            .collect::<BTreeSet<_>>();
        let actual = SpecClear::ALL
            .iter()
            .map(|clear| clear.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(schema_values, actual);
        for clear in SpecClear::ALL {
            assert_eq!(clear.as_str().parse::<SpecClear>(), Ok(*clear));
            assert_eq!(clear.to_string(), clear.as_str());
        }
        Ok(())
    }

    fn populated_spec() -> MachineSpec {
        MachineSpec {
            image: ImageRef::from("ubuntu:24.04"),
            arch: Some(Arch::X86_64),
            cpus: 4,
            cpus_max: Some(8),
            memory: gib(8),
            memory_max: Some(gib(16)),
            disk: gib(40),
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
                // Serialization fixture only: §7.2 rejects both user parts
                // together, while the drift checks need every leaf present.
                user_data_inline: Some("#cloud-config\n".to_owned()),
                network_config: Some(PathBuf::from("/tmp/network-config")),
                ssh_keys: vec![PathBuf::from("/tmp/id.pub")],
                ssh_authorized_keys: vec!["ssh-ed25519 AAAA inline@test".to_owned()],
                password: Some("hunter2".to_owned()),
                ssh_pwauth: true,
                provisioning: false,
            },
            vmm: VmmSpec {
                binary: Some(PathBuf::from("/opt/cloud-hypervisor")),
                firmware: Firmware::path("/opt/CLOUDHV.fd").expect("non-empty firmware path"),
                extra_args: vec!["--verbose".to_owned()],
                config_overlay: Some(json!({
                    "memory": {"shared": true},
                    "serial": null
                })),
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

    fn vector_patch(
        mount_host: &str,
        forward: &str,
        ssh_key: &str,
        vmm_arg: &str,
    ) -> MachineSpecPatch {
        MachineSpecPatch {
            network: Some(NetworkSpecPatch {
                forward: Some(vec![forward.parse().expect("valid forward")]),
                ..NetworkSpecPatch::default()
            }),
            mounts: Some(vec![mount(mount_host, &format!("/{mount_host}"))]),
            cloud_init: Some(CloudInitSpecPatch {
                ssh_keys: Some(vec![PathBuf::from(ssh_key)]),
                ..CloudInitSpecPatch::default()
            }),
            vmm: Some(VmmSpecPatch {
                extra_args: Some(vec![vmm_arg.to_owned()]),
                ..VmmSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        }
    }

    fn gib(value: u64) -> ByteSize {
        ByteSize::from_gib(value).expect("test size fits in bytes")
    }

    fn populated_patch() -> MachineSpecPatch {
        let spec = populated_spec();
        MachineSpecPatch {
            clear: Vec::new(),
            image: Some(spec.image),
            arch: spec.arch,
            cpus: Some(spec.cpus),
            cpus_max: spec.cpus_max,
            memory: Some(spec.memory),
            memory_max: spec.memory_max,
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
                user_data_inline: spec.cloud_init.user_data_inline,
                network_config: spec.cloud_init.network_config,
                ssh_keys: Some(spec.cloud_init.ssh_keys),
                ssh_authorized_keys: Some(spec.cloud_init.ssh_authorized_keys),
                password: spec.cloud_init.password,
                ssh_pwauth: Some(spec.cloud_init.ssh_pwauth),
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

    fn schema_leaf_paths(schema: &Value) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        collect_schema_leaf_paths(schema, schema, "", &mut paths);
        paths
    }

    fn expected_metadata_row(
        key: &str,
        append: bool,
    ) -> (String, String, Option<char>, PatchMerge, bool) {
        let long = match key {
            "network.mode" => "net".to_owned(),
            "network.forward" => "forward".to_owned(),
            "network.tap" => "tap".to_owned(),
            "mount" => "mount".to_owned(),
            "cloud_init.user_data" => "user-data".to_owned(),
            "cloud_init.user_data_inline" => "user-data-inline".to_owned(),
            "cloud_init.ssh_keys" => "ssh-key".to_owned(),
            "cloud_init.ssh_authorized_keys" => "ssh-authorized-key".to_owned(),
            "cloud_init.password" => "password-file".to_owned(),
            "cloud_init.ssh_pwauth" => "ssh-pwauth".to_owned(),
            "cloud_init.provisioning" => "no-provisioning".to_owned(),
            "vmm.extra_args" => "vmm-arg".to_owned(),
            "vmm.config_overlay" => "vmm-config".to_owned(),
            _ => key
                .chars()
                .map(|character| match character {
                    '.' | '_' => '-',
                    other => other,
                })
                .collect(),
        };
        (
            key.to_owned(),
            long,
            (key == "network.forward").then_some('p'),
            if append {
                PatchMerge::Append
            } else {
                PatchMerge::Replace
            },
            key == "mount",
        )
    }

    #[test]
    fn global_config_insecure_registries_valid_entries_expected_accepted() {
        let config = GlobalConfig::from_toml(
            "[images]\ninsecure_registries = [\"localhost:5000\", \"registry.internal\"]\n",
        )
        .expect("valid insecure registries");

        assert_eq!(
            config.images.insecure_registries,
            vec!["localhost:5000".to_owned(), "registry.internal".to_owned()]
        );
        let round_trip =
            GlobalConfig::from_toml(&config.to_toml().expect("serialize")).expect("reparse");
        assert_eq!(round_trip, config);
    }

    #[test]
    fn global_config_insecure_registries_scheme_or_path_expected_invalid_spec() {
        for entry in [
            "https://registry.example.com",
            "registry.example.com/v2",
            "registry.example.com:port",
            "",
        ] {
            let toml = format!(
                "[images]\ninsecure_registries = [{}]\n",
                serde_json::to_string(entry).expect("encode entry")
            );
            let error = GlobalConfig::from_toml(&toml)
                .expect_err(&format!("expected {entry:?} to be rejected"));
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert!(
                error.message().contains("images.insecure_registries"),
                "message for {entry:?}: {}",
                error.message()
            );
            assert!(error.hint().is_some(), "hint for {entry:?}");
        }
    }

    #[test]
    fn global_config_unknown_images_key_expected_suggestion() {
        let error = GlobalConfig::from_toml("[images]\ninsecure_registry = []\n")
            .expect_err("unknown images key");

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("images.insecure_registry"));
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("images.insecure_registries"))
        );
    }

    fn array_leaf_paths(value: &Value) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        collect_array_leaf_paths(value, "", &mut paths);
        paths
    }

    fn collect_array_leaf_paths(value: &Value, prefix: &str, paths: &mut BTreeSet<String>) {
        if prefix == "vmm.config_overlay" {
            return;
        }
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_array_leaf_paths(value, &path, paths);
                }
            }
            Value::Array(_) => {
                paths.insert(prefix.to_owned());
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    fn collect_schema_leaf_paths(
        root: &Value,
        schema: &Value,
        prefix: &str,
        paths: &mut BTreeSet<String>,
    ) {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            if let Some(resolved) = reference
                .strip_prefix('#')
                .and_then(|pointer| root.pointer(pointer))
            {
                collect_schema_leaf_paths(root, resolved, prefix, paths);
                return;
            }
        }

        for keyword in ["anyOf", "oneOf", "allOf"] {
            if let Some(variants) = schema.get(keyword).and_then(Value::as_array) {
                for variant in variants {
                    if variant.get("type").and_then(Value::as_str) != Some("null") {
                        collect_schema_leaf_paths(root, variant, prefix, paths);
                    }
                }
                return;
            }
        }

        if schema.get("type").and_then(Value::as_str) == Some("array") {
            if !prefix.is_empty() {
                paths.insert(prefix.to_owned());
            }
            return;
        }

        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            if properties.is_empty() && !prefix.is_empty() {
                paths.insert(prefix.to_owned());
                return;
            }
            for (key, property) in properties {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_schema_leaf_paths(root, property, &path, paths);
            }
            return;
        }

        if !prefix.is_empty() {
            paths.insert(prefix.to_owned());
        }
    }

    fn null_leaf_paths(value: &Value) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        collect_null_leaf_paths(value, "", &mut paths);
        paths
    }

    fn collect_null_leaf_paths(value: &Value, prefix: &str, paths: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_null_leaf_paths(value, &path, paths);
                }
            }
            Value::Array(_) | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
            Value::Null => {
                paths.insert(prefix.to_owned());
            }
        }
    }
}
