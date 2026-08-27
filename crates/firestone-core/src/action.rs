use std::time::Duration;

use serde::{Deserialize, Serialize};

/// An imperative operation accepted by the shared dispatcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    Start {
        name: String,
        wait: bool,
        timeout: Duration,
    },
    Stop {
        name: String,
        timeout: Duration,
        force: bool,
    },
    Restart {
        name: String,
        timeout: Duration,
    },
    Remove {
        name: String,
        force: bool,
    },
    List,
    Show {
        name: String,
    },
    ImageList,
    ImageRemove {
        id: String,
    },
    ImagePrune,
    Doctor {
        fix: bool,
    },
    Version,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Action;

    #[test]
    fn action_serialization_has_stable_type_tag() -> Result<(), serde_json::Error> {
        let serialized = serde_json::to_value(Action::Show {
            name: "ubuntu".to_owned(),
        })?;

        assert_eq!(serialized, json!({"type": "Show", "name": "ubuntu"}));
        Ok(())
    }
}
