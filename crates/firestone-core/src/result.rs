use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    CatalogFirmware, LogSource, MachineSpec, MachineState, MachineStatus, SpecWarning, Supervision,
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
}

/// Machine spec and state shared by show and the REST item route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineView {
    pub spec: MachineSpec,
    pub state: MachineState,
    pub supervision: Option<Supervision>,
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

/// Resolved process-wide roots exposed by the version action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPaths {
    pub config: String,
    pub data: String,
    pub runtime: String,
}

#[cfg(test)]
mod tests {
    use super::{RunResult, ShellResult, SshConfigResult};

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
