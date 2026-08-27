use serde::{Deserialize, Serialize};

use crate::ErrorInfo;

/// Identifier for one action step.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepId(String);

impl StepId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for StepId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for StepId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for StepId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Unit attached to a progress count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Bytes,
}

/// Severity for secondary action output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// One structured progress message emitted by an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    StepStart {
        id: StepId,
        label: String,
    },
    StepUpdate {
        id: StepId,
        detail: String,
    },
    Progress {
        id: StepId,
        done: u64,
        total: Option<u64>,
        unit: Unit,
    },
    StepDone {
        id: StepId,
        detail: Option<String>,
        elapsed_ms: u64,
    },
    StepSkip {
        id: StepId,
        reason: String,
    },
    StepFail {
        id: StepId,
        error: ErrorInfo,
    },
    Log {
        level: Level,
        message: String,
    },
    Result {
        action: String,
        payload: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Event, StepId};

    #[test]
    fn event_serialization_matches_ndjson_contract() -> Result<(), serde_json::Error> {
        let event = Event::StepDone {
            id: StepId::from("image"),
            detail: Some("cached".to_owned()),
            elapsed_ms: 12,
        };

        assert_eq!(
            serde_json::to_value(event)?,
            json!({
                "type": "StepDone",
                "id": "image",
                "detail": "cached",
                "elapsed_ms": 12
            })
        );
        Ok(())
    }

    #[test]
    fn result_event_round_trip_preserves_payload() -> Result<(), serde_json::Error> {
        let event = Event::Result {
            action: "version".to_owned(),
            payload: json!({"version": "0.1.0"}),
        };
        let encoded = serde_json::to_string(&event)?;
        let decoded = serde_json::from_str(&encoded)?;

        assert_eq!(event, decoded);
        Ok(())
    }
}
