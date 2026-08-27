use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{ErrorKind, FirestoneError};

const BUILT_IN_CATALOG: &str = include_str!("../../../catalog/images.toml");
const CATALOG_FILE_HINT: &str = "fix the catalog file and retry";

/// Firmware declared by a catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFirmware {
    Rhf,
    Edk2,
}

/// On-disk image format accepted by the image pull path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Qcow2,
    Raw,
}

/// Digest algorithm used by a checksum manifest or direct digest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    #[default]
    Sha256,
    Sha512,
}

/// Where the expected image checksum comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogChecksum {
    ManifestUrl(String),
    Sha256(String),
}

/// Download and checksum data for one host architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogArchSource {
    pub url: String,
    pub checksum: CatalogChecksum,
    pub checksum_algorithm: ChecksumAlgorithm,
}

/// One canonical distro release in a catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub distro: String,
    pub version: String,
    pub aliases: Vec<String>,
    pub default: bool,
    pub firmware: CatalogFirmware,
    pub format: ImageFormat,
    pub arch: BTreeMap<String, CatalogArchSource>,
}

impl CatalogEntry {
    #[must_use]
    pub fn canonical_reference(&self) -> String {
        format!("{}:{}", self.distro, self.version)
    }
}

/// A catalog entry resolved for one host architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCatalogEntry {
    pub canonical_reference: String,
    pub source: CatalogArchSource,
    pub checksum_algorithm: ChecksumAlgorithm,
    pub firmware: CatalogFirmware,
    pub format: ImageFormat,
    pub architecture: String,
}

/// The validated, deterministically ordered image catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    entries: BTreeMap<String, CatalogEntry>,
}

impl Catalog {
    /// Parses and validates the catalog embedded in the Firestone binary.
    pub fn built_in() -> Result<Self, FirestoneError> {
        let entries = parse_document(BUILT_IN_CATALOG, "built-in catalog", false)?;
        Self::from_entries(entries)
    }

    /// Loads the built-in catalog and ordered user overrides.
    ///
    /// A missing `config_catalog` is treated as an absent optional catalog. Every
    /// path in `extra_catalogs` is required. Later documents replace an entry
    /// with the same canonical `distro:version` reference.
    pub fn load(config_catalog: &Path, extra_catalogs: &[PathBuf]) -> Result<Self, FirestoneError> {
        let built_in = parse_document(BUILT_IN_CATALOG, "built-in catalog", false)?;
        let mut entries = entries_by_reference(built_in);

        merge_catalog_file(&mut entries, config_catalog, true)?;
        for path in extra_catalogs {
            merge_catalog_file(&mut entries, path, false)?;
        }

        Self::validate(entries)
    }

    /// Resolves a catalog reference and selects its host-architecture source.
    pub fn resolve(
        &self,
        reference: &str,
        host_architecture: &str,
    ) -> Result<ResolvedCatalogEntry, FirestoneError> {
        let entry = self
            .find(reference)
            .ok_or_else(|| self.unknown_image(reference))?;
        let canonical_reference = entry.canonical_reference();
        let source = entry.arch.get(host_architecture).cloned().ok_or_else(|| {
            let available = entry.arch.keys().cloned().collect::<Vec<_>>().join(", ");
            FirestoneError::new(
                ErrorKind::NotFound,
                format!(
                    "image '{canonical_reference}' has no source for architecture \
                     '{host_architecture}'; available architectures: {available}"
                ),
            )
            .with_hint("use a host with one of the available architectures")
        })?;

        Ok(ResolvedCatalogEntry {
            canonical_reference,
            checksum_algorithm: source.checksum_algorithm,
            source,
            firmware: entry.firmware,
            format: entry.format,
            architecture: host_architecture.to_owned(),
        })
    }

    /// Reports whether a default, version, or alias reference exists before
    /// selecting a host architecture.
    #[must_use]
    pub fn contains_reference(&self, reference: &str) -> bool {
        self.find(reference).is_some()
    }

    /// Iterates over canonical entries in lexical `distro:version` order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &CatalogEntry> {
        self.entries.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn from_entries(entries: Vec<CatalogEntry>) -> Result<Self, FirestoneError> {
        Self::validate(entries_by_reference(entries))
    }

    fn validate(entries: BTreeMap<String, CatalogEntry>) -> Result<Self, FirestoneError> {
        validate_defaults(&entries)?;
        validate_reference_names(&entries)?;
        Ok(Self { entries })
    }

    fn find(&self, reference: &str) -> Option<&CatalogEntry> {
        let (distro, selector) = reference
            .split_once(':')
            .map_or((reference, None), |(distro, selector)| {
                (distro, Some(selector))
            });

        match selector {
            Some(selector) => self.entries.values().find(|entry| {
                entry.distro == distro
                    && (entry.version == selector
                        || entry.aliases.iter().any(|alias| alias == selector))
            }),
            None => self
                .entries
                .values()
                .find(|entry| entry.distro == distro && entry.default),
        }
    }

    fn unknown_image(&self, reference: &str) -> FirestoneError {
        let closest = self.closest_names(reference, 3).join(", ");
        let message = if closest.is_empty() {
            format!("unknown image '{reference}'")
        } else {
            format!("unknown image '{reference}'; closest catalog images: {closest}")
        };
        FirestoneError::new(ErrorKind::NotFound, message)
            .with_hint("run `firestone images ls` to list catalog images")
    }

    fn closest_names(&self, reference: &str, limit: usize) -> Vec<String> {
        let mut names = BTreeSet::new();
        for entry in self.entries.values() {
            names.insert(entry.canonical_reference());
            if entry.default {
                names.insert(entry.distro.clone());
            }
            for alias in &entry.aliases {
                names.insert(format!("{}:{alias}", entry.distro));
            }
        }

        let mut ranked = names
            .into_iter()
            .map(|name| (levenshtein(reference, &name), name))
            .collect::<Vec<_>>();
        ranked.sort();
        ranked
            .into_iter()
            .take(limit)
            .map(|(_, name)| name)
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDocument {
    #[serde(default)]
    image: Vec<RawCatalogEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalogEntry {
    distro: String,
    version: String,
    aliases: Vec<String>,
    default: bool,
    firmware: CatalogFirmware,
    format: ImageFormat,
    arch: BTreeMap<String, RawArchSource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArchSource {
    url: String,
    checksum_url: Option<String>,
    sha256: Option<String>,
    #[serde(default)]
    checksum_alg: ChecksumAlgorithm,
}

fn merge_catalog_file(
    entries: &mut BTreeMap<String, CatalogEntry>,
    path: &Path,
    optional: bool,
) -> Result<(), FirestoneError> {
    let input = match fs::read_to_string(path) {
        Ok(input) => input,
        Err(error) if optional && optional_catalog_file_is_absent(path, &error) => return Ok(()),
        Err(error) => {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                ErrorKind::NotFound
            } else {
                ErrorKind::Generic
            };
            return Err(FirestoneError::new(
                kind,
                format!("cannot read catalog '{}': {error}", path.display()),
            )
            .with_hint("check the catalog path and file permissions")
            .with_source(error));
        }
    };

    let label = format!("catalog '{}'", path.display());
    for entry in parse_document(&input, &label, true)? {
        entries.insert(entry.canonical_reference(), entry);
    }
    Ok(())
}

fn parse_document(
    input: &str,
    source: &str,
    user_editable: bool,
) -> Result<Vec<CatalogEntry>, FirestoneError> {
    let document = toml::from_str::<CatalogDocument>(input).map_err(|error| {
        with_catalog_hint(
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("cannot parse {source}: {error}"),
            )
            .with_source(error),
            user_editable,
        )
    })?;

    let mut canonical_references = BTreeSet::new();
    let mut entries = Vec::with_capacity(document.image.len());
    for raw in document.image {
        let entry = convert_entry(raw, source, user_editable)?;
        let canonical_reference = entry.canonical_reference();
        if !canonical_references.insert(canonical_reference.clone()) {
            return Err(invalid_catalog(
                source,
                format!("duplicate canonical reference '{canonical_reference}'"),
                user_editable,
            ));
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn convert_entry(
    raw: RawCatalogEntry,
    source: &str,
    user_editable: bool,
) -> Result<CatalogEntry, FirestoneError> {
    validate_reference_component("distro", &raw.distro, source, user_editable)?;
    validate_reference_component("version", &raw.version, source, user_editable)?;

    let canonical_reference = format!("{}:{}", raw.distro, raw.version);
    let mut aliases = BTreeSet::new();
    for alias in &raw.aliases {
        validate_reference_component("alias", alias, source, user_editable)?;
        if !aliases.insert(alias.clone()) {
            return Err(invalid_catalog(
                source,
                format!("image '{canonical_reference}' repeats alias '{alias}'"),
                user_editable,
            ));
        }
    }

    if raw.arch.is_empty() {
        return Err(invalid_catalog(
            source,
            format!("image '{canonical_reference}' has no architecture sources"),
            user_editable,
        ));
    }

    let mut arch = BTreeMap::new();
    for (architecture, raw_source) in raw.arch {
        validate_architecture_name(&architecture, &canonical_reference, source, user_editable)?;
        let catalog_source = convert_arch_source(
            raw_source,
            &canonical_reference,
            &architecture,
            source,
            user_editable,
        )?;
        arch.insert(architecture, catalog_source);
    }

    Ok(CatalogEntry {
        distro: raw.distro,
        version: raw.version,
        aliases: aliases.into_iter().collect(),
        default: raw.default,
        firmware: raw.firmware,
        format: raw.format,
        arch,
    })
}

fn convert_arch_source(
    raw: RawArchSource,
    canonical_reference: &str,
    architecture: &str,
    source: &str,
    user_editable: bool,
) -> Result<CatalogArchSource, FirestoneError> {
    let field_context = format!("image '{canonical_reference}' architecture '{architecture}'");
    if !is_https_url(&raw.url) {
        return Err(invalid_catalog(
            source,
            format!(
                "{field_context} has invalid image URL '{}'; expected HTTPS with a host and \
                 without credentials or a fragment",
                raw.url
            ),
            user_editable,
        ));
    }

    let checksum = match (raw.checksum_url, raw.sha256) {
        (Some(checksum_url), None) => {
            if !is_https_url(&checksum_url) {
                return Err(invalid_catalog(
                    source,
                    format!(
                        "{field_context} has invalid checksum URL '{checksum_url}'; expected \
                         HTTPS with a host and without credentials or a fragment"
                    ),
                    user_editable,
                ));
            }
            CatalogChecksum::ManifestUrl(checksum_url)
        }
        (None, Some(sha256)) => {
            if raw.checksum_alg != ChecksumAlgorithm::Sha256 {
                return Err(invalid_catalog(
                    source,
                    format!("{field_context} uses direct sha256 with a non-sha256 algorithm"),
                    user_editable,
                ));
            }
            if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(invalid_catalog(
                    source,
                    format!("{field_context} has an invalid sha256 digest"),
                    user_editable,
                ));
            }
            CatalogChecksum::Sha256(sha256.to_ascii_lowercase())
        }
        (Some(_), Some(_)) => {
            return Err(invalid_catalog(
                source,
                format!("{field_context} sets both checksum_url and sha256"),
                user_editable,
            ));
        }
        (None, None) => {
            return Err(invalid_catalog(
                source,
                format!("{field_context} requires checksum_url or sha256"),
                user_editable,
            ));
        }
    };

    Ok(CatalogArchSource {
        url: raw.url,
        checksum,
        checksum_algorithm: raw.checksum_alg,
    })
}

fn validate_reference_component(
    field: &str,
    value: &str,
    source: &str,
    user_editable: bool,
) -> Result<(), FirestoneError> {
    if value.is_empty()
        || value.trim() != value
        || value.contains(':')
        || value.chars().any(char::is_whitespace)
    {
        return Err(invalid_catalog(
            source,
            format!("{field} '{value}' is not a valid catalog reference component"),
            user_editable,
        ));
    }
    Ok(())
}

fn validate_architecture_name(
    architecture: &str,
    canonical_reference: &str,
    source: &str,
    user_editable: bool,
) -> Result<(), FirestoneError> {
    if !matches!(architecture, "x86_64" | "aarch64") {
        return Err(invalid_catalog(
            source,
            format!(
                "image '{canonical_reference}' key 'image.arch.{architecture}' is unsupported; \
                 expected x86_64 or aarch64"
            ),
            user_editable,
        ));
    }
    Ok(())
}

fn validate_defaults(entries: &BTreeMap<String, CatalogEntry>) -> Result<(), FirestoneError> {
    let mut distro_entries: BTreeMap<&str, Vec<&CatalogEntry>> = BTreeMap::new();
    for entry in entries.values() {
        distro_entries.entry(&entry.distro).or_default().push(entry);
    }

    for (distro, releases) in distro_entries {
        let defaults = releases
            .iter()
            .filter(|entry| entry.default)
            .map(|entry| entry.canonical_reference())
            .collect::<Vec<_>>();
        if defaults.len() != 1 {
            let release_names = releases
                .iter()
                .map(|entry| entry.canonical_reference())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!(
                    "catalog distro '{distro}' must have exactly one default release; \
                     found {} across {release_names}",
                    defaults.len()
                ),
            )
            .with_hint(CATALOG_FILE_HINT));
        }
    }
    Ok(())
}

fn validate_reference_names(
    entries: &BTreeMap<String, CatalogEntry>,
) -> Result<(), FirestoneError> {
    let mut names: BTreeMap<(String, String), String> = BTreeMap::new();
    for entry in entries.values() {
        let canonical_reference = entry.canonical_reference();
        let mut selectors = entry.aliases.clone();
        selectors.push(entry.version.clone());
        selectors.sort();
        selectors.dedup();

        for selector in selectors {
            let name = (entry.distro.clone(), selector.clone());
            if let Some(previous) = names.insert(name, canonical_reference.clone()) {
                if previous != canonical_reference {
                    return Err(FirestoneError::new(
                        ErrorKind::InvalidSpec,
                        format!(
                            "catalog name '{}:{selector}' is ambiguous between '{previous}' and \
                             '{canonical_reference}'",
                            entry.distro
                        ),
                    )
                    .with_hint(CATALOG_FILE_HINT));
                }
            }
        }
    }
    Ok(())
}

fn entries_by_reference(entries: Vec<CatalogEntry>) -> BTreeMap<String, CatalogEntry> {
    entries
        .into_iter()
        .map(|entry| (entry.canonical_reference(), entry))
        .collect()
}

fn invalid_catalog(
    source: &str,
    detail: impl std::fmt::Display,
    user_editable: bool,
) -> FirestoneError {
    with_catalog_hint(
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("invalid {source}: {detail}"),
        ),
        user_editable,
    )
}

fn with_catalog_hint(error: FirestoneError, user_editable: bool) -> FirestoneError {
    if user_editable {
        error.with_hint(CATALOG_FILE_HINT)
    } else {
        error
    }
}

fn optional_catalog_file_is_absent(path: &Path, read_error: &std::io::Error) -> bool {
    read_error.kind() == std::io::ErrorKind::NotFound
        && matches!(
            fs::symlink_metadata(path),
            Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound
        )
}

fn is_https_url(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }

    let syntax_violation = std::cell::Cell::new(false);
    let record_violation = |_| syntax_violation.set(true);
    let Ok(parsed) = Url::options()
        .syntax_violation_callback(Some(&record_violation))
        .parse(value)
    else {
        return false;
    };
    !syntax_violation.get()
        && parsed.scheme() == "https"
        && parsed.has_host()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && !parsed.authority().contains('@')
        && parsed.fragment().is_none()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();

    for (left_index, left_character) in left.chars().enumerate() {
        let mut current = Vec::with_capacity(right.len() + 1);
        current.push(left_index + 1);
        for (right_index, right_character) in right.iter().enumerate() {
            let substitution =
                previous[right_index] + usize::from(left_character != *right_character);
            let insertion = current[right_index] + 1;
            let deletion = previous[right_index + 1] + 1;
            current.push(substitution.min(insertion).min(deletion));
        }
        previous = current;
    }

    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        BUILT_IN_CATALOG, Catalog, CatalogChecksum, CatalogFirmware, ChecksumAlgorithm,
        ImageFormat, entries_by_reference, is_https_url, parse_document,
    };
    use crate::ErrorKind;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    const TEST_SOURCE: &str = r#"
[[image]]
distro = "test"
version = "1"
aliases = ["stable"]
default = true
firmware = "rhf"
format = "qcow2"

[image.arch.x86_64]
url = "https://example.invalid/test.qcow2"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "firestone-catalog-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn catalog_from_toml(input: &str) -> Result<Catalog, crate::FirestoneError> {
        let entries = parse_document(input, "test catalog", true)?;
        Catalog::validate(entries_by_reference(entries))
    }

    fn error_from<T>(result: Result<T, crate::FirestoneError>) -> crate::FirestoneError {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error,
        }
    }

    fn replacement_source(url: &str) -> String {
        format!(
            r#"
[[image]]
distro = "ubuntu"
version = "24.04"
aliases = ["noble"]
default = true
firmware = "edk2"
format = "raw"

[image.arch.x86_64]
url = "{url}"
checksum_url = "https://example.invalid/SHA512SUMS"
checksum_alg = "sha512"
"#
        )
    }

    #[test]
    fn built_in_catalog_parses_five_releases_and_ten_sources() -> Result<(), Box<dyn Error>> {
        let catalog = Catalog::built_in()?;
        let sources = catalog
            .entries()
            .map(|entry| entry.arch.len())
            .sum::<usize>();

        assert_eq!(catalog.len(), 5);
        assert_eq!(sources, 10);
        Ok(())
    }

    #[test]
    fn catalog_default_and_alias_resolve_canonical_release() -> Result<(), Box<dyn Error>> {
        let catalog = Catalog::built_in()?;

        let default = catalog.resolve("ubuntu", "x86_64")?;
        let alias = catalog.resolve("debian:bookworm", "aarch64")?;

        assert_eq!(default.canonical_reference, "ubuntu:24.04");
        assert_eq!(default.architecture, "x86_64");
        assert_eq!(alias.canonical_reference, "debian:12");
        assert_eq!(alias.architecture, "aarch64");
        Ok(())
    }

    #[test]
    fn catalog_reference_membership_matches_default_version_and_alias() -> Result<(), Box<dyn Error>>
    {
        let catalog = Catalog::built_in()?;

        assert!(catalog.contains_reference("ubuntu"));
        assert!(catalog.contains_reference("ubuntu:24.04"));
        assert!(catalog.contains_reference("ubuntu:noble"));
        assert!(!catalog.contains_reference("ubunut"));
        Ok(())
    }

    #[test]
    fn catalog_exact_version_resolves_declared_data() -> Result<(), Box<dyn Error>> {
        let catalog = Catalog::built_in()?;

        let resolved = catalog.resolve("debian:13", "x86_64")?;

        assert_eq!(resolved.canonical_reference, "debian:13");
        assert_eq!(resolved.firmware, CatalogFirmware::Rhf);
        assert_eq!(resolved.format, ImageFormat::Qcow2);
        assert_eq!(resolved.checksum_algorithm, ChecksumAlgorithm::Sha512);
        assert!(matches!(
            resolved.source.checksum,
            CatalogChecksum::ManifestUrl(_)
        ));
        Ok(())
    }

    #[test]
    fn catalog_ordered_overrides_use_last_exact_replacement() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ordered")?;
        let config = directory.path().join("catalog.toml");
        let first = directory.path().join("first.toml");
        let last = directory.path().join("last.toml");
        fs::write(
            &config,
            replacement_source("https://example.invalid/config.raw"),
        )?;
        fs::write(
            &first,
            replacement_source("https://example.invalid/first.raw"),
        )?;
        fs::write(
            &last,
            replacement_source("https://example.invalid/last.raw"),
        )?;

        let catalog = Catalog::load(&config, &[first, last])?;
        let resolved = catalog.resolve("ubuntu", "x86_64")?;

        assert_eq!(resolved.source.url, "https://example.invalid/last.raw");
        assert_eq!(resolved.firmware, CatalogFirmware::Edk2);
        assert_eq!(resolved.format, ImageFormat::Raw);
        assert_eq!(resolved.checksum_algorithm, ChecksumAlgorithm::Sha512);
        Ok(())
    }

    #[test]
    fn catalog_duplicate_canonical_reference_is_rejected() {
        let input = format!("{TEST_SOURCE}\n{TEST_SOURCE}");

        let error = error_from(catalog_from_toml(&input));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error
                .message()
                .contains("duplicate canonical reference 'test:1'")
        );
    }

    #[test]
    fn catalog_duplicate_default_is_rejected() {
        let second = TEST_SOURCE
            .replace("version = \"1\"", "version = \"2\"")
            .replace("aliases = [\"stable\"]", "aliases = []");
        let input = format!("{TEST_SOURCE}\n{second}");

        let error = error_from(catalog_from_toml(&input));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("exactly one default release"));
    }

    #[test]
    fn catalog_ambiguous_alias_is_rejected() {
        let second = TEST_SOURCE
            .replace("version = \"1\"", "version = \"2\"")
            .replace("aliases = [\"stable\"]", "aliases = []")
            .replace("default = true", "default = false");
        let first = TEST_SOURCE.replace("[\"stable\"]", "[\"2\"]");

        let error = error_from(catalog_from_toml(&(first + &second)));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error
                .message()
                .contains("catalog name 'test:2' is ambiguous")
        );
    }

    #[test]
    fn catalog_unknown_key_is_rejected() {
        let input = TEST_SOURCE.replace("format = \"qcow2\"", "format = \"qcow2\"\nextra = true");

        let error = error_from(catalog_from_toml(&input));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("unknown field `extra`"));
    }

    #[test]
    fn catalog_missing_architecture_names_available_sources() -> Result<(), Box<dyn Error>> {
        let catalog = Catalog::built_in()?;

        let error = error_from(catalog.resolve("ubuntu:24.04", "riscv64"));

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(
            error
                .message()
                .contains("available architectures: aarch64, x86_64")
        );
        Ok(())
    }

    #[test]
    fn catalog_unknown_reference_suggestions_are_deterministic() -> Result<(), Box<dyn Error>> {
        let catalog = Catalog::built_in()?;

        let first = error_from(catalog.resolve("ubunut", "x86_64"));
        let second = error_from(catalog.resolve("ubunut", "x86_64"));

        assert_eq!(first.kind(), ErrorKind::NotFound);
        assert_eq!(first.message(), second.message());
        assert!(first.message().contains("closest catalog images: ubuntu"));
        Ok(())
    }

    #[test]
    fn catalog_override_parse_error_names_source_path() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("parse-error")?;
        let missing_config = directory.path().join("missing-config.toml");
        let override_path = directory.path().join("broken.toml");
        fs::write(&override_path, "[[image]\n")?;

        let error = error_from(Catalog::load(
            &missing_config,
            std::slice::from_ref(&override_path),
        ));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error
                .message()
                .contains(&override_path.display().to_string())
        );
        assert_eq!(error.hint(), Some("fix the catalog file and retry"));
        Ok(())
    }

    #[test]
    fn catalog_missing_optional_file_uses_built_in() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("missing-optional")?;
        let missing_config = directory.path().join("catalog.toml");

        let catalog = Catalog::load(&missing_config, &[])?;

        assert_eq!(catalog.len(), 5);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn catalog_optional_broken_symlink_returns_read_error() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("broken-symlink")?;
        let missing_target = directory.path().join("missing-target.toml");
        let config = directory.path().join("catalog.toml");
        std::os::unix::fs::symlink(&missing_target, &config)?;

        let error = error_from(Catalog::load(&config, &[]));

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.message().contains(&config.display().to_string()));
        assert_eq!(
            error.hint(),
            Some("check the catalog path and file permissions")
        );
        Ok(())
    }

    #[test]
    fn catalog_supported_https_urls_are_accepted() {
        let urls = [
            "https://images.example.invalid/image.qcow2",
            "https://192.0.2.1:8443/image.qcow2?build=1",
            "https://[2001:db8::1]/image.qcow2",
        ];

        for value in urls {
            assert!(is_https_url(value), "expected valid HTTPS URL: {value:?}");
        }
    }

    #[test]
    fn catalog_invalid_https_urls_are_rejected() {
        let urls = [
            "http://images.example.invalid/image.qcow2",
            "https:///image.qcow2",
            "https://:443/image.qcow2",
            "https://@images.example.invalid/image.qcow2",
            "https://user@images.example.invalid/image.qcow2",
            "https://user:password@images.example.invalid/image.qcow2",
            "https://images.example.invalid/image.qcow2#digest",
            "https://images.example.invalid/image name.qcow2",
            " https://images.example.invalid/image.qcow2",
            "https://images.example.invalid/image.qcow2\n",
            "https://[2001:db8::1/image.qcow2",
        ];

        for value in urls {
            assert!(
                !is_https_url(value),
                "expected invalid HTTPS URL: {value:?}"
            );
        }
    }

    #[test]
    fn catalog_unsupported_architecture_key_is_rejected() {
        let input = TEST_SOURCE.replace("image.arch.x86_64", "image.arch.riscv64");

        let error = error_from(catalog_from_toml(&input));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(
            error
                .message()
                .contains("key 'image.arch.riscv64' is unsupported")
        );
    }

    #[test]
    fn catalog_invalid_source_fields_are_rejected() {
        let cases = [
            TEST_SOURCE.replace("https://example.invalid", "http://example.invalid"),
            TEST_SOURCE.replace(
                "sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
                "checksum_url = \"https://example.invalid/SUMS\"\nchecksum_alg = \"md5\"",
            ),
            TEST_SOURCE.replace("format = \"qcow2\"", "format = \"vmdk\""),
            TEST_SOURCE.replace("firmware = \"rhf\"", "firmware = \"bios\""),
        ];

        for input in cases {
            let error = error_from(catalog_from_toml(&input));
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        }
    }

    #[test]
    fn catalog_empty_architecture_table_is_rejected() {
        let input = r#"
[[image]]
distro = "test"
version = "1"
aliases = []
default = true
firmware = "rhf"
format = "qcow2"
arch = {}
"#;

        let error = error_from(catalog_from_toml(input));

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("has no architecture sources"));
    }

    #[test]
    fn built_in_source_is_compiled_from_catalog_file() {
        assert!(BUILT_IN_CATALOG.contains("distro = \"ubuntu\""));
        assert!(BUILT_IN_CATALOG.contains("distro = \"fedora\""));
    }
}
