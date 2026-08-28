use serde::{Deserialize, Serialize};

use crate::{MachineSpec, MachineState, SpecWarning, Supervision};

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
