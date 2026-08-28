use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{ImageRef, MachineSpec, MachineSpecPatch};

/// An imperative operation accepted by the shared dispatcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    Create {
        name: String,
        spec: MachineSpec,
    },
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
    SetSpec {
        name: String,
        spec: MachineSpec,
    },
    PatchSpec {
        name: String,
        patch: MachineSpecPatch,
    },
    ImageList,
    ImagePull {
        r#ref: ImageRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    ImageRemove {
        id: String,
        force: bool,
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
    use crate::{ImageRef, MachineSpec, MachineSpecPatch};

    #[test]
    fn action_serialization_has_stable_type_tag() -> Result<(), serde_json::Error> {
        let serialized = serde_json::to_value(Action::Show {
            name: "ubuntu".to_owned(),
        })?;

        assert_eq!(serialized, json!({"type": "Show", "name": "ubuntu"}));
        Ok(())
    }

    #[test]
    fn action_image_pull_missing_sha256_round_trips_as_optional() -> Result<(), serde_json::Error> {
        let action = serde_json::from_value::<Action>(json!({
            "type": "ImagePull",
            "ref": "https://images.example.invalid/base.qcow2"
        }))?;
        assert_eq!(
            action,
            Action::ImagePull {
                r#ref: ImageRef::from("https://images.example.invalid/base.qcow2"),
                sha256: None,
            }
        );
        assert_eq!(
            serde_json::to_value(action)?,
            json!({
                "type": "ImagePull",
                "ref": "https://images.example.invalid/base.qcow2"
            })
        );
        Ok(())
    }

    #[test]
    fn action_strong_payloads_serialize_through_shared_types() -> Result<(), serde_json::Error> {
        let create = serde_json::to_value(Action::Create {
            name: "ubuntu".to_owned(),
            spec: MachineSpec::default(),
        })?;
        let pull = serde_json::to_value(Action::ImagePull {
            r#ref: ImageRef::from("ubuntu:24.04"),
            sha256: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            ),
        })?;
        let set = serde_json::to_value(Action::SetSpec {
            name: "ubuntu".to_owned(),
            spec: MachineSpec::default(),
        })?;
        let patch = serde_json::to_value(Action::PatchSpec {
            name: "ubuntu".to_owned(),
            patch: MachineSpecPatch {
                cpus: Some(4),
                ..MachineSpecPatch::default()
            },
        })?;
        let remove = serde_json::to_value(Action::ImageRemove {
            id: "ubuntu-24.04-x86_64-12345678".to_owned(),
            force: true,
        })?;

        assert_eq!(create["type"], "Create");
        assert_eq!(create["spec"]["image"], "ubuntu:24.04");
        assert_eq!(
            pull,
            json!({
                "type": "ImagePull",
                "ref": "ubuntu:24.04",
                "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            })
        );
        assert_eq!(set["type"], "SetSpec");
        assert_eq!(
            patch,
            json!({"type": "PatchSpec", "name": "ubuntu", "patch": {"cpus": 4}})
        );
        assert_eq!(
            remove,
            json!({
                "type": "ImageRemove",
                "id": "ubuntu-24.04-x86_64-12345678",
                "force": true
            })
        );
        Ok(())
    }
}
