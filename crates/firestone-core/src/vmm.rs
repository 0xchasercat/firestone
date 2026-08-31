use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::{
    Arch, CatalogFirmware, DependencyArtifact, DependencyManifest, ErrorKind, FirestoneError,
    Firmware, MacAddr, MachineSpec, MachineState, NetworkPlan, Paths, VirtiofsPlan, atomic,
    embedded_helpers::verified_pinned_artifact,
    virtiofs::{VIRTIOFS_NUM_QUEUES, VIRTIOFS_QUEUE_SIZE},
};

/// Inputs already resolved by image and state preparation before VMM creation.
#[derive(Debug, Clone, Copy)]
pub struct VmConfigInput<'a> {
    pub name: &'a str,
    pub spec: &'a MachineSpec,
    pub state: &'a MachineState,
    pub network: &'a NetworkPlan,
    pub filesystems: &'a [VirtiofsPlan],
    pub architecture: Arch,
    pub catalog_firmware: Option<CatalogFirmware>,
}

/// Recursively sorted JSON and the exact compact bytes sent to Cloud Hypervisor.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalVmConfig {
    value: Value,
    bytes: Vec<u8>,
}

impl CanonicalVmConfig {
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.value
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Serialize for CanonicalVmConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.value.serialize(serializer)
    }
}

#[derive(Debug, Serialize)]
struct VmConfig {
    cpus: CpusConfig,
    memory: MemoryConfig,
    payload: PayloadConfig,
    disks: Vec<DiskConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    net: Option<Vec<NetConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fs: Option<Vec<FsConfig>>,
    vsock: VsockConfig,
    serial: SerialConfig,
    console: ConsoleConfig,
    rng: RngConfig,
}

#[derive(Debug, Serialize)]
struct CpusConfig {
    boot_vcpus: u32,
    max_vcpus: u32,
}

#[derive(Debug, Serialize)]
struct MemoryConfig {
    size: u64,
    shared: bool,
}

#[derive(Debug, Serialize)]
struct PayloadConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct DiskConfig {
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    readonly: Option<bool>,
    image_type: ImageType,
    #[serde(skip_serializing_if = "Option::is_none")]
    backing_files: Option<bool>,
}

#[derive(Debug, Serialize)]
enum ImageType {
    Qcow2,
    Raw,
}

#[derive(Debug, Serialize)]
pub struct NetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    tap: Option<String>,
    mac: MacAddr,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<IpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mask: Option<IpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vhost_user: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vhost_socket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vhost_mode: Option<VhostMode>,
}
impl NetConfig {
    /// Maps a validated M3 network plan to the exact Cloud Hypervisor v53
    /// NetConfig fields. The vhost client role depends on verify item 14;
    /// TAP ip and mask absence depends on verify item 8.
    pub fn from_plan(plan: &NetworkPlan) -> Result<Option<Vec<Self>>, FirestoneError> {
        Ok(match plan {
            NetworkPlan::None => None,
            NetworkPlan::Passt(plan) => Some(vec![Self {
                tap: None,
                mac: plan.mac(),
                ip: None,
                mask: None,
                vhost_user: Some(true),
                vhost_socket: Some(utf8_path(plan.socket().path(), "passt vhost-user socket")?),
                vhost_mode: Some(VhostMode::Client),
            }]),
            NetworkPlan::Tap(plan) => Some(vec![Self {
                tap: Some(plan.name().to_owned()),
                mac: plan.mac(),
                ip: plan.ip(),
                mask: plan.mask(),
                vhost_user: None,
                vhost_socket: None,
                vhost_mode: None,
            }]),
        })
    }
}

/// Exact Cloud Hypervisor v53 `FsConfig` shape. v53 has no DAX or read-only field.
#[derive(Debug, Serialize)]
pub struct FsConfig {
    tag: String,
    socket: PathBuf,
    num_queues: usize,
    queue_size: u16,
}

impl FsConfig {
    /// Maps one validated virtiofsd plan to the exact v53 filesystem fields.
    #[must_use]
    pub fn from_plan(plan: &VirtiofsPlan) -> Self {
        Self {
            tag: plan.tag().to_owned(),
            socket: plan.socket().to_path_buf(),
            num_queues: VIRTIOFS_NUM_QUEUES,
            queue_size: VIRTIOFS_QUEUE_SIZE,
        }
    }
}

#[derive(Debug, Serialize)]
enum VhostMode {
    Client,
}

#[derive(Debug, Serialize)]
struct VsockConfig {
    cid: u32,
    socket: PathBuf,
}

#[derive(Debug, Serialize)]
struct SerialConfig {
    mode: ConsoleMode,
    file: PathBuf,
}

#[derive(Debug, Serialize)]
struct ConsoleConfig {
    mode: ConsoleMode,
}

#[derive(Debug, Serialize)]
enum ConsoleMode {
    File,
    Pty,
}

#[derive(Debug, Serialize)]
struct RngConfig {
    src: PathBuf,
}

enum EffectiveFirmware<'a> {
    Rhf,
    Edk2,
    Custom(&'a Path),
}

/// Builds Cloud Hypervisor v53 JSON, applies the RFC 7396 overlay, validates
/// Firestone-owned fields, and recursively sorts every object.
///
/// The field names and enum spellings are pinned by SPEC verify item 2.
pub fn canonical_vm_config(
    paths: &Paths,
    manifest: &DependencyManifest,
    input: VmConfigInput<'_>,
) -> Result<CanonicalVmConfig, FirestoneError> {
    let payload = resolve_payload(paths, manifest, input)?;
    let typed = base_vm_config(paths, input, payload)?;
    let base = serde_json::to_value(typed).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "cannot serialize Cloud Hypervisor VmConfig for machine `{}`",
                input.name
            ),
        )
        .with_hint("use UTF-8 paths in the machine configuration")
        .with_source(source)
    })?;

    let mut merged = base.clone();
    if let Some(overlay) = &input.spec.vmm.config_overlay {
        if !overlay.is_object() {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                "vmm.config_overlay must be a JSON object",
            )
            .with_hint("use an RFC 7396 object merge patch"));
        }
        apply_merge_patch(&mut merged, overlay);
    }
    validate_required_invariants(&base, &merged, input)?;
    sort_json(&mut merged);

    let bytes = serde_json::to_vec(&merged).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!(
                "cannot encode canonical Cloud Hypervisor VmConfig for machine `{}`",
                input.name
            ),
        )
        .with_hint("the generated vmconfig.json was not changed")
        .with_source(source)
    })?;
    Ok(CanonicalVmConfig {
        value: merged,
        bytes,
    })
}

/// Atomically persists the exact canonical bytes returned to the VMM client.
pub fn publish_vm_config(
    paths: &Paths,
    manifest: &DependencyManifest,
    input: VmConfigInput<'_>,
) -> Result<CanonicalVmConfig, FirestoneError> {
    let config = canonical_vm_config(paths, manifest, input)?;
    paths.validate_machine_data_directory(input.name)?;
    atomic::write_with_mode(
        &paths.machine_vmconfig(input.name)?,
        config.as_bytes(),
        0o600,
    )?;
    Ok(config)
}

fn base_vm_config(
    paths: &Paths,
    input: VmConfigInput<'_>,
    payload: PayloadConfig,
) -> Result<VmConfig, FirestoneError> {
    let net = NetConfig::from_plan(input.network)?;

    if input.state.cid != 3 {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "machine state must use fixed vsock CID 3, found {}",
                input.state.cid
            ),
        )
        .with_hint("persist vsock CID 3 before building VmConfig"));
    }

    let fs = if input.filesystems.is_empty() {
        None
    } else {
        Some(input.filesystems.iter().map(FsConfig::from_plan).collect())
    };

    let vcpus = u32::from(input.spec.cpus);
    Ok(VmConfig {
        cpus: CpusConfig {
            boot_vcpus: vcpus,
            max_vcpus: vcpus,
        },
        memory: MemoryConfig {
            size: input.spec.memory.as_bytes(),
            shared: true,
        },
        payload,
        disks: vec![
            DiskConfig {
                path: paths.machine_disk(input.name)?,
                readonly: None,
                image_type: ImageType::Qcow2,
                backing_files: Some(true),
            },
            DiskConfig {
                path: paths.machine_seed_image(input.name)?,
                readonly: Some(true),
                image_type: ImageType::Raw,
                backing_files: None,
            },
        ],
        net,
        fs,
        vsock: VsockConfig {
            cid: input.state.cid,
            socket: paths.machine_vsock_socket(input.name)?,
        },
        serial: SerialConfig {
            mode: ConsoleMode::File,
            file: paths.machine_console_log(input.name)?,
        },
        console: ConsoleConfig {
            mode: ConsoleMode::Pty,
        },
        rng: RngConfig {
            src: PathBuf::from("/dev/urandom"),
        },
    })
}

fn utf8_path(path: &Path, field: &str) -> Result<String, FirestoneError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("{field} path '{}' is not UTF-8", path.display()),
        )
        .with_hint("use a UTF-8 Firestone runtime directory")
    })
}

fn resolve_payload(
    paths: &Paths,
    manifest: &DependencyManifest,
    input: VmConfigInput<'_>,
) -> Result<PayloadConfig, FirestoneError> {
    let effective = effective_firmware(
        &input.spec.vmm.firmware,
        input.architecture,
        input.catalog_firmware,
    );
    match effective {
        EffectiveFirmware::Rhf => {
            let artifact =
                manifest.artifact("rust-hypervisor-firmware", input.architecture.as_str())?;
            let path = installed_firmware(paths, &artifact)?;
            Ok(PayloadConfig {
                firmware: None,
                kernel: Some(path),
            })
        }
        EffectiveFirmware::Edk2 => {
            let artifact =
                manifest.artifact("cloud-hypervisor-edk2", input.architecture.as_str())?;
            let path = installed_firmware(paths, &artifact)?;
            Ok(PayloadConfig {
                firmware: Some(path),
                kernel: None,
            })
        }
        EffectiveFirmware::Custom(path) => {
            if !path.is_absolute() {
                return Err(FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!("custom firmware path '{}' is not absolute", path.display()),
                )
                .with_hint("resolve custom firmware through Paths before building VmConfig"));
            }
            require_custom_firmware(path)?;
            Ok(PayloadConfig {
                firmware: Some(path.to_path_buf()),
                kernel: None,
            })
        }
    }
}

fn effective_firmware(
    firmware: &Firmware,
    architecture: Arch,
    catalog_firmware: Option<CatalogFirmware>,
) -> EffectiveFirmware<'_> {
    if let Some(path) = firmware.as_path() {
        return EffectiveFirmware::Custom(path);
    }
    if *firmware == Firmware::RHF {
        return EffectiveFirmware::Rhf;
    }
    if *firmware == Firmware::EDK2 {
        return EffectiveFirmware::Edk2;
    }

    match catalog_firmware.unwrap_or(match architecture {
        Arch::X86_64 => CatalogFirmware::Rhf,
        Arch::Aarch64 => CatalogFirmware::Edk2,
    }) {
        CatalogFirmware::Rhf => EffectiveFirmware::Rhf,
        CatalogFirmware::Edk2 => EffectiveFirmware::Edk2,
    }
}

/// Resolves the one Firestone-owned firmware artifact needed for this start.
/// Custom firmware paths remain entirely user-managed.
pub(crate) fn selected_pinned_firmware(
    manifest: &DependencyManifest,
    firmware: &Firmware,
    architecture: Arch,
    catalog_firmware: Option<CatalogFirmware>,
) -> Result<Option<DependencyArtifact>, FirestoneError> {
    let dependency = match effective_firmware(firmware, architecture, catalog_firmware) {
        EffectiveFirmware::Rhf => "rust-hypervisor-firmware",
        EffectiveFirmware::Edk2 => "cloud-hypervisor-edk2",
        EffectiveFirmware::Custom(_) => return Ok(None),
    };
    manifest
        .artifact(dependency, architecture.as_str())
        .map(Some)
}

fn installed_firmware(
    paths: &Paths,
    artifact: &DependencyArtifact,
) -> Result<PathBuf, FirestoneError> {
    verified_pinned_artifact(paths, artifact)
}

fn require_custom_firmware(path: &Path) -> Result<(), FirestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("firmware file {} is unavailable", path.display()),
        )
        .with_hint("correct vmm.firmware to an existing regular file")
        .with_source(source)
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("firmware path {} is not a regular file", path.display()),
        )
        .with_hint("set vmm.firmware to an existing regular file"));
    }
    Ok(())
}

fn apply_merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_object) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Some(target_object) = target.as_object_mut() else {
        return;
    };
    for (key, patch_value) in patch_object {
        if patch_value.is_null() {
            target_object.remove(key);
        } else {
            apply_merge_patch(
                target_object.entry(key.clone()).or_insert(Value::Null),
                patch_value,
            );
        }
    }
}

fn validate_required_invariants(
    base: &Value,
    candidate: &Value,
    input: VmConfigInput<'_>,
) -> Result<(), FirestoneError> {
    if let Err(path) = required_subset(base, candidate, "") {
        return Err(required_overlay_error(&path));
    }
    validate_required_defaults(candidate, input)?;
    let Some(object) = candidate.as_object() else {
        return Err(required_overlay_error("<root>"));
    };
    if matches!(input.network, NetworkPlan::None) && object.contains_key("net") {
        return Err(required_overlay_error("net"));
    }
    if input.filesystems.is_empty() && object.contains_key("fs") {
        return Err(required_overlay_error("fs"));
    }

    let payload = object.get("payload").and_then(Value::as_object);
    let required_payload = base.get("payload").and_then(Value::as_object);
    if let (Some(payload), Some(required_payload)) = (payload, required_payload) {
        if required_payload.contains_key("kernel") && payload.contains_key("firmware") {
            return Err(required_overlay_error("payload.firmware"));
        }
        if required_payload.contains_key("firmware") && payload.contains_key("kernel") {
            return Err(required_overlay_error("payload.kernel"));
        }
    }
    Ok(())
}

fn validate_required_defaults(
    candidate: &Value,
    input: VmConfigInput<'_>,
) -> Result<(), FirestoneError> {
    for path in [
        "/disks/0/readonly",
        "/disks/0/vhost_user",
        "/disks/1/backing_files",
        "/disks/1/vhost_user",
    ] {
        require_false_or_absent(candidate, path)?;
    }

    match input.network {
        NetworkPlan::Passt(_) => {
            for path in ["/net/0/tap", "/net/0/ip", "/net/0/mask"] {
                require_absent(candidate, path)?;
            }
        }
        NetworkPlan::Tap(_) => {
            require_false_or_absent(candidate, "/net/0/vhost_user")?;
            for path in ["/net/0/ip", "/net/0/mask", "/net/0/vhost_socket"] {
                require_absent(candidate, path)?;
            }
        }
        NetworkPlan::None => {}
    }
    Ok(())
}

fn require_false_or_absent(candidate: &Value, pointer: &str) -> Result<(), FirestoneError> {
    match candidate.pointer(pointer) {
        None | Some(Value::Bool(false)) => Ok(()),
        Some(_) => Err(required_overlay_error(&display_pointer(pointer))),
    }
}

fn require_absent(candidate: &Value, pointer: &str) -> Result<(), FirestoneError> {
    if candidate.pointer(pointer).is_none() {
        Ok(())
    } else {
        Err(required_overlay_error(&display_pointer(pointer)))
    }
}

fn display_pointer(pointer: &str) -> String {
    let mut display = String::new();
    for component in pointer.trim_start_matches('/').split('/') {
        if component.bytes().all(|byte| byte.is_ascii_digit()) {
            display.push('[');
            display.push_str(component);
            display.push(']');
        } else {
            if !display.is_empty() {
                display.push('.');
            }
            display.push_str(component);
        }
    }
    display
}

fn required_subset(required: &Value, candidate: &Value, path: &str) -> Result<(), String> {
    match required {
        Value::Object(required_object) => {
            let Some(candidate_object) = candidate.as_object() else {
                return Err(display_path(path));
            };
            for (key, required_value) in required_object {
                let child_path = object_path(path, key);
                let Some(candidate_value) = candidate_object.get(key) else {
                    return Err(child_path);
                };
                required_subset(required_value, candidate_value, &child_path)?;
            }
            Ok(())
        }
        Value::Array(required_array) => {
            let Some(candidate_array) = candidate.as_array() else {
                return Err(display_path(path));
            };
            if candidate_array.len() < required_array.len() {
                return Err(display_path(path));
            }
            for (index, required_value) in required_array.iter().enumerate() {
                let child_path = format!("{}[{index}]", display_path(path));
                required_subset(required_value, &candidate_array[index], &child_path)?;
            }
            Ok(())
        }
        _ if required == candidate => Ok(()),
        _ => Err(display_path(path)),
    }
}

fn object_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    }
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "<root>".to_owned()
    } else {
        path.to_owned()
    }
}

fn required_overlay_error(path: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("vmm.config_overlay changes required VmConfig field `{path}`"),
    )
    .with_hint("remove that overlay change; Firestone owns required boot and sidecar fields")
}

fn sort_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let mut entries = std::mem::take(object).into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            for (_, child) in &mut entries {
                sort_json(child);
            }
            object.extend(entries);
        }
        Value::Array(array) => {
            for child in array {
                sort_json(child);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsStr,
        fs, io,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
    };

    use super::{
        NetConfig, PayloadConfig, VmConfigInput, apply_merge_patch, base_vm_config,
        canonical_vm_config, publish_vm_config,
    };
    use crate::{
        Arch, CatalogFirmware, DependencyManifest, ErrorKind, Firmware, MachineSpec, MachineState,
        MachineStatus, MountSpec, NetMode, NetworkPlan, NetworkPlanOptions, NetworkSpec,
        PathInputs, Paths, StateImage, StateVersion, TapHost, VirtiofsPlan, VirtiofsSandbox,
        prepare_network, prepare_virtiofs_plans,
    };
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;

    static NO_NETWORK: NetworkPlan = NetworkPlan::None;
    static NO_FILESYSTEMS: [VirtiofsPlan; 0] = [];
    struct ReadyTapHost;

    impl TapHost for ReadyTapHost {
        fn tap_device_is_tap(&self, _name: &str) -> io::Result<bool> {
            Ok(true)
        }

        fn tun_is_accessible(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn network_plan(
        paths: &Paths,
        spec: &NetworkSpec,
    ) -> Result<NetworkPlan, crate::FirestoneError> {
        prepare_network(NetworkPlanOptions::new(
            paths,
            "demo",
            spec,
            "52:54:00:9a:1f:c3".parse().map_err(|source| {
                crate::FirestoneError::new(ErrorKind::InvalidSpec, "invalid fixture MAC")
                    .with_source(source)
            })?,
            OsStr::new("passt"),
            &ReadyTapHost,
        ))
    }

    const FIRMWARE_FIXTURE: &[u8] = b"firmware fixture";
    const VIRTIOFSD_FIXTURE: &[u8] = b"virtiofsd fixture";

    fn fixture_manifest() -> Result<DependencyManifest, crate::FirestoneError> {
        let firmware_sha = format!("{:x}", Sha256::digest(FIRMWARE_FIXTURE));
        let virtiofsd_sha = format!("{:x}", Sha256::digest(VIRTIOFSD_FIXTURE));
        DependencyManifest::parse(&format!(
            r#"manifest_version = 1

[dependency.rust-hypervisor-firmware]
version = "0.5.0"
availability = "binary"
[dependency.rust-hypervisor-firmware.x86_64]
asset = "hypervisor-fw"
install_name = "hypervisor-fw-0.5.0"
url = "https://example.invalid/hypervisor-fw-x86_64"
sha256 = "{firmware_sha}"
[dependency.rust-hypervisor-firmware.aarch64]
asset = "hypervisor-fw"
install_name = "hypervisor-fw-0.5.0"
url = "https://example.invalid/hypervisor-fw-aarch64"
sha256 = "{firmware_sha}"

[dependency.cloud-hypervisor-edk2]
version = "ch-test"
availability = "binary"
[dependency.cloud-hypervisor-edk2.x86_64]
asset = "CLOUDHV.fd"
install_name = "CLOUDHV-ch-test.fd"
url = "https://example.invalid/edk2-x86_64"
sha256 = "{firmware_sha}"
[dependency.cloud-hypervisor-edk2.aarch64]
asset = "CLOUDHV_EFI.fd"
install_name = "CLOUDHV_EFI-ch-test.fd"
url = "https://example.invalid/edk2-aarch64"
sha256 = "{firmware_sha}"

[dependency.virtiofsd]
version = "v1.14.0"
availability = "binary"
[dependency.virtiofsd.x86_64]
asset = "virtiofsd"
install_name = "virtiofsd-v1.14.0"
url = "https://example.invalid/virtiofsd"
sha256 = "{virtiofsd_sha}"
"#,
        ))
    }

    fn fixture_artifact_bytes(dependency: &str) -> Result<&'static [u8], io::Error> {
        match dependency {
            "rust-hypervisor-firmware" | "cloud-hypervisor-edk2" => Ok(FIRMWARE_FIXTURE),
            "virtiofsd" => Ok(VIRTIOFSD_FIXTURE),
            other => Err(io::Error::other(format!(
                "no fixture bytes for dependency {other}"
            ))),
        }
    }

    struct Fixture {
        _temp: TempDir,
        paths: Paths,
        manifest: DependencyManifest,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = tempfile::tempdir()?;
            let root = fs::canonicalize(temp.path())?;
            let paths = paths_for_root(root.clone())?;
            let machine_dir = paths.machine_dir("demo")?;
            let runtime_dir = paths.machine_runtime_dir("demo")?;
            let bin_dir = paths.bin_dir();
            fs::create_dir_all(&machine_dir)?;
            fs::create_dir_all(&runtime_dir)?;
            fs::create_dir_all(&bin_dir)?;
            for directory in [
                root,
                paths.data_dir().to_path_buf(),
                paths.machines_dir(),
                machine_dir,
                paths.runtime_dir().to_path_buf(),
                runtime_dir,
                bin_dir,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
            Ok(Self {
                _temp: temp,
                paths,
                manifest: fixture_manifest()?,
            })
        }

        fn install(
            &self,
            dependency: &str,
            architecture: Arch,
        ) -> Result<PathBuf, Box<dyn std::error::Error>> {
            let artifact = self.manifest.artifact(dependency, architecture.as_str())?;
            let path = self.paths.binary_file(&artifact.install_name)?;
            fs::write(&path, fixture_artifact_bytes(dependency)?)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(artifact.expected_mode()))?;
            Ok(path)
        }
    }

    #[test]
    fn base_vmconfig_without_sidecars_matches_v53_golden_mapping()
    -> Result<(), Box<dyn std::error::Error>> {
        let paths = paths_for_root(PathBuf::from("/firestone"))?;
        let spec = MachineSpec::default();
        let state = state(&paths)?;
        let input = input(&spec, &state, Arch::X86_64, None);
        let typed = base_vm_config(
            &paths,
            input,
            PayloadConfig {
                firmware: None,
                kernel: Some(PathBuf::from("/firestone/data/bin/hypervisor-fw-0.5.0")),
            },
        )?;

        assert_eq!(
            serde_json::to_value(typed)?,
            json!({
                "cpus": {"boot_vcpus": 2, "max_vcpus": 2},
                "memory": {"size": 2_147_483_648_u64, "shared": true},
                "payload": {"kernel": "/firestone/data/bin/hypervisor-fw-0.5.0"},
                "disks": [
                    {
                        "path": "/firestone/data/machines/demo/disk.qcow2",
                        "image_type": "Qcow2",
                        "backing_files": true
                    },
                    {
                        "path": "/firestone/data/machines/demo/seed.img",
                        "readonly": true,
                        "image_type": "Raw"
                    }
                ],
                "vsock": {"cid": 3, "socket": "/firestone/run/demo/vsock.sock"},
                "serial": {
                    "mode": "File",
                    "file": "/firestone/data/machines/demo/console.log"
                },
                "console": {"mode": "Pty"},
                "rng": {"src": "/dev/urandom"}
            })
        );
        Ok(())
    }

    #[test]
    fn default_vmconfig_uses_v53_supported_pty_console() -> Result<(), Box<dyn std::error::Error>> {
        let paths = paths_for_root(PathBuf::from("/firestone"))?;
        let spec = MachineSpec::default();
        let state = state(&paths)?;
        let config = base_vm_config(
            &paths,
            input(&spec, &state, Arch::X86_64, None),
            PayloadConfig {
                firmware: None,
                kernel: Some(PathBuf::from("/firestone/data/bin/hypervisor-fw-0.5.0")),
            },
        )?;

        assert_eq!(
            serde_json::to_value(config)?["console"],
            json!({"mode": "Pty"})
        );
        Ok(())
    }

    #[test]
    fn installed_firmware_architectures_map_to_exact_payload_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        for (architecture, firmware, dependency, field) in [
            (
                Arch::X86_64,
                Firmware::RHF,
                "rust-hypervisor-firmware",
                "kernel",
            ),
            (
                Arch::Aarch64,
                Firmware::RHF,
                "rust-hypervisor-firmware",
                "kernel",
            ),
            (
                Arch::X86_64,
                Firmware::EDK2,
                "cloud-hypervisor-edk2",
                "firmware",
            ),
            (
                Arch::Aarch64,
                Firmware::EDK2,
                "cloud-hypervisor-edk2",
                "firmware",
            ),
        ] {
            let expected = fixture.install(dependency, architecture)?;
            let mut spec = MachineSpec::default();
            spec.vmm.firmware = firmware;
            let state = state(&fixture.paths)?;

            let config = canonical_vm_config(
                &fixture.paths,
                &fixture.manifest,
                input(&spec, &state, architecture, None),
            )?;

            assert_eq!(
                config
                    .as_value()
                    .pointer(&format!("/payload/{field}"))
                    .and_then(Value::as_str),
                expected.to_str()
            );
            let other = if field == "kernel" {
                "firmware"
            } else {
                "kernel"
            };
            assert!(
                config
                    .as_value()
                    .pointer(&format!("/payload/{other}"))
                    .is_none()
            );
        }
        Ok(())
    }

    #[test]
    fn auto_firmware_catalog_and_local_defaults_map_by_architecture()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let rhf = fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let edk2_x86 = fixture.install("cloud-hypervisor-edk2", Arch::X86_64)?;
        let edk2_arm = fixture.install("cloud-hypervisor-edk2", Arch::Aarch64)?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let local_x86 = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )?;
        let catalog_x86 = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, Some(CatalogFirmware::Edk2)),
        )?;
        let local_arm = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::Aarch64, None),
        )?;

        assert_eq!(
            local_x86
                .as_value()
                .pointer("/payload/kernel")
                .and_then(Value::as_str),
            rhf.to_str()
        );
        assert_eq!(
            catalog_x86
                .as_value()
                .pointer("/payload/firmware")
                .and_then(Value::as_str),
            edk2_x86.to_str()
        );
        assert_eq!(
            local_arm
                .as_value()
                .pointer("/payload/firmware")
                .and_then(Value::as_str),
            edk2_arm.to_str()
        );
        Ok(())
    }

    #[test]
    fn custom_firmware_path_maps_to_payload_firmware() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let custom = fixture.paths.machine_dir("demo")?.join("custom.fd");
        fs::write(&custom, b"custom firmware")?;
        let mut spec = MachineSpec::default();
        spec.vmm.firmware = Firmware::path(custom.clone())?;
        let state = state(&fixture.paths)?;

        let config = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )?;

        assert_eq!(
            config
                .as_value()
                .pointer("/payload/firmware")
                .and_then(Value::as_str),
            custom.to_str()
        );
        assert!(config.as_value().pointer("/payload/kernel").is_none());
        Ok(())
    }

    #[test]
    fn network_modes_map_passt_tap_and_none_without_ip_mask()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let state = state(&fixture.paths)?;

        let passt_spec = MachineSpec::default();
        let passt_plan = network_plan(&fixture.paths, &passt_spec.network)?;
        let passt = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input_with_plans(
                &passt_spec,
                &state,
                &passt_plan,
                &NO_FILESYSTEMS,
                Arch::X86_64,
                None,
            ),
        )?;
        assert_eq!(
            passt.as_value().pointer("/net/0/vhost_user"),
            Some(&json!(true))
        );
        assert_eq!(
            passt.as_value().pointer("/net/0/vhost_mode"),
            Some(&json!("Client"))
        );

        let mut tap_spec = MachineSpec::default();
        tap_spec.network.mode = NetMode::Tap;
        tap_spec.network.tap = Some("tap0".to_owned());
        let tap_plan = network_plan(&fixture.paths, &tap_spec.network)?;
        let tap = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input_with_plans(
                &tap_spec,
                &state,
                &tap_plan,
                &NO_FILESYSTEMS,
                Arch::X86_64,
                None,
            ),
        )?;
        assert_eq!(
            tap.as_value().get("net"),
            Some(&json!([{"tap": "tap0", "mac": "52:54:00:9a:1f:c3"}]))
        );
        assert!(tap.as_value().pointer("/net/0/ip").is_none());
        assert!(tap.as_value().pointer("/net/0/mask").is_none());

        let mut none_spec = MachineSpec::default();
        none_spec.network.mode = NetMode::None;
        let none_plan = network_plan(&fixture.paths, &none_spec.network)?;
        let mut none_state = state.clone();
        none_state.mac = None;
        let none = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input_with_plans(
                &none_spec,
                &none_state,
                &none_plan,
                &NO_FILESYSTEMS,
                Arch::X86_64,
                None,
            ),
        )?;
        assert!(none.as_value().get("net").is_none());
        Ok(())
    }

    #[test]
    fn network_plans_map_to_exact_v53_json_goldens() -> Result<(), Box<dyn std::error::Error>> {
        #[derive(serde::Serialize)]
        struct NetworkFragment {
            #[serde(skip_serializing_if = "Option::is_none")]
            net: Option<Vec<NetConfig>>,
        }

        struct ReadyTapHost;
        impl TapHost for ReadyTapHost {
            fn tap_device_is_tap(&self, _name: &str) -> io::Result<bool> {
                Ok(true)
            }

            fn tun_is_accessible(&self) -> io::Result<()> {
                Ok(())
            }
        }

        let fixture = Fixture::new()?;
        let host = ReadyTapHost;
        let mac = "52:54:00:9a:1f:c3".parse()?;

        let passt_spec = NetworkSpec::default();
        let passt = prepare_network(NetworkPlanOptions::new(
            &fixture.paths,
            "demo",
            &passt_spec,
            mac,
            OsStr::new("passt"),
            &host,
        ))?;
        assert!(matches!(&passt, NetworkPlan::Passt(_)));
        assert_eq!(
            serde_json::to_value(NetworkFragment {
                net: NetConfig::from_plan(&passt)?,
            })?,
            json!({
                "net": [{
                    "mac": "52:54:00:9a:1f:c3",
                    "vhost_user": true,
                    "vhost_socket": fixture.paths.machine_net_socket("demo")?,
                    "vhost_mode": "Client"
                }]
            })
        );

        let tap_spec = NetworkSpec {
            mode: NetMode::Tap,
            tap: Some("tap0".to_owned()),
            ..NetworkSpec::default()
        };
        let tap = prepare_network(NetworkPlanOptions::new(
            &fixture.paths,
            "demo",
            &tap_spec,
            mac,
            OsStr::new("passt"),
            &host,
        ))?;
        assert!(matches!(&tap, NetworkPlan::Tap(_)));
        assert_eq!(
            serde_json::to_value(NetworkFragment {
                net: NetConfig::from_plan(&tap)?,
            })?,
            json!({"net": [{"tap": "tap0", "mac": "52:54:00:9a:1f:c3"}]})
        );

        let none_spec = NetworkSpec {
            mode: NetMode::None,
            ..NetworkSpec::default()
        };
        let none = prepare_network(NetworkPlanOptions::new(
            &fixture.paths,
            "demo",
            &none_spec,
            mac,
            OsStr::new("passt"),
            &host,
        ))?;
        assert!(matches!(&none, NetworkPlan::None));
        assert_eq!(
            serde_json::to_value(NetworkFragment {
                net: NetConfig::from_plan(&none)?,
            })?,
            json!({})
        );
        Ok(())
    }
    #[test]
    fn mounts_map_to_required_v53_fs_entries() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let virtiofsd = fixture.install("virtiofsd", Arch::X86_64)?;
        fs::set_permissions(&virtiofsd, fs::Permissions::from_mode(0o755))?;
        let first_host = fixture._temp.path().join("host-a");
        let second_host = fixture._temp.path().join("host-b");
        fs::create_dir(&first_host)?;
        fs::create_dir(&second_host)?;
        fs::set_permissions(&first_host, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&second_host, fs::Permissions::from_mode(0o700))?;
        let first_host = fs::canonicalize(first_host)?;
        let second_host = fs::canonicalize(second_host)?;
        let spec = MachineSpec {
            mounts: vec![
                MountSpec {
                    host: first_host,
                    guest: PathBuf::from("/guest/a"),
                    readonly: false,
                    tag: None,
                },
                MountSpec {
                    host: second_host,
                    guest: PathBuf::from("/guest/b"),
                    readonly: true,
                    tag: Some("source".to_owned()),
                },
            ],
            ..MachineSpec::default()
        };
        let filesystems = prepare_virtiofs_plans(
            &fixture.paths,
            &fixture.manifest,
            "demo",
            Arch::X86_64,
            &spec.mounts,
            VirtiofsSandbox::None,
        )?;
        let state = state(&fixture.paths)?;

        let config = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input_with_plans(&spec, &state, &NO_NETWORK, &filesystems, Arch::X86_64, None),
        )?;

        assert_eq!(
            config.as_value().get("fs"),
            Some(&json!([
                {
                    "tag": "share0",
                    "socket": fixture.paths.machine_fs_socket("demo", 0)?,
                    "num_queues": 1,
                    "queue_size": 1024
                },
                {
                    "tag": "source",
                    "socket": fixture.paths.machine_fs_socket("demo", 1)?,
                    "num_queues": 1,
                    "queue_size": 1024
                }
            ]))
        );
        Ok(())
    }

    #[test]
    fn merge_patch_nested_null_deletes_only_nested_member() {
        let mut target = json!({
            "memory": {"shared": true, "hugepages": false},
            "watchdog": true
        });
        apply_merge_patch(
            &mut target,
            &json!({"memory": {"hugepages": null, "prefault": true}}),
        );

        assert_eq!(
            target,
            json!({
                "memory": {"shared": true, "prefault": true},
                "watchdog": true
            })
        );
    }

    #[test]
    fn canonical_json_recursively_sorts_overlay_objects() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let mut spec = MachineSpec::default();
        spec.vmm.config_overlay = Some(serde_json::from_str(
            r#"{"zeta":{"z":1,"a":2},"memory":{"hugepages":true},"alpha":true}"#,
        )?);
        let state = state(&fixture.paths)?;

        let config = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )?;
        let text = std::str::from_utf8(config.as_bytes())?;

        assert!(text.starts_with(r#"{"alpha":true,"console":{"mode":"Pty""#));
        assert!(text.contains(r#""memory":{"hugepages":true,"shared":true,"size":2147483648}"#));
        assert!(text.ends_with(r#","zeta":{"a":2,"z":1}}"#));
        assert_eq!(serde_json::to_vec(config.as_value())?, config.as_bytes());
        Ok(())
    }

    #[test]
    fn overlay_required_field_changes_return_stable_invalid_spec_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let state = state(&fixture.paths)?;
        let passt_plan = network_plan(&fixture.paths, &NetworkSpec::default())?;

        for (overlay, field) in [
            (json!({"memory": {"shared": false}}), "memory.shared"),
            (json!({"rng": null}), "rng"),
            (
                json!({"disks": [
                    {
                        "path": fixture.paths.machine_disk("demo")?,
                        "image_type": "Qcow2",
                        "backing_files": false
                    },
                    {
                        "path": fixture.paths.machine_seed_image("demo")?,
                        "readonly": true,
                        "image_type": "Raw"
                    }
                ]}),
                "disks[0].backing_files",
            ),
            (
                json!({"disks": [
                    {
                        "path": fixture.paths.machine_disk("demo")?,
                        "readonly": true,
                        "image_type": "Qcow2",
                        "backing_files": true
                    },
                    {
                        "path": fixture.paths.machine_seed_image("demo")?,
                        "readonly": true,
                        "image_type": "Raw"
                    }
                ]}),
                "disks[0].readonly",
            ),
            (
                json!({"disks": [
                    {
                        "path": fixture.paths.machine_disk("demo")?,
                        "image_type": "Qcow2",
                        "backing_files": true
                    },
                    {
                        "path": fixture.paths.machine_seed_image("demo")?,
                        "readonly": true,
                        "image_type": "Raw",
                        "backing_files": true
                    }
                ]}),
                "disks[1].backing_files",
            ),
            (
                json!({"net": [{
                    "vhost_user": true,
                    "vhost_socket": fixture.paths.machine_net_socket("demo")?,
                    "vhost_mode": "Client",
                    "mac": "52:54:00:9a:1f:c3",
                    "ip": "192.168.249.1"
                }]}),
                "net[0].ip",
            ),
        ] {
            let mut spec = MachineSpec::default();
            spec.vmm.config_overlay = Some(overlay);
            let error = canonical_vm_config(
                &fixture.paths,
                &fixture.manifest,
                input_with_plans(
                    &spec,
                    &state,
                    &passt_plan,
                    &NO_FILESYSTEMS,
                    Arch::X86_64,
                    None,
                ),
            )
            .err()
            .ok_or("required overlay change should fail")?;
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert_eq!(
                error.message(),
                format!("vmm.config_overlay changes required VmConfig field `{field}`")
            );
            assert_eq!(
                error.hint(),
                Some("remove that overlay change; Firestone owns required boot and sidecar fields")
            );
        }
        Ok(())
    }

    #[test]
    fn publish_vmconfig_persists_exact_bytes_sent() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let config = publish_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )?;

        assert_eq!(
            fs::read(fixture.paths.machine_vmconfig("demo")?)?,
            config.as_bytes()
        );
        assert_eq!(serde_json::to_vec(&config)?, config.as_bytes());
        assert!(!config.as_bytes().ends_with(b"\n"));
        Ok(())
    }

    #[test]
    fn publish_vmconfig_symlinked_machine_directory_preserves_external_sentinel()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;
        let outside = tempfile::tempdir()?;
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"keep")?;
        let machine_dir = fixture.paths.machine_dir("demo")?;
        fs::remove_dir(&machine_dir)?;
        symlink(outside.path(), &machine_dir)?;

        let error = publish_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("symlinked machine directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&sentinel)?, b"keep");
        assert!(!outside.path().join("vmconfig.json").exists());
        Ok(())
    }

    #[test]
    fn publish_vmconfig_world_writable_machines_ancestry_refuses_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;
        fs::set_permissions(
            fixture.paths.machines_dir(),
            fs::Permissions::from_mode(0o777),
        )?;

        let error = publish_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("world-writable machines ancestry should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(!fixture.paths.machine_vmconfig("demo")?.exists());
        Ok(())
    }

    #[test]
    fn installed_firmware_world_writable_bin_directory_refuses_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        fs::set_permissions(fixture.paths.bin_dir(), fs::Permissions::from_mode(0o777))?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let error = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("world-writable binary directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[test]
    fn installed_firmware_symlinked_bin_directory_preserves_external_artifact()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let installed = fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let file_name = installed
            .file_name()
            .ok_or("installed firmware path should have a file name")?;
        let outside = tempfile::tempdir()?;
        let external = outside.path().join(file_name);
        fs::write(&external, b"external firmware")?;
        fs::remove_file(&installed)?;
        fs::remove_dir(fixture.paths.bin_dir())?;
        symlink(outside.path(), fixture.paths.bin_dir())?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let error = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("symlinked binary directory should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&external)?, b"external firmware");
        Ok(())
    }

    #[test]
    fn installed_firmware_symlinked_artifact_preserves_external_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let installed = fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let outside = tempfile::tempdir()?;
        let external = outside.path().join("firmware");
        fs::write(&external, b"external firmware")?;
        fs::remove_file(&installed)?;
        symlink(&external, &installed)?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let error = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("symlinked firmware artifact should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::read(&external)?, b"external firmware");
        Ok(())
    }

    #[test]
    fn installed_firmware_checksum_mismatch_refuses_vmconfig_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let installed = fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        fs::write(&installed, b"tampered firmware")?;
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o644))?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let error = publish_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("tampered firmware should fail")?;

        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert!(!fixture.paths.machine_vmconfig("demo")?.exists());
        Ok(())
    }

    #[test]
    fn missing_installed_firmware_returns_actionable_dependency_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let spec = MachineSpec::default();
        let state = state(&fixture.paths)?;

        let error = canonical_vm_config(
            &fixture.paths,
            &fixture.manifest,
            input(&spec, &state, Arch::X86_64, None),
        )
        .err()
        .ok_or("missing firmware should fail")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error
                .message()
                .contains("pinned 'rust-hypervisor-firmware'")
        );
        assert_eq!(
            error.hint(),
            Some("retry start so Firestone can install the selected pinned firmware")
        );
        Ok(())
    }

    #[test]
    fn vmconfig_cid_other_than_fixed_three_returns_invalid_spec()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        fixture.install("rust-hypervisor-firmware", Arch::X86_64)?;
        let spec = MachineSpec::default();

        for cid in [2, 4] {
            let mut state = state(&fixture.paths)?;
            state.cid = cid;
            let error = canonical_vm_config(
                &fixture.paths,
                &fixture.manifest,
                input(&spec, &state, Arch::X86_64, None),
            )
            .err()
            .ok_or("non-fixed CID should fail")?;

            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert_eq!(
                error.message(),
                format!("machine state must use fixed vsock CID 3, found {cid}")
            );
            assert_eq!(
                error.hint(),
                Some("persist vsock CID 3 before building VmConfig")
            );
        }
        Ok(())
    }

    fn paths_for_root(root: PathBuf) -> Result<Paths, crate::FirestoneError> {
        Paths::from_inputs(&PathInputs {
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
        })
    }

    fn state(paths: &Paths) -> Result<MachineState, crate::FirestoneError> {
        Ok(MachineState {
            version: StateVersion,
            status: MachineStatus::Created,
            image: StateImage {
                r#ref: "ubuntu:24.04".to_owned(),
                id: Some("ubuntu-24.04-x86_64-deadbeef".to_owned()),
                sha256: Some("deadbeef".to_owned()),
            },
            mac: Some("52:54:00:9a:1f:c3".to_owned()),
            cid: 3,
            instance_id: Some("iid-demo-deadbeef0000".to_owned()),
            shim_pid: None,
            vmm_pid: None,
            sidecar_pids: BTreeMap::new(),
            runtime_dir: paths.machine_runtime_dir("demo")?,
            started_at: None,
            forwards: Vec::new(),
            degraded: Vec::new(),
            last_exit: None,
        })
    }

    fn input<'a>(
        spec: &'a MachineSpec,
        state: &'a MachineState,
        architecture: Arch,
        catalog_firmware: Option<CatalogFirmware>,
    ) -> VmConfigInput<'a> {
        VmConfigInput {
            name: "demo",
            spec,
            state,
            network: &NO_NETWORK,
            filesystems: &NO_FILESYSTEMS,
            architecture,
            catalog_firmware,
        }
    }
    fn input_with_plans<'a>(
        spec: &'a MachineSpec,
        state: &'a MachineState,
        network: &'a NetworkPlan,
        filesystems: &'a [VirtiofsPlan],
        architecture: Arch,
        catalog_firmware: Option<CatalogFirmware>,
    ) -> VmConfigInput<'a> {
        VmConfigInput {
            name: "demo",
            spec,
            state,
            network,
            filesystems,
            architecture,
            catalog_firmware,
        }
    }
}
