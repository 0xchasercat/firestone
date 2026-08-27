use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::{ErrorKind, FirestoneError};

const BUNDLED_MANIFEST: &str = include_str!("../../../deps.toml");
const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// One immutable, architecture-specific dependency artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyArtifact {
    pub dependency: String,
    pub version: String,
    pub asset: String,
    pub install_name: String,
    pub url: String,
    pub sha256: String,
}

impl DependencyArtifact {
    #[must_use]
    pub fn executable(&self) -> bool {
        matches!(self.dependency.as_str(), "cloud-hypervisor" | "virtiofsd")
    }

    #[must_use]
    pub fn expected_mode(&self) -> u32 {
        if self.executable() { 0o755 } else { 0o644 }
    }
}

#[derive(Debug, Clone)]
pub struct DependencyManifest {
    manifest_version: u32,
    dependencies: BTreeMap<String, DependencyEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
    manifest_version: u32,
    #[serde(rename = "dependency")]
    dependencies: BTreeMap<String, DependencyEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct DependencyEntry {
    version: String,
    availability: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, flatten)]
    fields: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawArtifact {
    asset: String,
    install_name: String,
    url: String,
    sha256: String,
}

impl DependencyManifest {
    pub fn bundled() -> Result<Self, FirestoneError> {
        Self::parse(BUNDLED_MANIFEST)
    }

    pub fn parse(input: &str) -> Result<Self, FirestoneError> {
        let raw = toml::from_str::<RawManifest>(input).map_err(|source| {
            FirestoneError::new(ErrorKind::Dependency, "cannot parse dependency manifest")
                .with_hint("restore deps.toml from the Firestone release")
                .with_source(source)
        })?;
        if raw.manifest_version != SUPPORTED_MANIFEST_VERSION {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "dependency manifest version {} is unsupported",
                    raw.manifest_version
                ),
            )
            .with_hint(format!(
                "use dependency manifest version {SUPPORTED_MANIFEST_VERSION}"
            )));
        }

        Ok(Self {
            manifest_version: raw.manifest_version,
            dependencies: raw.dependencies,
        })
    }

    #[must_use]
    pub const fn manifest_version(&self) -> u32 {
        self.manifest_version
    }

    pub fn version(&self, dependency: &str) -> Result<&str, FirestoneError> {
        self.entry(dependency).map(|entry| entry.version.as_str())
    }

    /// Resolves and validates one binary artifact for the requested architecture.
    ///
    /// Source-only dependencies return a dependency error. The same method will
    /// resolve them without an API change once the manifest publishes a binary
    /// architecture table.
    pub fn artifact(
        &self,
        dependency: &str,
        architecture: &str,
    ) -> Result<DependencyArtifact, FirestoneError> {
        let entry = self.entry(dependency)?;
        if entry.availability != "binary" {
            let reason = entry
                .reason
                .as_deref()
                .unwrap_or("the release does not publish an immutable binary");
            return Err(source_only_error(
                dependency,
                &entry.version,
                architecture,
                reason,
            ));
        }

        let value = entry.fields.get(architecture).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "dependency `{dependency}` {} has no binary for {architecture}",
                    entry.version
                ),
            )
            .with_hint(format!(
                "publish an immutable {architecture} artifact and checksum in deps.toml"
            ))
        })?;
        let raw = value.clone().try_into::<RawArtifact>().map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("dependency `{dependency}` has an invalid {architecture} artifact entry"),
            )
            .with_hint("repair the generated deps.toml entry")
            .with_source(source)
        })?;

        validate_install_name(dependency, &raw.install_name)?;
        validate_url(dependency, &raw.url)?;
        validate_sha256(dependency, &raw.sha256)?;

        Ok(DependencyArtifact {
            dependency: dependency.to_owned(),
            version: entry.version.clone(),
            asset: raw.asset,
            install_name: raw.install_name,
            url: raw.url,
            sha256: raw.sha256.to_ascii_lowercase(),
        })
    }

    fn entry(&self, dependency: &str) -> Result<&DependencyEntry, FirestoneError> {
        self.dependencies.get(dependency).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("dependency manifest has no `{dependency}` entry"),
            )
            .with_hint("restore deps.toml from the Firestone release")
        })
    }
}

fn source_only_error(
    dependency: &str,
    version: &str,
    architecture: &str,
    reason: &str,
) -> FirestoneError {
    let hint = if dependency == "virtiofsd" {
        format!(
            "M0-05c release blocker: publish immutable virtiofsd {architecture} binaries and checksums in deps.toml"
        )
    } else {
        format!("publish an immutable {dependency} {architecture} binary and checksum in deps.toml")
    };

    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "dependency `{dependency}` {version} has no immutable {architecture} binary: {reason}"
        ),
    )
    .with_hint(hint)
}

fn validate_install_name(dependency: &str, install_name: &str) -> Result<(), FirestoneError> {
    let path = Path::new(install_name);
    let is_one_component = path.components().count() == 1 && path.file_name().is_some();
    if install_name.is_empty()
        || install_name.contains(['/', '\\'])
        || !is_one_component
        || install_name == "."
        || install_name == ".."
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("dependency `{dependency}` has unsafe install name `{install_name}`"),
        )
        .with_hint("use a single file name without path separators in deps.toml"));
    }
    Ok(())
}

fn validate_url(dependency: &str, url: &str) -> Result<(), FirestoneError> {
    let parsed = reqwest::Url::parse(url).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("dependency `{dependency}` has an invalid artifact URL"),
        )
        .with_hint("use an immutable HTTPS release URL in deps.toml")
        .with_source(source)
    })?;
    if parsed.scheme() != "https" {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("dependency `{dependency}` artifact URL is not HTTPS"),
        )
        .with_hint("use an immutable HTTPS release URL in deps.toml"));
    }
    Ok(())
}

fn validate_sha256(dependency: &str, sha256: &str) -> Result<(), FirestoneError> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("dependency `{dependency}` has an invalid SHA-256 value"),
        )
        .with_hint("regenerate deps.toml with scripts/pin-deps.sh"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DependencyManifest;
    use crate::ErrorKind;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn bundled_manifest_resolves_binary_and_reports_virtiofsd_release_blocker()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::bundled()?;
        let cloud_hypervisor = manifest.artifact("cloud-hypervisor", "x86_64")?;

        assert_eq!(manifest.manifest_version(), 1);
        assert_eq!(cloud_hypervisor.install_name, "cloud-hypervisor-v53.0");
        assert_eq!(cloud_hypervisor.expected_mode(), 0o755);

        let error = match manifest.artifact("virtiofsd", "x86_64") {
            Err(error) => error,
            Ok(_) => {
                return Err(std::io::Error::other("source-only dependency must fail").into());
            }
        };
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("no immutable x86_64 binary"));
        assert!(error.hint().is_some_and(|hint| hint.contains("M0-05c")));
        Ok(())
    }

    #[test]
    fn artifact_future_binary_entry_requires_no_parser_change()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::parse(&format!(
            r#"
manifest_version = 1
[dependency.virtiofsd]
version = "v1.14.0"
availability = "binary"
future_metadata = "accepted"
[dependency.virtiofsd.x86_64]
asset = "virtiofsd"
install_name = "virtiofsd-v1.14.0"
url = "https://example.invalid/virtiofsd"
sha256 = "{HASH}"
future_artifact_field = true
"#
        ))?;

        let artifact = manifest.artifact("virtiofsd", "x86_64")?;
        assert_eq!(artifact.install_name, "virtiofsd-v1.14.0");
        assert!(artifact.executable());
        Ok(())
    }

    #[test]
    fn artifact_unsafe_fields_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        for (install_name, url, hash) in [
            ("../escape", "https://example.invalid/a", HASH),
            ("safe", "http://example.invalid/a", HASH),
            ("safe", "https://example.invalid/a", "abc"),
        ] {
            let manifest = DependencyManifest::parse(&format!(
                r#"
manifest_version = 1
[dependency.test]
version = "1"
availability = "binary"
[dependency.test.x86_64]
asset = "test"
install_name = "{install_name}"
url = "{url}"
sha256 = "{hash}"
"#
            ))?;
            assert!(manifest.artifact("test", "x86_64").is_err());
        }
        Ok(())
    }
}
