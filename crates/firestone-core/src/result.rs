use serde::{Deserialize, Serialize};

use crate::{LogSource, MachineSpec, MachineState, MachineStatus, SpecWarning, Supervision};

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

/// A machine that reached the M1 running contract.
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
