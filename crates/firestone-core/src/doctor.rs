use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    Cmd, DependencyArtifact, DependencyManifest, ErrorKind, FirestoneError, InternalHelper,
    MachineLock, Paths, ReconcileRewrite, StateStore, VmmApiLivenessProbe, VmmPingProbe,
    embedded_helper, observe_liveness, reconcile, reconciled_state,
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder as TempBuilder;

const MINIMUM_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_DEPENDENCY_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const DOCTOR_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MINIMUM_PASST_DATE: u32 = 20_250_217;
const PASST_USERNS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PASST_USERNS_STDERR_BYTES: usize = 16 * 1024;
const AUDIT_TAIL_BYTES: u64 = 128 * 1024;
const PASST_USERNS_FAILURE: &str = "Couldn't create user namespace";
const PASST_LATER_ISOLATION_FAILURE: &str = "Failed to detach isolating namespaces";
pub const APPARMOR_PASST_PROFILE_NAME: &str = "firestone-passt-2025_02_17.a1e48a0";
pub const APPARMOR_PASST_EXECUTABLE: &str = "/usr/libexec/firestone/passt-2025_02_17.a1e48a0";
pub const APPARMOR_PASST_PROFILE: &str = "/etc/apparmor.d/firestone-passt-2025_02_17.a1e48a0";
const APPARMOR_PASST_PROFILE_CONTENT: &str = concat!(
    "abi <abi/4.0>,\n",
    "include <tunables/global>\n\n",
    "profile firestone-passt-2025_02_17.a1e48a0 ",
    "/usr/libexec/firestone/passt-2025_02_17.a1e48a0 flags=(unconfined) {\n",
    "  userns,\n",
    "}\n",
);
const REQUIRED_PASST_FLAGS: [&str; 6] = [
    "--foreground",
    "--one-off",
    "--vhost-user",
    "--socket-path",
    "--repair-path",
    "--log-file",
];
const PASST_PORT_GRAMMAR_ARGS: [&str; 5] = ["--tcp-ports", "none", "--udp-ports", "none", "--help"];
const CHECK_IDS: [DoctorCheckId; 13] = [
    DoctorCheckId::HostArch,
    DoctorCheckId::Kvm,
    DoctorCheckId::NestedVirtualization,
    DoctorCheckId::RuntimeDir,
    DoctorCheckId::VendoredBinaries,
    DoctorCheckId::Virtiofsd,
    DoctorCheckId::Passt,
    DoctorCheckId::QemuImg,
    DoctorCheckId::Ssh,
    DoctorCheckId::UserNamespaces,
    DoctorCheckId::SshKey,
    DoctorCheckId::DataSpace,
    DoctorCheckId::StaleState,
];
const VENDORED_DEPENDENCIES: [&str; 3] = [
    "cloud-hypervisor",
    "rust-hypervisor-firmware",
    "cloud-hypervisor-edk2",
];

/// Stable result level for one host check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

/// Stable identifier for one SPEC 17.3 check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckId {
    HostArch,
    Kvm,
    NestedVirtualization,
    RuntimeDir,
    VendoredBinaries,
    Virtiofsd,
    Passt,
    QemuImg,
    Ssh,
    UserNamespaces,
    SshKey,
    DataSpace,
    StaleState,
}

/// One deterministic host diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: DoctorCheckId,
    pub status: DoctorStatus,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl DoctorCheck {
    fn new(id: DoctorCheckId, status: DoctorStatus, reason: impl Into<String>) -> Self {
        Self {
            id,
            status,
            reason: reason.into(),
            fix: None,
            hint: None,
        }
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Ordered output shared by the CLI and REST adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == DoctorStatus::Fail)
    }
}

/// A stale state reconciled while running doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleStateObservation {
    machine: String,
    reason: String,
}

/// A machine state that doctor could not reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleStateFailure {
    machine: String,
    reason: String,
}

/// Controls the mutations allowed during one doctor run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorOptions {
    fix: bool,
    elevation_confirmed: bool,
}

impl DoctorOptions {
    #[must_use]
    pub const fn inspect() -> Self {
        Self {
            fix: false,
            elevation_confirmed: false,
        }
    }

    #[must_use]
    pub const fn fix(elevation_confirmed: bool) -> Self {
        Self {
            fix: true,
            elevation_confirmed,
        }
    }
}

/// Exact extracted passt bytes offered to the AppArmor installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPasstHelper {
    path: PathBuf,
    artifact: DependencyArtifact,
}

impl ExtractedPasstHelper {
    pub fn new(path: PathBuf, artifact: DependencyArtifact) -> Result<Self, FirestoneError> {
        if !path.is_absolute() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                "the extracted passt helper path is not absolute",
            )
            .with_hint("materialize the embedded helper through a resolved Paths location"));
        }
        if artifact.dependency != "passt" || artifact.version != crate::PINNED_PASST_VERSION {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "extracted passt helper identity {} {} does not match passt {}",
                    artifact.dependency,
                    artifact.version,
                    crate::PINNED_PASST_VERSION
                ),
            )
            .with_hint(
                "extract the exact passt artifact selected by DependencyManifest::embedded_passt",
            ));
        }
        Ok(Self { path, artifact })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn artifact(&self) -> &DependencyArtifact {
        &self.artifact
    }
}

/// A literal root-owned passt copy whose profile and bytes were verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPasst {
    executable: PathBuf,
    profile_name: &'static str,
}

impl VerifiedPasst {
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub const fn profile_name(&self) -> &'static str {
        self.profile_name
    }
}

/// Resolved Firestone paths and injectable host facts used by doctor.
#[derive(Debug, Clone)]
pub struct DoctorContext {
    pub paths: Paths,
    pub operating_system: String,
    pub architecture: String,
    pub kvm_device: PathBuf,
    pub nested_parameters: Vec<PathBuf>,
    pub user_namespace_sysctl: PathBuf,
    pub unprivileged_userns_clone_sysctl: PathBuf,
    pub apparmor_enabled: PathBuf,
    pub apparmor_restrict_userns: PathBuf,
    pub self_status: PathBuf,
    pub self_cgroup: PathBuf,
    pub container_markers: Vec<PathBuf>,
    pub audit_logs: Vec<PathBuf>,
    pub apparmor_passt_executable: PathBuf,
    pub apparmor_passt_profile: PathBuf,
    pub apparmor_loaded_profiles: PathBuf,
    pub extracted_passt: Option<ExtractedPasstHelper>,
    pub expected_root_uid: u32,
    pub os_release: PathBuf,
    pub group_file: PathBuf,
    pub search_path: OsString,
    pub hostname: String,
    pub manifest: DependencyManifest,
    pub proc_root: PathBuf,
    pub reconciled_at: String,
    pub minimum_data_free_bytes: u64,
}

impl DoctorContext {
    /// Builds production host facts from the process-wide resolved paths.
    #[must_use]
    pub fn from_paths(
        paths: Paths,
        manifest: DependencyManifest,
        hostname: impl Into<String>,
        reconciled_at: impl Into<String>,
    ) -> Self {
        let apparmor_passt_executable = paths.apparmor_passt_executable();
        let apparmor_passt_profile = paths.apparmor_passt_profile();
        let apparmor_loaded_profiles = paths.apparmor_loaded_profiles();
        Self {
            paths,
            operating_system: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            kvm_device: PathBuf::from("/dev/kvm"),
            nested_parameters: vec![
                PathBuf::from("/sys/module/kvm_intel/parameters/nested"),
                PathBuf::from("/sys/module/kvm_amd/parameters/nested"),
            ],
            user_namespace_sysctl: PathBuf::from("/proc/sys/user/max_user_namespaces"),
            unprivileged_userns_clone_sysctl: PathBuf::from(
                "/proc/sys/kernel/unprivileged_userns_clone",
            ),
            apparmor_enabled: PathBuf::from("/sys/module/apparmor/parameters/enabled"),
            apparmor_restrict_userns: PathBuf::from(
                "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            ),
            self_status: PathBuf::from("/proc/self/status"),
            self_cgroup: PathBuf::from("/proc/self/cgroup"),
            container_markers: vec![
                PathBuf::from("/.dockerenv"),
                PathBuf::from("/run/.containerenv"),
                PathBuf::from("/run/systemd/container"),
            ],
            audit_logs: vec![
                PathBuf::from("/var/log/audit/audit.log"),
                PathBuf::from("/var/log/kern.log"),
                PathBuf::from("/var/log/syslog"),
            ],
            apparmor_passt_executable,
            apparmor_passt_profile,
            apparmor_loaded_profiles,
            extracted_passt: None,
            expected_root_uid: 0,
            os_release: PathBuf::from("/etc/os-release"),
            group_file: PathBuf::from("/etc/group"),
            search_path: std::env::var_os("PATH").unwrap_or_default(),
            hostname: hostname.into(),
            manifest,
            proc_root: PathBuf::from("/proc"),
            reconciled_at: reconciled_at.into(),
            minimum_data_free_bytes: MINIMUM_FREE_BYTES,
        }
    }

    /// Supplies exact bytes materialized by the embedded-helper extraction layer.
    #[must_use]
    pub fn with_extracted_passt(mut self, helper: ExtractedPasstHelper) -> Self {
        self.extracted_passt = Some(helper);
        self
    }

    /// The product threshold from SPEC 17.3, exposed for explicit contexts.
    #[must_use]
    pub const fn default_minimum_data_free_bytes() -> u64 {
        MINIMUM_FREE_BYTES
    }
}

/// Runs all checks and applies requested fixes. Root changes require a separate
/// interactive confirmation represented by DoctorOptions.
pub fn run_doctor(
    context: &DoctorContext,
    options: DoctorOptions,
) -> Result<DoctorReport, FirestoneError> {
    if !options.fix {
        let stale_state = reconcile_machine_states(context, &VMM_PING_PROBE);
        return Ok(inspect(context, &BTreeMap::new(), &stale_state));
    }

    match HttpsDownloader::new() {
        Ok(downloader) => Ok(run_doctor_with_options(context, options, &downloader)),
        Err(error) => {
            let downloader = FailedDownloader {
                reason: firestone_error_reason(&error),
                hint: error.hint().map(str::to_owned),
            };
            Ok(run_doctor_with_options(context, options, &downloader))
        }
    }
}

#[cfg(test)]
fn run_doctor_with(
    context: &DoctorContext,
    fix: bool,
    fetcher: &dyn ArtifactFetcher,
) -> DoctorReport {
    let options = if fix {
        DoctorOptions::fix(false)
    } else {
        DoctorOptions::inspect()
    };
    run_doctor_with_options(context, options, fetcher)
}

fn run_doctor_with_options(
    context: &DoctorContext,
    options: DoctorOptions,
    fetcher: &dyn ArtifactFetcher,
) -> DoctorReport {
    let failures = if options.fix {
        perform_fixes(context, fetcher, options.elevation_confirmed)
    } else {
        BTreeMap::new()
    };
    let stale_state = reconcile_machine_states(context, &VMM_PING_PROBE);
    inspect(context, &failures, &stale_state)
}

#[derive(Debug, Default)]
struct StaleStateReport {
    observations: Vec<StaleStateObservation>,
    failures: Vec<StaleStateFailure>,
}

const VMM_PING_PROBE: VmmApiLivenessProbe = VmmApiLivenessProbe::new(DOCTOR_PROBE_TIMEOUT);
#[derive(Debug, Clone)]
struct FixFailure {
    reason: String,
    fix: Option<String>,
    hint: Option<String>,
}

impl From<FirestoneError> for FixFailure {
    fn from(error: FirestoneError) -> Self {
        Self {
            reason: firestone_error_reason(&error),
            fix: None,
            hint: error.hint().map(str::to_owned),
        }
    }
}

trait ArtifactFetcher {
    fn fetch(&self, url: &Url, output: &mut dyn Write) -> Result<(), FirestoneError>;
}
struct FailedDownloader {
    reason: String,
    hint: Option<String>,
}

impl ArtifactFetcher for FailedDownloader {
    fn fetch(&self, _url: &Url, _output: &mut dyn Write) -> Result<(), FirestoneError> {
        let mut error = FirestoneError::new(ErrorKind::Dependency, self.reason.clone());
        if let Some(hint) = &self.hint {
            error = error.with_hint(hint.clone());
        }
        Err(error)
    }
}

struct HttpsDownloader {
    client: Client,
}

impl HttpsDownloader {
    fn new() -> Result<Self, FirestoneError> {
        let policy = Policy::custom(|attempt| {
            if let Some(reason) = redirect_rejection(attempt.url(), attempt.previous().len()) {
                return attempt.error(reason);
            }
            attempt.follow()
        });
        let client = Client::builder()
            .redirect(policy)
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "cannot initialize the dependency downloader",
                )
                .with_hint("check the host TLS configuration")
                .with_source(source)
            })?;
        Ok(Self { client })
    }
}

fn redirect_rejection(url: &Url, previous_redirects: usize) -> Option<&'static str> {
    if previous_redirects >= 10 {
        Some("too many dependency download redirects")
    } else if url.scheme() != "https" {
        Some("dependency download redirect is not HTTPS")
    } else {
        None
    }
}

impl ArtifactFetcher for HttpsDownloader {
    fn fetch(&self, url: &Url, output: &mut dyn Write) -> Result<(), FirestoneError> {
        require_https(url)?;
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("cannot download dependency artifact from {url}"),
                )
                .with_hint("check network access and retry `firestone doctor --fix`")
                .with_source(source)
            })?;
        if content_length_exceeds_limit(response.content_length()) {
            return Err(artifact_too_large(url));
        }
        copy_bounded(&mut response, output, MAX_DEPENDENCY_ARTIFACT_BYTES, url)?;
        Ok(())
    }
}

fn content_length_exceeds_limit(content_length: Option<u64>) -> bool {
    content_length.is_some_and(|length| length > MAX_DEPENDENCY_ARTIFACT_BYTES)
}

fn copy_bounded(
    reader: &mut dyn Read,
    output: &mut dyn Write,
    maximum: u64,
    url: &Url,
) -> Result<(), FirestoneError> {
    let mut limited = reader.take(maximum.saturating_add(1));
    let copied = std::io::copy(&mut limited, output).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot write dependency download from {url}"),
        )
        .with_hint("check free space and retry `firestone doctor --fix`")
        .with_source(source)
    })?;
    if copied > maximum {
        return Err(artifact_too_large(url));
    }
    Ok(())
}

fn artifact_too_large(url: &Url) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "dependency artifact from {url} exceeds the {} byte safety limit",
            MAX_DEPENDENCY_ARTIFACT_BYTES
        ),
    )
    .with_hint("verify the pinned release asset before retrying")
}

fn inspect(
    context: &DoctorContext,
    fix_failures: &BTreeMap<DoctorCheckId, FixFailure>,
    stale_state: &StaleStateReport,
) -> DoctorReport {
    let kvm = check_kvm(context);
    let mut checks = vec![
        check_architecture(context),
        kvm.check,
        check_nested_virtualization(context, kvm.device_exists, kvm.device_accessible),
        check_runtime_dir(context),
        check_vendored(context),
        check_virtiofsd(context),
        check_passt(context),
        check_qemu_img(context),
        check_ssh(context),
        check_user_namespaces(context),
        check_ssh_key(context),
        check_data_space(context),
        check_stale_state(stale_state),
    ];

    for check in &mut checks {
        if let Some(failure) = fix_failures.get(&check.id) {
            check.status = DoctorStatus::Fail;
            check.reason = format!("fix failed: {}", failure.reason);
            if failure.fix.is_some() {
                check.fix.clone_from(&failure.fix);
            }
            if failure.hint.is_some() {
                check.hint.clone_from(&failure.hint);
            }
        }
    }

    debug_assert_eq!(
        checks.iter().map(|check| check.id).collect::<Vec<_>>(),
        CHECK_IDS
    );
    DoctorReport { checks }
}

fn check_architecture(context: &DoctorContext) -> DoctorCheck {
    if context.operating_system != "linux" {
        return DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Fail,
            format!(
                "host operating system {} is unsupported",
                context.operating_system
            ),
        )
        .with_hint("the Firestone Linux MVP requires an x86_64 host");
    }
    if context.architecture == "x86_64" {
        DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Ok,
            format!("host architecture {} is supported", context.architecture),
        )
    } else if context.architecture == "aarch64" {
        DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Fail,
            "aarch64 runtime support is deferred from the Linux x86_64 MVP",
        )
        .with_hint("aarch64 is compile-only until its KVM catalog matrix passes")
    } else {
        DoctorCheck::new(
            DoctorCheckId::HostArch,
            DoctorStatus::Fail,
            format!("host architecture {} is unsupported", context.architecture),
        )
        .with_hint("the Firestone Linux MVP requires an x86_64 host")
    }
}

struct KvmCheck {
    check: DoctorCheck,
    device_exists: bool,
    device_accessible: bool,
}

fn check_kvm(context: &DoctorContext) -> KvmCheck {
    let metadata = match fs::metadata(&context.kvm_device) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return KvmCheck {
                check: DoctorCheck::new(
                    DoctorCheckId::Kvm,
                    DoctorStatus::Fail,
                    format!("{} does not exist", context.kvm_device.display()),
                )
                .with_hint("enable hardware virtualization and load kvm_intel or kvm_amd"),
                device_exists: false,
                device_accessible: false,
            };
        }
        Err(source) => {
            return KvmCheck {
                check: DoctorCheck::new(
                    DoctorCheckId::Kvm,
                    DoctorStatus::Fail,
                    format!("cannot inspect {}: {source}", context.kvm_device.display()),
                )
                .with_hint("run `ls -l /dev/kvm` and repair access to the KVM device"),
                device_exists: false,
                device_accessible: false,
            };
        }
    };

    let group = device_group(&context.group_file, metadata.gid());
    let group_label = group
        .clone()
        .unwrap_or_else(|| format!("GID {}", metadata.gid()));
    match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&context.kvm_device)
    {
        Ok(_) => KvmCheck {
            check: DoctorCheck::new(
                DoctorCheckId::Kvm,
                DoctorStatus::Ok,
                format!(
                    "{} opens read/write; device group is {group_label}",
                    context.kvm_device.display()
                ),
            ),
            device_exists: true,
            device_accessible: true,
        },
        Err(source) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::Kvm,
                DoctorStatus::Fail,
                format!(
                    "{} does not open read/write: {source}; device group is {group_label}",
                    context.kvm_device.display()
                ),
            );
            if let Some(group) = group {
                check = check
                    .with_fix(format!("sudo usermod -aG {group} $USER"))
                    .with_hint("log out and back in after changing group membership");
            } else {
                check = check.with_hint(format!(
                    "grant $USER read/write access to device group GID {}",
                    metadata.gid()
                ));
            }
            KvmCheck {
                check,
                device_exists: true,
                device_accessible: false,
            }
        }
    }
}

fn check_nested_virtualization(
    context: &DoctorContext,
    kvm_exists: bool,
    kvm_accessible: bool,
) -> DoctorCheck {
    let nested = context
        .nested_parameters
        .iter()
        .find_map(|path| fs::read_to_string(path).ok().map(|value| (path, value)));
    let detail = nested.map_or_else(
        || "nested virtualization parameter is unavailable".to_owned(),
        |(path, value)| {
            let enabled = matches!(value.trim(), "1" | "Y" | "y");
            format!(
                "{} reports nested virtualization {}",
                path.display(),
                if enabled { "enabled" } else { "disabled" }
            )
        },
    );

    if kvm_accessible {
        DoctorCheck::new(
            DoctorCheckId::NestedVirtualization,
            DoctorStatus::Ok,
            format!("KVM is usable; {detail}"),
        )
    } else if kvm_exists {
        DoctorCheck::new(
            DoctorCheckId::NestedVirtualization,
            DoctorStatus::Warn,
            format!("KVM exists but is not usable; {detail}"),
        )
        .with_hint("repair read/write access to /dev/kvm before starting a machine")
    } else {
        DoctorCheck::new(
            DoctorCheckId::NestedVirtualization,
            DoctorStatus::Warn,
            format!("KVM device is absent; {detail}"),
        )
        .with_hint("enable virtualization in firmware, or nested KVM in the outer hypervisor")
    }
}

fn check_runtime_dir(context: &DoctorContext) -> DoctorCheck {
    let runtime_dir = context.paths.runtime_dir();
    match context.paths.validate_runtime_dir() {
        Ok(()) if context.paths.uses_runtime_fallback() => DoctorCheck::new(
            DoctorCheckId::RuntimeDir,
            DoctorStatus::Warn,
            format!(
                "XDG_RUNTIME_DIR is unavailable; using secure fallback {}",
                runtime_dir.display()
            ),
        )
        .with_hint("set XDG_RUNTIME_DIR to a user-owned runtime directory"),
        Ok(()) => DoctorCheck::new(
            DoctorCheckId::RuntimeDir,
            DoctorStatus::Ok,
            format!("runtime directory {} is secure", runtime_dir.display()),
        ),
        Err(error) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::RuntimeDir,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_fix("firestone doctor --fix");
            if let Some(hint) = error.hint() {
                check = check.with_hint(hint);
            }
            check
        }
    }
}

fn unsafe_owned_directory_check(id: DoctorCheckId, error: &FirestoneError) -> DoctorCheck {
    let mut check = DoctorCheck::new(id, DoctorStatus::Fail, firestone_error_reason(error));
    if let Some(hint) = error.hint() {
        check = check.with_hint(hint);
    }
    check
}
fn check_vendored(context: &DoctorContext) -> DoctorCheck {
    if let Err(error) = context.paths.validate_owned_data_directory(
        &context.paths.bin_dir(),
        "binary directory",
        true,
    ) {
        return unsafe_owned_directory_check(DoctorCheckId::VendoredBinaries, &error);
    }
    let mut failures = Vec::new();
    let mut installed = Vec::new();
    let mut hint = None;
    for dependency in VENDORED_DEPENDENCIES {
        match context.manifest.artifact(dependency, &context.architecture) {
            Ok(artifact) => match artifact_state(&context.paths.bin_dir(), &artifact) {
                Ok(()) => installed.push(format!("{} {}", artifact.dependency, artifact.version)),
                Err(reason) => failures.push(reason),
            },
            Err(error) => {
                failures.push(firestone_error_reason(&error));
                if hint.is_none() {
                    hint = error.hint().map(str::to_owned);
                }
            }
        }
    }

    if failures.is_empty() {
        DoctorCheck::new(
            DoctorCheckId::VendoredBinaries,
            DoctorStatus::Ok,
            format!("verified {}", installed.join(", ")),
        )
    } else {
        let mut check = DoctorCheck::new(
            DoctorCheckId::VendoredBinaries,
            DoctorStatus::Fail,
            failures.join("; "),
        )
        .with_fix("firestone doctor --fix");
        if let Some(hint) = hint {
            check = check.with_hint(hint);
        }
        check
    }
}

fn check_virtiofsd(context: &DoctorContext) -> DoctorCheck {
    if let Err(error) = context.paths.validate_owned_data_directory(
        &context.paths.bin_dir(),
        "binary directory",
        true,
    ) {
        return unsafe_owned_directory_check(DoctorCheckId::Virtiofsd, &error);
    }
    match context
        .manifest
        .artifact("virtiofsd", &context.architecture)
    {
        Ok(artifact) => match artifact_state(&context.paths.bin_dir(), &artifact) {
            Ok(()) => DoctorCheck::new(
                DoctorCheckId::Virtiofsd,
                DoctorStatus::Ok,
                format!("verified virtiofsd {}", artifact.version),
            ),
            Err(reason) => DoctorCheck::new(DoctorCheckId::Virtiofsd, DoctorStatus::Fail, reason)
                .with_fix("firestone doctor --fix"),
        },
        Err(error) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::Virtiofsd,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            );
            if let Some(hint) = error.hint() {
                check = check.with_hint(hint);
            }
            check
        }
    }
}

/// Checks the local passt grammar required by verify items 14 and 15.
///
/// Version and help output establish the exact M3 command options. They do not
/// resolve runtime interoperability with Cloud Hypervisor; the M3 network test
/// remains the verify-14 gate.
fn check_passt(context: &DoctorContext) -> DoctorCheck {
    let program = match selected_passt(context) {
        Ok(Some((program, _))) => program,
        Ok(None) => return missing_passt_check(context),
        Err(error) => {
            let mut check = DoctorCheck::new(
                DoctorCheckId::Passt,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            );
            if let Some(hint) = error.hint() {
                check = check.with_hint(hint);
            }
            return check;
        }
    };
    let version_output = match Cmd::new(&program)
        .arg("--version")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return DoctorCheck::new(
                DoctorCheckId::Passt,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_hint("repair the passt installation and retry");
        }
    };
    if !version_output.success() {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt --version failed: {}",
                command_failure_reason(&version_output)
            ),
        )
        .with_hint("repair the passt installation and retry");
    }
    let help_output = match Cmd::new(&program)
        .arg("--help")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return DoctorCheck::new(
                DoctorCheckId::Passt,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_hint("repair the passt installation and retry");
        }
    };
    if !help_output.success() {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt --help failed: {}",
                command_failure_reason(&help_output)
            ),
        )
        .with_hint("repair the passt installation and retry");
    }

    let grammar_output = match Cmd::new(&program)
        .args(PASST_PORT_GRAMMAR_ARGS)
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return DoctorCheck::new(
                DoctorCheckId::Passt,
                DoctorStatus::Fail,
                firestone_error_reason(&error),
            )
            .with_hint("repair the passt installation and retry");
        }
    };
    if !grammar_output.success() {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt TCP/UDP grammar probe failed: {}",
                command_failure_reason(&grammar_output)
            ),
        )
        .with_hint(passt_install_hint(context));
    }

    let combined_version = format!(
        "{}\n{}",
        version_output.stdout_lossy(),
        version_output.stderr_lossy()
    );
    let Some(version) = parse_passt_version(&combined_version) else {
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            "passt version output has no date-and-hash release tag",
        )
        .with_hint(passt_install_hint(context));
    };
    let help = format!(
        "{}\n{}",
        help_output.stdout_lossy(),
        help_output.stderr_lossy()
    );
    let missing_flags = REQUIRED_PASST_FLAGS
        .iter()
        .copied()
        .filter(|required| !help.split_whitespace().any(|token| token == *required))
        .collect::<Vec<_>>();
    if version.date < MINIMUM_PASST_DATE || !missing_flags.is_empty() {
        let detail = if missing_flags.is_empty() {
            "release date is too old".to_owned()
        } else {
            format!("missing {}", missing_flags.join(", "))
        };
        return DoctorCheck::new(
            DoctorCheckId::Passt,
            DoctorStatus::Fail,
            format!(
                "passt {} does not meet the {} M3 network minimum: {detail}",
                version.raw,
                crate::PINNED_PASST_VERSION
            ),
        )
        .with_hint(passt_install_hint(context));
    }

    DoctorCheck::new(
        DoctorCheckId::Passt,
        DoctorStatus::Ok,
        format!(
            "passt {} is installed with the pinned M3 command grammar",
            version.raw
        ),
    )
}

fn check_qemu_img(context: &DoctorContext) -> DoctorCheck {
    if let Some(helper) = embedded_helper(InternalHelper::QemuImg) {
        return DoctorCheck::new(
            DoctorCheckId::QemuImg,
            DoctorStatus::Ok,
            format!(
                "embedded qemu-img {} is included with verified release bytes",
                helper.version()
            ),
        );
    }
    check_program(
        context,
        DoctorCheckId::QemuImg,
        "qemu-img",
        Package::QemuImg,
        "qemu-img is available",
    )
}

fn check_program(
    context: &DoctorContext,
    id: DoctorCheckId,
    program: &str,
    package: Package,
    ok_reason: &str,
) -> DoctorCheck {
    let Some(program_path) = find_on_path(program, &context.search_path) else {
        return missing_program_check(context, id, package, program);
    };
    match Cmd::new(program_path)
        .arg("--version")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) if output.success() => DoctorCheck::new(id, DoctorStatus::Ok, ok_reason),
        Ok(output) => DoctorCheck::new(
            id,
            DoctorStatus::Fail,
            format!(
                "{program} --version failed: {}",
                command_failure_reason(&output)
            ),
        )
        .with_hint(format!("repair the {program} installation and retry")),
        Err(error) => DoctorCheck::new(id, DoctorStatus::Fail, firestone_error_reason(&error))
            .with_hint(format!("repair the {program} installation and retry")),
    }
}

fn check_ssh(context: &DoctorContext) -> DoctorCheck {
    let ssh = probe_ssh_tool(context, "ssh", &["-V"], true);
    let keygen = probe_ssh_tool(context, "ssh-keygen", &["-?"], false);
    if ssh.is_ok() && keygen.is_ok() {
        DoctorCheck::new(
            DoctorCheckId::Ssh,
            DoctorStatus::Ok,
            "ssh and ssh-keygen are available",
        )
    } else {
        let reasons = [ssh.err(), keygen.err()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
        let mut check = DoctorCheck::new(DoctorCheckId::Ssh, DoctorStatus::Fail, reasons);
        if let Some(fix) = install_command(context, Package::OpenSsh) {
            check = check.with_fix(fix);
        } else {
            check = check.with_hint("install the OpenSSH client package for this distribution");
        }
        check
    }
}

fn probe_ssh_tool(
    context: &DoctorContext,
    program: &str,
    args: &[&str],
    require_success: bool,
) -> Result<(), String> {
    let path = find_on_path(program, &context.search_path)
        .ok_or_else(|| format!("{program} not found on PATH"))?;
    let output = Cmd::new(path)
        .args(args)
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .output()
        .map_err(|error| firestone_error_reason(&error))?;
    if require_success && !output.success() {
        return Err(format!(
            "{program} probe failed: {}",
            command_failure_reason(&output)
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NumericFact {
    Value(u64),
    Missing,
    Unreadable,
    Invalid,
}

impl NumericFact {
    fn detail(&self, name: &str, optional: bool) -> String {
        match self {
            Self::Value(value) => format!("{name}={value}"),
            Self::Missing if optional => format!("{name}=not exposed"),
            Self::Missing => format!("{name}=missing"),
            Self::Unreadable => format!("{name}=unreadable"),
            Self::Invalid => format!("{name}=invalid"),
        }
    }

    fn is_zero(&self) -> bool {
        matches!(self, Self::Value(0))
    }

    fn is_one(&self) -> bool {
        matches!(self, Self::Value(1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppArmorState {
    Enabled,
    Disabled,
    Missing,
    Unreadable,
    Invalid,
}

impl AppArmorState {
    fn detail(&self) -> &'static str {
        match self {
            Self::Enabled => "apparmor=enabled",
            Self::Disabled => "apparmor=disabled",
            Self::Missing => "apparmor=not exposed",
            Self::Unreadable => "apparmor=unreadable",
            Self::Invalid => "apparmor=invalid",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelfPolicyState {
    seccomp: Option<u64>,
    no_new_privs: Option<u64>,
    container: String,
}

impl SelfPolicyState {
    fn detail(&self) -> String {
        format!(
            "self_seccomp={}; self_no_new_privs={}; container={}",
            self.seccomp
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            self.no_new_privs
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            self.container
        )
    }

    fn has_restricting_context(&self) -> bool {
        self.seccomp.is_some_and(|value| value > 0) || self.container != "not detected"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PasstUsernsProbe {
    Available(String),
    Denied { pid: Option<u32>, stderr: String },
    Inconclusive(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditEvidence {
    Correlated(PathBuf),
    ReadableWithoutMatch,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserNamespaceDiagnosis {
    status: DoctorStatus,
    reason: String,
    apparmor_remediation: bool,
    hint: String,
}

/// Returns the versioned root copy only after its bytes, profile, ownership,
/// protected parent directories, and loaded policy have all been verified.
pub fn resolve_verified_apparmor_passt(
    paths: &Paths,
    artifact: &DependencyArtifact,
) -> Result<Option<VerifiedPasst>, FirestoneError> {
    resolve_verified_apparmor_passt_at(
        &paths.apparmor_passt_executable(),
        &paths.apparmor_passt_profile(),
        &paths.apparmor_loaded_profiles(),
        artifact,
        0,
    )
}

fn resolve_verified_apparmor_passt_at(
    executable: &Path,
    profile: &Path,
    loaded_profiles: &Path,
    artifact: &DependencyArtifact,
    expected_root_uid: u32,
) -> Result<Option<VerifiedPasst>, FirestoneError> {
    if artifact.dependency != "passt" || artifact.version != crate::PINNED_PASST_VERSION {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            "cannot verify an AppArmor passt copy against a different dependency identity",
        )
        .with_hint("resolve the exact embedded x86_64 passt artifact first"));
    }

    let executable_metadata = match fs::symlink_metadata(executable) {
        Ok(metadata) => Some(metadata),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "cannot inspect AppArmor passt target {}",
                    executable.display()
                ),
            )
            .with_source(source));
        }
    };
    let profile_metadata = match fs::symlink_metadata(profile) {
        Ok(metadata) => Some(metadata),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "cannot inspect AppArmor passt profile {}",
                    profile.display()
                ),
            )
            .with_source(source));
        }
    };
    let (executable_metadata, profile_metadata) = match (executable_metadata, profile_metadata) {
        (None, None) => return Ok(None),
        (Some(executable_metadata), Some(profile_metadata)) => {
            (executable_metadata, profile_metadata)
        }
        _ => {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                "the AppArmor passt installation is incomplete",
            )
            .with_hint("rerun firestone doctor --fix from an interactive terminal"));
        }
    };
    verify_root_owned_file(
        executable,
        &executable_metadata,
        expected_root_uid,
        true,
        "AppArmor passt executable",
    )?;
    verify_root_owned_file(
        profile,
        &profile_metadata,
        expected_root_uid,
        false,
        "AppArmor passt profile",
    )?;
    verify_root_owned_parent(executable, expected_root_uid)?;
    verify_root_owned_parent(profile, expected_root_uid)?;

    let actual_hash = hash_file(executable, "AppArmor passt executable")?;
    if actual_hash != artifact.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "AppArmor passt target {} has SHA-256 {actual_hash}, expected {}",
                executable.display(),
                artifact.sha256
            ),
        )
        .with_hint("do not run the mismatched root copy; reinstall the exact embedded bytes"));
    }
    let profile_bytes = fs::read(profile).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot read AppArmor passt profile {}", profile.display()),
        )
        .with_source(source)
    })?;
    if profile_bytes != APPARMOR_PASST_PROFILE_CONTENT.as_bytes() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "AppArmor passt profile {} has unexpected content",
                profile.display()
            ),
        )
        .with_hint("replace it with the literal Firestone profile through doctor --fix"));
    }
    let loaded = fs::read_to_string(loaded_profiles).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot confirm loaded AppArmor profiles through {}",
                loaded_profiles.display()
            ),
        )
        .with_source(source)
    })?;
    let profile_loaded = loaded.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(APPARMOR_PASST_PROFILE_NAME)
            && fields.any(|field| field == "(unconfined)")
    });
    if !profile_loaded {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("AppArmor profile {APPARMOR_PASST_PROFILE_NAME} is not loaded"),
        )
        .with_hint(format!("sudo apparmor_parser -r {}", profile.display())));
    }

    Ok(Some(VerifiedPasst {
        executable: executable.to_path_buf(),
        profile_name: APPARMOR_PASST_PROFILE_NAME,
    }))
}

fn verify_root_owned_file(
    path: &Path,
    metadata: &fs::Metadata,
    expected_root_uid: u32,
    executable: bool,
    label: &str,
) -> Result<(), FirestoneError> {
    let mode = metadata.permissions().mode() & 0o7777;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != expected_root_uid
        || mode & 0o022 != 0
        || (executable && mode & 0o111 == 0)
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{label} {} is not a root-owned, non-writable regular file with the required execute mode (uid {}, mode {mode:04o})",
                path.display(),
                metadata.uid()
            ),
        )
        .with_hint("remove the unsafe installation and rerun firestone doctor --fix"));
    }
    Ok(())
}

fn verify_root_owned_parent(path: &Path, expected_root_uid: u32) -> Result<(), FirestoneError> {
    let Some(parent) = path.parent() else {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "AppArmor attachment {} has no parent directory",
                path.display()
            ),
        ));
    };
    let metadata = fs::symlink_metadata(parent).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot inspect protected parent {}", parent.display()),
        )
        .with_source(source)
    })?;
    let mode = metadata.permissions().mode() & 0o7777;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != expected_root_uid
        || mode & 0o022 != 0
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "AppArmor attachment parent {} is not a root-owned, non-writable directory (uid {}, mode {mode:04o})",
                parent.display(),
                metadata.uid()
            ),
        )
        .with_hint("use the literal protected /usr/libexec/firestone installation path"));
    }
    Ok(())
}

fn hash_file(path: &Path, label: &str) -> Result<String, FirestoneError> {
    let mut file = File::open(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot open {label} {} for hashing", path.display()),
        )
        .with_source(source)
    })?;
    sha256_reader(&mut file).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("cannot hash {label} {}", path.display()),
        )
        .with_source(source)
    })
}

fn read_numeric_fact(path: &Path) -> NumericFact {
    match fs::read_to_string(path) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .map_or(NumericFact::Invalid, NumericFact::Value),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => NumericFact::Missing,
        Err(_) => NumericFact::Unreadable,
    }
}

fn read_apparmor_state(path: &Path) -> AppArmorState {
    match fs::read_to_string(path) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" | "1" => AppArmorState::Enabled,
            "n" | "no" | "0" => AppArmorState::Disabled,
            _ => AppArmorState::Invalid,
        },
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => AppArmorState::Missing,
        Err(_) => AppArmorState::Unreadable,
    }
}

fn read_self_policy(context: &DoctorContext) -> SelfPolicyState {
    let status = fs::read_to_string(&context.self_status).ok();
    let status_value = |name: &str| {
        status.as_deref().and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
    };
    SelfPolicyState {
        seccomp: status_value("Seccomp:"),
        no_new_privs: status_value("NoNewPrivs:"),
        container: detect_container(context),
    }
}

fn detect_container(context: &DoctorContext) -> String {
    for marker in &context.container_markers {
        match fs::read_to_string(marker) {
            Ok(value) if !value.trim().is_empty() => return value.trim().to_owned(),
            Ok(_) | Err(_)
                if marker.file_name() == Some(OsStr::new(".dockerenv")) && marker.exists() =>
            {
                return "docker".to_owned();
            }
            Ok(_) | Err(_)
                if marker.file_name() == Some(OsStr::new(".containerenv")) && marker.exists() =>
            {
                return "podman".to_owned();
            }
            Ok(_) | Err(_) => {}
        }
    }
    let Ok(cgroup) = fs::read_to_string(&context.self_cgroup) else {
        return "unknown".to_owned();
    };
    let lower = cgroup.to_ascii_lowercase();
    for (needle, label) in [
        ("kubepods", "kubernetes"),
        ("docker", "docker"),
        ("containerd", "containerd"),
        ("libpod", "podman"),
        ("lxc", "lxc"),
    ] {
        if lower.contains(needle) {
            return label.to_owned();
        }
    }
    "not detected".to_owned()
}
fn selected_passt(context: &DoctorContext) -> Result<Option<(PathBuf, bool)>, FirestoneError> {
    if let Ok(artifact) = context.manifest.embedded_passt(&context.architecture) {
        if let Some(verified) = resolve_verified_apparmor_passt_at(
            &context.apparmor_passt_executable,
            &context.apparmor_passt_profile,
            &context.apparmor_loaded_profiles,
            &artifact,
            context.expected_root_uid,
        )? {
            return Ok(Some((verified.executable, true)));
        }
        let installed = context.paths.bin_dir().join(&artifact.install_name);
        if artifact_state(&context.paths.bin_dir(), &artifact).is_ok() {
            return Ok(Some((installed, true)));
        }
    }
    let Some(program) = find_on_path("passt", &context.search_path) else {
        return Ok(None);
    };
    let exact = Cmd::new(&program)
        .arg("--version")
        .stdin_null()
        .timeout(DOCTOR_PROBE_TIMEOUT)
        .capture_limit(PASST_USERNS_STDERR_BYTES)
        .error_kind(ErrorKind::Dependency)
        .output()
        .ok()
        .and_then(|output| {
            parse_passt_version(&format!(
                "{}\n{}",
                output.stdout_lossy(),
                output.stderr_lossy()
            ))
        })
        .is_some_and(|version| version.raw == crate::PINNED_PASST_VERSION);
    Ok(Some((program, exact)))
}

fn passt_userns_probe(context: &DoctorContext) -> PasstUsernsProbe {
    let (program, exact_pinned) = match selected_passt(context) {
        Ok(Some(selected)) => selected,
        Ok(None) => {
            return PasstUsernsProbe::Inconclusive(
                "passt is unavailable, so its mandatory userns stage was not tested".to_owned(),
            );
        }
        Err(error) => {
            return PasstUsernsProbe::Inconclusive(firestone_error_reason(&error));
        }
    };
    let probe_dir = match TempBuilder::new()
        .prefix(".firestone-passt-userns-")
        .tempdir_in(context.paths.runtime_dir())
    {
        Ok(directory) => directory,
        Err(source) => {
            return PasstUsernsProbe::Inconclusive(format!(
                "cannot create the bounded passt probe directory: {source}"
            ));
        }
    };
    let socket = probe_dir.path().join("net.sock");
    let output = Cmd::new(program)
        .args(["--foreground", "--one-off", "--socket-path"])
        .arg(socket.as_os_str())
        .args(["--tcp-ports", "none", "--udp-ports", "none"])
        .stdin_null()
        .timeout(PASST_USERNS_PROBE_TIMEOUT)
        .capture_limit(PASST_USERNS_STDERR_BYTES)
        .error_kind(ErrorKind::Dependency)
        .output();
    match output {
        Ok(output) => {
            let stderr = bounded_stderr(&output);
            if stderr.contains(PASST_USERNS_FAILURE) {
                PasstUsernsProbe::Denied {
                    pid: Some(output.pid()),
                    stderr,
                }
            } else if stderr.contains(PASST_LATER_ISOLATION_FAILURE) {
                PasstUsernsProbe::Available(format!(
                    "passt pid {} reached the later namespace-detach stage after userns creation",
                    output.pid()
                ))
            } else {
                PasstUsernsProbe::Inconclusive(format!(
                    "passt exited before a recognized userns stage: {stderr}"
                ))
            }
        }
        Err(error) if error.kind() == ErrorKind::Timeout => {
            let detail = error.message();
            if detail.contains(PASST_USERNS_FAILURE) {
                PasstUsernsProbe::Denied {
                    pid: None,
                    stderr: bounded_text(detail),
                }
            } else if detail.contains(PASST_LATER_ISOLATION_FAILURE) {
                PasstUsernsProbe::Available(
                    "passt reached the later namespace-detach stage after userns creation"
                        .to_owned(),
                )
            } else if exact_pinned {
                PasstUsernsProbe::Available(format!(
                    "pinned passt remained active for {} ms after its synchronous userns stage",
                    PASST_USERNS_PROBE_TIMEOUT.as_millis()
                ))
            } else {
                PasstUsernsProbe::Inconclusive(
                    "a non-pinned passt stayed active, but its startup ordering is not authoritative"
                        .to_owned(),
                )
            }
        }
        Err(error) => PasstUsernsProbe::Inconclusive(format!(
            "cannot run the bounded passt userns probe: {}",
            firestone_error_reason(&error)
        )),
    }
}

fn bounded_stderr(output: &crate::CmdOutput) -> String {
    let mut stderr = bounded_text(&output.stderr_lossy());
    if output.stderr_truncated() {
        stderr.push_str(" [stderr truncated at 16384 bytes]");
    }
    stderr
}

fn bounded_text(value: &str) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        "<empty>".to_owned()
    } else {
        text
    }
}

fn correlated_audit_evidence(
    context: &DoctorContext,
    pid: Option<u32>,
    started: u64,
) -> AuditEvidence {
    let Some(pid) = pid else {
        return AuditEvidence::Unreadable;
    };
    let mut readable = false;
    for path in &context.audit_logs {
        let Ok(contents) = read_tail_text(path, AUDIT_TAIL_BYTES) else {
            continue;
        };
        readable = true;
        if contents.lines().any(|line| {
            line.contains(r#"apparmor="DENIED""#)
                && line.contains(r#"operation="userns_create""#)
                && audit_has_pid(line, pid)
                && audit_is_recent(line, started)
        }) {
            return AuditEvidence::Correlated(path.clone());
        }
    }
    if readable {
        AuditEvidence::ReadableWithoutMatch
    } else {
        AuditEvidence::Unreadable
    }
}

fn audit_has_pid(line: &str, pid: u32) -> bool {
    let field = format!("pid={pid}");
    line.match_indices(&field).any(|(index, _)| {
        line.as_bytes()
            .get(index + field.len())
            .is_none_or(|byte| byte.is_ascii_whitespace())
    })
}

fn audit_is_recent(line: &str, started: u64) -> bool {
    let Some(after) = line.split("audit(").nth(1) else {
        return true;
    };
    let Some(seconds) = after.split(['.', ':']).next() else {
        return true;
    };
    seconds
        .parse::<u64>()
        .map_or(true, |seconds| seconds >= started.saturating_sub(2))
}

fn read_tail_text(path: &Path, maximum: u64) -> Result<String, std::io::Error> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > maximum {
        file.seek(SeekFrom::Start(length - maximum))?;
    }
    let mut bytes = Vec::with_capacity(length.min(maximum) as usize);
    let mut limited = file.take(maximum);
    limited.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn diagnose_user_namespaces(context: &DoctorContext) -> UserNamespaceDiagnosis {
    let max = read_numeric_fact(&context.user_namespace_sysctl);
    let clone = read_numeric_fact(&context.unprivileged_userns_clone_sysctl);
    let apparmor = read_apparmor_state(&context.apparmor_enabled);
    let restrict = read_numeric_fact(&context.apparmor_restrict_userns);
    let self_policy = read_self_policy(context);
    let probe_started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let probe = passt_userns_probe(context);
    let facts = format!(
        "{}; {}; {}; {}; {}",
        max.detail("max_user_namespaces", false),
        clone.detail("unprivileged_userns_clone", true),
        apparmor.detail(),
        restrict.detail("restrict_unprivileged_userns", true),
        self_policy.detail()
    );
    let qemu_note = "qemu-img does not require user namespaces";

    if max.is_zero() {
        return UserNamespaceDiagnosis {
            status: DoctorStatus::Fail,
            reason: format!(
                "confirmed userns denial: max_user_namespaces=0; {facts}; {qemu_note}"
            ),
            apparmor_remediation: false,
            hint: "Firestone never changes namespace sysctls; ask the host administrator to review user.max_user_namespaces before running passt".to_owned(),
        };
    }
    if clone.is_zero() {
        return UserNamespaceDiagnosis {
            status: DoctorStatus::Fail,
            reason: format!(
                "confirmed userns denial: unprivileged_userns_clone=0; {facts}; {qemu_note}"
            ),
            apparmor_remediation: false,
            hint: "Firestone never changes namespace sysctls; ask the host administrator to review kernel.unprivileged_userns_clone before running passt".to_owned(),
        };
    }

    match probe {
        PasstUsernsProbe::Available(evidence) => UserNamespaceDiagnosis {
            status: DoctorStatus::Ok,
            reason: format!(
                "passt userns creation confirmed: {evidence}; {facts}; {qemu_note}"
            ),
            apparmor_remediation: false,
            hint: "passt can use its mandatory user namespace stage".to_owned(),
        },
        PasstUsernsProbe::Denied { pid, stderr } => {
            let audit = correlated_audit_evidence(context, pid, probe_started);
            let apparmor_consistent = apparmor == AppArmorState::Enabled && restrict.is_one();
            match audit {
                AuditEvidence::Correlated(path) => UserNamespaceDiagnosis {
                    status: DoctorStatus::Fail,
                    reason: format!(
                        "confirmed AppArmor userns denial: correlated passt pid audit record in {}; passt stderr: {stderr}; {facts}; {qemu_note}",
                        path.display()
                    ),
                    apparmor_remediation: true,
                    hint: format!(
                        "doctor --fix can install exact embedded bytes at {APPARMOR_PASST_EXECUTABLE} and attach only {APPARMOR_PASST_PROFILE_NAME}; it never changes sysctls"
                    ),
                },
                AuditEvidence::ReadableWithoutMatch | AuditEvidence::Unreadable
                    if apparmor_consistent =>
                {
                    let audit_detail = if matches!(audit, AuditEvidence::Unreadable) {
                        "audit evidence was not readable"
                    } else {
                        "readable audit evidence had no correlated denial"
                    };
                    UserNamespaceDiagnosis {
                        status: DoctorStatus::Fail,
                        reason: format!(
                            "passt userns denial is consistent with AppArmor but not confirmed: {audit_detail}; passt stderr: {stderr}; {facts}; {qemu_note}"
                        ),
                        apparmor_remediation: true,
                        hint: format!(
                            "doctor --fix can install a literal profile for {APPARMOR_PASST_EXECUTABLE}; it never grants userns to a user-writable wildcard or changes sysctls"
                        ),
                    }
                }
                AuditEvidence::ReadableWithoutMatch | AuditEvidence::Unreadable
                    if self_policy.has_restricting_context() =>
                {
                    UserNamespaceDiagnosis {
                        status: DoctorStatus::Fail,
                        reason: format!(
                            "passt confirmed userns creation failed; container or seccomp policy is consistent with the denial, but the cause is not confirmed; passt stderr: {stderr}; {facts}; {qemu_note}"
                        ),
                        apparmor_remediation: false,
                        hint: "inspect the outer container seccomp and namespace policy; Firestone does not weaken it or change sysctls".to_owned(),
                    }
                }
                AuditEvidence::ReadableWithoutMatch | AuditEvidence::Unreadable => {
                    UserNamespaceDiagnosis {
                        status: DoctorStatus::Fail,
                        reason: format!(
                            "passt confirmed userns creation failed, but the denying policy is not confirmed; passt stderr: {stderr}; {facts}; {qemu_note}"
                        ),
                        apparmor_remediation: false,
                        hint: "inspect host userns policy and readable audit logs; a generic unshare result is not authoritative for passt".to_owned(),
                    }
                }
            }
        }
        PasstUsernsProbe::Inconclusive(detail) => UserNamespaceDiagnosis {
            status: DoctorStatus::Warn,
            reason: format!(
                "passt userns result is inconclusive: {detail}; {facts}; {qemu_note}"
            ),
            apparmor_remediation: false,
            hint: "repair or install the pinned passt helper and retry; generic unshare alone is not proof".to_owned(),
        },
    }
}

/// Checks passt's mandatory first user namespace stage and reports policy facts.
fn check_user_namespaces(context: &DoctorContext) -> DoctorCheck {
    let diagnosis = diagnose_user_namespaces(context);
    let mut check = DoctorCheck::new(
        DoctorCheckId::UserNamespaces,
        diagnosis.status,
        diagnosis.reason,
    )
    .with_hint(diagnosis.hint);
    if diagnosis.apparmor_remediation {
        check = check.with_fix("firestone doctor --fix");
    }
    check
}

fn check_ssh_key(context: &DoctorContext) -> DoctorCheck {
    if let Err(error) =
        context
            .paths
            .validate_owned_data_directory(&context.paths.ssh_dir(), "SSH directory", true)
    {
        return unsafe_owned_directory_check(DoctorCheckId::SshKey, &error);
    }
    let private_path = context.paths.ssh_private_key();
    let public_path = context.paths.ssh_public_key();
    let private = fs::symlink_metadata(&private_path);
    let public = fs::symlink_metadata(&public_path);
    match (private, public) {
        (Ok(private), Ok(public))
            if private.is_file()
                && !private.file_type().is_symlink()
                && public.is_file()
                && !public.file_type().is_symlink() =>
        {
            let private_mode = private.permissions().mode() & 0o7777;
            let public_mode = public.permissions().mode() & 0o7777;
            if private.uid() == context.paths.uid()
                && public.uid() == context.paths.uid()
                && private_mode == 0o600
                && public_mode == 0o644
            {
                DoctorCheck::new(
                    DoctorCheckId::SshKey,
                    DoctorStatus::Ok,
                    format!(
                        "Firestone SSH key is present at {} with private mode 0600 and public mode 0644",
                        private_path.display()
                    ),
                )
            } else {
                DoctorCheck::new(
                    DoctorCheckId::SshKey,
                    DoctorStatus::Fail,
                    format!(
                        "Firestone SSH key pair is insecure: private uid {} mode {private_mode:04o}; public uid {} mode {public_mode:04o}",
                        private.uid(),
                        public.uid()
                    ),
                )
                .with_hint(
                    "use current-user regular files with private mode 0600 and public mode 0644",
                )
            }
        }
        (Err(private_error), Err(public_error))
            if private_error.kind() == std::io::ErrorKind::NotFound
                && public_error.kind() == std::io::ErrorKind::NotFound =>
        {
            DoctorCheck::new(
                DoctorCheckId::SshKey,
                DoctorStatus::Fail,
                "Firestone SSH key is missing",
            )
            .with_fix("firestone doctor --fix")
        }
        _ => DoctorCheck::new(
            DoctorCheckId::SshKey,
            DoctorStatus::Fail,
            "Firestone SSH key pair is incomplete, symlinked, or unreadable",
        )
        .with_hint(format!(
            "move the incomplete key pair out of {} and run `firestone doctor --fix`",
            private_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .display()
        )),
    }
}

fn check_data_space(context: &DoctorContext) -> DoctorCheck {
    let data_dir = context.paths.data_dir();
    if let Err(error) =
        context
            .paths
            .validate_owned_data_directory(data_dir, "data directory", true)
    {
        let check = DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Fail,
            error.message().to_owned(),
        );
        return match error.hint() {
            Some(hint) => check.with_hint(hint.to_owned()),
            None => check,
        };
    }
    let Some(path) = nearest_existing_ancestor(data_dir) else {
        return DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Warn,
            format!(
                "cannot find an existing filesystem for {}",
                data_dir.display()
            ),
        );
    };
    match fs2::available_space(path) {
        Ok(bytes) if bytes >= context.minimum_data_free_bytes => DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Ok,
            format!(
                "data filesystem has {} bytes free (minimum {})",
                bytes, context.minimum_data_free_bytes
            ),
        ),
        Ok(bytes) => DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Warn,
            format!(
                "data filesystem has {} bytes free; {} bytes are required",
                bytes, context.minimum_data_free_bytes
            ),
        )
        .with_hint("free space on the Firestone data filesystem before pulling an image"),
        Err(source) => DoctorCheck::new(
            DoctorCheckId::DataSpace,
            DoctorStatus::Warn,
            format!("cannot measure free space for {}: {source}", path.display()),
        ),
    }
}

fn check_stale_state(stale_state: &StaleStateReport) -> DoctorCheck {
    if !stale_state.failures.is_empty() {
        let failures = stale_state
            .failures
            .iter()
            .map(|failure| format!("{} ({})", failure.machine, failure.reason))
            .collect::<Vec<_>>()
            .join(", ");
        let observations = stale_state
            .observations
            .iter()
            .map(|observation| format!("{} ({})", observation.machine, observation.reason))
            .collect::<Vec<_>>()
            .join(", ");
        let reason = if observations.is_empty() {
            format!("could not reconcile machine states: {failures}")
        } else {
            format!(
                "could not reconcile machine states: {failures}; reconciled stale machine states: {observations}"
            )
        };
        return DoctorCheck::new(DoctorCheckId::StaleState, DoctorStatus::Fail, reason)
            .with_hint("repair the named machine state or lock error and rerun doctor");
    }
    if stale_state.observations.is_empty() {
        return DoctorCheck::new(
            DoctorCheckId::StaleState,
            DoctorStatus::Ok,
            "no stale machine states were observed",
        );
    }
    let details = stale_state
        .observations
        .iter()
        .map(|observation| format!("{} ({})", observation.machine, observation.reason))
        .collect::<Vec<_>>()
        .join(", ");
    DoctorCheck::new(
        DoctorCheckId::StaleState,
        DoctorStatus::Ok,
        format!("reconciled stale machine states: {details}"),
    )
}

fn reconcile_machine_states(context: &DoctorContext, vmm: &dyn VmmPingProbe) -> StaleStateReport {
    let mut report = StaleStateReport::default();
    let machines_dir = context.paths.machines_dir();
    let storage_validation = context
        .paths
        .validate_owned_data_directory(context.paths.data_dir(), "data directory", true)
        .and_then(|()| {
            context
                .paths
                .validate_owned_data_directory(&machines_dir, "machines directory", true)
        });
    if let Err(error) = storage_validation {
        report.failures.push(StaleStateFailure {
            machine: "<machines>".to_owned(),
            reason: error.message().to_owned(),
        });
        return report;
    }
    let entries = match fs::read_dir(machines_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return report,
        Err(source) => {
            report.failures.push(StaleStateFailure {
                machine: "<machines>".to_owned(),
                reason: format!("cannot enumerate machine directories: {source}"),
            });
            return report;
        }
    };

    let mut machine_entries = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => machine_entries.push((entry.file_name(), entry)),
            Err(source) => report.failures.push(StaleStateFailure {
                machine: "<machines>".to_owned(),
                reason: format!("cannot read machine directory entry: {source}"),
            }),
        }
    }
    machine_entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (name_os, entry) in machine_entries {
        let display_name = name_os.to_string_lossy().into_owned();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(source) => {
                report.failures.push(StaleStateFailure {
                    machine: display_name,
                    reason: format!("cannot inspect machine directory type: {source}"),
                });
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = name_os.to_str() else {
            report.failures.push(StaleStateFailure {
                machine: display_name,
                reason: "machine directory name is not valid UTF-8".to_owned(),
            });
            continue;
        };
        let machine_dir = match context.paths.machine_dir(name) {
            Ok(machine_dir) => machine_dir,
            Err(error) => {
                report.failures.push(StaleStateFailure {
                    machine: name.to_owned(),
                    reason: firestone_error_reason(&error),
                });
                continue;
            }
        };
        if let Err(error) =
            context
                .paths
                .validate_owned_data_directory(&machine_dir, "machine directory", false)
        {
            report.failures.push(StaleStateFailure {
                machine: name.to_owned(),
                reason: firestone_error_reason(&error),
            });
            continue;
        }
        match reconcile_machine_state(context, name, vmm) {
            Ok(Some(observation)) => report.observations.push(observation),
            Ok(None) => {}
            Err(error) => report.failures.push(StaleStateFailure {
                machine: name.to_owned(),
                reason: firestone_error_reason(&error),
            }),
        }
    }
    report.observations.sort_by(|left, right| {
        left.machine
            .cmp(&right.machine)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    report.failures.sort_by(|left, right| {
        left.machine
            .cmp(&right.machine)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    report
}

/// Reads one machine through live process and VMM observations.
///
/// The common read-only path does not take the machine lock. A stale active
/// state is re-read and persisted only after acquiring the lock.
pub fn read_reconciled_machine_state_live(
    paths: &Paths,
    name: &str,
    reconciled_at: &str,
) -> Result<crate::LiveMachineState, FirestoneError> {
    let (live, _) = read_reconciled_machine_state_with(
        paths,
        name,
        Path::new("/proc"),
        reconciled_at,
        &VMM_PING_PROBE,
        None,
    )?;
    Ok(live)
}

/// Reads and, when needed, reconciles one machine while its lock is already held.
pub fn read_reconciled_machine_state_live_locked(
    paths: &Paths,
    name: &str,
    reconciled_at: &str,
    lock: &MachineLock,
) -> Result<crate::LiveMachineState, FirestoneError> {
    let (live, _) = read_reconciled_machine_state_with(
        paths,
        name,
        Path::new("/proc"),
        reconciled_at,
        &VMM_PING_PROBE,
        Some(lock),
    )?;
    Ok(live)
}

fn reconcile_machine_state(
    context: &DoctorContext,
    name: &str,
    vmm: &dyn VmmPingProbe,
) -> Result<Option<StaleStateObservation>, FirestoneError> {
    let (_, reason) = read_reconciled_machine_state_with(
        &context.paths,
        name,
        &context.proc_root,
        &context.reconciled_at,
        vmm,
        None,
    )?;
    Ok(reason.map(|reason| StaleStateObservation {
        machine: name.to_owned(),
        reason,
    }))
}

fn read_reconciled_machine_state_with(
    paths: &Paths,
    name: &str,
    proc_root: &Path,
    reconciled_at: &str,
    vmm: &dyn VmmPingProbe,
    held_lock: Option<&MachineLock>,
) -> Result<(crate::LiveMachineState, Option<String>), FirestoneError> {
    let state_store = StateStore::new(paths.machine_state(name)?);
    let runtime_dir = paths.machine_runtime_dir(name)?;
    let api_socket = paths.machine_api_socket(name)?;
    let mut state = state_store.read()?;
    let observation = observe_liveness(name, &state, &runtime_dir, &api_socket, proc_root, vmm)?;
    let mut reconciliation = reconcile(state.status, observation);

    if reconciliation.rewrite.is_none() {
        state.status = reconciliation.status;
        return Ok((
            crate::LiveMachineState {
                state,
                supervision: reconciliation.supervision,
            },
            None,
        ));
    }

    let acquired_lock;
    let lock = match held_lock {
        Some(lock) => lock,
        None => {
            let lock_path = paths.machine_lock(name)?;
            let mut events = |_event: crate::Event| Ok(());
            acquired_lock = MachineLock::acquire(name, &lock_path, &mut events)?;
            &acquired_lock
        }
    };
    // The shim may have completed its final atomic write while this reader was
    // waiting. Re-read and re-observe under the lock before deciding to write.
    state = state_store.read()?;
    let observation = observe_liveness(name, &state, &runtime_dir, &api_socket, proc_root, vmm)?;
    reconciliation = reconcile(state.status, observation);

    let reason = reconciliation
        .rewrite
        .as_ref()
        .map(|ReconcileRewrite::Stopped { reason }| reason.as_str().to_owned());
    if let Some(effective) = reconciled_state(&state, &reconciliation, reconciled_at) {
        state_store.write_reconciliation(&state, &reconciliation, reconciled_at, lock)?;
        return Ok((
            crate::LiveMachineState {
                state: effective,
                supervision: reconciliation.supervision,
            },
            reason,
        ));
    }

    state.status = reconciliation.status;
    Ok((
        crate::LiveMachineState {
            state,
            supervision: reconciliation.supervision,
        },
        None,
    ))
}

fn perform_fixes(
    context: &DoctorContext,
    fetcher: &dyn ArtifactFetcher,
    elevation_confirmed: bool,
) -> BTreeMap<DoctorCheckId, FixFailure> {
    let mut failures = BTreeMap::new();

    let data_ready = record_directory_fix(
        &context.paths,
        &mut failures,
        context.paths.data_dir(),
        "data directory",
        DoctorCheckId::DataSpace,
    );
    let bin_dir = context.paths.bin_dir();
    let bin_ready = data_ready
        && record_directory_fix(
            &context.paths,
            &mut failures,
            &bin_dir,
            "binary directory",
            DoctorCheckId::VendoredBinaries,
        );
    if let Err(error) = context.paths.ensure_runtime_dir() {
        record_fix_failure(&mut failures, DoctorCheckId::RuntimeDir, error);
    }
    let ssh_private_key = context.paths.ssh_private_key();
    let ssh_public_key = context.paths.ssh_public_key();
    let key_ready = if data_ready {
        ssh_private_key.parent().is_some_and(|key_dir| {
            record_directory_fix(
                &context.paths,
                &mut failures,
                key_dir,
                "SSH key directory",
                DoctorCheckId::SshKey,
            )
        })
    } else {
        false
    };

    if bin_ready {
        for dependency in VENDORED_DEPENDENCIES {
            fix_artifact(
                context,
                dependency,
                DoctorCheckId::VendoredBinaries,
                fetcher,
                &mut failures,
            );
        }
        fix_artifact(
            context,
            "virtiofsd",
            DoctorCheckId::Virtiofsd,
            fetcher,
            &mut failures,
        );
    }

    if key_ready && path_is_missing(&ssh_private_key) && path_is_missing(&ssh_public_key) {
        if let Err(error) = generate_ssh_key(context) {
            record_fix_failure(&mut failures, DoctorCheckId::SshKey, error);
        }
    }

    let userns = diagnose_user_namespaces(context);
    if userns.apparmor_remediation {
        if let Err(failure) = fix_apparmor_passt(context, elevation_confirmed) {
            record_fix_failure_value(&mut failures, DoctorCheckId::UserNamespaces, failure);
        }
    }
    failures
}

#[derive(Debug, Clone)]
enum ElevationProvider {
    Direct,
    Sudo(PathBuf),
    Pkexec(PathBuf),
}

fn fix_apparmor_passt(
    context: &DoctorContext,
    elevation_confirmed: bool,
) -> Result<(), FixFailure> {
    if context.operating_system != "linux" || context.architecture != "x86_64" {
        return Err(FixFailure {
            reason: "AppArmor passt repair is available only on Linux x86_64".to_owned(),
            fix: None,
            hint: Some("use the supported Linux x86_64 runtime".to_owned()),
        });
    }
    let helper = context.extracted_passt.as_ref().ok_or_else(|| FixFailure {
        reason: "embedded passt helper extraction has not provided installable bytes".to_owned(),
        fix: None,
        hint: Some(
            "extract DependencyManifest::embedded_passt through the helper runtime and supply ExtractedPasstHelper before retrying"
                .to_owned(),
        ),
    })?;
    let manifest_artifact = context
        .manifest
        .embedded_passt(&context.architecture)
        .map_err(FixFailure::from)?;
    if helper.artifact != manifest_artifact {
        return Err(FixFailure {
            reason: "extracted passt bytes do not match the bundled dependency manifest identity"
                .to_owned(),
            fix: None,
            hint: Some(
                "discard the extraction and materialize the bundled passt payload again".to_owned(),
            ),
        });
    }
    verify_extracted_passt(context, helper).map_err(FixFailure::from)?;
    stage_apparmor_profile(context).map_err(FixFailure::from)?;
    let manual = manual_apparmor_commands(context, helper.path()).map_err(FixFailure::from)?;
    if !elevation_confirmed {
        return Err(FixFailure {
            reason: "AppArmor repair elevation was not explicitly confirmed in an interactive TTY"
                .to_owned(),
            fix: Some(manual),
            hint: Some(
                "run the exact commands manually, or rerun firestone doctor --fix in an interactive TTY; no sysctl will be changed"
                    .to_owned(),
            ),
        });
    }

    let install = find_on_path("install", &context.search_path).ok_or_else(|| FixFailure {
        reason: "install is unavailable for the AppArmor repair".to_owned(),
        fix: Some(manual.clone()),
        hint: Some("install GNU coreutils and retry from an interactive TTY".to_owned()),
    })?;
    let parser =
        find_on_path("apparmor_parser", &context.search_path).ok_or_else(|| FixFailure {
            reason: "apparmor_parser is unavailable for the AppArmor repair".to_owned(),
            fix: Some(manual.clone()),
            hint: Some(
                "install the AppArmor userspace tools and retry from an interactive TTY".to_owned(),
            ),
        })?;
    let elevation = select_elevation(context).ok_or_else(|| FixFailure {
        reason: "sudo and pkexec are unavailable for the confirmed AppArmor repair".to_owned(),
        fix: Some(manual.clone()),
        hint: Some("run the exact root commands manually".to_owned()),
    })?;
    let executable_parent =
        context
            .apparmor_passt_executable
            .parent()
            .ok_or_else(|| FixFailure {
                reason: "the AppArmor passt target has no parent directory".to_owned(),
                fix: Some(manual.clone()),
                hint: None,
            })?;

    run_elevated(
        &elevation,
        &install,
        &[
            OsString::from("-d"),
            OsString::from("-o"),
            OsString::from("root"),
            OsString::from("-g"),
            OsString::from("root"),
            OsString::from("-m"),
            OsString::from("0755"),
            OsString::from("--"),
            executable_parent.as_os_str().to_owned(),
        ],
    )
    .map_err(FixFailure::from)?;
    run_elevated(
        &elevation,
        &install,
        &[
            OsString::from("-o"),
            OsString::from("root"),
            OsString::from("-g"),
            OsString::from("root"),
            OsString::from("-m"),
            OsString::from("0755"),
            OsString::from("--"),
            helper.path().as_os_str().to_owned(),
            context.apparmor_passt_executable.as_os_str().to_owned(),
        ],
    )
    .map_err(FixFailure::from)?;
    run_elevated(
        &elevation,
        &install,
        &[
            OsString::from("-o"),
            OsString::from("root"),
            OsString::from("-g"),
            OsString::from("root"),
            OsString::from("-m"),
            OsString::from("0644"),
            OsString::from("--"),
            context
                .paths
                .apparmor_passt_staged_profile()
                .into_os_string(),
            context.apparmor_passt_profile.as_os_str().to_owned(),
        ],
    )
    .map_err(FixFailure::from)?;
    run_elevated(
        &elevation,
        &parser,
        &[
            OsString::from("-r"),
            context.apparmor_passt_profile.as_os_str().to_owned(),
        ],
    )
    .map_err(FixFailure::from)?;

    match resolve_verified_apparmor_passt_at(
        &context.apparmor_passt_executable,
        &context.apparmor_passt_profile,
        &context.apparmor_loaded_profiles,
        helper.artifact(),
        context.expected_root_uid,
    )
    .map_err(FixFailure::from)?
    {
        Some(_) => Ok(()),
        None => Err(FixFailure {
            reason: "the AppArmor passt installation was not present after install completed"
                .to_owned(),
            fix: Some(manual),
            hint: Some("inspect the privileged command output and retry".to_owned()),
        }),
    }
}

fn verify_extracted_passt(
    context: &DoctorContext,
    helper: &ExtractedPasstHelper,
) -> Result<(), FirestoneError> {
    let metadata = fs::symlink_metadata(helper.path()).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot inspect extracted passt bytes at {}",
                helper.path().display()
            ),
        )
        .with_source(source)
    })?;
    let mode = metadata.permissions().mode() & 0o7777;
    let owned =
        metadata.uid() == context.paths.uid() || metadata.uid() == context.expected_root_uid;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || !owned
        || mode & 0o022 != 0
        || mode & 0o111 == 0
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "extracted passt {} is not an owned, non-writable executable regular file (uid {}, mode {mode:04o})",
                helper.path().display(),
                metadata.uid()
            ),
        )
        .with_hint("discard the unsafe extraction and materialize the embedded bytes again"));
    }
    let actual = hash_file(helper.path(), "extracted passt helper")?;
    if actual != helper.artifact().sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "extracted passt helper has SHA-256 {actual}, expected {}",
                helper.artifact().sha256
            ),
        )
        .with_hint("discard the extraction; never elevate mismatched helper bytes"));
    }
    Ok(())
}

fn stage_apparmor_profile(context: &DoctorContext) -> Result<(), FirestoneError> {
    let directory = context.paths.apparmor_staging_dir();
    create_firestone_dir(&context.paths, &directory, "AppArmor staging directory")?;
    let profile = context.paths.apparmor_passt_staged_profile();
    crate::atomic::write_with_mode(&profile, APPARMOR_PASST_PROFILE_CONTENT.as_bytes(), 0o600)?;
    let metadata = fs::symlink_metadata(&profile).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot verify staged AppArmor profile {}",
                profile.display()
            ),
        )
        .with_source(source)
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != context.paths.uid()
        || metadata.permissions().mode() & 0o7777 != 0o600
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("staged AppArmor profile {} is unsafe", profile.display()),
        )
        .with_hint("repair the Firestone data directory and retry"));
    }
    Ok(())
}

fn manual_apparmor_commands(
    context: &DoctorContext,
    helper: &Path,
) -> Result<String, FirestoneError> {
    let executable_parent = context
        .apparmor_passt_executable
        .parent()
        .ok_or_else(|| FirestoneError::new(ErrorKind::Dependency, "invalid passt target path"))?;
    let staged_profile = context.paths.apparmor_passt_staged_profile();
    Ok([
        format!(
            "sudo install -d -o root -g root -m 0755 -- {}",
            shell_quote_path(executable_parent)?
        ),
        format!(
            "sudo install -o root -g root -m 0755 -- {} {}",
            shell_quote_path(helper)?,
            shell_quote_path(&context.apparmor_passt_executable)?
        ),
        format!(
            "sudo install -o root -g root -m 0644 -- {} {}",
            shell_quote_path(&staged_profile)?,
            shell_quote_path(&context.apparmor_passt_profile)?
        ),
        format!(
            "sudo apparmor_parser -r {}",
            shell_quote_path(&context.apparmor_passt_profile)?
        ),
    ]
    .join(" && "))
}

fn shell_quote_path(path: &Path) -> Result<String, FirestoneError> {
    let value = path.to_str().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "cannot render a manual command for non-UTF-8 path {}",
                path.display()
            ),
        )
        .with_hint("use a UTF-8 Firestone data path for privileged AppArmor repair")
    })?;
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push(char::from(39));
    for character in value.chars() {
        if character == char::from(39) {
            quoted.push_str(r#"'"'"'"#);
        } else {
            quoted.push(character);
        }
    }
    quoted.push(char::from(39));
    Ok(quoted)
}

fn select_elevation(context: &DoctorContext) -> Option<ElevationProvider> {
    if context.paths.uid() == 0 {
        Some(ElevationProvider::Direct)
    } else if let Some(sudo) = find_on_path("sudo", &context.search_path) {
        Some(ElevationProvider::Sudo(sudo))
    } else {
        find_on_path("pkexec", &context.search_path).map(ElevationProvider::Pkexec)
    }
}

fn run_elevated(
    elevation: &ElevationProvider,
    program: &Path,
    arguments: &[OsString],
) -> Result<(), FirestoneError> {
    let command = match elevation {
        ElevationProvider::Direct => Cmd::new(program.as_os_str().to_owned()),
        ElevationProvider::Sudo(sudo) => Cmd::new(sudo.as_os_str().to_owned())
            .arg("--")
            .arg(program.as_os_str().to_owned()),
        ElevationProvider::Pkexec(pkexec) => {
            Cmd::new(pkexec.as_os_str().to_owned()).arg(program.as_os_str().to_owned())
        }
    };
    command
        .args(arguments.iter().cloned())
        .stdin_inherit()
        .interactive_stdout_to_stderr()
        .error_kind(ErrorKind::Dependency)
        .run_interactive()
}

fn record_fix_failure_value(
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
    check_id: DoctorCheckId,
    next: FixFailure,
) {
    failures
        .entry(check_id)
        .and_modify(|failure| {
            failure.reason.push_str("; ");
            failure.reason.push_str(&next.reason);
            if failure.fix.is_none() {
                failure.fix.clone_from(&next.fix);
            }
            if failure.hint.is_none() {
                failure.hint.clone_from(&next.hint);
            }
        })
        .or_insert(next);
}
fn record_directory_fix(
    paths: &Paths,
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
    path: &Path,
    label: &str,
    check_id: DoctorCheckId,
) -> bool {
    match create_firestone_dir(paths, path, label) {
        Ok(()) => true,
        Err(error) => {
            record_fix_failure(failures, check_id, error);
            false
        }
    }
}

fn fix_artifact(
    context: &DoctorContext,
    dependency: &str,
    check_id: DoctorCheckId,
    fetcher: &dyn ArtifactFetcher,
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
) {
    let artifact = match context.manifest.artifact(dependency, &context.architecture) {
        Ok(artifact) => artifact,
        Err(error) => {
            if dependency != "virtiofsd" {
                record_fix_failure(failures, check_id, error);
            }
            return;
        }
    };
    if artifact_state(&context.paths.bin_dir(), &artifact).is_ok() {
        return;
    }
    if let Err(error) = install_artifact(&context.paths.bin_dir(), &artifact, fetcher) {
        record_fix_failure(failures, check_id, error);
    }
}

fn record_fix_failure(
    failures: &mut BTreeMap<DoctorCheckId, FixFailure>,
    check_id: DoctorCheckId,
    error: FirestoneError,
) {
    record_fix_failure_value(failures, check_id, FixFailure::from(error));
}

fn dependency_install_io_hint(directory: &Path) -> String {
    format!(
        "check read/write permissions and free space for {} and retry `firestone doctor --fix`",
        directory.display()
    )
}

fn dependency_install_io_error(
    artifact: &DependencyArtifact,
    directory: &Path,
    phase: &str,
    source: std::io::Error,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "cannot {phase} for dependency `{}` in {}",
            artifact.dependency,
            directory.display()
        ),
    )
    .with_hint(dependency_install_io_hint(directory))
    .with_source(source)
}

fn hash_dependency_download(
    reader: &mut dyn Read,
    artifact: &DependencyArtifact,
    directory: &Path,
) -> Result<String, FirestoneError> {
    sha256_reader(reader).map_err(|source| {
        dependency_install_io_error(
            artifact,
            directory,
            "read partial download for hashing",
            source,
        )
    })
}

fn install_artifact(
    bin_dir: &Path,
    artifact: &DependencyArtifact,
    fetcher: &dyn ArtifactFetcher,
) -> Result<(), FirestoneError> {
    let url = Url::parse(&artifact.url).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("invalid download URL for `{}`", artifact.dependency),
        )
        .with_source(source)
    })?;
    require_https(&url)?;

    let mut partial = TempBuilder::new()
        .prefix(&format!(".{}.", artifact.install_name))
        .suffix(".partial")
        .tempfile_in(bin_dir)
        .map_err(|source| {
            dependency_install_io_error(artifact, bin_dir, "create partial download", source)
        })?;
    fetcher
        .fetch(&url, partial.as_file_mut())
        .map_err(|source| {
            let kind = source.kind();
            let install_hint = dependency_install_io_hint(bin_dir);
            let hint = source.hint().map_or_else(
                || install_hint.clone(),
                |upstream| format!("{upstream}; {install_hint}"),
            );
            FirestoneError::new(
                kind,
                format!(
                    "cannot stream download for dependency `{}` into {}",
                    artifact.dependency,
                    bin_dir.display()
                ),
            )
            .with_hint(hint)
            .with_source(source)
        })?;
    partial.as_file_mut().flush().map_err(|source| {
        dependency_install_io_error(artifact, bin_dir, "flush partial download", source)
    })?;
    partial.as_file_mut().sync_all().map_err(|source| {
        dependency_install_io_error(artifact, bin_dir, "sync partial download", source)
    })?;
    partial
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|source| {
            dependency_install_io_error(artifact, bin_dir, "rewind partial download", source)
        })?;
    let actual = hash_dependency_download(partial.as_file_mut(), artifact, bin_dir)?;
    if actual != artifact.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "checksum mismatch for `{}`: expected {}, got {actual}",
                artifact.dependency, artifact.sha256
            ),
        )
        .with_hint("the partial download was removed; retry from a trusted network"));
    }

    partial
        .as_file()
        .set_permissions(fs::Permissions::from_mode(artifact.expected_mode()))
        .map_err(|source| {
            dependency_install_io_error(artifact, bin_dir, "set partial download mode", source)
        })?;
    partial.as_file().sync_all().map_err(|source| {
        dependency_install_io_error(artifact, bin_dir, "sync partial download mode", source)
    })?;

    let destination = bin_dir.join(&artifact.install_name);
    partial.persist(&destination).map_err(|error| {
        dependency_install_io_error(
            artifact,
            bin_dir,
            "atomically publish downloaded artifact",
            error.error,
        )
    })?;
    sync_directory(bin_dir, &artifact.dependency)?;
    artifact_state(bin_dir, artifact).map_err(|reason| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "installed `{}` at {} failed readback: {reason}",
                artifact.dependency,
                destination.display()
            ),
        )
        .with_hint(format!(
            "{}; remove {} if readback remains invalid",
            dependency_install_io_hint(bin_dir),
            destination.display()
        ))
    })
}

fn generate_ssh_key(context: &DoctorContext) -> Result<(), FirestoneError> {
    let keygen = find_on_path("ssh-keygen", &context.search_path).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "cannot generate Firestone SSH key: ssh-keygen is not on PATH",
        )
        .with_hint("install the OpenSSH client package")
    })?;
    crate::ssh::ensure_ssh_identity_with(&context.paths, keygen.as_os_str(), &context.hostname)
        .map(|_| ())
}

fn create_firestone_dir(paths: &Paths, path: &Path, label: &str) -> Result<(), FirestoneError> {
    paths
        .ensure_owned_data_directory(path, label, true)
        .map(|_| ())
}

fn artifact_state(bin_dir: &Path, artifact: &DependencyArtifact) -> Result<(), String> {
    let path = bin_dir.join(&artifact.install_name);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|source| format!("{} is unavailable: {source}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let actual =
        sha256_path(&path).map_err(|source| format!("cannot hash {}: {source}", path.display()))?;
    if actual != artifact.sha256 {
        return Err(format!(
            "{} checksum mismatch: expected {}, got {actual}",
            path.display(),
            artifact.sha256
        ));
    }
    let actual_mode = metadata.permissions().mode() & 0o777;
    let expected_mode = artifact.expected_mode();
    if actual_mode != expected_mode {
        return Err(format!(
            "{} has mode {actual_mode:04o}; expected {expected_mode:04o}",
            path.display()
        ));
    }
    Ok(())
}

fn path_is_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn sha256_path(path: &Path) -> Result<String, std::io::Error> {
    sha256_reader(&mut File::open(path)?)
}

fn sha256_reader(reader: &mut dyn Read) -> Result<String, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn require_https(url: &Url) -> Result<(), FirestoneError> {
    if url.scheme() == "https" {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("refusing non-HTTPS dependency download URL {url}"),
        )
        .with_hint("dependency downloads and every redirect must use HTTPS"))
    }
}

fn device_group(group_file: &Path, gid: u32) -> Option<String> {
    let contents = fs::read_to_string(group_file).ok()?;
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let group_gid = fields.next()?.parse::<u32>().ok()?;
        (group_gid == gid).then(|| name.to_owned())
    })
}

fn find_on_path(program: &str, search_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path).find_map(|directory| {
        let candidate = directory.join(program);
        let metadata = fs::metadata(&candidate).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}

fn command_failure_reason(output: &crate::CmdOutput) -> String {
    let status = output
        .status()
        .code()
        .map_or_else(|| "signal".to_owned(), |code| format!("status {code}"));
    let stderr = output.last_stderr_lines();
    if stderr.is_empty() {
        status
    } else {
        format!("{status}; {}", stderr.join(" | "))
    }
}

fn firestone_error_reason(error: &FirestoneError) -> String {
    let mut reason = error.message().to_owned();
    let mut source = std::error::Error::source(error);
    for _ in 0..4 {
        let Some(next) = source else {
            break;
        };
        reason.push_str(": ");
        reason.push_str(&bounded_detail(&next.to_string()));
        source = next.source();
    }
    reason
}

fn bounded_detail(detail: &str) -> String {
    let mut characters = detail.chars();
    let mut bounded = characters.by_ref().take(1024).collect::<String>();
    if characters.next().is_some() {
        bounded.push_str("...[truncated]");
    }
    bounded
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PasstVersion {
    raw: String,
    date: u32,
}

fn parse_passt_version(output: &str) -> Option<PasstVersion> {
    output.split_whitespace().find_map(|token| {
        let trimmed = token.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '.'
        });
        let (date_part, packaged_commit) = trimmed.split_once('.')?;
        let packaged_commit = packaged_commit.split('-').next()?;
        let commit = packaged_commit.strip_prefix('g').unwrap_or(packaged_commit);
        if !(7..=40).contains(&commit.len()) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let (year, month, day) = if let Some(compact) = date_part.strip_prefix("0^") {
            if compact.len() != 8 || !compact.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            (
                compact[0..4].parse::<u32>().ok()?,
                compact[4..6].parse::<u32>().ok()?,
                compact[6..8].parse::<u32>().ok()?,
            )
        } else {
            let mut components = date_part.split('_');
            let values = (
                components.next()?.parse::<u32>().ok()?,
                components.next()?.parse::<u32>().ok()?,
                components.next()?.parse::<u32>().ok()?,
            );
            if components.next().is_some() {
                return None;
            }
            values
        };
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let date = year
            .checked_mul(10_000)?
            .checked_add(month.checked_mul(100)?)?
            .checked_add(day)?;
        Some(PasstVersion {
            raw: trimmed.to_owned(),
            date,
        })
    })
}

fn passt_install_hint(context: &DoctorContext) -> String {
    let fields = fs::read_to_string(&context.os_release)
        .ok()
        .map(|contents| parse_os_release(&contents))
        .unwrap_or_default();
    if fields.get("ID").map(String::as_str) == Some("ubuntu")
        && fields.get("VERSION_ID").map(String::as_str) == Some("24.04")
    {
        return concat!(
            "Ubuntu 24.04's passt package is too old; run `sudo apt-get install -y ",
            "build-essential ca-certificates git`, clone `https://passt.top/passt` at tag ",
            "`2025_02_17.a1e48a0`, verify commit ",
            "`a1e48a02ff3550eb7875a7df6726086e9b3a1213`, run `make passt` in that ",
            "checkout, then run `sudo install -m0755 passt /usr/local/bin/passt`"
        )
        .to_owned();
    }
    install_command(context, Package::Passt).map_or_else(
        || {
            format!(
                "install passt {} or newer with the pinned M3 command options",
                crate::PINNED_PASST_VERSION
            )
        },
        |command| {
            format!(
                "run `{command}`, then rerun doctor; Firestone requires {} or newer with the pinned M3 command options",
                crate::PINNED_PASST_VERSION
            )
        },
    )
}

fn missing_passt_check(context: &DoctorContext) -> DoctorCheck {
    DoctorCheck::new(
        DoctorCheckId::Passt,
        DoctorStatus::Fail,
        "passt not found on PATH",
    )
    .with_hint(passt_install_hint(context))
}

#[derive(Debug, Clone, Copy)]
enum Package {
    Passt,
    QemuImg,
    OpenSsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DistroFamily {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

fn missing_program_check(
    context: &DoctorContext,
    id: DoctorCheckId,
    package: Package,
    program: &str,
) -> DoctorCheck {
    let mut check = DoctorCheck::new(
        id,
        DoctorStatus::Fail,
        format!("{program} not found on PATH"),
    );
    if let Some(fix) = install_command(context, package) {
        check = check.with_fix(fix);
    } else {
        check = check.with_hint(format!("install the package that provides {program}"));
    }
    check
}

fn install_command(context: &DoctorContext, package: Package) -> Option<String> {
    let family = distro_family(&context.os_release)?;
    Some(
        match (family, package) {
            (DistroFamily::Apt, Package::Passt) => "sudo apt-get install passt",
            (DistroFamily::Apt, Package::QemuImg) => "sudo apt-get install qemu-utils",
            (DistroFamily::Apt, Package::OpenSsh) => "sudo apt-get install openssh-client",
            (DistroFamily::Dnf, Package::Passt) => "sudo dnf install passt",
            (DistroFamily::Dnf, Package::QemuImg) => "sudo dnf install qemu-img",
            (DistroFamily::Dnf, Package::OpenSsh) => "sudo dnf install openssh-clients",
            (DistroFamily::Pacman, Package::Passt) => "sudo pacman -S passt",
            (DistroFamily::Pacman, Package::QemuImg) => "sudo pacman -S qemu-img",
            (DistroFamily::Pacman, Package::OpenSsh) => "sudo pacman -S openssh",
            (DistroFamily::Zypper, Package::Passt) => "sudo zypper install passt",
            (DistroFamily::Zypper, Package::QemuImg) => "sudo zypper install qemu-tools",
            (DistroFamily::Zypper, Package::OpenSsh) => "sudo zypper install openssh",
        }
        .to_owned(),
    )
}

fn distro_family(os_release: &Path) -> Option<DistroFamily> {
    let fields = parse_os_release(&fs::read_to_string(os_release).ok()?);
    let id = fields.get("ID").map(String::as_str).unwrap_or_default();
    let like = fields
        .get("ID_LIKE")
        .map(String::as_str)
        .unwrap_or_default();
    let candidates = std::iter::once(id).chain(like.split_whitespace());
    for candidate in candidates {
        match candidate {
            "debian" | "ubuntu" => return Some(DistroFamily::Apt),
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => {
                return Some(DistroFamily::Dnf);
            }
            "arch" | "manjaro" => return Some(DistroFamily::Pacman),
            "opensuse" | "suse" | "sles" => return Some(DistroFamily::Zypper),
            _ => {}
        }
    }
    None
}

fn parse_os_release(contents: &str) -> BTreeMap<String, String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, raw_value) = line.split_once('=')?;
            let value = raw_value
                .trim()
                .trim_matches(|character| character == '"' || character == '\'')
                .to_owned();
            Some((key.to_owned(), value))
        })
        .collect()
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

fn sync_directory(path: &Path, dependency: &str) -> Result<(), FirestoneError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "cannot sync installed dependency `{dependency}` directory {}",
                    path.display()
                ),
            )
            .with_hint(dependency_install_io_hint(path))
            .with_source(source)
        })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        ffi::OsString,
        fs,
        io::{Cursor, Read, Write},
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixListener,
        },
        path::{Path, PathBuf},
        thread,
        time::{Duration, Instant},
    };

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::{
        APPARMOR_PASST_PROFILE_CONTENT, ArtifactFetcher, CHECK_IDS, DoctorCheck, DoctorCheckId,
        DoctorContext, DoctorOptions, DoctorReport, DoctorStatus, ExtractedPasstHelper,
        MAX_DEPENDENCY_ARTIFACT_BYTES, MINIMUM_PASST_DATE, Package, artifact_state,
        check_architecture, check_kvm, check_passt, check_user_namespaces,
        content_length_exceeds_limit, copy_bounded, create_firestone_dir, distro_family,
        firestone_error_reason, generate_ssh_key, hash_dependency_download, install_artifact,
        install_command, parse_passt_version, read_reconciled_machine_state_with,
        reconcile_machine_state, redirect_rejection, require_https, run_doctor_with,
        run_doctor_with_options, sync_directory,
    };
    use crate::{
        DependencyArtifact, DependencyManifest, ErrorKind, ExitReason, FirestoneError, LastExit,
        MachineLock, MachineState, MachineStatus, PathInputs, Paths, StateStore,
        VmmApiLivenessProbe, VmmPingProbe,
    };

    struct FakeFetcher {
        payloads: BTreeMap<String, Vec<u8>>,
        calls: RefCell<Vec<String>>,
        fail_after_write: bool,
    }

    impl FakeFetcher {
        fn new(payloads: BTreeMap<String, Vec<u8>>) -> Self {
            Self {
                payloads,
                calls: RefCell::new(Vec::new()),
                fail_after_write: false,
            }
        }

        fn failing(payloads: BTreeMap<String, Vec<u8>>) -> Self {
            Self {
                payloads,
                calls: RefCell::new(Vec::new()),
                fail_after_write: true,
            }
        }
    }

    impl ArtifactFetcher for FakeFetcher {
        fn fetch(&self, url: &reqwest::Url, output: &mut dyn Write) -> Result<(), FirestoneError> {
            self.calls.borrow_mut().push(url.to_string());
            let payload = self.payloads.get(url.as_str()).ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("fake fetcher has no payload for {url}"),
                )
            })?;
            output.write_all(payload).map_err(|source| {
                FirestoneError::new(ErrorKind::Dependency, "fake fetch write failed")
                    .with_source(source)
            })?;
            if self.fail_after_write {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    "injected stream failure",
                ));
            }
            Ok(())
        }
    }

    struct FinalStateDuringPing {
        store: StateStore,
        state: MachineState,
        wrote: Cell<bool>,
    }

    impl VmmPingProbe for FinalStateDuringPing {
        fn ping(&self, _api_socket: &Path) -> Result<bool, FirestoneError> {
            if !self.wrote.replace(true) {
                self.store.write_from_shim(&self.state)?;
            }
            Ok(false)
        }
    }

    struct FixedPing(bool);

    impl VmmPingProbe for FixedPing {
        fn ping(&self, _api_socket: &Path) -> Result<bool, FirestoneError> {
            Ok(self.0)
        }
    }

    struct Fixture {
        _temp: TempDir,
        context: DoctorContext,
        payloads: BTreeMap<String, Vec<u8>>,
        privileged_marker: PathBuf,
    }

    impl Fixture {
        fn healthy() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = TempDir::new()?;
            let canonical_root = fs::canonicalize(temp.path())?;
            let root = canonical_root.as_path();
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            let data_dir = root.join("data");
            let bin_dir = data_dir.join("bin");
            let runtime_dir = root.join("run");
            let key_dir = data_dir.join("ssh");
            let fake_bin = root.join("fake-bin");
            for path in [&data_dir, &bin_dir, &runtime_dir, &key_dir, &fake_bin] {
                fs::create_dir_all(path)?;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }

            let payloads = artifact_payloads();
            let manifest = manifest_for_payloads(&payloads)?;
            for dependency in [
                "cloud-hypervisor",
                "rust-hypervisor-firmware",
                "cloud-hypervisor-edk2",
                "virtiofsd",
            ] {
                let artifact = manifest.artifact(dependency, "x86_64")?;
                let payload = payloads
                    .get(&artifact.url)
                    .ok_or("fixture payload is missing")?;
                let path = bin_dir.join(&artifact.install_name);
                fs::write(&path, payload)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(artifact.expected_mode()))?;
            }

            let ssh_private_key = key_dir.join("id_ed25519");
            let ssh_public_key = key_dir.join("id_ed25519.pub");
            fs::write(&ssh_private_key, "private fixture")?;
            fs::write(&ssh_public_key, "ssh-ed25519 fixture")?;
            fs::set_permissions(&ssh_private_key, fs::Permissions::from_mode(0o600))?;
            fs::set_permissions(&ssh_public_key, fs::Permissions::from_mode(0o644))?;

            let kvm_device = root.join("kvm");
            fs::write(&kvm_device, "fixture")?;
            fs::set_permissions(&kvm_device, fs::Permissions::from_mode(0o660))?;
            let gid = fs::metadata(&kvm_device)?.gid();
            let group_file = root.join("group");
            fs::write(&group_file, format!("kvm:x:{gid}:\n"))?;
            let nested = root.join("nested");
            fs::write(&nested, "Y\n")?;
            let sysctl = root.join("max_user_namespaces");
            fs::write(&sysctl, "1024\n")?;
            let clone_sysctl = root.join("unprivileged_userns_clone");
            fs::write(
                &clone_sysctl,
                "1
",
            )?;
            let apparmor_enabled = root.join("apparmor-enabled");
            fs::write(
                &apparmor_enabled,
                "N
",
            )?;
            let apparmor_restrict = root.join("apparmor-restrict-userns");
            fs::write(
                &apparmor_restrict,
                "0
",
            )?;
            let self_status = root.join("self-status");
            fs::write(
                &self_status,
                "NoNewPrivs:	0
Seccomp:	0
",
            )?;
            let self_cgroup = root.join("self-cgroup");
            fs::write(
                &self_cgroup,
                "0::/
",
            )?;
            let audit_log = root.join("audit.log");
            fs::write(&audit_log, "")?;
            let system_root = root.join("system");
            let apparmor_passt_dir = system_root.join("usr/libexec/firestone");
            let apparmor_profile_dir = system_root.join("etc/apparmor.d");
            fs::create_dir_all(&apparmor_passt_dir)?;
            fs::create_dir_all(&apparmor_profile_dir)?;
            fs::set_permissions(&apparmor_passt_dir, fs::Permissions::from_mode(0o700))?;
            fs::set_permissions(&apparmor_profile_dir, fs::Permissions::from_mode(0o700))?;
            let apparmor_passt_executable = apparmor_passt_dir.join("passt-2025_02_17.a1e48a0");
            let apparmor_passt_profile =
                apparmor_profile_dir.join("firestone-passt-2025_02_17.a1e48a0");
            let apparmor_loaded_profiles = root.join("loaded-apparmor-profiles");
            fs::write(&apparmor_loaded_profiles, "")?;
            let os_release = root.join("os-release");
            fs::write(&os_release, "ID=ubuntu\nID_LIKE=debian\n")?;

            let privileged_marker = root.join("sudo-ran");
            write_executable(
                &fake_bin.join("sudo"),
                &format!("printf ran > '{}'; exit 99", privileged_marker.display()),
            )?;
            write_executable(
                &fake_bin.join("passt"),
                r#"case "$1" in --version) printf 'passt 2025_02_17.a1e48a0
' ;; --help) printf '%s
' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file' ;; --tcp-ports) exit 0 ;; --foreground) printf 'Failed to detach isolating namespaces: Operation not permitted
' >&2; exit 1 ;; *) exit 2 ;; esac"#,
            )?;
            write_executable(&fake_bin.join("qemu-img"), "printf 'qemu-img fixture\\n'")?;
            write_executable(&fake_bin.join("ssh"), "printf 'OpenSSH fixture\\n' >&2")?;
            write_executable(
                &fake_bin.join("ssh-keygen"),
                "key=''; while [ \"$#\" -gt 0 ]; do if [ \"$1\" = '-f' ]; then shift; key=$1; fi; shift; done; [ -n \"$key\" ] || exit 2; umask 077; printf private > \"$key\"; printf 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P firestone@test\n' > \"$key.pub\"",
            )?;
            write_executable(&fake_bin.join("unshare"), "exit 0")?;

            let paths = Paths::from_inputs(&PathInputs {
                current_dir: root.to_path_buf(),
                home_dir: None,
                firestone_home: None,
                firestone_config_dir: Some(root.join("config")),
                firestone_data_dir: Some(data_dir),
                firestone_runtime_dir: Some(runtime_dir),
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: fs::metadata(root)?.uid(),
            })?;
            let proc_root = root.join("proc");
            fs::create_dir(&proc_root)?;
            let context = DoctorContext {
                paths,
                operating_system: "linux".to_owned(),
                architecture: "x86_64".to_owned(),
                kvm_device,
                nested_parameters: vec![nested],
                user_namespace_sysctl: sysctl,
                unprivileged_userns_clone_sysctl: clone_sysctl,
                apparmor_enabled,
                apparmor_restrict_userns: apparmor_restrict,
                self_status,
                self_cgroup,
                container_markers: vec![
                    root.join(".dockerenv"),
                    root.join(".containerenv"),
                    root.join("systemd-container"),
                ],
                audit_logs: vec![audit_log],
                apparmor_passt_executable,
                apparmor_passt_profile,
                apparmor_loaded_profiles,
                extracted_passt: None,
                expected_root_uid: fs::metadata(root)?.uid(),
                os_release,
                group_file,
                search_path: OsString::from(fake_bin),
                hostname: "fixture".to_owned(),
                manifest,
                proc_root,
                reconciled_at: "2026-08-28T00:00:00Z".to_owned(),
                minimum_data_free_bytes: 0,
            };

            Ok(Self {
                _temp: temp,
                context,
                payloads,
                privileged_marker,
            })
        }
    }

    fn write_running_state(paths: &Paths, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        fs::create_dir_all(paths.machines_dir())?;
        fs::set_permissions(paths.machines_dir(), fs::Permissions::from_mode(0o700))?;
        let machine_dir = paths.machine_dir(name)?;
        fs::create_dir(&machine_dir)?;
        fs::set_permissions(&machine_dir, fs::Permissions::from_mode(0o700))?;
        let state = serde_json::json!({
            "version": 1,
            "status": "running",
            "image": {"ref": "ubuntu:24.04", "id": "fixture", "sha256": "fixture"},
            "mac": null,
            "cid": 3,
            "instance_id": "iid-fixture",
            "shim_pid": null,
            "vmm_pid": null,
            "sidecar_pids": {},
            "runtime_dir": paths.machine_runtime_dir(name)?,
            "started_at": "2026-08-28T00:00:00Z",
            "forwards": [],
            "degraded": [],
            "last_exit": null
        });
        fs::write(
            paths.machine_state(name)?,
            serde_json::to_vec_pretty(&state)?,
        )?;
        Ok(())
    }

    fn artifact_payloads() -> BTreeMap<String, Vec<u8>> {
        [
            (
                "https://example.invalid/cloud-hypervisor",
                b"cloud-hypervisor".to_vec(),
            ),
            (
                "https://example.invalid/rhf",
                b"rust-hypervisor-firmware".to_vec(),
            ),
            (
                "https://example.invalid/edk2",
                b"cloud-hypervisor-edk2".to_vec(),
            ),
            ("https://example.invalid/virtiofsd", b"virtiofsd".to_vec()),
            (
                "https://example.invalid/passt",
                br#"#!/bin/sh
case "$1" in
  --version) printf 'passt 2025_02_17.a1e48a0
' ;;
  --help) printf '%s
' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file' ;;
  --tcp-ports) exit 0 ;;
  --foreground) printf "Failed to detach isolating namespaces: Operation not permitted
" >&2; exit 1 ;;
  *) exit 2 ;;
esac
"#
                .to_vec(),
            ),
        ]
        .into_iter()
        .map(|(url, payload)| (url.to_owned(), payload))
        .collect()
    }

    fn manifest_for_payloads(
        payloads: &BTreeMap<String, Vec<u8>>,
    ) -> Result<DependencyManifest, FirestoneError> {
        let entries = [
            (
                "cloud-hypervisor",
                "v53.0",
                "cloud-hypervisor-v53.0",
                "https://example.invalid/cloud-hypervisor",
            ),
            (
                "rust-hypervisor-firmware",
                "0.5.0",
                "hypervisor-fw-0.5.0",
                "https://example.invalid/rhf",
            ),
            (
                "cloud-hypervisor-edk2",
                "ch-test",
                "CLOUDHV-ch-test.fd",
                "https://example.invalid/edk2",
            ),
            (
                "virtiofsd",
                "v1.14.0",
                "virtiofsd-v1.14.0",
                "https://example.invalid/virtiofsd",
            ),
            (
                "passt",
                crate::PINNED_PASST_VERSION,
                "passt-2025_02_17.a1e48a0",
                "https://example.invalid/passt",
            ),
        ];
        let mut input = "manifest_version = 1\n".to_owned();
        for (dependency, version, install_name, url) in entries {
            let payload = payloads.get(url).ok_or_else(|| {
                FirestoneError::new(ErrorKind::Dependency, "fixture payload missing")
            })?;
            input.push_str(&format!(
                "\n[dependency.{dependency}]\nversion = \"{version}\"\navailability = \"binary\"\n[dependency.{dependency}.x86_64]\nasset = \"asset\"\ninstall_name = \"{install_name}\"\nurl = \"{url}\"\nsha256 = \"{}\"\n",
                sha256(payload)
            ));
        }
        DependencyManifest::parse(&input)
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn write_executable(path: &Path, body: &str) -> Result<(), std::io::Error> {
        fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
    }
    fn configure_apparmor_passt_denial(
        fixture: &Fixture,
    ) -> Result<(), Box<dyn std::error::Error>> {
        fs::write(
            &fixture.context.apparmor_enabled,
            "Y
",
        )?;
        fs::write(
            &fixture.context.apparmor_restrict_userns,
            "1
",
        )?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            r#"if [ "$1" = "--version" ]; then
  printf 'passt 2025_02_17.a1e48a0
'
elif [ "$1" = "--help" ]; then
  printf '%s
' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file'
elif [ "$1" = "--tcp-ports" ]; then
  exit 0
elif [ "$1" = "--foreground" ]; then
  printf "Couldn't create user namespace: Permission denied
" >&2
  exit 1
else
  exit 2
fi"#,
        )?;
        Ok(())
    }

    fn provide_extracted_passt(
        fixture: &mut Fixture,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let artifact = fixture.context.manifest.embedded_passt("x86_64")?;
        let payload = fixture
            .payloads
            .get(&artifact.url)
            .ok_or("fixture passt payload missing")?;
        let source = fixture.context.paths.bin_dir().join("extracted-passt");
        fs::write(&source, payload)?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
        fixture.context.extracted_passt =
            Some(ExtractedPasstHelper::new(source.clone(), artifact)?);
        Ok(source)
    }

    fn check(report: &DoctorReport, id: DoctorCheckId) -> &DoctorCheck {
        let index = match id {
            DoctorCheckId::HostArch => 0,
            DoctorCheckId::Kvm => 1,
            DoctorCheckId::NestedVirtualization => 2,
            DoctorCheckId::RuntimeDir => 3,
            DoctorCheckId::VendoredBinaries => 4,
            DoctorCheckId::Virtiofsd => 5,
            DoctorCheckId::Passt => 6,
            DoctorCheckId::QemuImg => 7,
            DoctorCheckId::Ssh => 8,
            DoctorCheckId::UserNamespaces => 9,
            DoctorCheckId::SshKey => 10,
            DoctorCheckId::DataSpace => 11,
            DoctorCheckId::StaleState => 12,
        };
        &report.checks[index]
    }

    #[test]
    fn report_healthy_context_contains_thirteen_ordered_checks()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let fetcher = FakeFetcher::new(fixture.payloads.clone());
        let report = run_doctor_with(&fixture.context, false, &fetcher);

        assert_eq!(report.checks.len(), 13);
        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.id)
                .collect::<Vec<_>>(),
            CHECK_IDS
        );
        assert!(!report.has_failures(), "{report:#?}");
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == DoctorStatus::Ok)
        );
        assert!(fetcher.calls.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn aarch64_runtime_is_reported_as_deferred() -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.architecture = "aarch64".to_owned();

        let host = check_architecture(&fixture.context);

        assert_eq!(host.status, DoctorStatus::Fail);
        assert!(host.reason.contains("runtime support is deferred"));
        assert!(
            host.hint
                .as_deref()
                .is_some_and(|hint| hint.contains("compile-only"))
        );
        Ok(())
    }

    #[test]
    fn report_mixed_failures_preserves_spec_order_and_does_not_short_circuit()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.operating_system = "macos".to_owned();
        fixture.context.architecture = "riscv64".to_owned();
        fs::remove_file(&fixture.context.kvm_device)?;
        fs::remove_file(fixture.context.paths.ssh_private_key())?;
        fixture.context.minimum_data_free_bytes = u64::MAX;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.id)
                .collect::<Vec<_>>(),
            CHECK_IDS
        );
        assert_eq!(
            check(&report, DoctorCheckId::HostArch).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check(&report, DoctorCheckId::Kvm).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check(&report, DoctorCheckId::NestedVirtualization).status,
            DoctorStatus::Warn
        );
        assert_eq!(
            check(&report, DoctorCheckId::RuntimeDir).status,
            DoctorStatus::Ok
        );
        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check(&report, DoctorCheckId::DataSpace).status,
            DoctorStatus::Warn
        );
        assert!(report.has_failures());
        Ok(())
    }

    #[test]
    fn runtime_missing_fails_without_read_only_creation() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        fs::remove_dir(fixture.context.paths.runtime_dir())?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::RuntimeDir).status,
            DoctorStatus::Fail
        );
        assert!(!fixture.context.paths.runtime_dir().exists());
        Ok(())
    }

    #[test]
    fn runtime_missing_fix_creates_secure_directory() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::remove_dir(fixture.context.paths.runtime_dir())?;

        let report = run_doctor_with(
            &fixture.context,
            true,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::RuntimeDir).status,
            DoctorStatus::Ok
        );
        let mode = fs::metadata(fixture.context.paths.runtime_dir())?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        Ok(())
    }

    #[test]
    fn doctor_context_from_paths_preserves_resolved_paths() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        let context = DoctorContext::from_paths(
            fixture.context.paths.clone(),
            fixture.context.manifest.clone(),
            "test-host",
            "2026-08-28T01:02:03Z",
        );

        assert_eq!(context.paths.data_dir(), fixture.context.paths.data_dir());
        assert_eq!(
            context.paths.runtime_dir(),
            fixture.context.paths.runtime_dir()
        );
        assert_eq!(context.hostname, "test-host");
        assert_eq!(context.reconciled_at, "2026-08-28T01:02:03Z");
        assert_eq!(
            context.minimum_data_free_bytes,
            DoctorContext::default_minimum_data_free_bytes()
        );
        Ok(())
    }

    fn probe_vmm_status(status: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let socket = directory.path().join("api.sock");
        let listener = UnixListener::bind(&socket)?;
        let response = if status == "200" {
            let body = br#"{"build_version":"v53.0","version":"53.0.0","pid":42,"features":[]}"#;
            let mut response =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            response.extend_from_slice(body);
            response
        } else {
            format!("HTTP/1.1 {status} No Content\r\n\r\n").into_bytes()
        };
        let server = thread::spawn(move || -> Result<(), std::io::Error> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0_u8; 256];
            let mut used = 0;
            while used < request.len()
                && !request[..used]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
            {
                let read = stream.read(&mut request[used..])?;
                if read == 0 {
                    break;
                }
                used += read;
            }
            assert!(request[..used].starts_with(b"GET /api/v1/vmm.ping HTTP/1.1\r\n"));
            stream.write_all(&response)?;
            Ok(())
        });

        let pinged = VmmApiLivenessProbe::new(Duration::from_secs(5)).ping(&socket)?;
        server
            .join()
            .map_err(|_| std::io::Error::other("VMM ping fixture panicked"))??;
        Ok(pinged)
    }

    #[test]
    fn vmm_ping_probe_requires_http_200() -> Result<(), Box<dyn std::error::Error>> {
        assert!(probe_vmm_status("200")?);
        assert!(probe_vmm_status("204").is_err());
        Ok(())
    }

    #[test]
    fn vmm_ping_probe_uses_one_absolute_deadline() -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let socket = directory.path().join("api.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = thread::spawn(move || -> Result<(), std::io::Error> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0_u8; 256];
            let mut used = 0;
            while used < request.len()
                && !request[..used]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
            {
                let read = stream.read(&mut request[used..])?;
                if read == 0 {
                    return Ok(());
                }
                used += read;
            }
            for byte in b"HTTP/1.1 200 Slow\r\n" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Ok(())
        });

        let started = Instant::now();
        let error = VmmApiLivenessProbe::new(Duration::from_millis(80))
            .ping(&socket)
            .err()
            .ok_or("slow liveness probe unexpectedly returned a boolean")?;
        let elapsed = started.elapsed();
        server
            .join()
            .map_err(|_| std::io::Error::other("VMM slow-ping fixture panicked"))??;

        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert!(elapsed < Duration::from_millis(250));
        Ok(())
    }
    #[test]
    fn report_serialization_preserves_stable_ids_and_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );
        let value = serde_json::to_value(report)?;
        let Some(checks) = value["checks"].as_array() else {
            return Err(std::io::Error::other("checks should be an array").into());
        };
        assert_eq!(checks[0]["id"], "host_arch");
        assert_eq!(checks[12]["id"], "stale_state");
        assert_eq!(checks[0]["status"], "ok");
        Ok(())
    }

    #[test]
    fn fix_missing_artifacts_and_key_rechecks_modes_without_privileged_commands()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        for dependency in [
            "cloud-hypervisor",
            "rust-hypervisor-firmware",
            "cloud-hypervisor-edk2",
            "virtiofsd",
        ] {
            let artifact = fixture.context.manifest.artifact(dependency, "x86_64")?;
            fs::remove_file(fixture.context.paths.bin_dir().join(artifact.install_name))?;
        }
        fs::remove_file(fixture.context.paths.ssh_private_key())?;
        fs::remove_file(fixture.context.paths.ssh_public_key())?;
        let fetcher = FakeFetcher::new(fixture.payloads.clone());

        let report = run_doctor_with(&fixture.context, true, &fetcher);

        assert_eq!(
            check(&report, DoctorCheckId::VendoredBinaries).status,
            DoctorStatus::Ok
        );
        assert_eq!(
            check(&report, DoctorCheckId::Virtiofsd).status,
            DoctorStatus::Ok
        );
        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Ok
        );
        assert_eq!(fetcher.calls.borrow().len(), 4);
        assert!(!fixture.privileged_marker.exists());
        for dependency in [
            "cloud-hypervisor",
            "rust-hypervisor-firmware",
            "cloud-hypervisor-edk2",
            "virtiofsd",
        ] {
            let artifact = fixture.context.manifest.artifact(dependency, "x86_64")?;
            artifact_state(&fixture.context.paths.bin_dir(), &artifact)?;
        }

        let second = run_doctor_with(&fixture.context, true, &fetcher);
        assert!(!second.has_failures());
        assert_eq!(fetcher.calls.borrow().len(), 4);
        Ok(())
    }

    #[test]
    fn install_io_failures_keep_dependency_kind_path_phase_and_actionable_hint()
    -> Result<(), Box<dyn std::error::Error>> {
        struct DeniedReader;

        impl Read for DeniedReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "read denied",
                ))
            }
        }

        let directory = TempDir::new()?;
        let artifact = DependencyArtifact {
            dependency: "cloud-hypervisor".to_owned(),
            version: "v1".to_owned(),
            asset: "tool".to_owned(),
            install_name: "tool-v1".to_owned(),
            url: "https://example.invalid/tool".to_owned(),
            sha256: sha256(b"expected"),
        };
        let hash_error = hash_dependency_download(&mut DeniedReader, &artifact, directory.path())
            .err()
            .ok_or("expected partial download read failure")?;
        assert_eq!(hash_error.kind(), ErrorKind::Dependency);
        assert!(
            hash_error
                .message()
                .contains("read partial download for hashing")
        );
        assert!(hash_error.message().contains("cloud-hypervisor"));
        assert!(
            hash_error
                .message()
                .contains(&directory.path().display().to_string())
        );
        let hash_hint = hash_error
            .hint()
            .ok_or("hash I/O error should have a hint")?;
        for expected in [
            "read/write permissions",
            "free space",
            "firestone doctor --fix",
        ] {
            assert!(hash_hint.contains(expected));
        }

        let missing = directory.path().join("missing-bin");
        let sync_error = sync_directory(&missing, "cloud-hypervisor")
            .err()
            .ok_or("expected dependency directory sync failure")?;
        assert_eq!(sync_error.kind(), ErrorKind::Dependency);
        assert!(sync_error.message().contains("sync installed dependency"));
        assert!(
            sync_error
                .message()
                .contains(&missing.display().to_string())
        );
        let sync_hint = sync_error
            .hint()
            .ok_or("sync I/O error should have a hint")?;
        assert!(sync_hint.contains("read/write permissions"));
        assert!(sync_hint.contains("free space"));
        Ok(())
    }

    #[test]
    fn download_checksum_mismatch_preserves_target_and_removes_partial()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let destination = dir.path().join("tool-v1");
        fs::write(&destination, "existing")?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
        let artifact = DependencyArtifact {
            dependency: "cloud-hypervisor".to_owned(),
            version: "v1".to_owned(),
            asset: "tool".to_owned(),
            install_name: "tool-v1".to_owned(),
            url: "https://example.invalid/tool".to_owned(),
            sha256: sha256(b"expected"),
        };
        let fetcher = FakeFetcher::new(BTreeMap::from([(
            artifact.url.clone(),
            b"different".to_vec(),
        )]));

        let error = match install_artifact(dir.path(), &artifact, &fetcher) {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("checksum mismatch should fail").into()),
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert_eq!(fs::read(&destination)?, b"existing");
        let names = fs::read_dir(dir.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(names, [OsString::from("tool-v1")]);
        Ok(())
    }

    #[test]
    fn download_stream_failure_removes_partial_and_valid_download_sets_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let payload = b"valid-tool".to_vec();
        let artifact = DependencyArtifact {
            dependency: "cloud-hypervisor".to_owned(),
            version: "v1".to_owned(),
            asset: "tool".to_owned(),
            install_name: "tool-v1".to_owned(),
            url: "https://example.invalid/tool".to_owned(),
            sha256: sha256(&payload),
        };
        let payloads = BTreeMap::from([(artifact.url.clone(), payload)]);
        let destination = dir.path().join("tool-v1");
        fs::write(&destination, "existing corrupt artifact")?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
        let failing = FakeFetcher::failing(payloads.clone());
        let stream_error = install_artifact(dir.path(), &artifact, &failing)
            .err()
            .ok_or("expected injected stream failure")?;
        assert_eq!(stream_error.kind(), ErrorKind::Dependency);
        assert!(stream_error.message().contains("stream download"));
        assert!(stream_error.message().contains("cloud-hypervisor"));
        assert!(
            stream_error
                .message()
                .contains(&dir.path().display().to_string())
        );
        let stream_hint = stream_error
            .hint()
            .ok_or("stream failure should have an install hint")?;
        assert!(stream_hint.contains("read/write permissions"));
        assert!(stream_hint.contains("free space"));
        assert_eq!(fs::read(&destination)?, b"existing corrupt artifact");
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);

        install_artifact(dir.path(), &artifact, &FakeFetcher::new(payloads))?;
        artifact_state(dir.path(), &artifact)?;
        assert_eq!(fs::read(&destination)?, b"valid-tool");
        assert_eq!(
            fs::metadata(dir.path().join("tool-v1"))?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        Ok(())
    }

    #[test]
    fn artifact_matching_symlink_is_not_trusted() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let artifact = fixture
            .context
            .manifest
            .artifact("cloud-hypervisor", "x86_64")?;
        let path = fixture.context.paths.bin_dir().join(&artifact.install_name);
        let outside = fixture.context.paths.data_dir().join("outside-tool");
        fs::write(&outside, fs::read(&path)?)?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755))?;
        fs::remove_file(&path)?;
        std::os::unix::fs::symlink(&outside, &path)?;

        let reason = match artifact_state(&fixture.context.paths.bin_dir(), &artifact) {
            Err(reason) => reason,
            Ok(()) => return Err(std::io::Error::other("symlink should not be accepted").into()),
        };

        assert!(reason.contains("not a regular file"));
        Ok(())
    }

    #[test]
    fn download_http_and_https_downgrade_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let http = reqwest::Url::parse("http://example.invalid/tool")?;
        let https = reqwest::Url::parse("https://example.invalid/tool")?;

        assert!(require_https(&http).is_err());
        assert_eq!(
            redirect_rejection(&http, 0),
            Some("dependency download redirect is not HTTPS")
        );
        assert_eq!(
            redirect_rejection(&https, 10),
            Some("too many dependency download redirects")
        );
        assert_eq!(redirect_rejection(&https, 0), None);
        Ok(())
    }

    #[test]
    fn download_declared_and_streamed_oversize_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(content_length_exceeds_limit(Some(
            MAX_DEPENDENCY_ARTIFACT_BYTES + 1
        )));
        assert!(!content_length_exceeds_limit(Some(
            MAX_DEPENDENCY_ARTIFACT_BYTES
        )));

        let url = reqwest::Url::parse("https://example.invalid/oversize")?;
        let mut reader = Cursor::new(b"123456".to_vec());
        let mut output = Vec::new();
        let error = match copy_bounded(&mut reader, &mut output, 4, &url) {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("oversize body should fail").into()),
        };
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(output, b"12345");
        assert!(error.message().contains("safety limit"));
        Ok(())
    }

    #[test]
    fn passt_version_and_capability_parsing_enforces_authoritative_minimum()
    -> Result<(), Box<dyn std::error::Error>> {
        let minimum = parse_passt_version("passt 2025_02_17.a1e48a0")
            .ok_or_else(|| std::io::Error::other("minimum version should parse"))?;
        let old = parse_passt_version("passt 2025_01_21.4f2c8e7")
            .ok_or_else(|| std::io::Error::other("old version should parse"))?;
        let fedora = parse_passt_version("passt 0^20260728.gf8df3f1-2.fc44.x86_64")
            .ok_or_else(|| std::io::Error::other("Fedora version should parse"))?;
        assert_eq!(minimum.date, MINIMUM_PASST_DATE);
        assert!(old.date < MINIMUM_PASST_DATE);
        assert_eq!(fedora.date, 20_260_728);
        assert_eq!(fedora.raw, "0^20260728.gf8df3f1-2.fc44.x86_64");
        assert!(parse_passt_version("passt 4294967295_12_31.abcdef0").is_none());
        assert!(parse_passt_version("passt 0^20260217.gnothex-2.fc44").is_none());

        let fixture = Fixture::healthy()?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2025_02_17.a1e48a0\\n' ;; --help) printf 'socket mode only\\n' ;; esac",
        )?;
        let check = check_passt(&fixture.context);
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.reason.contains("M3 network minimum"));
        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2025_02_17.a1e48a0\\n' ;; --help) printf '%s\\n' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file' ;; --tcp-ports) exit 23 ;; esac",
        )?;
        let grammar = check_passt(&fixture.context);
        assert_eq!(grammar.status, DoctorStatus::Fail);
        assert!(grammar.reason.contains("grammar probe failed"));
        Ok(())
    }

    #[test]
    fn passt_bare_date_substring_option_and_nonzero_help_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(parse_passt_version("passt 2024_12_11").is_none());
        assert!(parse_passt_version("passt 2024_12_11.not-hash").is_none());

        let fixture = Fixture::healthy()?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2025_02_17.a1e48a0\\n' ;; --help) printf '%s\\n' '--vhost-user-old' ;; esac",
        )?;
        let substring = check_passt(&fixture.context);
        assert_eq!(substring.status, DoctorStatus::Fail);

        write_executable(
            &passt,
            "case \"$1\" in --version) printf 'passt 2025_02_17.a1e48a0\\n' ;; --help) printf '%s\\n' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file --tcp-ports --udp-ports'; exit 2 ;; esac",
        )?;
        let nonzero = check_passt(&fixture.context);
        assert_eq!(nonzero.status, DoctorStatus::Fail);
        assert!(nonzero.reason.contains("--help failed"));
        Ok(())
    }

    #[test]
    fn ubuntu_24_04_passt_hint_names_verified_source_build()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        fs::remove_file(passt)?;
        fs::write(
            &fixture.context.os_release,
            "ID=ubuntu\nVERSION_ID=\"24.04\"\nID_LIKE=debian\n",
        )?;

        let check = check_passt(&fixture.context);

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.fix.is_none());
        let hint = check
            .hint
            .as_deref()
            .ok_or_else(|| std::io::Error::other("Ubuntu passt failure should have a hint"))?;
        for required in [
            "sudo apt-get install -y build-essential ca-certificates git",
            "https://passt.top/passt",
            "2025_02_17.a1e48a0",
            "a1e48a02ff3550eb7875a7df6726086e9b3a1213",
            "sudo install -m0755 passt /usr/local/bin/passt",
        ] {
            assert!(hint.contains(required), "missing {required:?} in {hint:?}");
        }
        Ok(())
    }

    #[test]
    fn distro_families_produce_exact_install_suggestions() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        for (contents, expected) in [
            ("ID=ubuntu\n", "sudo apt-get install qemu-utils"),
            ("ID=fedora\n", "sudo dnf install qemu-img"),
            ("ID=arch\n", "sudo pacman -S qemu-img"),
            (
                "ID=opensuse-leap\nID_LIKE=\"suse opensuse\"\n",
                "sudo zypper install qemu-tools",
            ),
        ] {
            fs::write(&fixture.context.os_release, contents)?;
            assert!(distro_family(&fixture.context.os_release).is_some());
            assert_eq!(
                install_command(&fixture.context, Package::QemuImg).as_deref(),
                Some(expected)
            );
        }
        fs::write(&fixture.context.os_release, "ID=unknown\n")?;
        assert!(install_command(&fixture.context, Package::QemuImg).is_none());
        Ok(())
    }

    #[test]
    fn userns_sysctls_are_diagnosed_without_mutation() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::write(
            &fixture.context.user_namespace_sysctl,
            "0
",
        )?;
        let max_zero = check_user_namespaces(&fixture.context);
        assert_eq!(max_zero.status, DoctorStatus::Fail);
        assert!(max_zero.reason.contains("max_user_namespaces=0"));
        assert!(
            max_zero
                .reason
                .contains("qemu-img does not require user namespaces")
        );
        assert!(
            max_zero
                .hint
                .is_some_and(|hint| hint.contains("never changes"))
        );

        fs::write(
            &fixture.context.user_namespace_sysctl,
            "1024
",
        )?;
        fs::write(
            &fixture.context.unprivileged_userns_clone_sysctl,
            "0
",
        )?;
        let clone_zero = check_user_namespaces(&fixture.context);
        assert_eq!(clone_zero.status, DoctorStatus::Fail);
        assert!(clone_zero.reason.contains("unprivileged_userns_clone=0"));
        Ok(())
    }

    #[test]
    fn passt_later_isolation_failure_proves_userns_succeeded()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let unshare = PathBuf::from(&fixture.context.search_path).join("unshare");
        write_executable(&unshare, "printf generic-unshare-failed >&2; exit 77")?;

        let check = check_user_namespaces(&fixture.context);

        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.reason.contains("later namespace-detach stage"));
        assert!(check.reason.contains("self_seccomp=0"));
        Ok(())
    }

    #[test]
    fn passt_first_stage_failure_is_only_consistent_with_apparmor_without_audit()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::write(
            &fixture.context.apparmor_enabled,
            "Y
",
        )?;
        fs::write(
            &fixture.context.apparmor_restrict_userns,
            "1
",
        )?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            r#"case "$1" in --version) printf 'passt 2025_02_17.a1e48a0
' ;; --help) printf '%s
' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file' ;; --tcp-ports) exit 0 ;; --foreground) printf "Couldn't create user namespace: Permission denied
" >&2; exit 1 ;; *) exit 2 ;; esac"#,
        )?;

        let check = check_user_namespaces(&fixture.context);

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(
            check
                .reason
                .contains("consistent with AppArmor but not confirmed")
        );
        assert_eq!(check.fix.as_deref(), Some("firestone doctor --fix"));
        Ok(())
    }

    #[test]
    fn passt_userns_stderr_is_bounded_and_marks_truncation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::write(
            &fixture.context.apparmor_enabled,
            "Y
",
        )?;
        fs::write(
            &fixture.context.apparmor_restrict_userns,
            "1
",
        )?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        write_executable(
            &passt,
            r#"if [ "$1" = "--version" ]; then
  printf 'passt 2025_02_17.a1e48a0
'
elif [ "$1" = "--help" ]; then
  printf '%s
' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file'
elif [ "$1" = "--tcp-ports" ]; then
  exit 0
elif [ "$1" = "--foreground" ]; then
  index=0
  while [ "$index" -lt 20000 ]; do printf x >&2; index=$((index + 1)); done
  printf "Couldn't create user namespace: Permission denied
" >&2
  exit 1
fi"#,
        )?;

        let check = check_user_namespaces(&fixture.context);

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.reason.contains("stderr truncated at 16384 bytes"));
        assert!(check.reason.len() < super::PASST_USERNS_STDERR_BYTES + 2_048);
        Ok(())
    }
    #[test]
    fn correlated_passt_pid_audit_record_confirms_apparmor_denial()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::write(
            &fixture.context.apparmor_enabled,
            "Y
",
        )?;
        fs::write(
            &fixture.context.apparmor_restrict_userns,
            "1
",
        )?;
        let passt = PathBuf::from(&fixture.context.search_path).join("passt");
        let audit = &fixture.context.audit_logs[0];
        write_executable(
            &passt,
            &format!(
                r#"case "$1" in --version) printf 'passt 2025_02_17.a1e48a0
' ;; --help) printf '%s
' '--foreground --one-off --vhost-user --socket-path --repair-path --log-file' ;; --tcp-ports) exit 0 ;; --foreground) printf 'apparmor="DENIED" operation="userns_create" pid=%s comm="passt"
' "$$" > '{}'; printf "Couldn't create user namespace: Permission denied
" >&2; exit 1 ;; *) exit 2 ;; esac"#,
                audit.display()
            ),
        )?;

        let check = check_user_namespaces(&fixture.context);

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.reason.contains("confirmed AppArmor userns denial"));
        assert!(check.reason.contains(&audit.display().to_string()));
        Ok(())
    }
    #[test]
    fn verified_passt_requires_exact_bytes_root_identity_and_loaded_literal_profile()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        let source = provide_extracted_passt(&mut fixture)?;
        let artifact = fixture.context.manifest.embedded_passt("x86_64")?;
        fs::write(
            &fixture.context.apparmor_passt_executable,
            fs::read(source)?,
        )?;
        fs::set_permissions(
            &fixture.context.apparmor_passt_executable,
            fs::Permissions::from_mode(0o755),
        )?;
        fs::write(
            &fixture.context.apparmor_passt_profile,
            APPARMOR_PASST_PROFILE_CONTENT,
        )?;
        fs::set_permissions(
            &fixture.context.apparmor_passt_profile,
            fs::Permissions::from_mode(0o644),
        )?;
        fs::write(
            &fixture.context.apparmor_loaded_profiles,
            format!(
                "{} (unconfined)
",
                super::APPARMOR_PASST_PROFILE_NAME
            ),
        )?;

        let verified = super::resolve_verified_apparmor_passt_at(
            &fixture.context.apparmor_passt_executable,
            &fixture.context.apparmor_passt_profile,
            &fixture.context.apparmor_loaded_profiles,
            &artifact,
            fixture.context.expected_root_uid,
        )?
        .ok_or("verified passt was not resolved")?;
        assert_eq!(
            verified.executable(),
            fixture.context.apparmor_passt_executable
        );
        assert_eq!(verified.profile_name(), super::APPARMOR_PASST_PROFILE_NAME);
        assert!(
            APPARMOR_PASST_PROFILE_CONTENT
                .starts_with("abi <abi/4.0>,\ninclude <tunables/global>\n\n")
        );
        assert!(APPARMOR_PASST_PROFILE_CONTENT.contains(super::APPARMOR_PASST_EXECUTABLE));
        assert!(!APPARMOR_PASST_PROFILE_CONTENT.contains('*'));
        let path_passt = PathBuf::from(&fixture.context.search_path).join("passt");
        fs::remove_file(path_passt)?;
        let passt_check = check_passt(&fixture.context);
        assert_eq!(passt_check.status, DoctorStatus::Ok, "{passt_check:#?}");
        assert!(passt_check.reason.contains(crate::PINNED_PASST_VERSION));

        fs::set_permissions(
            &fixture.context.apparmor_passt_executable,
            fs::Permissions::from_mode(0o777),
        )?;
        let error = super::resolve_verified_apparmor_passt_at(
            &fixture.context.apparmor_passt_executable,
            &fixture.context.apparmor_passt_profile,
            &fixture.context.apparmor_loaded_profiles,
            &artifact,
            fixture.context.expected_root_uid,
        )
        .err()
        .ok_or("world-writable passt target unexpectedly verified")?;
        assert!(error.message().contains("non-writable regular file"));
        Ok(())
    }
    #[test]
    fn noninteractive_apparmor_fix_refuses_elevation_with_exact_commands()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        configure_apparmor_passt_denial(&fixture)?;
        let source = provide_extracted_passt(&mut fixture)?;
        let before_max = fs::read(&fixture.context.user_namespace_sysctl)?;
        let fetcher = FakeFetcher::new(fixture.payloads.clone());

        let report = run_doctor_with_options(&fixture.context, DoctorOptions::fix(false), &fetcher);

        let userns = check(&report, DoctorCheckId::UserNamespaces);
        assert_eq!(userns.status, DoctorStatus::Fail);
        assert!(
            userns
                .reason
                .contains("elevation was not explicitly confirmed")
        );
        let commands = userns
            .fix
            .as_deref()
            .ok_or("manual AppArmor commands were not rendered")?;
        for required in [
            "sudo install -d -o root -g root -m 0755",
            "sudo install -o root -g root -m 0755",
            "sudo install -o root -g root -m 0644",
            "sudo apparmor_parser -r",
            &source.display().to_string(),
        ] {
            assert!(
                commands.contains(required),
                "missing {required:?} in {commands:?}"
            );
        }
        assert_eq!(
            fs::read(fixture.context.paths.apparmor_passt_staged_profile())?,
            APPARMOR_PASST_PROFILE_CONTENT.as_bytes()
        );
        assert!(!fixture.privileged_marker.exists());
        assert_eq!(
            fs::read(&fixture.context.user_namespace_sysctl)?,
            before_max
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn confirmed_apparmor_fix_routes_fake_privileged_commands_and_rechecks()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        configure_apparmor_passt_denial(&fixture)?;
        let source = provide_extracted_passt(&mut fixture)?;
        let artifact = fixture.context.manifest.embedded_passt("x86_64")?;
        let fake_bin = PathBuf::from(&fixture.context.search_path);
        let marker = &fixture.privileged_marker;
        write_executable(
            &fake_bin.join("sudo"),
            &format!(
                r#"printf 'sudo %s
' "$*" >> '{}'
if [ "$1" = "--" ]; then shift; fi
exec "$@""#,
                marker.display()
            ),
        )?;
        write_executable(
            &fake_bin.join("install"),
            &format!(
                r#"printf 'install %s
' "$*" >> '{}'
last=
previous=
for argument in "$@"; do previous=$last; last=$argument; done
case " $* " in
  *" -d "*) mkdir -p "$last"; chmod 0755 "$last" ;;
  *" -m 0755 "*) cp "$previous" "$last"; chmod 0755 "$last" ;;
  *) cp "$previous" "$last"; chmod 0644 "$last" ;;
esac"#,
                marker.display()
            ),
        )?;
        write_executable(
            &fake_bin.join("apparmor_parser"),
            &format!(
                r#"printf 'apparmor_parser %s
' "$*" >> '{}'
printf '{} (unconfined)
' > '{}'"#,
                marker.display(),
                super::APPARMOR_PASST_PROFILE_NAME,
                fixture.context.apparmor_loaded_profiles.display()
            ),
        )?;
        let before_max = fs::read(&fixture.context.user_namespace_sysctl)?;
        let fetcher = FakeFetcher::new(fixture.payloads.clone());

        let report = run_doctor_with_options(&fixture.context, DoctorOptions::fix(true), &fetcher);

        let userns = check(&report, DoctorCheckId::UserNamespaces);
        assert_eq!(userns.status, DoctorStatus::Ok, "{userns:#?}");
        assert!(userns.reason.contains("userns creation confirmed"));
        assert_eq!(
            fs::read(&fixture.context.apparmor_passt_executable)?,
            fs::read(source)?
        );
        assert_eq!(
            super::hash_file(&fixture.context.apparmor_passt_executable, "test")?,
            artifact.sha256
        );
        let target = fs::metadata(&fixture.context.apparmor_passt_executable)?;
        assert_eq!(target.uid(), fixture.context.expected_root_uid);
        assert_eq!(target.permissions().mode() & 0o022, 0);
        let invocations = fs::read_to_string(marker)?;
        assert!(invocations.contains("install -d"));
        assert!(invocations.contains("apparmor_parser -r"));
        assert_eq!(
            fs::read(&fixture.context.user_namespace_sysctl)?,
            before_max
        );
        Ok(())
    }
    #[test]
    fn kvm_open_failure_reports_detected_device_group_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.kvm_device = fixture.context.paths.runtime_dir().to_path_buf();

        let kvm = check_kvm(&fixture.context);

        assert_eq!(kvm.check.status, DoctorStatus::Fail);
        assert_eq!(kvm.check.fix.as_deref(), Some("sudo usermod -aG kvm $USER"));
        assert!(kvm.check.reason.contains("device group is kvm"));
        Ok(())
    }

    #[test]
    fn directory_fix_rejects_symlink_without_changing_target_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let target = fixture.context.paths.data_dir().join("directory-target");
        let link = fixture.context.paths.data_dir().join("directory-link");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        std::os::unix::fs::symlink(&target, &link)?;

        let error = match create_firestone_dir(&fixture.context.paths, &link, "test directory") {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("directory symlink should fail").into()),
        };

        assert!(error.message().contains("symbolic link"));
        assert_eq!(fs::metadata(target)?.permissions().mode() & 0o777, 0o755);
        Ok(())
    }
    #[test]
    fn doctor_unsafe_data_directory_reports_thirteen_checks_without_escape()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        fs::set_permissions(
            fixture.context.paths.data_dir(),
            fs::Permissions::from_mode(0o777),
        )?;

        let report = super::run_doctor(&fixture.context, DoctorOptions::inspect())?;

        assert_eq!(report.checks.len(), 13);
        assert_eq!(
            check(&report, DoctorCheckId::DataSpace).status,
            DoctorStatus::Fail
        );
        assert!(
            check(&report, DoctorCheckId::DataSpace)
                .reason
                .contains("group/world write access")
        );
        Ok(())
    }
    #[test]
    fn doctor_symlinked_binary_directory_reports_unsafe_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let bin_dir = fixture.context.paths.bin_dir();
        let outside = fixture.context.paths.data_dir().join("outside-bin");
        fs::rename(&bin_dir, &outside)?;
        std::os::unix::fs::symlink(&outside, &bin_dir)?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        for id in [DoctorCheckId::VendoredBinaries, DoctorCheckId::Virtiofsd] {
            let dependency = check(&report, id);
            assert_eq!(dependency.status, DoctorStatus::Fail);
            assert!(dependency.reason.contains("symbolic link"));
        }
        Ok(())
    }

    #[test]
    fn doctor_symlinked_ssh_directory_reports_unsafe_path() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        let ssh_dir = fixture.context.paths.ssh_dir();
        let outside = fixture.context.paths.data_dir().join("outside-ssh");
        fs::rename(&ssh_dir, &outside)?;
        std::os::unix::fs::symlink(&outside, &ssh_dir)?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        let ssh = check(&report, DoctorCheckId::SshKey);
        assert_eq!(ssh.status, DoctorStatus::Fail);
        assert!(ssh.reason.contains("symbolic link"));
        Ok(())
    }

    #[test]
    fn doctor_permissive_machine_directory_reports_stale_state_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let name = "unsafe-machine";
        write_running_state(&fixture.context.paths, name)?;
        fs::set_permissions(
            fixture.context.paths.machine_dir(name)?,
            fs::Permissions::from_mode(0o777),
        )?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        let stale = check(&report, DoctorCheckId::StaleState);
        assert_eq!(stale.status, DoctorStatus::Fail);
        assert!(stale.reason.contains(name));
        assert!(stale.reason.contains("group/world write access"));
        assert_eq!(
            StateStore::new(fixture.context.paths.machine_state(name)?)
                .read()?
                .status,
            MachineStatus::Running
        );
        Ok(())
    }
    #[test]
    fn ssh_key_complete_symlink_pair_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let private_target = fixture.context.paths.data_dir().join("private-target");
        let public_target = fixture.context.paths.data_dir().join("public-target");
        fs::write(&private_target, "private")?;
        fs::write(&public_target, "public")?;
        fs::set_permissions(&private_target, fs::Permissions::from_mode(0o600))?;
        fs::remove_file(fixture.context.paths.ssh_private_key())?;
        fs::remove_file(fixture.context.paths.ssh_public_key())?;
        std::os::unix::fs::symlink(&private_target, fixture.context.paths.ssh_private_key())?;
        std::os::unix::fs::symlink(&public_target, fixture.context.paths.ssh_public_key())?;

        let report = run_doctor_with(
            &fixture.context,
            true,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Fail
        );
        assert!(
            fs::symlink_metadata(fixture.context.paths.ssh_private_key())?
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(private_target)?, b"private");
        Ok(())
    }

    #[test]
    fn stale_machine_states_reconcile_in_sorted_order() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        write_running_state(&fixture.context.paths, "zeta")?;
        write_running_state(&fixture.context.paths, "alpha")?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        let stale = check(&report, DoctorCheckId::StaleState);
        assert_eq!(stale.status, DoctorStatus::Ok);
        assert!(
            stale.reason.find("alpha").is_some_and(|alpha| {
                stale.reason.find("zeta").is_some_and(|zeta| alpha < zeta)
            })
        );
        for name in ["alpha", "zeta"] {
            let state: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.context.paths.machine_state(name)?)?)?;
            assert_eq!(state["status"], "stopped");
            assert_eq!(state["last_exit"]["reason"], "host reboot");
        }
        Ok(())
    }

    #[test]
    fn live_read_ping_without_shim_reports_unsupervised_without_rewrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let name = "live";
        write_running_state(&fixture.context.paths, name)?;
        fs::create_dir(fixture.context.paths.machine_runtime_dir(name)?)?;
        let store = StateStore::new(fixture.context.paths.machine_state(name)?);
        let mut stored = store.read()?;
        stored.status = MachineStatus::Starting;
        store.write_from_shim(&stored)?;
        let mut events = Vec::new();
        let lock = MachineLock::acquire(
            name,
            &fixture.context.paths.machine_lock(name)?,
            &mut events,
        )?;

        let (effective, reason) = read_reconciled_machine_state_with(
            &fixture.context.paths,
            name,
            &fixture.context.proc_root,
            &fixture.context.reconciled_at,
            &FixedPing(true),
            Some(&lock),
        )?;

        assert_eq!(effective.state.status, MachineStatus::Running);
        assert_eq!(
            effective.supervision,
            Some(crate::Supervision::Unsupervised)
        );
        assert_eq!(reason, None);
        assert_eq!(store.read()?.status, MachineStatus::Starting);
        drop(lock);
        Ok(())
    }

    #[test]
    fn live_read_stale_state_with_held_lock_rewrites_without_contention()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let name = "locked-stale";
        write_running_state(&fixture.context.paths, name)?;
        let store = StateStore::new(fixture.context.paths.machine_state(name)?);
        let mut events = Vec::new();
        let lock = MachineLock::acquire(
            name,
            &fixture.context.paths.machine_lock(name)?,
            &mut events,
        )?;

        let (effective, reason) = read_reconciled_machine_state_with(
            &fixture.context.paths,
            name,
            &fixture.context.proc_root,
            &fixture.context.reconciled_at,
            &FixedPing(false),
            Some(&lock),
        )?;

        assert_eq!(effective.state.status, MachineStatus::Stopped);
        assert_eq!(reason.as_deref(), Some("host reboot"));
        assert_eq!(store.read()?.status, MachineStatus::Stopped);
        Ok(())
    }
    #[test]
    fn reconciliation_rereads_after_shim_final_state_write()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let name = "race";
        write_running_state(&fixture.context.paths, name)?;
        fs::create_dir(fixture.context.paths.machine_runtime_dir(name)?)?;
        let store = StateStore::new(fixture.context.paths.machine_state(name)?);
        let mut final_state = store.read()?;
        final_state.status = MachineStatus::Stopped;
        final_state.started_at = None;
        final_state.last_exit = Some(LastExit {
            at: "2026-08-28T00:00:01Z".to_owned(),
            code: Some(17),
            signal: None,
            reason: ExitReason::Failure("vmm exited".to_owned()),
        });
        let expected = final_state.clone();
        let ping = FinalStateDuringPing {
            store: store.clone(),
            state: final_state,
            wrote: Cell::new(false),
        };

        let observation = reconcile_machine_state(&fixture.context, name, &ping)?;

        assert!(observation.is_none());
        assert_eq!(store.read()?, expected);
        Ok(())
    }

    #[test]
    fn stale_machine_failure_does_not_abort_later_reconciliation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        write_running_state(&fixture.context.paths, "zeta")?;
        fs::create_dir(fixture.context.paths.machine_dir("alpha")?)?;
        fs::write(fixture.context.paths.machine_state("alpha")?, b"{")?;

        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        let stale = check(&report, DoctorCheckId::StaleState);
        assert_eq!(stale.status, DoctorStatus::Fail);
        assert!(stale.reason.contains("alpha"));
        assert!(stale.reason.contains("zeta (host reboot)"));
        let zeta: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.context.paths.machine_state("zeta")?)?)?;
        assert_eq!(zeta["status"], "stopped");
        assert!(
            report
                .checks
                .iter()
                .filter(|check| check.id != DoctorCheckId::StaleState)
                .all(|check| check.status == DoctorStatus::Ok)
        );
        Ok(())
    }
    #[test]
    fn report_error_reason_includes_bounded_source_context() {
        let error = FirestoneError::new(ErrorKind::Dependency, "cannot run probe")
            .with_source(std::io::Error::from(std::io::ErrorKind::PermissionDenied));

        let reason = firestone_error_reason(&error);

        assert!(reason.contains("cannot run probe"));
        assert!(reason.contains("permission denied"));
    }

    #[test]
    fn fix_incomplete_ssh_key_does_not_overwrite_existing_private_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let original = fs::read(fixture.context.paths.ssh_private_key())?;
        fs::remove_file(fixture.context.paths.ssh_public_key())?;

        let report = run_doctor_with(
            &fixture.context,
            true,
            &FakeFetcher::new(fixture.payloads.clone()),
        );

        assert_eq!(
            check(&report, DoctorCheckId::SshKey).status,
            DoctorStatus::Fail
        );
        assert_eq!(fs::read(fixture.context.paths.ssh_private_key())?, original);
        assert!(!fixture.context.paths.ssh_public_key().exists());
        Ok(())
    }

    #[test]
    fn failed_ssh_key_generation_rolls_back_and_can_retry() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::healthy()?;
        let private_key = fixture.context.paths.ssh_private_key();
        let public_key = fixture.context.paths.ssh_public_key();
        fs::remove_file(&private_key)?;
        fs::remove_file(&public_key)?;
        let keygen = PathBuf::from(&fixture.context.search_path).join("ssh-keygen");
        write_executable(
            &keygen,
            r#"key=''; while [ "$#" -gt 0 ]; do if [ "$1" = '-f' ]; then shift; key=$1; fi; shift; done; printf private > "$key"; exit 23"#,
        )?;

        assert!(generate_ssh_key(&fixture.context).is_err());
        assert!(!private_key.exists());
        assert!(!public_key.exists());

        write_executable(
            &keygen,
            r#"key=''; while [ "$#" -gt 0 ]; do if [ "$1" = '-f' ]; then shift; key=$1; fi; shift; done; [ -n "$key" ] || exit 2; umask 077; printf private > "$key"; printf 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P firestone@test\n' > "$key.pub""#,
        )?;
        generate_ssh_key(&fixture.context)?;

        assert!(private_key.is_file());
        assert!(public_key.is_file());
        assert_eq!(
            fs::metadata(private_key)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn fix_stream_failure_retains_failed_check_and_complete_report()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::healthy()?;
        let artifact = fixture
            .context
            .manifest
            .artifact("cloud-hypervisor", "x86_64")?;
        fs::remove_file(fixture.context.paths.bin_dir().join(&artifact.install_name))?;
        let fetcher = FakeFetcher::failing(fixture.payloads.clone());

        let report = run_doctor_with(&fixture.context, true, &fetcher);

        assert_eq!(report.checks.len(), 13);
        let vendored = check(&report, DoctorCheckId::VendoredBinaries);
        assert_eq!(vendored.status, DoctorStatus::Fail);
        assert!(
            vendored.reason.contains(
                "fix failed: cannot stream download for dependency `cloud-hypervisor` into"
            )
        );
        assert!(
            vendored
                .reason
                .contains(&fixture.context.paths.bin_dir().display().to_string())
        );
        let hint = vendored
            .hint
            .as_deref()
            .ok_or("stream fix failure should retain its install hint")?;
        assert!(hint.contains("read/write permissions"));
        assert!(hint.contains("free space"));
        assert!(
            fs::read_dir(fixture.context.paths.bin_dir())?
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".partial"))
        );
        Ok(())
    }

    #[test]
    fn doctor_bundled_virtiofsd_missing_binary_reports_fix()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::healthy()?;
        fixture.context.manifest = DependencyManifest::bundled()?;
        let report = run_doctor_with(
            &fixture.context,
            false,
            &FakeFetcher::new(fixture.payloads.clone()),
        );
        let check = check(&report, DoctorCheckId::Virtiofsd);
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.reason.contains("virtiofsd-v1.14.0"));
        assert!(check.reason.contains("checksum mismatch"));
        assert_eq!(check.fix.as_deref(), Some("firestone doctor --fix"));
        assert!(check.hint.is_none());
        Ok(())
    }
}
