use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ByteSize, CatalogFirmware, LogSource, MachineSpec, MachineState, MachineStatus, SpecWarning,
    Supervision, snapshot::SnapshotKind,
};

/// Effective firmware for one architecture offered by a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogArchitectureSummary {
    pub architecture: String,
    pub firmware: CatalogFirmware,
}

/// One merged catalog row shared by the CLI and REST API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntrySummary {
    pub reference: String,
    pub aliases: Vec<String>,
    pub architectures: Vec<CatalogArchitectureSummary>,
}

/// One machine row shared by CLI list output and the REST collection route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineSummary {
    pub name: String,
    pub status: String,
    pub image: String,
    pub cpus: u8,
    pub memory: String,
    pub uptime: Option<String>,
    pub forwards: Vec<String>,
    /// Spec forwards differ from the forwards this running machine applied at
    /// spawn; always false when the machine is not running (§12.4).
    #[serde(default)]
    pub forwards_pending: bool,
}

/// Machine spec and state shared by show and the REST item route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineView {
    pub spec: MachineSpec,
    pub state: MachineState,
    pub supervision: Option<Supervision>,
    /// Spec forwards differ from the forwards this running machine applied at
    /// spawn; always false when the machine is not running (§12.4).
    #[serde(default)]
    pub forwards_pending: bool,
    /// The spec's image reference, resolved through the catalog, names an image
    /// other than the one this running machine booted; always false when the
    /// machine is not running (§8.2).
    #[serde(default)]
    pub image_pending: bool,
}

/// Newly-created machine returned by every action surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineRecord {
    pub name: String,
    pub spec: MachineSpec,
    pub state: MachineState,
}

/// A machine that reached running, plus SSH readiness when Start.wait is true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartResult {
    pub name: String,
    pub status: MachineStatus,
    pub elapsed_ms: u64,
    pub forwards: Vec<String>,
    pub mounts: Vec<String>,
}

/// The effective CPU and memory of a machine after one resize action.
///
/// `applied_live` is true only when Cloud Hypervisor accepted `vm.resize` for a
/// running machine. Otherwise the values were written to the spec and take
/// effect on the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeResult {
    pub name: String,
    pub applied_live: bool,
    pub cpus: u8,
    pub memory: ByteSize,
}

/// A completed or idempotently skipped stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopResult {
    pub name: String,
    pub status: MachineStatus,
    pub elapsed_ms: u64,
}

/// Machine publications removed by one atomic CLI action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveResult {
    pub removed: Vec<String>,
}

/// Terminal metadata for a bounded log read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsResult {
    pub name: String,
    pub source: LogSource,
    pub lines: u32,
    pub follow: bool,
}

/// OpenSSH configuration produced without exposing key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfigResult {
    pub name: String,
    pub host: String,
    pub config: String,
}

/// Exact remote process outcome retained by run when cleanup is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellResult {
    pub name: String,
    pub user: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// Exact scp invocation planned for one `firestone cp` operand pair.
///
/// The payload carries argv only. It never contains key material, and the CLI is the only surface
/// that executes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpResult {
    pub name: String,
    pub user: String,
    pub recursive: bool,
    pub program: String,
    pub args: Vec<String>,
}

/// Create/start/shell/remove outcome for one run orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub name: String,
    pub created: bool,
    pub removed: bool,
    pub shell: ShellResult,
}

/// Owned form of a non-fatal spec validation warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecWarningPayload {
    pub key: String,
    pub message: String,
}

impl From<&SpecWarning> for SpecWarningPayload {
    fn from(warning: &SpecWarning) -> Self {
        Self {
            key: warning.key.to_owned(),
            message: warning.message.clone(),
        }
    }
}

/// Effective spec returned after an edit or API update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecResult {
    pub spec: MachineSpec,
    pub warnings: Vec<SpecWarningPayload>,
}

/// Reproducible build identity and runtime layout returned by the version action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResult {
    pub version: String,
    pub identity: VersionIdentity,
    pub architecture: String,
    pub dependencies: BTreeMap<String, VersionDependency>,
    pub paths: VersionPaths,
}

/// Release and optional source revision embedded in the executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionIdentity {
    pub release: String,
    pub git_commit: Option<String>,
}

/// One architecture-selected dependency pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionDependency {
    pub version: String,
    pub sha256: String,
}

/// One machine copied from a stopped or created source by the clone action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneResult {
    pub source: String,
    pub dest: String,
    /// Virtual size of the destination overlay, or zero when none was materialized.
    pub disk_bytes: u64,
}

/// One snapshot published by the snapshot create action (SPEC §23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub name: String,
    pub snapshot: String,
    pub kind: SnapshotKind,
    /// Virtual size of the copied overlay, or zero when the machine had none.
    pub disk_bytes: u64,
    /// Guest memory captured by a warm snapshot; absent for a cold one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

/// One row of the snapshot list, projected from the snapshot's metadata.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub snapshot: String,
    pub kind: SnapshotKind,
    pub created_at: String,
    pub image_id: Option<String>,
    pub disk_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

/// Every published snapshot of one machine, ordered by identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotListResult {
    pub snapshots: Vec<SnapshotSummary>,
}

/// One completed whole-machine rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRestoreResult {
    pub name: String,
    pub snapshot: String,
    /// True when the machine is running again after the restore.
    pub started: bool,
}

/// One deleted snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRemoveResult {
    pub name: String,
    pub snapshot: String,
}

/// Which class of artifact one prune entry belongs to (SPEC §26).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PruneKind {
    /// A stale per-machine runtime directory.
    Runtime,
    /// A rotated `console.log.previous`.
    Log,
    /// An unfinished `.partial` or `.removing-` artifact.
    Partial,
    /// An unfinished snapshot directory under `snapshots/`.
    SnapshotPartial,
    /// A stored base image no machine and no snapshot references.
    Image,
    /// A whole machine removed by the destructive `machines` tier.
    Machine,
}

impl PruneKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Log => "log",
            Self::Partial => "partial",
            Self::SnapshotPartial => "snapshot-partial",
            Self::Image => "image",
            Self::Machine => "machine",
        }
    }
}

impl std::fmt::Display for PruneKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `pad` rather than `write_str`: the CLI prints the kind in a fixed
        // column, and `write_str` would silently ignore the width.
        formatter.pad(self.as_str())
    }
}

/// One artifact system prune removed, or would remove under `--dry-run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneItem {
    pub kind: PruneKind,
    /// Stable identifier: a machine name, an image id, or a data-directory
    /// relative path, depending on `kind` (SPEC §26).
    pub id: String,
    /// Bytes the artifact occupied on disk, measured before deletion.
    pub bytes: u64,
}

/// Everything one system prune reclaimed, or would reclaim (SPEC §26).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneResult {
    pub dry_run: bool,
    pub reclaimed_bytes: u64,
    pub removed: Vec<PruneItem>,
}

/// Resolved process-wide roots exposed by the version action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPaths {
    pub config: String,
    pub data: String,
    pub runtime: String,
}

#[cfg(test)]
mod tests {
    use super::{
        PruneItem, PruneKind, PruneResult, RunResult, ShellResult, SnapshotListResult,
        SnapshotResult, SnapshotSummary, SshConfigResult,
    };
    use crate::snapshot::SnapshotKind;

    #[test]
    fn prune_result_serializes_every_kind_as_its_stable_lowercase_word()
    -> Result<(), serde_json::Error> {
        let result = PruneResult {
            dry_run: true,
            reclaimed_bytes: 6,
            removed: vec![
                PruneItem {
                    kind: PruneKind::Runtime,
                    id: "dev".to_owned(),
                    bytes: 1,
                },
                PruneItem {
                    kind: PruneKind::Log,
                    id: "dev/console.log.previous".to_owned(),
                    bytes: 1,
                },
                PruneItem {
                    kind: PruneKind::Partial,
                    id: "machines/dev/disk.qcow2.partial".to_owned(),
                    bytes: 1,
                },
                PruneItem {
                    kind: PruneKind::SnapshotPartial,
                    id: "dev/snapshots/.partial-snap".to_owned(),
                    bytes: 1,
                },
                PruneItem {
                    kind: PruneKind::Image,
                    id: "image-0123".to_owned(),
                    bytes: 1,
                },
                PruneItem {
                    kind: PruneKind::Machine,
                    id: "old".to_owned(),
                    bytes: 1,
                },
            ],
        };
        let encoded = serde_json::to_value(&result)?;
        assert_eq!(encoded["dry_run"], true);
        assert_eq!(encoded["reclaimed_bytes"], 6);
        let kinds = encoded["removed"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item["kind"].clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            kinds,
            vec![
                serde_json::json!("runtime"),
                serde_json::json!("log"),
                serde_json::json!("partial"),
                serde_json::json!("snapshot-partial"),
                serde_json::json!("image"),
                serde_json::json!("machine"),
            ]
        );
        assert_eq!(serde_json::from_value::<PruneResult>(encoded)?, result);
        for kind in [
            PruneKind::Runtime,
            PruneKind::Log,
            PruneKind::Partial,
            PruneKind::SnapshotPartial,
            PruneKind::Image,
            PruneKind::Machine,
        ] {
            assert_eq!(
                serde_json::to_value(kind)?,
                serde_json::json!(kind.to_string())
            );
        }
        Ok(())
    }

    #[test]
    fn snapshot_results_serialize_kind_as_a_stable_lowercase_word() -> Result<(), serde_json::Error>
    {
        let cold = SnapshotResult {
            name: "dev".to_owned(),
            snapshot: "snap-20260902-123456".to_owned(),
            kind: SnapshotKind::Cold,
            disk_bytes: 4096,
            memory_bytes: None,
        };
        assert_eq!(
            serde_json::to_value(&cold)?,
            serde_json::json!({
                "name": "dev",
                "snapshot": "snap-20260902-123456",
                "kind": "cold",
                "disk_bytes": 4096
            })
        );
        assert_eq!(
            serde_json::from_value::<SnapshotResult>(serde_json::to_value(&cold)?)?,
            cold
        );

        let warm = SnapshotResult {
            kind: SnapshotKind::Warm,
            memory_bytes: Some(2_147_483_648),
            ..cold
        };
        assert_eq!(serde_json::to_value(&warm)?["kind"], "warm");
        assert_eq!(
            serde_json::to_value(&warm)?["memory_bytes"],
            2_147_483_648_u64
        );

        let list = SnapshotListResult {
            snapshots: vec![SnapshotSummary {
                snapshot: "snap-20260902-123456".to_owned(),
                kind: SnapshotKind::Cold,
                created_at: "2026-09-02T12:34:56Z".to_owned(),
                image_id: None,
                disk_bytes: 4096,
                memory_bytes: None,
            }],
        };
        assert_eq!(
            serde_json::from_value::<SnapshotListResult>(serde_json::to_value(&list)?)?,
            list
        );
        Ok(())
    }

    #[test]
    fn m2_terminal_results_round_trip_without_key_material() -> Result<(), serde_json::Error> {
        let config = SshConfigResult {
            name: "demo".to_owned(),
            host: "firestone.demo".to_owned(),
            config: "Host firestone.demo\n  IdentityFile /data/ssh/id_ed25519\n".to_owned(),
        };
        let encoded = serde_json::to_value(&config)?;
        assert_eq!(serde_json::from_value::<SshConfigResult>(encoded)?, config);

        let run = RunResult {
            name: "demo".to_owned(),
            created: true,
            removed: true,
            shell: ShellResult {
                name: "demo".to_owned(),
                user: "root".to_owned(),
                exit_code: Some(23),
                signal: None,
            },
        };
        assert_eq!(
            serde_json::from_value::<RunResult>(serde_json::to_value(&run)?)?,
            run
        );
        Ok(())
    }
}
