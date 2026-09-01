//! View models for the embedded web UI.
//!
//! Nothing here talks to the host. Each type is built purely from a shared
//! action result — the same payload the REST routes serialize — and carries
//! only what a template needs. Presentation decisions (which action a row
//! offers, how a byte count reads) live here so templates stay declarative,
//! and status is always emitted as a stable token, never as a colour.

use std::net::IpAddr;

use firestone_core::{
    CloudInitSpec, DoctorCheck, DoctorReport, DoctorStatus, MachineSpec, MachineState,
    MachineStatus, MachineSummary, MachineView, MountSpec, NetMode, NetworkSpec, PortForward,
    Protocol, SnapshotKind, SnapshotSummary, VersionResult, VmmSpec,
};
use serde::{Deserialize, Serialize};

/// The parts of a stored image the UI reads.
///
/// `StoredImage` is serialize-only in the shared crate, so rather than widen a
/// core type for a presentation concern the UI decodes just these fields from
/// the same `images-ls` payload the REST route returns. Unknown fields are
/// ignored, so the core type stays free to grow.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CachedImage {
    pub metadata: CachedImageMetadata,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CachedImageMetadata {
    pub id: String,
    pub source_ref: String,
    pub size: u64,
    /// `oci` or `disk`. The sidecar omits the field for a disk image (§8.5),
    /// so an absent value is a disk image rather than an unknown one.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub pulled_at: String,
}

/// Build identity shown in the sidebar.
#[derive(Debug, Clone, Serialize)]
pub struct VersionInfo {
    pub version: String,
    pub architecture: String,
    pub identity: String,
    pub paths: VersionPathsInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionPathsInfo {
    pub config: String,
    pub data: String,
    pub runtime: String,
}

impl From<&VersionResult> for VersionInfo {
    fn from(result: &VersionResult) -> Self {
        let commit = result
            .identity
            .git_commit
            .as_deref()
            .map_or_else(String::new, |commit| format!(" · {commit}"));
        Self {
            version: result.version.clone(),
            architecture: result.architecture.clone(),
            identity: format!(
                "firestone {} · release {}{commit}",
                result.version, result.identity.release
            ),
            paths: VersionPathsInfo {
                config: result.paths.config.clone(),
                data: result.paths.data.clone(),
                runtime: result.paths.runtime.clone(),
            },
        }
    }
}

impl VersionInfo {
    /// Placeholder identity for chrome rendered when the version action is
    /// itself unavailable. The frame must never depend on a successful read.
    pub fn unknown() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            architecture: "unknown".to_owned(),
            identity: format!("firestone {}", env!("CARGO_PKG_VERSION")),
            paths: VersionPathsInfo {
                config: String::new(),
                data: String::new(),
                runtime: String::new(),
            },
        }
    }
}

/// Rolled-up doctor state for the top-bar pill and the overview banner.
#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub status: &'static str,
    pub label: &'static str,
    pub counts: String,
    pub summary: String,
    pub fail_count: usize,
    pub fail_noun: &'static str,
}

impl From<&DoctorReport> for HostInfo {
    fn from(report: &DoctorReport) -> Self {
        let mut ok = 0usize;
        let mut warn = 0usize;
        let mut fail = 0usize;
        for check in &report.checks {
            match check.status {
                DoctorStatus::Ok => ok += 1,
                DoctorStatus::Warn => warn += 1,
                DoctorStatus::Fail => fail += 1,
            }
        }

        // The pill reports the worst finding, because that is the one that
        // decides whether a machine can start.
        let (status, label) = if fail > 0 {
            ("fail", "Host blocked")
        } else if warn > 0 {
            ("warn", "Host ready")
        } else {
            ("ok", "Host ready")
        };

        let mut counts = format!("{ok} ok");
        if warn > 0 {
            counts.push_str(&format!(" · {warn} warn"));
        }
        if fail > 0 {
            counts.push_str(&format!(" · {fail} fail"));
        }

        Self {
            status,
            label,
            counts,
            summary: report
                .checks
                .iter()
                .filter(|check| check.status != DoctorStatus::Ok)
                .map(|check| format!("{}: {}", check_id(check), check.reason))
                .collect::<Vec<_>>()
                .join("\n"),
            fail_count: fail,
            fail_noun: if fail == 1 { "check is" } else { "checks are" },
        }
    }
}

impl HostInfo {
    /// Shown when doctor could not run at all. Deliberately not "ok": an
    /// unknown host is not a healthy one, and claiming otherwise would be the
    /// single most misleading thing this UI could say.
    pub fn unknown() -> Self {
        Self {
            status: "warn",
            label: "Host unknown",
            counts: "checks unavailable".to_owned(),
            summary: "doctor could not run on this host".to_owned(),
            fail_count: 0,
            fail_noun: "checks are",
        }
    }
}

/// One doctor row.
#[derive(Debug, Clone, Serialize)]
pub struct CheckInfo {
    pub id: String,
    pub status: &'static str,
    pub reason: String,
    pub hint: Option<String>,
}

impl From<&DoctorCheck> for CheckInfo {
    fn from(check: &DoctorCheck) -> Self {
        Self {
            id: check_id(check),
            status: match check.status {
                DoctorStatus::Ok => "ok",
                DoctorStatus::Warn => "warn",
                DoctorStatus::Fail => "fail",
            },
            reason: check.reason.clone(),
            hint: check.hint.clone().or_else(|| check.fix.clone()),
        }
    }
}

fn check_id(check: &DoctorCheck) -> String {
    serde_json::to_value(check.id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "check".to_owned())
}

/// One overview headline number.
#[derive(Debug, Clone, Serialize)]
pub struct Stat {
    pub label: &'static str,
    pub value: String,
    pub sub: String,
    pub icon: &'static str,
}

const ICON_MACHINES: &str = "M8 1.8 14 5v6L8 14.2 2 11V5ZM2 5l6 3.2L14 5M8 8.2v6";
const ICON_CPU: &str = "M4.5 4.5h7v7h-7ZM8 1.5v3M8 11.5v3M1.5 8h3M11.5 8h3";
const ICON_MEMORY: &str = "M2.5 5h11v5.5h-11ZM5 10.5v2M8 10.5v2M11 10.5v2";
const ICON_IMAGES: &str = "M8 1.8 14 5 8 8.2 2 5ZM2 8l6 3.2L14 8M2 11l6 3.2L14 11";

/// Builds the four overview statistics.
///
/// Every number is derived from what Firestone actually knows. Host capacity
/// is deliberately absent: no shared result reports total host CPU or memory,
/// so the cards report what is allocated rather than inventing a denominator.
pub fn stats(machines: &[MachineSummary], images: &[CachedImage]) -> Vec<Stat> {
    let running: Vec<&MachineSummary> = machines
        .iter()
        .filter(|machine| machine.status == "running")
        .collect();
    let idle = machines.len().saturating_sub(running.len());
    let cpus: u32 = running.iter().map(|machine| u32::from(machine.cpus)).sum();
    let memory_mib: u64 = running
        .iter()
        .filter_map(|machine| parse_size_mib(&machine.memory))
        .sum();
    let cache_bytes: u64 = images.iter().map(|image| image.metadata.size).sum();

    vec![
        Stat {
            label: "Machines",
            value: machines.len().to_string(),
            sub: format!("{} running · {idle} idle", running.len()),
            icon: ICON_MACHINES,
        },
        Stat {
            label: "vCPUs allocated",
            value: cpus.to_string(),
            sub: format!(
                "across {} running {}",
                running.len(),
                plural(running.len(), "machine")
            ),
            icon: ICON_CPU,
        },
        Stat {
            label: "Memory allocated",
            value: format_mib(memory_mib),
            sub: format!(
                "across {} running {}",
                running.len(),
                plural(running.len(), "machine")
            ),
            icon: ICON_MEMORY,
        },
        Stat {
            label: "Image cache",
            value: format_bytes(cache_bytes),
            sub: format!("{} {} stored", images.len(), plural(images.len(), "image")),
            icon: ICON_IMAGES,
        },
    ]
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

/// One machines-table row.
#[derive(Debug, Clone, Serialize)]
pub struct MachineRow {
    pub name: String,
    pub status: String,
    pub image: String,
    pub resources: String,
    pub forwards_text: String,
    /// The applied forwards as chips, linkified where a browser can follow
    /// them (§16.5). Built from the same strings `forwards_text` reports.
    pub forwards: Vec<ForwardChip>,
    /// The spec configures forwards this running machine has not applied
    /// (§12.5). Rendered beside the chips, never instead of them.
    pub forwards_pending: bool,
    /// Whether `clone` would be accepted right now (§24.2: the source must be
    /// `created` or `stopped`). The menu item is offered disabled otherwise,
    /// so the refusal is read before the round trip rather than after it.
    pub clonable: bool,
    pub uptime: String,
    /// `None` while the machine is mid-transition: offering a lifecycle button
    /// against a state the server is still resolving is how a UI lies.
    pub action: Option<&'static str>,
    pub action_label: &'static str,
}

impl From<&MachineSummary> for MachineRow {
    fn from(summary: &MachineSummary) -> Self {
        let (action, action_label) = row_action(&summary.status);
        Self {
            name: summary.name.clone(),
            status: summary.status.clone(),
            image: summary.image.clone(),
            resources: format!("{} vCPU · {}", summary.cpus, summary.memory),
            forwards_text: if summary.forwards.is_empty() {
                "—".to_owned()
            } else {
                summary.forwards.join("  ")
            },
            // Only a running machine's forwards are reachable, so only those
            // become links: a chip on a stopped machine would be a dead one.
            forwards: forward_chips(&summary.forwards, summary.status == "running"),
            forwards_pending: summary.forwards_pending,
            clonable: is_clonable(&summary.status),
            uptime: summary.uptime.clone().unwrap_or_else(|| "—".to_owned()),
            action,
            action_label,
        }
    }
}

fn row_action(status: &str) -> (Option<&'static str>, &'static str) {
    match status {
        "running" => (Some("stop"), "Stop"),
        "starting" | "stopping" => (None, "···"),
        _ => (Some("start"), "Start"),
    }
}

/// One compact overview row.
#[derive(Debug, Clone, Serialize)]
pub struct OverviewMachine {
    pub name: String,
    pub status: String,
    pub image: String,
    pub note: String,
    /// Whether this row carries the client-side CPU poll (§16.5).
    pub metrics: bool,
}

impl From<&MachineSummary> for OverviewMachine {
    fn from(summary: &MachineSummary) -> Self {
        Self {
            name: summary.name.clone(),
            status: summary.status.clone(),
            image: summary.image.clone(),
            note: match (summary.status.as_str(), summary.uptime.as_deref()) {
                ("running", Some(uptime)) => format!("up {uptime}"),
                (status, _) => status.to_owned(),
            },
            metrics: false,
        }
    }
}

/// How many overview rows may poll `GET /v1/machines/{name}/metrics`.
///
/// Every polling row is one request every five seconds, so an unbounded fleet
/// would turn a glance at the overview into steady load on the very host the
/// numbers describe. Eight is the cap; the rest of the fleet reports status
/// and uptime only, and the detail page reports the whole picture for one
/// machine. Normative in SPEC §16.5.
pub const OVERVIEW_METRICS_CAP: usize = 8;

/// Builds the overview rows, marking the first `OVERVIEW_METRICS_CAP` running
/// machines as the ones that poll for utilization.
///
/// The order is the order the list action returned, so the same machines are
/// marked on every poll rather than rotating under the reader.
pub fn overview_machines(machines: &[MachineSummary]) -> Vec<OverviewMachine> {
    let mut remaining = OVERVIEW_METRICS_CAP;
    machines
        .iter()
        .map(|summary| {
            let mut row = OverviewMachine::from(summary);
            if row.status == "running" && remaining > 0 {
                row.metrics = true;
                remaining -= 1;
            }
            row
        })
        .collect()
}

/// A key/value pair in the detail meta strip or a spec group.
#[derive(Debug, Clone, Serialize)]
pub struct Pair {
    pub key: &'static str,
    pub value: String,
}

/// One titled block of spec rows.
#[derive(Debug, Clone, Serialize)]
pub struct SpecGroup {
    pub title: &'static str,
    pub rows: Vec<Pair>,
}

/// Everything the detail screen renders for one machine.
#[derive(Debug, Clone, Serialize)]
pub struct MachineDetail {
    pub name: String,
    pub status: String,
    pub image: String,
    pub degraded: Option<String>,
    pub action: Option<&'static str>,
    pub action_label_long: &'static str,
    pub is_running: bool,
    pub meta: Vec<Pair>,
    pub spec_groups: Vec<SpecGroup>,
    /// Spec forwards this running machine has not applied yet (§12.4).
    pub forwards_pending: bool,
    /// The applied forwards as chips, linkified while the machine runs.
    pub forwards: Vec<ForwardChip>,
    /// Whether `clone` would be accepted right now (§24.2).
    pub clonable: bool,
    /// Editable spec fields that observably differ from what the running
    /// instance is using, named as an operator would name them.
    ///
    /// Only fields `state.json` actually records can be compared, so this is
    /// deliberately narrow and never a claim that the rest agrees: see
    /// [`observable_drift`].
    pub drift: Vec<&'static str>,
    pub drift_summary: String,
}

impl MachineDetail {
    pub fn new(name: &str, view: &MachineView) -> Self {
        let status = status_token(view.state.status);
        let (action, action_label_long) = match view.state.status {
            MachineStatus::Running => (Some("stop"), "Stop"),
            MachineStatus::Starting => (None, "Starting…"),
            MachineStatus::Stopping => (None, "Stopping…"),
            _ => (Some("start"), "Start"),
        };

        let drift = observable_drift(view);
        Self {
            name: name.to_owned(),
            status: status.to_owned(),
            image: view.state.image.r#ref.clone(),
            degraded: if view.state.degraded.is_empty() {
                None
            } else {
                Some(view.state.degraded.join(", "))
            },
            action,
            action_label_long,
            is_running: view.state.status == MachineStatus::Running,
            meta: meta_rows(&view.state, view.supervision.is_some()),
            spec_groups: spec_groups(&view.spec),
            forwards_pending: view.forwards_pending,
            // The applied set, exactly as §12.5 defines it: what a client can
            // reach right now, never the configured set the spec holds.
            forwards: forward_chips(
                &view.state.forwards,
                view.state.status == MachineStatus::Running,
            ),
            clonable: is_clonable(status),
            drift_summary: drift.join(", "),
            drift,
        }
    }
}

/// Spec fields that provably disagree with the machine that is running.
///
/// `state.json` records the image reference, the MAC and the forwards the
/// running instance actually applied, and nothing else about the spec. So this
/// reports exactly those three and stays silent about the rest rather than
/// guessing: an empty list means "nothing observable has drifted", never
/// "the running machine matches the spec". Changes to fields the state does not
/// record — cpus, memory, disk, user, mounts, network mode — are announced by
/// the edit dialog itself when it saves them against a running machine, and
/// that client-side marker is cleared the next time the machine starts.
///
/// Image and forward drift are decided by the dispatcher rather than here: both
/// need a canonical comparison this projection cannot make. `state.image.ref`
/// holds the catalog's canonical `distro:version`, so `ubuntu` in the spec is
/// the same image as `ubuntu:24.04` in the state, and only the side holding the
/// catalog can say so (§8.2).
fn observable_drift(view: &MachineView) -> Vec<&'static str> {
    if view.state.status != MachineStatus::Running {
        return Vec::new();
    }
    let mut drift = Vec::new();
    if view.image_pending {
        drift.push("image");
    }
    if view.forwards_pending {
        drift.push("port forwards");
    }
    if let (Some(spec_mac), Some(state_mac)) =
        (view.spec.network.mac.as_ref(), view.state.mac.as_deref())
        && !spec_mac.to_string().eq_ignore_ascii_case(state_mac)
    {
        drift.push("mac");
    }
    drift
}

fn status_token(status: MachineStatus) -> &'static str {
    match status {
        MachineStatus::Created => "created",
        MachineStatus::Starting => "starting",
        MachineStatus::Running => "running",
        MachineStatus::Stopping => "stopping",
        MachineStatus::Stopped => "stopped",
        MachineStatus::Failed => "failed",
    }
}

fn meta_rows(state: &MachineState, supervised: bool) -> Vec<Pair> {
    let mut rows = vec![
        Pair {
            key: "cid",
            value: state.cid.to_string(),
        },
        Pair {
            key: "vmm pid",
            value: state.vmm_pid.map_or_else(dash, |pid| pid.to_string()),
        },
        Pair {
            key: "mac",
            value: state.mac.clone().unwrap_or_else(dash),
        },
        Pair {
            key: "started",
            value: state.started_at.clone().unwrap_or_else(dash),
        },
        Pair {
            key: "runtime dir",
            value: state.runtime_dir.display().to_string(),
        },
    ];
    if state.vmm_pid.is_some() {
        rows.push(Pair {
            key: "supervision",
            value: if supervised {
                "supervised"
            } else {
                "unsupervised"
            }
            .to_owned(),
        });
    }
    if let Some(last_exit) = &state.last_exit {
        let detail = match (last_exit.code, last_exit.signal) {
            (Some(code), _) => format!("exit {code}"),
            (_, Some(signal)) => format!("signal {signal}"),
            _ => last_exit.reason.as_str().to_owned(),
        };
        rows.push(Pair {
            key: "last exit",
            value: format!("{detail} · {}", last_exit.reason.as_str()),
        });
    }
    rows
}

fn spec_groups(spec: &MachineSpec) -> Vec<SpecGroup> {
    vec![
        SpecGroup {
            title: "Resources",
            rows: vec![
                Pair {
                    key: "image",
                    value: spec.image.as_str().to_owned(),
                },
                Pair {
                    key: "arch",
                    value: spec
                        .arch
                        .map_or_else(|| "null (host)".to_owned(), |arch| arch.to_string()),
                },
                Pair {
                    key: "cpus",
                    value: spec.cpus.to_string(),
                },
                Pair {
                    key: "memory",
                    value: spec.memory.to_string(),
                },
                Pair {
                    key: "disk",
                    value: spec.disk.to_string(),
                },
                Pair {
                    key: "user",
                    value: spec.user.clone(),
                },
            ],
        },
        network_group(&spec.network),
        mounts_group(&spec.mounts),
        cloud_init_group(&spec.cloud_init),
        vmm_group(&spec.vmm),
    ]
}

/// Shared folders, rendered in the same `HOST:GUEST[:ro]` grammar the CLI and
/// the create form accept, with the tag the guest will actually see. The tag
/// is derived rather than shown as null, because `share<i>` is what virtiofs
/// is given when the field is unset.
fn mounts_group(mounts: &[MountSpec]) -> SpecGroup {
    SpecGroup {
        title: "Mounts",
        rows: if mounts.is_empty() {
            vec![Pair {
                key: "mount",
                value: "[]".to_owned(),
            }]
        } else {
            mounts
                .iter()
                .enumerate()
                .map(|(index, mount)| Pair {
                    key: "mount",
                    value: format!(
                        "{}:{}{} · tag {}",
                        mount.host.display(),
                        mount.guest.display(),
                        if mount.readonly { ":ro" } else { "" },
                        mount.effective_tag(index)
                    ),
                })
                .collect()
        },
    }
}

fn network_group(network: &NetworkSpec) -> SpecGroup {
    SpecGroup {
        title: "Network",
        rows: vec![
            Pair {
                key: "mode",
                value: net_mode_token(network.mode).to_owned(),
            },
            Pair {
                key: "forward",
                value: if network.forward.is_empty() {
                    "[]".to_owned()
                } else {
                    network
                        .forward
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            },
            Pair {
                key: "tap",
                value: network.tap.clone().unwrap_or_else(null),
            },
            Pair {
                key: "mac",
                value: network
                    .mac
                    .as_ref()
                    .map_or_else(|| "null (generated)".to_owned(), ToString::to_string),
            },
        ],
    }
}

fn cloud_init_group(cloud_init: &CloudInitSpec) -> SpecGroup {
    SpecGroup {
        title: "Cloud-init",
        rows: vec![
            Pair {
                key: "provisioning",
                value: cloud_init.provisioning.to_string(),
            },
            // Paths only. Cloud-init contents are never read into the UI, and
            // never logged, per SPEC.
            Pair {
                key: "user_data",
                value: path_or_null(cloud_init.user_data.as_deref()),
            },
            // Inline user-data is configured content, so it is reported by
            // size and never by value: this fragment is rendered into a page,
            // a browser cache and a view-source, none of which SPEC §10.5
            // counts as the 0600 machine file the user owns.
            Pair {
                key: "user_data_inline",
                value: cloud_init
                    .user_data_inline
                    .as_ref()
                    .map_or_else(null, |inline| format!("{} bytes", inline.len())),
            },
            Pair {
                key: "network_config",
                value: path_or_null(cloud_init.network_config.as_deref()),
            },
            Pair {
                key: "ssh_keys",
                value: if cloud_init.ssh_keys.is_empty() {
                    "[]".to_owned()
                } else {
                    cloud_init
                        .ssh_keys
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            },
            // A public key is not a secret, but a list of them is noise in a
            // spec table and a fingerprinting surface in a shared screen, so
            // the count is what the tab reports.
            Pair {
                key: "ssh_authorized_keys",
                value: count_or_empty(cloud_init.ssh_authorized_keys.len(), "key", "keys"),
            },
            // Never the value, and never a length either: a password's length
            // is worth something to whoever is looking over the shoulder.
            Pair {
                key: "password",
                value: if cloud_init.password.is_some() {
                    "set".to_owned()
                } else {
                    "unset".to_owned()
                },
            },
            Pair {
                key: "ssh_pwauth",
                value: cloud_init.ssh_pwauth.to_string(),
            },
        ],
    }
}

/// `[]` for nothing, otherwise a count with the right noun.
fn count_or_empty(count: usize, singular: &str, plural: &str) -> String {
    match count {
        0 => "[]".to_owned(),
        1 => format!("1 {singular}"),
        _ => format!("{count} {plural}"),
    }
}

fn vmm_group(vmm: &VmmSpec) -> SpecGroup {
    SpecGroup {
        title: "VMM",
        rows: vec![
            Pair {
                key: "binary",
                value: vmm.binary.as_deref().map_or_else(
                    || "null (embedded)".to_owned(),
                    |path| path.display().to_string(),
                ),
            },
            Pair {
                key: "firmware",
                value: vmm.firmware.to_string(),
            },
            Pair {
                key: "extra_args",
                value: if vmm.extra_args.is_empty() {
                    "[]".to_owned()
                } else {
                    vmm.extra_args.join(" ")
                },
            },
            Pair {
                key: "config_overlay",
                value: vmm
                    .config_overlay
                    .as_ref()
                    .map_or_else(null, |_| "set".to_owned()),
            },
        ],
    }
}

/// `NetMode` has no `Display`; its serde token is the stable name the CLI,
/// the API and this UI all use.
pub fn net_mode_token(mode: NetMode) -> &'static str {
    match mode {
        NetMode::Passt => "passt",
        NetMode::Tap => "tap",
        NetMode::None => "none",
    }
}

fn dash() -> String {
    "—".to_owned()
}

fn null() -> String {
    "null".to_owned()
}

fn path_or_null(path: Option<&std::path::Path>) -> String {
    path.map_or_else(null, |path| path.display().to_string())
}

/// One catalog card.
///
/// Deliberately carries no "default" badge. `catalog/images.toml` marks a
/// default version per distribution, but `CatalogEntrySummary` — the shared
/// result both the CLI and this UI read — does not expose that flag, and a UI
/// that guesses at a marker is worse than one that omits it.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogCard {
    pub reference: String,
    pub cached: bool,
    pub cached_id: String,
    pub size: String,
    pub chips: Vec<String>,
}

/// Human byte size using decimal units, matching how image sizes are quoted.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            if *unit == *"B" || whole >= 100 {
                return format!("{whole} {unit}");
            }
            let tenths = (bytes % scale) * 10 / scale;
            return format!("{whole}.{tenths} {unit}");
        }
    }
    "0 B".to_owned()
}

/// Renders a whole-MiB count as the canonical `NNNM` / `NNNG` Firestone form.
fn format_mib(mib: u64) -> String {
    if mib == 0 {
        "0".to_owned()
    } else if mib % 1024 == 0 {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

/// Reads back a canonical Firestone size string such as `8G` or `512M`.
fn parse_size_mib(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    let (digits, scale) = match trimmed.as_bytes().last()? {
        b'G' | b'g' => (&trimmed[..trimmed.len() - 1], 1024),
        b'M' | b'm' => (&trimmed[..trimmed.len() - 1], 1),
        _ => (trimmed, 1),
    };
    digits.parse::<u64>().ok().map(|whole| whole * scale)
}

#[cfg(test)]
mod tests {
    use super::{format_bytes, format_mib, parse_size_mib, plural, row_action};

    #[test]
    fn format_bytes_scales_and_keeps_one_decimal_below_one_hundred() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_500), "1.5 kB");
        assert_eq!(format_bytes(642_000_000), "642 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn parse_size_mib_reads_canonical_firestone_sizes() {
        assert_eq!(parse_size_mib("8G"), Some(8192));
        assert_eq!(parse_size_mib("512M"), Some(512));
        assert_eq!(parse_size_mib(" 2G "), Some(2048));
        assert_eq!(parse_size_mib("2048"), Some(2048));
        assert_eq!(parse_size_mib("nonsense"), None);
    }

    #[test]
    fn format_mib_prefers_whole_gibibytes() {
        assert_eq!(format_mib(0), "0");
        assert_eq!(format_mib(2048), "2 GiB");
        assert_eq!(format_mib(1536), "1536 MiB");
    }

    #[test]
    fn row_action_withholds_a_button_while_a_machine_transitions() {
        assert_eq!(row_action("running"), (Some("stop"), "Stop"));
        assert_eq!(row_action("stopped"), (Some("start"), "Start"));
        assert_eq!(row_action("failed"), (Some("start"), "Start"));
        assert_eq!(row_action("starting").0, None);
        assert_eq!(row_action("stopping").0, None);
    }

    #[test]
    fn overview_machines_mark_only_the_first_running_rows_for_metrics() {
        use super::{OVERVIEW_METRICS_CAP, overview_machines};
        use firestone_core::MachineSummary;

        let summary = |name: &str, status: &str| MachineSummary {
            name: name.to_owned(),
            status: status.to_owned(),
            image: "ubuntu:24.04".to_owned(),
            cpus: 2,
            memory: "2G".to_owned(),
            uptime: None,
            forwards: Vec::new(),
            forwards_pending: false,
        };

        let mut fleet = vec![summary("idle", "stopped"), summary("busy", "starting")];
        for index in 0..OVERVIEW_METRICS_CAP + 3 {
            fleet.push(summary(&format!("node-{index}"), "running"));
        }

        let rows = overview_machines(&fleet);
        assert_eq!(rows.len(), fleet.len(), "no machine may be dropped");
        assert_eq!(
            rows.iter().filter(|row| row.metrics).count(),
            OVERVIEW_METRICS_CAP
        );
        // Neither a stopped nor a transitioning machine has counters to read.
        assert!(!rows[0].metrics);
        assert!(!rows[1].metrics);
        assert!(rows[2].metrics, "the first running machine must poll");
        assert!(
            !rows[2 + OVERVIEW_METRICS_CAP].metrics,
            "the row past the cap must not poll"
        );
    }

    #[test]
    fn plural_matches_the_count() {
        assert_eq!(plural(1, "machine"), "machine");
        assert_eq!(plural(0, "machine"), "machines");
        assert_eq!(plural(3, "image"), "images");
    }
}

// ================================== snapshots, clone, images, prune (M6-26) ==
//
// Everything below projects a landed shared result into the markup one of the
// new surfaces renders. Nothing here talks to the host, and nothing here
// parses a Firestone grammar of its own: a forward is read back through the
// same §12.4 parser that wrote it, so the chip and the string it came from can
// never disagree.

/// One host-to-guest port forward as the UI draws it.
///
/// `href` is empty whenever a browser could not usefully follow the chip, and
/// the template renders a plain span in that case. That is the whole
/// linkability decision, made once, on the server, where the forward is
/// already parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForwardChip {
    /// The canonical `[proto:][bind:]HOST:GUEST` text, verbatim.
    pub text: String,
    /// `http://…` for a followable forward, empty otherwise.
    pub href: String,
    /// `tcp` or `udp`; `?` when the recorded value no longer parses.
    pub protocol: &'static str,
}

/// Builds the chips for one applied forward list.
///
/// `live` is the machine's own running state. A forward that is recorded but
/// not currently applied — a stopped machine still carries the set its last
/// start used — is rendered as text, because a link to a port nothing is
/// listening on is worse than no link at all.
///
/// A value that no longer parses is kept verbatim and never linkified, the
/// same discipline §12.5 applies when it compares the two sets.
pub fn forward_chips(forwards: &[String], live: bool) -> Vec<ForwardChip> {
    forwards
        .iter()
        .map(|forward| match forward.parse::<PortForward>() {
            Ok(parsed) => ForwardChip {
                text: parsed.to_string(),
                href: if live {
                    forward_href(&parsed).unwrap_or_default()
                } else {
                    String::new()
                },
                protocol: parsed.protocol().as_str(),
            },
            Err(_) => ForwardChip {
                text: forward.clone(),
                href: String::new(),
                protocol: "?",
            },
        })
        .collect()
}

/// The URL a single TCP forward answers on, or `None` when there is no single
/// URL to name.
///
/// Three rules, each of them a claim the UI must be able to defend:
///
/// - **UDP is never linkified.** `http://` over UDP is not something a browser
///   can open, and a chip that navigates nowhere teaches the reader that the
///   chips lie.
/// - **A range is never linkified.** `8000-8010:80-90` is eleven forwards; the
///   first of them is not the forward, and picking one silently would be a
///   guess dressed as a fact.
/// - **The bind address is honoured, and an unspecified one becomes
///   loopback.** passt binds `0.0.0.0` on every host address, but the browser
///   is on this host, so `127.0.0.1` is both reachable and the narrower claim.
pub fn forward_href(forward: &PortForward) -> Option<String> {
    if forward.protocol() != Protocol::Tcp {
        return None;
    }
    let host = forward.host();
    if host.start() != host.end() {
        return None;
    }
    let port = host.start();
    Some(match forward.bind() {
        None => format!("http://127.0.0.1:{port}"),
        Some(IpAddr::V4(address)) if address.is_unspecified() => {
            format!("http://127.0.0.1:{port}")
        }
        Some(IpAddr::V6(address)) if address.is_unspecified() => {
            format!("http://[::1]:{port}")
        }
        Some(IpAddr::V4(address)) => format!("http://{address}:{port}"),
        // A literal IPv6 authority is bracketed, or the port reads as another
        // hextet and the URL names a different address entirely.
        Some(IpAddr::V6(address)) => format!("http://[{address}]:{port}"),
    })
}

/// Whether `clone` would accept this machine as a source right now.
///
/// §24.2: the source must be `created` or `stopped`, and the check runs before
/// the source lock is taken so a running machine is refused rather than
/// queued. The UI reads the same rule so the refusal is visible before the
/// request, and `ls` display statuses such as `running (unsupervised)` are
/// compared by their leading word rather than by an exact match.
pub fn is_clonable(status: &str) -> bool {
    matches!(
        status.split_whitespace().next().unwrap_or_default(),
        "created" | "stopped"
    )
}

/// One row of the machine detail snapshots tab.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRow {
    pub snapshot: String,
    /// `cold` or `warm` — the word §23 defines, never a colour.
    pub kind: &'static str,
    pub created: String,
    pub size: String,
    /// Guest memory a warm snapshot captured; an em dash for a cold one,
    /// because a cold snapshot captured no memory rather than zero bytes.
    pub memory: String,
    pub image_id: String,
    /// Warm restores always start the machine again (§23.5), and the row says
    /// so rather than letting the confirm dialog surprise the operator.
    pub warm: bool,
}

/// Projects the snapshot list result into rows, newest identifier first.
///
/// The dispatcher orders by identifier and the default names are
/// `snap-<yyyymmdd>-<hhmmss>`, so reversing that order puts the most recent
/// snapshot — the one an operator reaches for under pressure — at the top.
pub fn snapshot_rows(snapshots: &[SnapshotSummary]) -> Vec<SnapshotRow> {
    let mut rows: Vec<SnapshotRow> = snapshots
        .iter()
        .map(|summary| SnapshotRow {
            snapshot: summary.snapshot.clone(),
            kind: summary.kind.as_str(),
            created: summary.created_at.clone(),
            size: format_bytes(summary.disk_bytes),
            memory: summary.memory_bytes.map_or_else(dash, format_bytes),
            image_id: summary.image_id.clone().unwrap_or_else(dash),
            warm: summary.kind == SnapshotKind::Warm,
        })
        .collect();
    rows.reverse();
    rows
}

/// One row of the cached-images table on the catalog screen.
#[derive(Debug, Clone, Serialize)]
pub struct ImageRow {
    pub reference: String,
    pub id: String,
    /// The leading identity of the id, which is what an operator reads; the
    /// full value stays in the row's title attribute.
    pub short_id: String,
    pub size: String,
    pub bytes: u64,
    pub pulled: String,
    /// True for an image built from an OCI reference (§8.5).
    pub oci: bool,
}

/// Projects the image store listing into table rows, largest first.
///
/// Size order is the order that matters on a screen whose only destructive
/// affordance is "reclaim disk": the row worth deleting is at the top.
pub fn image_rows(images: &[CachedImage]) -> Vec<ImageRow> {
    let mut rows: Vec<ImageRow> = images
        .iter()
        .map(|image| ImageRow {
            reference: image.metadata.source_ref.clone(),
            id: image.metadata.id.clone(),
            short_id: short_id(&image.metadata.id),
            size: format_bytes(image.metadata.size),
            bytes: image.metadata.size,
            pulled: if image.metadata.pulled_at.is_empty() {
                dash()
            } else {
                image.metadata.pulled_at.clone()
            },
            oci: image.metadata.kind.as_deref() == Some("oci"),
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.reference.cmp(&right.reference))
    });
    rows
}

/// The first nineteen characters of an image id, which keeps the `sha256-`
/// prefix and twelve hex digits — enough to name one image on this host.
fn short_id(id: &str) -> String {
    if id.chars().count() <= 19 {
        return id.to_owned();
    }
    id.chars().take(19).collect::<String>() + "…"
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod feature_tests {
    use firestone_core::{PortForward, SnapshotKind, SnapshotSummary};

    use super::{
        CachedImage, CachedImageMetadata, ForwardChip, forward_chips, forward_href, image_rows,
        short_id, snapshot_rows,
    };

    fn chip(text: &str, href: &str, protocol: &'static str) -> ForwardChip {
        ForwardChip {
            text: text.to_owned(),
            href: href.to_owned(),
            protocol,
        }
    }

    #[test]
    fn forward_href_links_a_single_tcp_port_at_the_address_it_bound() {
        let cases: [(&str, Option<&str>); 8] = [
            // No bind: loopback is what this browser can reach.
            ("8080:80", Some("http://127.0.0.1:8080")),
            ("tcp:8080:80", Some("http://127.0.0.1:8080")),
            // A bind address is honoured verbatim.
            ("192.168.1.5:8080:80", Some("http://192.168.1.5:8080")),
            // Unspecified means "every address", and this browser is on this
            // host, so loopback is the reachable and narrower claim.
            ("0.0.0.0:8080:80", Some("http://127.0.0.1:8080")),
            ("[::]:8080:80", Some("http://[::1]:8080")),
            // An IPv6 authority must be bracketed or the port reads as a hextet.
            ("[::1]:9090:90", Some("http://[::1]:9090")),
            // UDP has no http URL, and a range is many forwards rather than one.
            ("udp:5353:5353", None),
            ("8000-8010:80-90", None),
        ];
        for (raw, expected) in cases {
            let forward: PortForward = raw.parse().expect("a valid forward");
            assert_eq!(
                forward_href(&forward).as_deref(),
                expected,
                "{raw} produced the wrong href"
            );
        }
    }

    #[test]
    fn forward_chips_linkify_only_a_running_machine() {
        let forwards = vec!["8080:80".to_owned(), "udp:5353:5353".to_owned()];

        assert_eq!(
            forward_chips(&forwards, true),
            vec![
                chip("8080:80", "http://127.0.0.1:8080", "tcp"),
                chip("udp:5353:5353", "", "udp"),
            ]
        );
        // A stopped machine has applied nothing a browser can reach.
        assert_eq!(
            forward_chips(&forwards, false),
            vec![chip("8080:80", "", "tcp"), chip("udp:5353:5353", "", "udp")]
        );
    }

    #[test]
    fn forward_chips_keep_an_unparseable_recorded_value_verbatim() {
        let chips = forward_chips(&["not-a-forward".to_owned()], true);
        assert_eq!(chips, vec![chip("not-a-forward", "", "?")]);
    }

    #[test]
    fn snapshot_rows_report_the_tier_and_leave_a_cold_memory_figure_absent() {
        let summaries = vec![
            SnapshotSummary {
                snapshot: "snap-20260901-010000".to_owned(),
                kind: SnapshotKind::Cold,
                created_at: "2026-09-01T01:00:00Z".to_owned(),
                image_id: Some("sha256-abc".to_owned()),
                disk_bytes: 1_500_000_000,
                memory_bytes: None,
            },
            SnapshotSummary {
                snapshot: "snap-20260902-020000".to_owned(),
                kind: SnapshotKind::Warm,
                created_at: "2026-09-02T02:00:00Z".to_owned(),
                image_id: None,
                disk_bytes: 0,
                memory_bytes: Some(8_589_934_592),
            },
        ];

        let rows = snapshot_rows(&summaries);
        // Newest first: the snapshot reached for under pressure is on top.
        assert_eq!(rows[0].snapshot, "snap-20260902-020000");
        assert_eq!(rows[0].kind, "warm");
        assert!(rows[0].warm);
        assert_eq!(rows[0].memory, "8.5 GB");
        assert_eq!(rows[0].image_id, "—");

        assert_eq!(rows[1].kind, "cold");
        assert!(!rows[1].warm);
        // A cold snapshot captured no memory; it did not capture zero bytes.
        assert_eq!(rows[1].memory, "—");
        assert_eq!(rows[1].size, "1.5 GB");
    }

    #[test]
    fn image_rows_badge_an_oci_image_and_shorten_its_id() {
        let image = |id: &str, kind: Option<&str>, size: u64| CachedImage {
            metadata: CachedImageMetadata {
                id: id.to_owned(),
                source_ref: "ubuntu:24.04".to_owned(),
                size,
                kind: kind.map(ToOwned::to_owned),
                pulled_at: "2026-09-01T05:00:00Z".to_owned(),
            },
        };

        let rows = image_rows(&[
            image("sha256-0123456789abcdef0123", None, 642_000_000),
            image("sha256-fedcba9876543210", Some("oci"), 1_500_000_000),
        ]);

        // Largest first: the row worth reclaiming is the one on top.
        assert_eq!(rows[0].size, "1.5 GB");
        assert!(rows[0].oci, "an oci image must be badged");
        assert!(!rows[1].oci, "an absent kind is a disk image, not unknown");
        assert_eq!(rows[1].short_id, "sha256-0123456789ab…");
    }

    #[test]
    fn short_id_keeps_a_shorter_identity_whole() {
        assert_eq!(short_id("sha256-abc"), "sha256-abc");
        assert_eq!(short_id(""), "");
    }
}
