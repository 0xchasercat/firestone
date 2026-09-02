use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{ByteSize, ImageRef, MachineSpec, MachineSpecPatch};

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
        names: Vec<String>,
        force: bool,
    },
    List,
    Show {
        name: String,
        vmconfig: bool,
    },
    SetSpec {
        name: String,
        spec: MachineSpec,
    },
    PatchSpec {
        name: String,
        patch: MachineSpecPatch,
    },
    Logs {
        name: String,
        source: LogSource,
        lines: u32,
        follow: bool,
    },
    Cp {
        source: String,
        target: String,
        recursive: bool,
    },
    Metrics {
        name: String,
    },
    /// One on-demand sample of the host itself (§25).
    HostMetrics,
    CatalogList,
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
    ImageInspect {
        id: String,
    },
    ImagePrune,
    Doctor {
        fix: bool,
        #[serde(default)]
        elevation_confirmed: bool,
    },
    Version,
    Clone {
        source: String,
        dest: String,
        #[serde(default)]
        fresh_disk: bool,
    },
    Resize {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpus: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory: Option<ByteSize>,
    },
    SnapshotCreate {
        name: String,
        /// Snapshot identifier. Absent asks for `snap-<yyyymmdd>-<hhmmss>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
    },
    SnapshotList {
        name: String,
    },
    SnapshotRestore {
        name: String,
        snapshot: String,
        /// Stop a running machine before restoring instead of refusing.
        #[serde(default)]
        force: bool,
        /// Start the machine after a cold restore. Warm restores always start.
        #[serde(default)]
        start: bool,
        /// Deadline for the stop `force` performs.
        timeout: Duration,
    },
    SnapshotRemove {
        name: String,
        snapshot: String,
    },
    /// Reclaims disk space held by Firestone's own artifacts (SPEC §26).
    ///
    /// The default scope removes only inert debris. `machines` is the one
    /// destructive tier and is refused unless `force` is also true.
    SystemPrune {
        #[serde(default)]
        machines: bool,
        #[serde(default)]
        images: bool,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        dry_run: bool,
    },
}

/// One bounded, Firestone-owned machine log selected by the CLI or API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogSource {
    Console,
    Vmm,
    Shim,
    Passt,
    Virtiofsd(u16),
}

impl fmt::Display for LogSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Console => formatter.write_str("console"),
            Self::Vmm => formatter.write_str("vmm"),
            Self::Shim => formatter.write_str("shim"),
            Self::Passt => formatter.write_str("passt"),
            Self::Virtiofsd(index) => write!(formatter, "virtiofsd-{index}"),
        }
    }
}

impl FromStr for LogSource {
    type Err = ParseLogSourceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "console" => Ok(Self::Console),
            "vmm" => Ok(Self::Vmm),
            "shim" => Ok(Self::Shim),
            "passt" => Ok(Self::Passt),
            _ => value
                .strip_prefix("virtiofsd-")
                .filter(|index| !index.is_empty())
                .and_then(|index| index.parse::<u16>().ok())
                .map(Self::Virtiofsd)
                .ok_or(ParseLogSourceError),
        }
    }
}

impl Serialize for LogSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LogSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Closed-value parse failure for LogSource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("source must be console, vmm, shim, passt, or virtiofsd-N")]
pub struct ParseLogSourceError;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Action, LogSource};
    use crate::{ImageRef, MachineSpec, MachineSpecPatch};

    #[test]
    fn action_serialization_has_stable_type_tag() -> Result<(), serde_json::Error> {
        let serialized = serde_json::to_value(Action::Show {
            name: "ubuntu".to_owned(),
            vmconfig: false,
        })?;

        assert_eq!(
            serialized,
            json!({"type": "Show", "name": "ubuntu", "vmconfig": false})
        );
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

    #[test]
    fn action_lifecycle_and_log_payloads_round_trip() -> Result<(), serde_json::Error> {
        let action = Action::Logs {
            name: "ubuntu".to_owned(),
            source: LogSource::Virtiofsd(2),
            lines: 200,
            follow: true,
        };

        let encoded = serde_json::to_value(&action)?;
        assert_eq!(
            encoded,
            json!({
                "type": "Logs",
                "name": "ubuntu",
                "source": "virtiofsd-2",
                "lines": 200,
                "follow": true
            })
        );
        assert_eq!(serde_json::from_value::<Action>(encoded)?, action);

        let remove = Action::Remove {
            names: vec!["one".to_owned(), "two".to_owned()],
            force: true,
        };
        assert_eq!(
            serde_json::from_value::<Action>(serde_json::to_value(&remove)?)?,
            remove
        );
        Ok(())
    }

    #[test]
    fn action_cp_round_trips_operands_and_recursive_flag() -> Result<(), serde_json::Error> {
        let action = Action::Cp {
            source: "./notes.txt".to_owned(),
            target: "dev:/tmp/notes.txt".to_owned(),
            recursive: true,
        };
        let encoded = serde_json::to_value(&action)?;
        assert_eq!(
            encoded,
            json!({
                "type": "Cp",
                "source": "./notes.txt",
                "target": "dev:/tmp/notes.txt",
                "recursive": true
            })
        );
        assert_eq!(serde_json::from_value::<Action>(encoded)?, action);
        Ok(())
    }

    #[test]
    fn action_metrics_round_trips_with_its_stable_type_tag() -> Result<(), serde_json::Error> {
        let action = Action::Metrics {
            name: "ubuntu".to_owned(),
        };
        let encoded = serde_json::to_value(&action)?;
        assert_eq!(encoded, json!({"type": "Metrics", "name": "ubuntu"}));
        assert_eq!(serde_json::from_value::<Action>(encoded)?, action);
        Ok(())
    }

    #[test]
    fn action_host_metrics_round_trips_without_operands() -> Result<(), serde_json::Error> {
        let encoded = serde_json::to_value(Action::HostMetrics)?;
        assert_eq!(encoded, json!({"type": "HostMetrics"}));
        assert_eq!(
            serde_json::from_value::<Action>(encoded)?,
            Action::HostMetrics
        );
        Ok(())
    }

    #[test]
    fn action_clone_defaults_fresh_disk_and_round_trips() -> Result<(), serde_json::Error> {
        let defaulted = serde_json::from_value::<Action>(json!({
            "type": "Clone",
            "source": "dev",
            "dest": "dev-copy"
        }))?;
        assert_eq!(
            defaulted,
            Action::Clone {
                source: "dev".to_owned(),
                dest: "dev-copy".to_owned(),
                fresh_disk: false,
            }
        );

        let fresh = Action::Clone {
            source: "dev".to_owned(),
            dest: "dev-copy".to_owned(),
            fresh_disk: true,
        };
        let encoded = serde_json::to_value(&fresh)?;
        assert_eq!(
            encoded,
            json!({
                "type": "Clone",
                "source": "dev",
                "dest": "dev-copy",
                "fresh_disk": true
            })
        );
        assert_eq!(serde_json::from_value::<Action>(encoded)?, fresh);
        Ok(())
    }

    #[test]
    fn action_snapshot_variants_round_trip_with_optional_name_and_flags()
    -> Result<(), serde_json::Error> {
        let automatic = serde_json::from_value::<Action>(json!({
            "type": "SnapshotCreate",
            "name": "dev"
        }))?;
        assert_eq!(
            automatic,
            Action::SnapshotCreate {
                name: "dev".to_owned(),
                snapshot: None,
            }
        );
        assert_eq!(
            serde_json::to_value(&automatic)?,
            json!({"type": "SnapshotCreate", "name": "dev"})
        );

        let named = Action::SnapshotCreate {
            name: "dev".to_owned(),
            snapshot: Some("before-upgrade".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(&named)?,
            json!({"type": "SnapshotCreate", "name": "dev", "snapshot": "before-upgrade"})
        );

        let list = Action::SnapshotList {
            name: "dev".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&list)?,
            json!({"type": "SnapshotList", "name": "dev"})
        );

        let restore = Action::SnapshotRestore {
            name: "dev".to_owned(),
            snapshot: "before-upgrade".to_owned(),
            force: true,
            start: true,
            timeout: std::time::Duration::from_secs(30),
        };
        let encoded = serde_json::to_value(&restore)?;
        assert_eq!(encoded["type"], "SnapshotRestore");
        assert_eq!(encoded["force"], true);
        assert_eq!(encoded["start"], true);
        assert_eq!(serde_json::from_value::<Action>(encoded)?, restore);

        let defaults = serde_json::from_value::<Action>(json!({
            "type": "SnapshotRestore",
            "name": "dev",
            "snapshot": "before-upgrade",
            "timeout": {"secs": 30, "nanos": 0}
        }))?;
        assert_eq!(
            defaults,
            Action::SnapshotRestore {
                name: "dev".to_owned(),
                snapshot: "before-upgrade".to_owned(),
                force: false,
                start: false,
                timeout: std::time::Duration::from_secs(30),
            }
        );

        let remove = Action::SnapshotRemove {
            name: "dev".to_owned(),
            snapshot: "before-upgrade".to_owned(),
        };
        assert_eq!(
            serde_json::from_value::<Action>(serde_json::to_value(&remove)?)?,
            remove
        );
        Ok(())
    }

    #[test]
    fn action_system_prune_defaults_every_tier_to_false_and_round_trips()
    -> Result<(), serde_json::Error> {
        let defaulted = serde_json::from_value::<Action>(json!({"type": "SystemPrune"}))?;
        assert_eq!(
            defaulted,
            Action::SystemPrune {
                machines: false,
                images: false,
                force: false,
                dry_run: false,
            }
        );

        let full = Action::SystemPrune {
            machines: true,
            images: true,
            force: true,
            dry_run: true,
        };
        let encoded = serde_json::to_value(&full)?;
        assert_eq!(
            encoded,
            json!({
                "type": "SystemPrune",
                "machines": true,
                "images": true,
                "force": true,
                "dry_run": true
            })
        );
        assert_eq!(serde_json::from_value::<Action>(encoded)?, full);
        Ok(())
    }

    #[test]
    fn log_source_accepts_only_owned_log_names() {
        for (value, expected) in [
            ("console", LogSource::Console),
            ("vmm", LogSource::Vmm),
            ("shim", LogSource::Shim),
            ("passt", LogSource::Passt),
            ("virtiofsd-12", LogSource::Virtiofsd(12)),
        ] {
            assert_eq!(value.parse(), Ok(expected));
            assert_eq!(expected.to_string(), value);
        }
        for value in ["", "console.log", "virtiofsd-", "virtiofsd--1", "../shim"] {
            assert!(value.parse::<LogSource>().is_err(), "accepted {value}");
        }
    }
    #[test]
    fn start_readiness_action_round_trips_wait_and_timeout() -> Result<(), serde_json::Error> {
        for wait in [false, true] {
            let action = Action::Start {
                name: "ubuntu".to_owned(),
                wait,
                timeout: std::time::Duration::from_millis(12_345),
            };
            let encoded = serde_json::to_value(&action)?;
            assert_eq!(encoded["type"], "Start");
            assert_eq!(encoded["name"], "ubuntu");
            assert_eq!(encoded["wait"], wait);
            assert_eq!(serde_json::from_value::<Action>(encoded)?, action);
        }
        Ok(())
    }
}
