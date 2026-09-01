//! View models for the embedded web UI.
//!
//! Nothing here talks to the host. Each type is built purely from a shared
//! action result — the same payload the REST routes serialize — and carries
//! only what a template needs. Presentation decisions (which action a row
//! offers, how a byte count reads) live here so templates stay declarative,
//! and status is always emitted as a stable token, never as a colour.

use firestone_core::{
    CloudInitSpec, DoctorCheck, DoctorReport, DoctorStatus, MachineSpec, MachineState,
    MachineStatus, MachineSummary, MachineView, MountSpec, NetMode, NetworkSpec, VersionResult,
    VmmSpec,
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
        }
    }
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
        ],
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
