use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::{ErrorKind, FirestoneError};

const BUNDLED_MANIFEST: &str = include_str!("../../../deps.toml");
const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// Manifest name of the pinned kernel used for OCI direct kernel boot.
pub const DIRECT_BOOT_KERNEL_DEPENDENCY: &str = "cloud-hypervisor-kernel";

/// Pinned direct-boot kernel release recorded in `deps.toml`.
pub const PINNED_DIRECT_BOOT_KERNEL_VERSION: &str = "ch-release-v6.16.9-20260508";

/// Manifest name of Firestone's own guest PID 1 payload (SPEC §10.5, §17.2).
pub const FIRESTONE_INIT_DEPENDENCY: &str = "firestone-init";

/// Pinned `firestone-init` release recorded in `deps.toml`.
pub const PINNED_FIRESTONE_INIT_VERSION: &str = "v0.1.0";

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
        matches!(
            self.dependency.as_str(),
            "cloud-hypervisor" | "virtiofsd" | "passt" | "qemu-img"
        )
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
    #[serde(default)]
    architectures: Option<Vec<String>>,
    #[serde(default, flatten)]
    fields: BTreeMap<String, toml::Value>,
}

impl DependencyEntry {
    fn supports_architecture(&self, architecture: &str) -> bool {
        self.architectures
            .as_ref()
            .is_none_or(|architectures| architectures.iter().any(|value| value == architecture))
    }
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

    /// Resolves every binary dependency for one architecture in name order.
    pub fn artifacts(
        &self,
        architecture: &str,
    ) -> Result<BTreeMap<String, DependencyArtifact>, FirestoneError> {
        let mut artifacts = BTreeMap::new();
        for (name, entry) in &self.dependencies {
            if entry.supports_architecture(architecture) {
                artifacts.insert(name.clone(), self.artifact(name, architecture)?);
            }
        }
        Ok(artifacts)
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
        if !entry.supports_architecture(architecture) {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "dependency `{dependency}` {} is outside its {architecture} runtime scope",
                    entry.version
                ),
            )
            .with_hint("use a release target listed in the dependency architectures field"));
        }
        match entry.availability.as_str() {
            "binary" => {}
            "source-only" => {
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
            availability => {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "dependency `{dependency}` has unsupported availability `{availability}`"
                    ),
                )
                .with_hint("use `binary` or `source-only` in deps.toml"));
            }
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

    /// Resolves the embedded x86_64 passt payload used by the targeted
    /// AppArmor installation path.
    pub fn embedded_passt(&self, architecture: &str) -> Result<DependencyArtifact, FirestoneError> {
        if architecture != "x86_64" {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "the embedded passt helper is unavailable for {architecture}; its runtime scope is x86_64"
                ),
            )
            .with_hint("run Firestone on Linux x86_64 for AppArmor passt remediation"));
        }
        let artifact = self.artifact("passt", architecture).map_err(|error| {
            FirestoneError::new(
                ErrorKind::Dependency,
                "embedded passt helper metadata is unavailable",
            )
            .with_hint(
                "provide the pinned x86_64 passt payload and extraction result before AppArmor repair",
            )
            .with_source(error)
        })?;
        if artifact.version != crate::PINNED_PASST_VERSION {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "embedded passt helper version {} does not match pinned {}",
                    artifact.version,
                    crate::PINNED_PASST_VERSION
                ),
            )
            .with_hint("regenerate the embedded helper manifest from the pinned passt source"));
        }
        Ok(artifact)
    }

    /// Resolves the pinned direct-boot kernel used by OCI machines (SPEC §9.1).
    ///
    /// The kernel is a lazily installed data payload, never an executable, and
    /// the firmware payload path is unaffected by this accessor.
    pub fn direct_boot_kernel(
        &self,
        architecture: &str,
    ) -> Result<DependencyArtifact, FirestoneError> {
        let artifact = self
            .artifact(DIRECT_BOOT_KERNEL_DEPENDENCY, architecture)
            .map_err(|error| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("pinned direct-boot kernel metadata is unavailable for {architecture}"),
                )
                .with_hint(
                    "regenerate deps.toml with scripts/pin-deps.sh refresh --arch all before booting an OCI machine",
                )
                .with_source(error)
            })?;
        if artifact.executable() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                "pinned direct-boot kernel is classified as an executable payload",
            )
            .with_hint("publish the kernel image as a mode-0644 dependency artifact"));
        }
        Ok(artifact)
    }

    /// Resolves the pinned `firestone-init` payload injected into a packed OCI
    /// rootfs (SPEC §8.5, §10.5, §17.2).
    ///
    /// The payload is data, never an executable published for the host to run:
    /// the injection gives it its own 0755 tar header inside the guest image.
    pub fn firestone_init(&self, architecture: &str) -> Result<DependencyArtifact, FirestoneError> {
        let artifact = self
            .artifact(FIRESTONE_INIT_DEPENDENCY, architecture)
            .map_err(|error| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("pinned firestone-init metadata is unavailable for {architecture}"),
                )
                .with_hint(
                    "regenerate deps.toml with scripts/pin-deps.sh refresh --arch all before packing an OCI image",
                )
                .with_source(error)
            })?;
        if artifact.executable() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                "pinned firestone-init is classified as an executable payload",
            )
            .with_hint("publish the guest init as a mode-0644 dependency artifact"));
        }
        Ok(artifact)
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

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn bundled_manifest_binary_dependencies_resolve() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::bundled()?;
        let cloud_hypervisor = manifest.artifact("cloud-hypervisor", "x86_64")?;
        let virtiofsd = manifest.artifact("virtiofsd", "x86_64")?;

        assert_eq!(manifest.manifest_version(), 1);
        assert_eq!(cloud_hypervisor.install_name, "cloud-hypervisor-v53.0");
        assert_eq!(cloud_hypervisor.expected_mode(), 0o755);
        assert_eq!(virtiofsd.version, "v1.14.0");
        assert_eq!(virtiofsd.install_name, "virtiofsd-v1.14.0");
        assert_eq!(
            virtiofsd.sha256,
            "9ad3e33c45dd816b24ad483b60ca469974ba54c3b37ef93be3da2a623986646f"
        );
        assert_eq!(virtiofsd.expected_mode(), 0o755);
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

    #[test]
    fn artifact_unknown_availability_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::parse(
            r#"
manifest_version = 1
[dependency.test]
version = "1"
availability = "sometimes"
"#,
        )?;

        let error = match manifest.artifact("test", "x86_64") {
            Err(error) => error,
            Ok(_) => return Err(std::io::Error::other("availability should fail").into()),
        };
        assert!(error.message().contains("unsupported availability"));
        Ok(())
    }
    #[test]
    fn artifacts_architecture_scope_omits_unsupported_runtime_payloads()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::parse(&format!(
            r#"
manifest_version = 1
[dependency.portable]
version = "1"
availability = "binary"
[dependency.portable.x86_64]
asset = "portable-x86"
install_name = "portable"
url = "https://example.invalid/portable-x86"
sha256 = "{HASH}"
[dependency.portable.aarch64]
asset = "portable-arm"
install_name = "portable"
url = "https://example.invalid/portable-arm"
sha256 = "{HASH}"
[dependency.x86-only]
version = "1"
availability = "binary"
architectures = ["x86_64"]
[dependency.x86-only.x86_64]
asset = "x86-only"
install_name = "x86-only"
url = "https://example.invalid/x86-only"
sha256 = "{HASH}"
"#
        ))?;

        assert_eq!(
            manifest
                .artifacts("x86_64")?
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["portable".to_owned(), "x86-only".to_owned()]
        );
        assert_eq!(
            manifest
                .artifacts("aarch64")?
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["portable".to_owned()]
        );
        let error = manifest
            .artifact("x86-only", "aarch64")
            .err()
            .ok_or("aarch64 unexpectedly resolved the x86-only artifact")?;
        assert!(
            error
                .message()
                .contains("outside its aarch64 runtime scope")
        );
        Ok(())
    }

    #[test]
    fn embedded_passt_requires_exact_x86_64_pinned_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::parse(&format!(
            r#"
manifest_version = 1
[dependency.passt]
version = "2025_02_17.a1e48a0"
availability = "binary"
[dependency.passt.x86_64]
asset = "passt"
install_name = "passt-2025_02_17.a1e48a0"
url = "https://example.invalid/passt"
sha256 = "{HASH}"
"#
        ))?;

        let passt = manifest.embedded_passt("x86_64")?;
        assert_eq!(passt.dependency, "passt");
        assert!(passt.executable());
        assert_eq!(passt.expected_mode(), 0o755);
        let error = manifest
            .embedded_passt("aarch64")
            .err()
            .ok_or("aarch64 embedded passt unexpectedly resolved")?;
        assert!(error.message().contains("runtime scope is x86_64"));
        Ok(())
    }

    #[test]
    fn direct_boot_kernel_bundled_manifest_resolves_both_architectures()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::bundled()?;

        let x86_64 = manifest.direct_boot_kernel("x86_64")?;
        assert_eq!(x86_64.dependency, super::DIRECT_BOOT_KERNEL_DEPENDENCY);
        assert_eq!(x86_64.version, super::PINNED_DIRECT_BOOT_KERNEL_VERSION);
        assert_eq!(x86_64.asset, "bzImage-x86_64");
        assert_eq!(x86_64.install_name, "bzImage-ch-release-v6.16.9-20260508");
        assert_eq!(
            x86_64.sha256,
            "58088758f601a04ef85b09cf23db5530d51edc039ed47afbf2264c5b762cb568"
        );
        assert!(!x86_64.executable());
        assert_eq!(x86_64.expected_mode(), 0o644);

        let aarch64 = manifest.direct_boot_kernel("aarch64")?;
        assert_eq!(aarch64.asset, "Image-arm64");
        assert_eq!(aarch64.install_name, "Image-ch-release-v6.16.9-20260508");
        assert_eq!(
            aarch64.sha256,
            "69d1b1235381ec50f1b45cf771a7dff4a9013d452833ab34682d6283e2114010"
        );
        assert_eq!(aarch64.expected_mode(), 0o644);
        Ok(())
    }

    #[test]
    fn direct_boot_kernel_missing_entry_reports_dependency_hint()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = DependencyManifest::parse(
            r#"
manifest_version = 1
[dependency.cloud-hypervisor]
version = "v53.0"
availability = "binary"
"#,
        )?;

        let error = manifest
            .direct_boot_kernel("x86_64")
            .err()
            .ok_or("the absent direct-boot kernel unexpectedly resolved")?;
        assert!(
            error
                .message()
                .contains("pinned direct-boot kernel metadata is unavailable for x86_64")
        );
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("scripts/pin-deps.sh refresh --arch all"))
        );
        Ok(())
    }
}
