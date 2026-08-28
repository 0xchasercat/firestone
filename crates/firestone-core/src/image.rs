use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg, OFlag, openat},
    sys::stat::Mode,
};
use reqwest::{
    blocking::Client,
    header::{ACCEPT_ENCODING, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256, Sha512};
use url::Url;

use crate::{
    Arch, ByteSize, Catalog, CatalogChecksum, CatalogFirmware, ChecksumAlgorithm, Cmd, ErrorKind,
    Event, EventSink, FirestoneError, ImageFormat, ImageRef, Level, MachineLock, MachineState,
    Paths, StateImage, StateStore, StepId, Unit, atomic,
    bounded::{self, BoundedReadError},
    catalog::parse_https_url,
};
const IMAGE_METADATA_VERSION: u32 = 1;
const IMAGE_ID_PREFIX: &str = "image-";
const IMAGE_ID_HEX_LENGTH: usize = 64;
const IMAGE_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const QEMU_INFO_TIMEOUT: Duration = Duration::from_secs(30);
const QEMU_CREATE_TIMEOUT: Duration = Duration::from_secs(60);
const QEMU_CONVERT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_HTTPS_REDIRECTS: usize = 5;
const OWNED_DIRECTORY_MODE: u32 = 0o700;
const LOCK_FILE_MODE: u32 = 0o600;
const SIDECAR_FILE_MODE: u32 = 0o600;
const BASE_FILE_MODE: u32 = 0o400;
const OVERLAY_FILE_MODE: u32 = 0o600;
const QCOW2_MAGIC: [u8; 4] = *b"QFI\xfb";

fn redirect_limit_exceeded(previous_redirects: usize) -> bool {
    previous_redirects > MAX_HTTPS_REDIRECTS
}

/// The only image sidecar version accepted by this release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ImageMetadataVersion;

impl Serialize for ImageMetadataVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(IMAGE_METADATA_VERSION)
    }
}

impl<'de> Deserialize<'de> for ImageMetadataVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = u32::deserialize(deserializer)?;
        if version == IMAGE_METADATA_VERSION {
            Ok(Self)
        } else {
            Err(de::Error::custom(format!(
                "unsupported image metadata version {version}; expected {IMAGE_METADATA_VERSION}"
            )))
        }
    }
}

/// Strict version-one contents of an image sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageMetadata {
    pub version: ImageMetadataVersion,
    pub id: String,
    pub generation: u64,
    pub source_ref: String,
    pub source_url: Option<String>,
    pub source_sha256: String,
    pub stored_sha256: String,
    pub architecture: Arch,
    pub firmware: Option<CatalogFirmware>,
    pub source_format: ImageFormat,
    pub stored_format: ImageFormat,
    pub verification_algorithm: Option<ChecksumAlgorithm>,
    pub verification_digest: Option<String>,
    pub size: u64,
    pub pulled_at: String,
}

#[derive(Deserialize)]
struct RequiredNullable<T>(Option<T>);

impl<'de> Deserialize<'de> for ImageMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            version: ImageMetadataVersion,
            id: String,
            generation: u64,
            source_ref: String,
            source_url: RequiredNullable<String>,
            source_sha256: String,
            stored_sha256: String,
            architecture: Arch,
            firmware: RequiredNullable<CatalogFirmware>,
            source_format: ImageFormat,
            stored_format: ImageFormat,
            verification_algorithm: RequiredNullable<ChecksumAlgorithm>,
            verification_digest: RequiredNullable<String>,
            size: u64,
            pulled_at: String,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            version: wire.version,
            id: wire.id,
            generation: wire.generation,
            source_ref: wire.source_ref,
            source_url: wire.source_url.0,
            source_sha256: wire.source_sha256,
            stored_sha256: wire.stored_sha256,
            architecture: wire.architecture,
            firmware: wire.firmware.0,
            source_format: wire.source_format,
            stored_format: wire.stored_format,
            verification_algorithm: wire.verification_algorithm.0,
            verification_digest: wire.verification_digest.0,
            size: wire.size,
            pulled_at: wire.pulled_at,
        })
    }
}

impl ImageMetadata {
    fn validate(&self) -> Result<(), FirestoneError> {
        validate_image_id(&self.id)?;
        if self.generation == 0 {
            return Err(invalid_sidecar(
                &self.id,
                "generation must be greater than zero",
            ));
        }
        if self.source_ref.is_empty() || self.source_ref.trim() != self.source_ref {
            return Err(invalid_sidecar(
                &self.id,
                "source_ref must be non-empty and trimmed",
            ));
        }
        if !is_lower_hex(&self.source_sha256, 64) {
            return Err(invalid_sidecar(
                &self.id,
                "source_sha256 must contain 64 lowercase hexadecimal characters",
            ));
        }
        if !is_lower_hex(&self.stored_sha256, 64) {
            return Err(invalid_sidecar(
                &self.id,
                "stored_sha256 must contain 64 lowercase hexadecimal characters",
            ));
        }
        if self.stored_format != ImageFormat::Qcow2 {
            return Err(invalid_sidecar(&self.id, "stored_format must be qcow2"));
        }
        if self.source_format == ImageFormat::Qcow2 && self.source_sha256 != self.stored_sha256 {
            return Err(invalid_sidecar(
                &self.id,
                "qcow2 source and stored SHA-256 values must match",
            ));
        }
        if self.size == 0 {
            return Err(invalid_sidecar(&self.id, "size must be greater than zero"));
        }
        if self.pulled_at.parse::<jiff::Timestamp>().is_err() {
            return Err(invalid_sidecar(
                &self.id,
                "pulled_at must be an RFC 3339 timestamp",
            ));
        }
        if let Some(source_url) = &self.source_url {
            let Some(parsed) = parse_https_url(source_url) else {
                return Err(invalid_sidecar(
                    &self.id,
                    "source_url must be a strict HTTPS URL",
                ));
            };
            if parsed.as_str() != source_url {
                return Err(invalid_sidecar(
                    &self.id,
                    "source_url must use its canonical URL representation",
                ));
            }
            if self.source_ref == *source_url {
                if self.firmware.is_some() {
                    return Err(invalid_sidecar(
                        &self.id,
                        "direct URL image firmware must be null",
                    ));
                }
            } else if self.firmware.is_none() {
                return Err(invalid_sidecar(
                    &self.id,
                    "catalog image firmware must be present",
                ));
            }
        } else {
            if !Path::new(&self.source_ref).is_absolute() {
                return Err(invalid_sidecar(
                    &self.id,
                    "a source without source_url must use an absolute local source_ref",
                ));
            }
            if self.firmware.is_some() {
                return Err(invalid_sidecar(
                    &self.id,
                    "local image firmware must be null",
                ));
            }
        }
        match (
            self.verification_algorithm,
            self.verification_digest.as_deref(),
        ) {
            (None, None) => {}
            (Some(ChecksumAlgorithm::Sha256), Some(digest)) if is_lower_hex(digest, 64) => {
                if digest != self.source_sha256 {
                    return Err(invalid_sidecar(
                        &self.id,
                        "SHA-256 verification digest must equal source_sha256",
                    ));
                }
            }
            (Some(ChecksumAlgorithm::Sha512), Some(digest)) if is_lower_hex(digest, 128) => {}
            (Some(_), Some(_)) => {
                return Err(invalid_sidecar(
                    &self.id,
                    "verification digest has the wrong hexadecimal length",
                ));
            }
            _ => {
                return Err(invalid_sidecar(
                    &self.id,
                    "verification algorithm and digest must both be present or absent",
                ));
            }
        }

        let expected_id = stable_image_id(
            &self.source_ref,
            self.source_url.as_deref(),
            self.architecture,
            &self.source_sha256,
        );
        if self.id != expected_id {
            return Err(invalid_sidecar(
                &self.id,
                "id does not match the complete immutable source identity",
            ));
        }
        Ok(())
    }

    fn verification(&self) -> Option<ImageVerification> {
        match (
            self.verification_algorithm,
            self.verification_digest.as_ref(),
        ) {
            (Some(algorithm), Some(digest)) => Some(ImageVerification {
                algorithm,
                digest: digest.clone(),
            }),
            _ => None,
        }
    }
}

/// A complete digest used to verify source bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageVerification {
    pub algorithm: ChecksumAlgorithm,
    pub digest: String,
}

/// The concrete transport selected for an image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSourceLocation {
    Https(String),
    Local(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalSourceSnapshot {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Debug, Clone)]
struct OpenedLocalSource {
    path: PathBuf,
    file: Arc<File>,
    snapshot: LocalSourceSnapshot,
}

/// A catalog, HTTPS, or local image resolved for one host architecture.
#[derive(Debug, Clone)]
pub struct ResolvedImageSource {
    pub source_ref: String,
    pub source_url: Option<String>,
    pub architecture: Arch,
    pub source_format: Option<ImageFormat>,
    pub firmware: Option<CatalogFirmware>,
    pub verification: Option<ImageVerification>,
    pub location: ImageSourceLocation,
    checksum: ExpectedChecksum,
    local_source: Option<OpenedLocalSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpectedChecksum {
    None,
    Digest(ImageVerification),
    Manifest {
        url: String,
        algorithm: ChecksumAlgorithm,
    },
}

/// Inputs for an explicit image pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePullRequest {
    pub image: ImageRef,
    pub sha256: Option<String>,
    pub source_base: PathBuf,
}

impl ImagePullRequest {
    #[must_use]
    pub fn new(image: ImageRef, source_base: impl Into<PathBuf>) -> Self {
        Self {
            image,
            sha256: None,
            source_base: source_base.into(),
        }
    }
}

/// One strict sidecar and its owned qcow2 base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredImage {
    pub metadata: ImageMetadata,
    pub path: PathBuf,
}

/// Result of resolving or pulling an image for lifecycle use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PulledImage {
    pub metadata: ImageMetadata,
    pub path: PathBuf,
    pub firmware: Option<CatalogFirmware>,
    pub cached: bool,
}

/// `qemu-img info` data validated for an owned image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageInspection {
    pub image: StoredImage,
    pub virtual_size: u64,
}

/// Result of deleting one image cache pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageRemoveResult {
    pub id: String,
    pub bytes_freed: u64,
    pub referenced_by: Vec<String>,
}

/// Result of deleting every unreferenced valid image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImagePruneResult {
    pub removed: Vec<String>,
    pub bytes_freed: u64,
}

/// A validated machine overlay and its exact backing file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverlayInfo {
    pub path: PathBuf,
    pub backing_path: PathBuf,
    pub virtual_size: u64,
    pub cached: bool,
}

/// The image and overlay prepared atomically with respect to image-store mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreparedMachineImage {
    pub image: PulledImage,
    pub overlay: OverlayInfo,
}

trait Clock: Send + Sync {
    fn now(&self) -> String;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> String {
        jiff::Timestamp::now().to_string()
    }
}

struct HttpResponse {
    body: Box<dyn Read>,
    content_length: Option<u64>,
    content_type: Option<String>,
}

trait HttpSource: Send + Sync {
    fn get(&self, url: &Url) -> Result<HttpResponse, FirestoneError>;
}

struct ReqwestHttpSource {
    client: Client,
}

impl ReqwestHttpSource {
    fn new() -> Result<Self, FirestoneError> {
        let redirect = Policy::custom(|attempt| {
            if redirect_limit_exceeded(attempt.previous().len()) {
                attempt.error("HTTPS download exceeded five redirects")
            } else if parse_https_url(attempt.url().as_str()).is_none() {
                attempt.error("HTTPS download redirected to an invalid or non-HTTPS URL")
            } else {
                attempt.follow()
            }
        });
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        let client = Client::builder()
            .default_headers(headers)
            .redirect(redirect)
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "cannot initialize the HTTPS image client",
                )
                .with_hint("check the host TLS certificate configuration")
                .with_source(source)
            })?;
        Ok(Self { client })
    }
}

impl HttpSource for ReqwestHttpSource {
    fn get(&self, url: &Url) -> Result<HttpResponse, FirestoneError> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|source| download_error(url, source))?;
        let content_length = response.content_length();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        Ok(HttpResponse {
            body: Box::new(response),
            content_length,
            content_type,
        })
    }
}

/// Owned image cache, verification, conversion, removal, and overlay APIs.
pub struct ImageStore {
    paths: Paths,
    catalog: Catalog,
    architecture: Arch,
    qemu_img: PathBuf,
    http: Arc<dyn HttpSource>,
    clock: Arc<dyn Clock>,
}

impl ImageStore {
    /// Creates a store for an explicit supported host architecture.
    pub fn new(
        paths: Paths,
        catalog: Catalog,
        architecture: Arch,
        qemu_img: impl Into<PathBuf>,
    ) -> Result<Self, FirestoneError> {
        Ok(Self {
            paths,
            catalog,
            architecture,
            qemu_img: qemu_img.into(),
            http: Arc::new(ReqwestHttpSource::new()?),
            clock: Arc::new(SystemClock),
        })
    }

    /// Creates a store using the architecture of the current executable.
    pub fn for_host(
        paths: Paths,
        catalog: Catalog,
        qemu_img: impl Into<PathBuf>,
    ) -> Result<Self, FirestoneError> {
        let architecture = Arch::current().map_err(|message| {
            FirestoneError::new(ErrorKind::Dependency, message)
                .with_hint("run Firestone on an x86_64 or aarch64 host")
        })?;
        Self::new(paths, catalog, architecture, qemu_img)
    }

    #[must_use]
    pub const fn architecture(&self) -> Arch {
        self.architecture
    }

    /// Resolves local files first, then strict HTTPS URLs, then the catalog.
    pub fn resolve(
        &self,
        image: &ImageRef,
        supplied_sha256: Option<&str>,
        source_base: &Path,
    ) -> Result<ResolvedImageSource, FirestoneError> {
        let value = image.as_str();
        let local = self
            .paths
            .resolve_input_path(Path::new(value), source_base, "image")?;
        if let Some(opened) = try_open_local_source(&local)? {
            if supplied_sha256.is_some() {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    "--sha256 is accepted only for HTTPS image URLs",
                )
                .with_hint("remove --sha256 when pulling a local file"));
            }
            let source_ref = opened.path.to_str().ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!("local image path '{}' is not UTF-8", opened.path.display()),
                )
                .with_hint("rename the path using UTF-8 characters and retry")
            })?;
            return Ok(ResolvedImageSource {
                source_ref: source_ref.to_owned(),
                source_url: None,
                architecture: self.architecture,
                source_format: None,
                firmware: None,
                verification: None,
                location: ImageSourceLocation::Local(opened.path.clone()),
                checksum: ExpectedChecksum::None,
                local_source: Some(opened),
            });
        }

        if let Some(url) = parse_https_url(value) {
            let verification = supplied_sha256
                .map(validate_sha256)
                .transpose()?
                .map(|digest| ImageVerification {
                    algorithm: ChecksumAlgorithm::Sha256,
                    digest,
                });
            let source_url = url.to_string();
            return Ok(ResolvedImageSource {
                source_ref: source_url.clone(),
                source_url: Some(source_url.clone()),
                architecture: self.architecture,
                source_format: None,
                firmware: None,
                verification: verification.clone(),
                location: ImageSourceLocation::Https(source_url),
                checksum: verification.map_or(ExpectedChecksum::None, ExpectedChecksum::Digest),
                local_source: None,
            });
        }

        if supplied_sha256.is_some() {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                "--sha256 is accepted only for HTTPS image URLs",
            )
            .with_hint("use a strict https:// URL or remove --sha256"));
        }

        match self.catalog.resolve(value, self.architecture.as_str()) {
            Ok(resolved) => {
                let source_url = parse_https_url(&resolved.source.url)
                    .map(|url| url.to_string())
                    .ok_or_else(|| {
                        FirestoneError::new(
                            ErrorKind::InvalidSpec,
                            format!(
                                "catalog image '{}' has an invalid HTTPS URL",
                                resolved.canonical_reference
                            ),
                        )
                    })?;
                let checksum = match resolved.source.checksum {
                    CatalogChecksum::Sha256(digest) => {
                        ExpectedChecksum::Digest(ImageVerification {
                            algorithm: ChecksumAlgorithm::Sha256,
                            digest,
                        })
                    }
                    CatalogChecksum::ManifestUrl(url) => ExpectedChecksum::Manifest {
                        url,
                        algorithm: resolved.checksum_algorithm,
                    },
                };
                let verification = match &checksum {
                    ExpectedChecksum::Digest(verification) => Some(verification.clone()),
                    _ => None,
                };
                Ok(ResolvedImageSource {
                    source_ref: resolved.canonical_reference,
                    source_url: Some(source_url.clone()),
                    architecture: self.architecture,
                    source_format: Some(resolved.format),
                    firmware: Some(resolved.firmware),
                    verification,
                    location: ImageSourceLocation::Https(source_url),
                    checksum,
                    local_source: None,
                })
            }
            Err(catalog_error)
                if value.contains('/') || value.starts_with('.') || value.starts_with('~') =>
            {
                Err(FirestoneError::new(
                    ErrorKind::NotFound,
                    format!("local image path '{}' does not exist", local.display()),
                )
                .with_hint("check the path or use a known catalog image reference")
                .with_source(catalog_error))
            }
            Err(error) => Err(error),
        }
    }

    fn resolve_persisted(&self, reference: &str) -> Result<ResolvedImageSource, FirestoneError> {
        if let Some(url) = parse_https_url(reference) {
            let source_url = url.to_string();
            return Ok(ResolvedImageSource {
                source_ref: source_url.clone(),
                source_url: Some(source_url.clone()),
                architecture: self.architecture,
                source_format: None,
                firmware: None,
                verification: None,
                location: ImageSourceLocation::Https(source_url),
                checksum: ExpectedChecksum::None,
                local_source: None,
            });
        }
        if Path::new(reference).is_absolute() {
            let Some(opened) = try_open_local_source(Path::new(reference))? else {
                return Err(FirestoneError::new(
                    ErrorKind::NotFound,
                    format!("persisted local image path '{reference}' does not exist"),
                )
                .with_hint("restore the source or use the already-pinned owned image"));
            };
            return Ok(ResolvedImageSource {
                source_ref: opened.path.to_string_lossy().into_owned(),
                source_url: None,
                architecture: self.architecture,
                source_format: None,
                firmware: None,
                verification: None,
                location: ImageSourceLocation::Local(opened.path.clone()),
                checksum: ExpectedChecksum::None,
                local_source: Some(opened),
            });
        }
        let resolved = self
            .catalog
            .resolve(reference, self.architecture.as_str())?;
        let source_url = parse_https_url(&resolved.source.url)
            .map(|url| url.to_string())
            .ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!(
                        "catalog image '{}' has an invalid HTTPS URL",
                        resolved.canonical_reference
                    ),
                )
            })?;
        let checksum = match resolved.source.checksum {
            CatalogChecksum::Sha256(digest) => ExpectedChecksum::Digest(ImageVerification {
                algorithm: ChecksumAlgorithm::Sha256,
                digest,
            }),
            CatalogChecksum::ManifestUrl(url) => ExpectedChecksum::Manifest {
                url,
                algorithm: resolved.checksum_algorithm,
            },
        };
        let verification = match &checksum {
            ExpectedChecksum::Digest(verification) => Some(verification.clone()),
            _ => None,
        };
        Ok(ResolvedImageSource {
            source_ref: resolved.canonical_reference,
            source_url: Some(source_url.clone()),
            architecture: self.architecture,
            source_format: Some(resolved.format),
            firmware: Some(resolved.firmware),
            verification,
            location: ImageSourceLocation::Https(source_url),
            checksum,
            local_source: None,
        })
    }

    /// Pulls and verifies an image, always publishing an owned qcow2 base.
    pub fn pull(
        &self,
        request: &ImagePullRequest,
        events: &mut dyn EventSink,
    ) -> Result<PulledImage, FirestoneError> {
        self.ensure_store()?;
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;
        let source = self.resolve(
            &request.image,
            request.sha256.as_deref(),
            &request.source_base,
        )?;
        self.pull_locked(source, events)
    }

    /// Lists strict, complete image pairs in stable id order without hashing bases.
    pub fn list(&self) -> Result<Vec<StoredImage>, FirestoneError> {
        if !self.store_exists_for_read()? {
            return Ok(Vec::new());
        }
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;
        self.list_locked()
    }

    /// Verifies sidecar identity, stored SHA-256, and qcow2 inspection data.
    pub fn inspect(&self, id: &str) -> Result<ImageInspection, FirestoneError> {
        if !self.store_exists_for_read()? {
            return Err(image_not_found(id));
        }
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;
        let image = self.load_verified_pair(id)?;
        let info = self.qemu_info(&image.path)?;
        validate_base_info(id, &info)?;
        Ok(ImageInspection {
            image,
            virtual_size: info.virtual_size,
        })
    }

    /// Returns sorted machine names whose state references `id`.
    pub fn referencing_machines(&self, id: &str) -> Result<Vec<String>, FirestoneError> {
        validate_image_id(id)?;
        Ok(self.image_references()?.remove(id).unwrap_or_default())
    }

    /// Removes one image, refusing referenced content unless `force` is true.
    pub fn remove(&self, id: &str, force: bool) -> Result<ImageRemoveResult, FirestoneError> {
        validate_image_id(id)?;
        self.ensure_store()?;
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;
        let referenced_by = self.image_references()?.remove(id).unwrap_or_default();
        if !force && !referenced_by.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "image `{id}` is referenced by machine(s): {}",
                    referenced_by.join(", ")
                ),
            )
            .with_hint("remove the machines first or retry with --force"));
        }
        let bytes_freed = self.remove_pair(id)?;
        Ok(ImageRemoveResult {
            id: id.to_owned(),
            bytes_freed,
            referenced_by,
        })
    }

    /// Removes every valid stored image absent from all complete machine states.
    pub fn prune(&self) -> Result<ImagePruneResult, FirestoneError> {
        self.ensure_store()?;
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;
        let references = self.image_references()?;
        let images = self.list_locked()?;
        let mut removed = Vec::new();
        let mut bytes_freed = 0_u64;
        for image in images {
            if references.contains_key(&image.metadata.id) {
                continue;
            }
            bytes_freed = bytes_freed
                .checked_add(self.remove_pair(&image.metadata.id)?)
                .ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        "pruned image byte count overflowed u64",
                    )
                })?;
            removed.push(image.metadata.id);
        }
        Ok(ImagePruneResult {
            removed,
            bytes_freed,
        })
    }

    /// Pins immutable identity in state before lazily creating the machine overlay.
    pub fn prepare_machine_image(
        &self,
        name: &str,
        state: &mut MachineState,
        source_base: &Path,
        disk_size: ByteSize,
        machine_lock: &MachineLock,
        events: &mut dyn EventSink,
    ) -> Result<PreparedMachineImage, FirestoneError> {
        self.validate_machine_lock(name, machine_lock)?;
        self.ensure_store()?;
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;

        let image = match (&state.image.id, &state.image.sha256) {
            (Some(id), Some(_)) => {
                self.emit_image_start(&state.image.r#ref, events)?;
                let stored = self.load_verified_pair(id)?;
                let firmware =
                    self.validate_pinned_image(name, &state.image, &stored, source_base)?;
                events.emit(Event::StepSkip {
                    id: StepId::from("image"),
                    reason: "cached".to_owned(),
                })?;
                PulledImage {
                    metadata: stored.metadata,
                    path: stored.path,
                    firmware,
                    cached: true,
                }
            }
            (None, None) => {
                let requested_ref = state.image.r#ref.clone();
                let resolved = self.resolve_persisted(&requested_ref);
                let image = match resolved {
                    Ok(source) => match self.find_latest_for_source(&source)? {
                        Some(stored) => {
                            self.emit_image_start(&source.source_ref, events)?;
                            events.emit(Event::StepSkip {
                                id: StepId::from("image"),
                                reason: "cached".to_owned(),
                            })?;
                            PulledImage {
                                firmware: stored.metadata.firmware,
                                metadata: stored.metadata,
                                path: stored.path,
                                cached: true,
                            }
                        }
                        None => self.pull_locked(source, events)?,
                    },
                    Err(resolve_error) => {
                        match self.find_latest_by_canonical_ref(&requested_ref)? {
                            Some(stored) => {
                                self.emit_image_start(&requested_ref, events)?;
                                events.emit(Event::StepSkip {
                                    id: StepId::from("image"),
                                    reason: "cached".to_owned(),
                                })?;
                                PulledImage {
                                    firmware: stored.metadata.firmware,
                                    metadata: stored.metadata,
                                    path: stored.path,
                                    cached: true,
                                }
                            }
                            None => return Err(resolve_error),
                        }
                    }
                };
                self.validate_image_architecture(name, &image.metadata)?;
                let previous = state.image.clone();
                state.image = StateImage {
                    r#ref: image.metadata.source_ref.clone(),
                    id: Some(image.metadata.id.clone()),
                    sha256: Some(image.metadata.source_sha256.clone()),
                };
                let write_result = StateStore::new(self.paths.machine_state(name)?)
                    .write_from_locked_action(state, machine_lock);
                if let Err(error) = write_result {
                    state.image = previous;
                    return Err(error);
                }
                image
            }
            _ => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!("machine `{name}` has a partial image identity"),
                )
                .with_hint("repair state.json so image id and sha256 are both present or absent"));
            }
        };

        let overlay =
            self.create_overlay_locked(name, &state.image, disk_size, machine_lock, Some(&image))?;
        Ok(PreparedMachineImage { image, overlay })
    }
    /// Lazily creates and verifies one overlay while protecting its backing image.
    pub fn create_overlay(
        &self,
        name: &str,
        image: &StateImage,
        disk_size: ByteSize,
        machine_lock: &MachineLock,
    ) -> Result<OverlayInfo, FirestoneError> {
        self.validate_machine_lock(name, machine_lock)?;
        if !self.store_exists_for_read()? {
            return Err(
                FirestoneError::new(ErrorKind::NotFound, "image store does not exist")
                    .with_hint("pull the machine image before creating its overlay"),
            );
        }
        let _lock = self.acquire_lock()?;
        self.cleanup_stale_partials()?;
        self.create_overlay_locked(name, image, disk_size, machine_lock, None)
    }

    fn pull_locked(
        &self,
        mut source: ResolvedImageSource,
        events: &mut dyn EventSink,
    ) -> Result<PulledImage, FirestoneError> {
        let started = Instant::now();
        self.emit_image_start(&source.source_ref, events)?;
        self.resolve_expected_checksum(&mut source)?;
        if source.verification.is_none() && matches!(source.location, ImageSourceLocation::Https(_))
        {
            events.emit(Event::Log {
                level: Level::Warn,
                message: format!(
                    "image URL '{}' has no checksum; use --sha256 to verify it",
                    source.source_ref
                ),
            })?;
        }

        if source.verification.is_some() {
            if let Some(cached) = self.find_exact_cache(&source)? {
                events.emit(Event::StepSkip {
                    id: StepId::from("image"),
                    reason: "cached".to_owned(),
                })?;
                return Ok(PulledImage {
                    firmware: cached.metadata.firmware,
                    metadata: cached.metadata,
                    path: cached.path,
                    cached: true,
                });
            }
        }

        let operation = operation_key(&source);
        let source_partial = self.paths.image_source_partial(&operation)?;
        let stored_partial = self.paths.image_stored_partial(&operation)?;
        remove_stale_partial(&source_partial)?;
        remove_stale_partial(&stored_partial)?;
        let mut cleanup = CleanupGuard::new();
        cleanup.track(source_partial.clone());
        cleanup.track(stored_partial.clone());

        let staged = self.stage_source(&source, &source_partial, events)?;
        self.verify_staged_source(&source, &staged)?;
        let source_format = source.source_format.unwrap_or(staged.detected_format);
        if source_format != staged.detected_format {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "image '{}' declared {source_format:?} but downloaded bytes are {:?}",
                    source.source_ref, staged.detected_format
                ),
            )
            .with_hint("fix the catalog format or source URL"));
        }

        let id = stable_image_id(
            &source.source_ref,
            source.source_url.as_deref(),
            source.architecture,
            &staged.source_sha256,
        );
        if let Some(cached) =
            self.existing_identity(&id, &source, &staged.source_sha256, source_format)?
        {
            events.emit(Event::StepSkip {
                id: StepId::from("image"),
                reason: "cached".to_owned(),
            })?;
            return Ok(PulledImage {
                firmware: cached.metadata.firmware,
                metadata: cached.metadata,
                path: cached.path,
                cached: true,
            });
        }

        let candidate = if source_format == ImageFormat::Raw {
            self.convert_raw(&source_partial, &stored_partial)?;
            &stored_partial
        } else {
            &source_partial
        };
        validate_created_regular_file(candidate, "staged qcow2 image")?;
        let info = self.qemu_info(candidate)?;
        validate_base_info(&id, &info)?;

        let (stored_sha256, stored_size) = if source_format == ImageFormat::Qcow2 {
            (staged.source_sha256.clone(), staged.size)
        } else {
            hash_file_with_size(candidate)?
        };
        set_file_mode(candidate, BASE_FILE_MODE, "staged qcow2 image")?;
        sync_file(candidate, "staged qcow2 image")?;

        let base_path = self.paths.image_base(&id)?;
        let sidecar_path = self.paths.image_metadata(&id)?;
        ensure_pair_absent(&base_path, &sidecar_path, &id)?;
        cleanup.track(base_path.clone());
        cleanup.track(sidecar_path.clone());
        publish_no_replace(candidate, &base_path)?;

        let generation = self.next_generation(&source.source_ref, source.architecture)?;
        let metadata = ImageMetadata {
            version: ImageMetadataVersion,
            id: id.clone(),
            generation,
            source_ref: source.source_ref.clone(),
            source_url: source.source_url.clone(),
            source_sha256: staged.source_sha256,
            stored_sha256,
            architecture: source.architecture,
            firmware: source.firmware,
            source_format,
            stored_format: ImageFormat::Qcow2,
            verification_algorithm: source.verification.as_ref().map(|value| value.algorithm),
            verification_digest: source
                .verification
                .as_ref()
                .map(|value| value.digest.clone()),
            size: stored_size,
            pulled_at: self.clock.now(),
        };
        metadata.validate()?;
        atomic::write_json_with_mode(&sidecar_path, &metadata, SIDECAR_FILE_MODE)?;
        self.paths.validate_owned_data_file(
            &sidecar_path,
            "image sidecar",
            SIDECAR_FILE_MODE,
            false,
        )?;
        remove_stale_partial(&source_partial)?;
        remove_stale_partial(&stored_partial)?;
        sync_directory(&self.paths.images_dir(), "images directory")?;
        cleanup.disarm();

        events.emit(Event::StepDone {
            id: StepId::from("image"),
            detail: Some(format!(
                "{} · {} · {} bytes",
                source.source_ref, source.architecture, stored_size
            )),
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })?;
        Ok(PulledImage {
            firmware: metadata.firmware,
            metadata,
            path: base_path,
            cached: false,
        })
    }

    fn stage_source(
        &self,
        source: &ResolvedImageSource,
        partial: &Path,
        events: &mut dyn EventSink,
    ) -> Result<StagedSource, FirestoneError> {
        match &source.location {
            ImageSourceLocation::Local(path) => {
                let opened = source.local_source.as_ref().ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Conflict,
                        format!(
                            "local image '{}' was not opened for this pull",
                            path.display()
                        ),
                    )
                })?;
                let mut input = opened.file.as_ref();
                let staged = stream_source(
                    &mut input,
                    partial,
                    Some(opened.snapshot.size),
                    ErrorKind::Generic,
                    source.verification.as_ref().map(|value| value.algorithm),
                    events,
                )?;
                let after = opened.file.metadata().map_err(|source| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        format!("cannot re-inspect local image '{}'", path.display()),
                    )
                    .with_source(source)
                })?;
                if local_source_snapshot(&after) != opened.snapshot {
                    return Err(FirestoneError::new(
                        ErrorKind::Checksum,
                        format!(
                            "local image '{}' changed while it was copied",
                            path.display()
                        ),
                    )
                    .with_hint("stop modifying the source image and retry"));
                }
                Ok(staged)
            }
            ImageSourceLocation::Https(url) => {
                let parsed = parse_https_url(url).ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::InvalidSpec,
                        format!("image source URL '{url}' is not strict HTTPS"),
                    )
                })?;
                let mut response = self.http.get(&parsed)?;
                stream_source(
                    response.body.as_mut(),
                    partial,
                    response.content_length,
                    ErrorKind::Checksum,
                    source.verification.as_ref().map(|value| value.algorithm),
                    events,
                )
            }
        }
    }

    fn verify_staged_source(
        &self,
        source: &ResolvedImageSource,
        staged: &StagedSource,
    ) -> Result<(), FirestoneError> {
        let Some(verification) = &source.verification else {
            return Ok(());
        };
        let actual = match verification.algorithm {
            ChecksumAlgorithm::Sha256 => staged.source_sha256.as_str(),
            ChecksumAlgorithm::Sha512 => staged.source_sha512.as_deref().ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    "SHA-512 verifier was not computed for a SHA-512 image",
                )
            })?,
        };
        if actual != verification.digest {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "{} checksum mismatch for '{}': expected {}, got {actual}",
                    algorithm_name(verification.algorithm),
                    source.source_ref,
                    verification.digest
                ),
            )
            .with_hint("retry the pull; if it fails again, refresh the catalog checksum"));
        }
        Ok(())
    }

    fn resolve_expected_checksum(
        &self,
        source: &mut ResolvedImageSource,
    ) -> Result<(), FirestoneError> {
        match &source.checksum {
            ExpectedChecksum::None => {
                source.verification = None;
            }
            ExpectedChecksum::Digest(verification) => {
                source.verification = Some(verification.clone());
            }
            ExpectedChecksum::Manifest { url, algorithm } => {
                let manifest_url = parse_https_url(url).ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::InvalidSpec,
                        format!("checksum manifest URL '{url}' is not strict HTTPS"),
                    )
                })?;
                let image_url = source.source_url.as_deref().ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::InvalidSpec,
                        "checksum manifest requires an HTTPS image URL",
                    )
                })?;
                let filename = image_filename(image_url)?;
                let manifest = self.fetch_manifest(&manifest_url)?;
                let digest = parse_checksum_manifest(&manifest, *algorithm, &filename)?;
                source.verification = Some(ImageVerification {
                    algorithm: *algorithm,
                    digest,
                });
            }
        }
        Ok(())
    }

    fn fetch_manifest(&self, url: &Url) -> Result<String, FirestoneError> {
        let response = self.http.get(url)?;
        if response
            .content_type
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"))
        {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!("checksum manifest '{url}' returned HTML"),
            )
            .with_hint("check the catalog checksum_url"));
        }
        if response
            .content_length
            .is_some_and(|length| length > MAX_MANIFEST_BYTES)
        {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "checksum manifest '{url}' exceeds the {} byte limit",
                    MAX_MANIFEST_BYTES
                ),
            )
            .with_hint("use a bounded SHA256SUMS or SHA512SUMS manifest"));
        }

        let capacity = response
            .content_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(MAX_MANIFEST_BYTES as usize);
        let mut bytes = Vec::with_capacity(capacity);
        let mut limited = response.body.take(MAX_MANIFEST_BYTES + 1);
        limited.read_to_end(&mut bytes).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Checksum,
                format!("cannot read checksum manifest '{url}'"),
            )
            .with_hint("retry the pull")
            .with_source(source)
        })?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "checksum manifest '{url}' exceeds the {} byte limit",
                    MAX_MANIFEST_BYTES
                ),
            )
            .with_hint("use a bounded SHA256SUMS or SHA512SUMS manifest"));
        }
        String::from_utf8(bytes).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Checksum,
                format!("checksum manifest '{url}' is not UTF-8"),
            )
            .with_hint("use a UTF-8 SHA256SUMS or SHA512SUMS manifest")
            .with_source(source)
        })
    }

    fn find_exact_cache(
        &self,
        source: &ResolvedImageSource,
    ) -> Result<Option<StoredImage>, FirestoneError> {
        let mut matches = self
            .list_locked()?
            .into_iter()
            .filter(|stored| metadata_matches_source(&stored.metadata, source, true))
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.metadata.id.cmp(&right.metadata.id));
        if matches.len() > 1 {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "image source '{}' has multiple cache entries for the same complete checksum",
                    source.source_ref
                ),
            )
            .with_hint("inspect and remove the duplicate image entry"));
        }
        matches
            .pop()
            .map(|stored| self.verify_stored_pair(stored))
            .transpose()
    }

    fn find_latest_for_source(
        &self,
        source: &ResolvedImageSource,
    ) -> Result<Option<StoredImage>, FirestoneError> {
        let mut matches = self
            .list_locked()?
            .into_iter()
            .filter(|stored| metadata_matches_cache_source(&stored.metadata, source))
            .collect::<Vec<_>>();
        let Some(maximum) = matches
            .iter()
            .map(|stored| stored.metadata.generation)
            .max()
        else {
            return Ok(None);
        };
        matches.retain(|stored| stored.metadata.generation == maximum);
        if matches.len() != 1 {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "image source '{}' has ambiguous generation {maximum}",
                    source.source_ref
                ),
            )
            .with_hint("move duplicate image sidecars aside and retry"));
        }
        matches
            .pop()
            .map(|stored| self.verify_stored_pair(stored))
            .transpose()
    }

    fn find_latest_by_canonical_ref(
        &self,
        source_ref: &str,
    ) -> Result<Option<StoredImage>, FirestoneError> {
        let mut matches = self
            .list_locked()?
            .into_iter()
            .filter(|stored| {
                stored.metadata.source_ref == source_ref
                    && stored.metadata.architecture == self.architecture
            })
            .collect::<Vec<_>>();
        let Some(maximum) = matches
            .iter()
            .map(|stored| stored.metadata.generation)
            .max()
        else {
            return Ok(None);
        };
        matches.retain(|stored| stored.metadata.generation == maximum);
        if matches.len() != 1 {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("image source '{source_ref}' has ambiguous generation {maximum}"),
            )
            .with_hint("move duplicate image sidecars aside and retry"));
        }
        matches
            .pop()
            .map(|stored| self.verify_stored_pair(stored))
            .transpose()
    }

    fn next_generation(&self, source_ref: &str, architecture: Arch) -> Result<u64, FirestoneError> {
        let generations = self
            .list_locked()?
            .into_iter()
            .filter(|stored| {
                stored.metadata.source_ref == source_ref
                    && stored.metadata.architecture == architecture
            })
            .map(|stored| stored.metadata.generation)
            .collect::<Vec<_>>();
        let Some(maximum) = generations.iter().copied().max() else {
            return Ok(1);
        };
        if generations
            .iter()
            .filter(|generation| **generation == maximum)
            .count()
            != 1
        {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("image source '{source_ref}' has ambiguous generation {maximum}"),
            )
            .with_hint("move duplicate image sidecars aside and retry"));
        }
        maximum.checked_add(1).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("image source '{source_ref}' generation overflow"),
            )
            .with_hint("move obsolete image generations aside and retry")
        })
    }

    fn existing_identity(
        &self,
        id: &str,
        source: &ResolvedImageSource,
        source_sha256: &str,
        source_format: ImageFormat,
    ) -> Result<Option<StoredImage>, FirestoneError> {
        let base = self.paths.image_base(id)?;
        let sidecar = self.paths.image_metadata(id)?;
        let base_exists = path_exists_without_following(&base)?;
        let sidecar_exists = path_exists_without_following(&sidecar)?;
        match (base_exists, sidecar_exists) {
            (false, false) => Ok(None),
            (true, true) => {
                let stored = self.load_verified_pair(id)?;
                let immutable_match = stored.metadata.source_ref == source.source_ref
                    && stored.metadata.source_url == source.source_url
                    && stored.metadata.source_sha256 == source_sha256
                    && stored.metadata.architecture == source.architecture
                    && stored.metadata.source_format == source_format;
                if !immutable_match {
                    return Err(FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("image id `{id}` collides with different immutable metadata"),
                    )
                    .with_hint("move the conflicting image files aside and retry"));
                }

                let mut metadata = stored.metadata;
                let mut changed = false;
                if let Some(verification) = &source.verification {
                    if metadata.verification().as_ref() != Some(verification) {
                        metadata.verification_algorithm = Some(verification.algorithm);
                        metadata.verification_digest = Some(verification.digest.clone());
                        changed = true;
                    }
                }
                if source.firmware.is_some() && metadata.firmware != source.firmware {
                    metadata.firmware = source.firmware;
                    changed = true;
                }
                if changed {
                    metadata.generation =
                        self.next_generation(&source.source_ref, source.architecture)?;
                    metadata.pulled_at = self.clock.now();
                    metadata.validate()?;
                    atomic::write_json_with_mode(&sidecar, &metadata, SIDECAR_FILE_MODE)?;
                }
                Ok(Some(StoredImage {
                    metadata,
                    path: stored.path,
                }))
            }
            _ => Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("image `{id}` has an incomplete base/sidecar pair"),
            )
            .with_hint("retry so Firestone can recover the incomplete image pair")),
        }
    }
    /// Uses the pinned qemu-img 8.2.2 raw conversion argv from verify 4.
    fn convert_raw(&self, source: &Path, target: &Path) -> Result<(), FirestoneError> {
        Cmd::new(self.qemu_img.as_os_str())
            .arg("convert")
            .arg("-f")
            .arg("raw")
            .arg("-O")
            .arg("qcow2")
            .arg(source.as_os_str())
            .arg(target.as_os_str())
            .timeout(QEMU_CONVERT_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;
        Ok(())
    }

    /// Uses the pinned qemu-img 8.2.2 JSON inspection argv from verify 4/5.
    fn qemu_info(&self, path: &Path) -> Result<QemuInfo, FirestoneError> {
        let output = Cmd::new(self.qemu_img.as_os_str())
            .arg("info")
            .arg("--output=json")
            .arg("-f")
            .arg("qcow2")
            .arg(path.as_os_str())
            .timeout(QEMU_INFO_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;
        let value =
            serde_json::from_slice::<serde_json::Value>(output.stdout()).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("qemu-img returned invalid JSON for '{}'", path.display()),
                )
                .with_hint("install qemu-img 8.2.2 or a compatible release")
                .with_source(source)
            })?;
        reject_hidden_qemu_dependencies(&value, path)?;
        let format = required_qemu_string(&value, "format", path)?;
        let virtual_size = value
            .get("virtual-size")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| missing_qemu_info_field(path, "virtual-size"))?;
        let backing_filename = optional_qemu_string(&value, "backing-filename", path)?;
        let backing_filename_format =
            optional_qemu_string(&value, "backing-filename-format", path)?;
        let full_backing_filename = optional_qemu_string(&value, "full-backing-filename", path)?;
        let dirty_flag = optional_qemu_bool(&value, "dirty-flag", path)?;
        let format_specific = parse_qcow2_format_specific(&value, path)?;
        Ok(QemuInfo {
            format,
            virtual_size,
            backing_filename,
            backing_filename_format,
            full_backing_filename,
            dirty_flag,
            corrupt: format_specific.corrupt,
            data_file: format_specific.data_file,
            data_file_raw: format_specific.data_file_raw,
        })
    }

    fn list_locked(&self) -> Result<Vec<StoredImage>, FirestoneError> {
        let mut ids = Vec::new();
        let entries = fs::read_dir(self.paths.images_dir()).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "cannot read images directory '{}'",
                    self.paths.images_dir().display()
                ),
            )
            .with_hint("check the Firestone data directory permissions")
            .with_source(source)
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot read an images directory entry")
                    .with_source(source)
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    "images directory contains a non-UTF-8 file name",
                )
                .with_hint("move the unknown file out of the images directory"));
            };
            if let Some(id) = name.strip_suffix(".json") {
                validate_image_id(id)?;
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        ids.dedup();
        ids.into_iter().map(|id| self.load_pair(&id)).collect()
    }

    fn load_pair(&self, id: &str) -> Result<StoredImage, FirestoneError> {
        validate_image_id(id)?;
        let sidecar = self.paths.image_metadata(id)?;
        let base = self.paths.image_base(id)?;
        let bytes = read_owned_bounded(
            &self.paths,
            &sidecar,
            "image sidecar",
            SIDECAR_FILE_MODE,
            MAX_SIDECAR_BYTES,
        )?;
        let metadata = serde_json::from_slice::<ImageMetadata>(&bytes).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot parse image sidecar '{}'", sidecar.display()),
            )
            .with_hint("replace the sidecar with strict version-one metadata")
            .with_source(source)
        })?;
        metadata.validate()?;
        if metadata.id != id {
            return Err(invalid_sidecar(
                id,
                "id does not match the sidecar file name",
            ));
        }
        self.paths
            .validate_owned_data_file(&base, "image base", BASE_FILE_MODE, false)?;
        let actual_size = fs::symlink_metadata(&base)
            .map_err(|source| image_file_error("inspect", &base, source))?
            .len();
        if actual_size != metadata.size {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "image base '{}' has size {actual_size}; sidecar records {}",
                    base.display(),
                    metadata.size
                ),
            )
            .with_hint("remove the corrupted image and pull it again"));
        }
        Ok(StoredImage {
            metadata,
            path: base,
        })
    }

    fn load_verified_pair(&self, id: &str) -> Result<StoredImage, FirestoneError> {
        let stored = self.load_pair(id)?;
        self.verify_stored_pair(stored)
    }

    fn verify_stored_pair(&self, stored: StoredImage) -> Result<StoredImage, FirestoneError> {
        let actual_sha256 = hash_file_sha256(&stored.path)?;
        if actual_sha256 != stored.metadata.stored_sha256 {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "stored image `{}` does not match stored_sha256",
                    stored.metadata.id
                ),
            )
            .with_hint("remove the corrupted image and pull it again"));
        }
        Ok(stored)
    }

    fn remove_pair(&self, id: &str) -> Result<u64, FirestoneError> {
        let stored = self.load_pair(id)?;
        let sidecar = self.paths.image_metadata(id)?;
        let base_tombstone = self.paths.image_base_removal(id)?;
        let sidecar_tombstone = self.paths.image_metadata_removal(id)?;
        if path_exists_without_following(&base_tombstone)?
            || path_exists_without_following(&sidecar_tombstone)?
        {
            return Err(FirestoneError::new(
                ErrorKind::Busy,
                format!("image `{id}` has an unfinished removal"),
            )
            .with_hint("retry so Firestone can finish the prior removal"));
        }

        fs::rename(&sidecar, &sidecar_tombstone)
            .map_err(|source| image_file_error("stage removal of", &sidecar, source))?;
        sync_directory(&self.paths.images_dir(), "images directory")?;
        if let Err(source) = fs::rename(&stored.path, &base_tombstone) {
            let removal_error = image_file_error("stage removal of", &stored.path, source);
            return match fs::rename(&sidecar_tombstone, &sidecar)
                .and_then(|()| File::open(self.paths.images_dir()))
                .and_then(|directory| directory.sync_all())
            {
                Ok(()) => Err(removal_error),
                Err(restore_error) => Err(FirestoneError::new(
                    ErrorKind::Generic,
                    format!(
                        "{}; cannot restore image sidecar: {restore_error}",
                        removal_error.message()
                    ),
                )
                .with_hint("retry so Firestone can finish the interrupted removal")),
            };
        }
        sync_directory(&self.paths.images_dir(), "images directory")?;
        self.remove_tombstone(
            &sidecar_tombstone,
            "image sidecar removal tombstone",
            SIDECAR_FILE_MODE,
        )?;
        self.remove_tombstone(
            &base_tombstone,
            "image base removal tombstone",
            BASE_FILE_MODE,
        )?;
        sync_directory(&self.paths.images_dir(), "images directory")?;
        Ok(stored.metadata.size)
    }

    fn image_references(&self) -> Result<BTreeMap<String, Vec<String>>, FirestoneError> {
        let machines_dir = self.paths.machines_dir();
        self.paths
            .validate_owned_data_directory(self.paths.data_dir(), "data directory", true)?;
        self.paths
            .validate_owned_data_directory(&machines_dir, "machines directory", true)?;
        if !path_exists_without_following(&machines_dir)? {
            return Ok(BTreeMap::new());
        }
        self.paths
            .validate_owned_data_directory(&machines_dir, "machines directory", false)?;

        let mut entries = fs::read_dir(&machines_dir)
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!(
                        "cannot read machines directory '{}'",
                        machines_dir.display()
                    ),
                )
                .with_hint("check the Firestone data directory permissions")
                .with_source(source)
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot read a machines directory entry")
                    .with_source(source)
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);

        let mut references: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in entries {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|source| {
                image_file_error("inspect machine directory entry", &entry.path(), source)
            })?;
            if metadata.file_type().is_symlink() {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "machines directory contains symlink '{}'",
                        entry.path().display()
                    ),
                )
                .with_hint("move the symlink out of the machines directory"));
            }
            if !metadata.is_dir() {
                continue;
            }
            let name = entry.file_name().into_string().map_err(|_| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "machines directory contains a non-UTF-8 machine name",
                )
                .with_hint("move the invalid machine directory aside")
            })?;
            let machine_dir = self.paths.machine_dir(&name)?;
            self.paths
                .validate_owned_data_directory(&machine_dir, "machine directory", false)?;
            let state_path = self.paths.machine_state(&name)?;
            if !path_exists_without_following(&state_path)? {
                continue;
            }
            validate_regular_nofollow(&state_path, "machine state")?;
            let state = StateStore::new(state_path).read()?;
            if let Some(id) = state.image.id {
                references.entry(id).or_default().push(name);
            }
        }
        for names in references.values_mut() {
            names.sort();
            names.dedup();
        }
        Ok(references)
    }

    fn absolute_local_reference(
        &self,
        reference: &str,
        source_base: &Path,
    ) -> Result<String, FirestoneError> {
        let path = self
            .paths
            .resolve_input_path(Path::new(reference), source_base, "image")?;
        path.to_str().map(ToOwned::to_owned).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("local image path '{}' is not UTF-8", path.display()),
            )
            .with_hint("rename the path using UTF-8 characters and retry")
        })
    }

    fn validate_image_architecture(
        &self,
        name: &str,
        metadata: &ImageMetadata,
    ) -> Result<(), FirestoneError> {
        if metadata.architecture == self.architecture {
            return Ok(());
        }
        Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "machine `{name}` image `{}` is for {}; host architecture is {}",
                metadata.id, metadata.architecture, self.architecture
            ),
        )
        .with_hint("pull the image on a host with the matching architecture"))
    }

    fn canonical_pinned_reference(
        &self,
        reference: &str,
        metadata: &ImageMetadata,
        source_base: &Path,
    ) -> Result<String, FirestoneError> {
        if metadata.source_url.is_none() {
            return self.absolute_local_reference(reference, source_base);
        }
        if let Some(url) = parse_https_url(reference) {
            return Ok(url.to_string());
        }
        if self.catalog.contains_reference(reference) {
            return Ok(self
                .catalog
                .resolve(reference, self.architecture.as_str())?
                .canonical_reference);
        }
        Ok(reference.to_owned())
    }

    fn validate_pinned_image(
        &self,
        name: &str,
        image: &StateImage,
        stored: &StoredImage,
        source_base: &Path,
    ) -> Result<Option<CatalogFirmware>, FirestoneError> {
        self.validate_image_architecture(name, &stored.metadata)?;
        let Some(source_sha256) = image.sha256.as_deref() else {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has no pinned source SHA-256"),
            ));
        };
        if stored.metadata.source_sha256 != source_sha256 {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "machine `{name}` pins image `{}` with a different source SHA-256",
                    stored.metadata.id
                ),
            )
            .with_hint("restore the pinned image or recreate the machine"));
        }
        let canonical =
            self.canonical_pinned_reference(&image.r#ref, &stored.metadata, source_base)?;
        if canonical != stored.metadata.source_ref {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "machine `{name}` image reference '{canonical}' does not match pinned source '{}'",
                    stored.metadata.source_ref
                ),
            )
            .with_hint("restore the original image reference or recreate the machine"));
        }
        Ok(stored.metadata.firmware)
    }

    fn create_overlay_locked(
        &self,
        name: &str,
        image: &StateImage,
        disk_size: ByteSize,
        machine_lock: &MachineLock,
        verified: Option<&PulledImage>,
    ) -> Result<OverlayInfo, FirestoneError> {
        self.validate_machine_lock(name, machine_lock)?;
        let (Some(id), Some(_)) = (&image.id, &image.sha256) else {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has no pinned image identity"),
            )
            .with_hint("resolve and persist the image before creating an overlay"));
        };
        let machine_dir = self.paths.machine_dir(name)?;
        let stored = match verified {
            Some(pulled) if pulled.metadata.id == *id => StoredImage {
                metadata: pulled.metadata.clone(),
                path: pulled.path.clone(),
            },
            Some(_) => {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("prepared image does not match machine `{name}` pin"),
                ));
            }
            None => self.load_verified_pair(id)?,
        };
        let _firmware = self.validate_pinned_image(name, image, &stored, &machine_dir)?;
        let base_info = self.qemu_info(&stored.path)?;
        validate_base_info(id, &base_info)?;
        if disk_size.as_bytes() < base_info.virtual_size {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!(
                    "machine `{name}` disk is {} bytes but base image `{id}` requires at least {} bytes",
                    disk_size.as_bytes(),
                    base_info.virtual_size
                ),
            )
            .with_hint("increase the machine disk size and retry"));
        }

        let overlay = self.paths.machine_disk(name)?;
        let partial = self.paths.machine_disk_partial(name)?;
        match fs::symlink_metadata(&overlay) {
            Ok(_) => {
                self.paths.validate_owned_data_file(
                    &overlay,
                    "machine overlay",
                    OVERLAY_FILE_MODE,
                    false,
                )?;
                let info = self.qemu_info(&overlay)?;
                validate_overlay_info(&overlay, &stored.path, disk_size.as_bytes(), &info)?;
                return Ok(OverlayInfo {
                    path: overlay,
                    backing_path: stored.path,
                    virtual_size: info.virtual_size,
                    cached: true,
                });
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(image_file_error("inspect", &overlay, source)),
        }

        remove_stale_partial(&partial)?;
        let mut cleanup = CleanupGuard::new();
        cleanup.track(partial.clone());

        // Pinned qemu-img 8.2.2 overlay argv; verify 5 is functionally closed
        // for the x86_64 edk2 observation recorded in SPEC section 21.
        Cmd::new(self.qemu_img.as_os_str())
            .arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg("-F")
            .arg("qcow2")
            .arg("-b")
            .arg(stored.path.as_os_str())
            .arg(partial.as_os_str())
            .arg(disk_size.as_bytes().to_string())
            .timeout(QEMU_CREATE_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;
        validate_created_regular_file(&partial, "machine overlay partial")?;
        set_file_mode(&partial, OVERLAY_FILE_MODE, "machine overlay partial")?;
        sync_file(&partial, "machine overlay partial")?;
        let info = self.qemu_info(&partial)?;
        validate_overlay_info(&partial, &stored.path, disk_size.as_bytes(), &info)?;
        cleanup.track(overlay.clone());
        publish_no_replace(&partial, &overlay)?;
        sync_directory(
            overlay.parent().ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!("overlay '{}' has no parent directory", overlay.display()),
                )
            })?,
            "machine directory",
        )?;
        cleanup.disarm();
        Ok(OverlayInfo {
            path: overlay,
            backing_path: stored.path,
            virtual_size: info.virtual_size,
            cached: false,
        })
    }

    fn validate_machine_lock(
        &self,
        name: &str,
        machine_lock: &MachineLock,
    ) -> Result<(), FirestoneError> {
        let expected = self.paths.machine_lock(name)?;
        if machine_lock.path() != expected {
            return Err(FirestoneError::new(
                ErrorKind::Busy,
                format!("machine lock does not protect machine `{name}`"),
            )
            .with_hint("acquire the target machine lock before preparing its image"));
        }
        self.paths.validate_owned_data_directory(
            &self.paths.machine_dir(name)?,
            "machine directory",
            false,
        )
    }

    fn emit_image_start(
        &self,
        source_ref: &str,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        events.emit(Event::StepStart {
            id: StepId::from("image"),
            label: source_ref.to_owned(),
        })
    }

    fn ensure_store(&self) -> Result<(), FirestoneError> {
        self.paths
            .ensure_owned_data_directory(self.paths.data_dir(), "data directory", true)?;
        self.paths.ensure_owned_data_directory(
            &self.paths.images_dir(),
            "images directory",
            false,
        )?;
        let mode = fs::symlink_metadata(self.paths.images_dir())
            .map_err(|source| image_file_error("inspect", &self.paths.images_dir(), source))?
            .mode()
            & 0o7777;
        if mode != OWNED_DIRECTORY_MODE {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "images directory '{}' has mode {mode:04o}; expected 0700",
                    self.paths.images_dir().display()
                ),
            )
            .with_hint("restrict the images directory to the Firestone user"));
        }
        Ok(())
    }

    fn store_exists_for_read(&self) -> Result<bool, FirestoneError> {
        self.paths
            .validate_owned_data_directory(self.paths.data_dir(), "data directory", true)?;
        if !path_exists_without_following(self.paths.data_dir())? {
            return Ok(false);
        }
        self.paths
            .validate_owned_data_directory(self.paths.data_dir(), "data directory", false)?;
        self.paths.validate_owned_data_directory(
            &self.paths.images_dir(),
            "images directory",
            true,
        )?;
        if !path_exists_without_following(&self.paths.images_dir())? {
            return Ok(false);
        }
        self.paths.validate_owned_data_directory(
            &self.paths.images_dir(),
            "images directory",
            false,
        )?;
        Ok(true)
    }

    fn acquire_lock(&self) -> Result<ImageStoreLock, FirestoneError> {
        ImageStoreLock::acquire(&self.paths, LOCK_TIMEOUT, LOCK_POLL_INTERVAL)
    }

    fn cleanup_stale_partials(&self) -> Result<(), FirestoneError> {
        let entries = fs::read_dir(self.paths.images_dir())
            .map_err(|source| image_file_error("read", &self.paths.images_dir(), source))?;
        let mut removal_ids = BTreeSet::new();
        let mut removed_partial = false;
        for entry in entries {
            let entry = entry.map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot read an images directory entry")
                    .with_source(source)
            })?;
            let name = entry.file_name().into_string().map_err(|_| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "images directory contains a non-UTF-8 file name",
                )
                .with_hint("move the unknown file out of the images directory")
            })?;
            if is_known_partial_name(&name) {
                let path = entry.path();
                validate_regular_nofollow(&path, "image partial")?;
                fs::remove_file(&path)
                    .map_err(|source| image_file_error("remove stale", &path, source))?;
                removed_partial = true;
                continue;
            }
            if let Some(id) = removal_id_from_name(&name) {
                removal_ids.insert(id.to_owned());
            }
        }
        if removed_partial {
            sync_directory(&self.paths.images_dir(), "images directory")?;
        }
        for id in removal_ids {
            self.complete_stale_removal(&id)?;
        }
        self.recover_incomplete_pairs()
    }

    fn recover_incomplete_pairs(&self) -> Result<(), FirestoneError> {
        let mut pairs = BTreeMap::<String, ImagePairPresence>::new();
        for entry in fs::read_dir(self.paths.images_dir())
            .map_err(|source| image_file_error("read", &self.paths.images_dir(), source))?
        {
            let entry = entry.map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot read an images directory entry")
                    .with_source(source)
            })?;
            let name = entry.file_name().into_string().map_err(|_| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "images directory contains a non-UTF-8 file name",
                )
                .with_hint("move the unknown file out of the images directory")
            })?;
            let Some((id, artifact)) = image_artifact_from_name(&name)? else {
                continue;
            };
            let presence = pairs.entry(id).or_default();
            match artifact {
                ImageArtifact::Base => presence.base = true,
                ImageArtifact::Sidecar => presence.sidecar = true,
                ImageArtifact::SidecarTemp => presence.sidecar_temp = true,
            }
        }

        let references = if pairs
            .values()
            .any(|presence| !(presence.base && presence.sidecar))
        {
            self.image_references()?
        } else {
            BTreeMap::new()
        };
        let mut changed = false;
        for (id, presence) in pairs {
            let complete = presence.base && presence.sidecar;
            if !complete {
                if let Some(machine_names) = references.get(&id).filter(|names| !names.is_empty()) {
                    return Err(FirestoneError::new(
                        ErrorKind::Checksum,
                        format!(
                            "referenced image `{id}` has an incomplete base/sidecar publication; referenced by machine(s): {}",
                            machine_names.join(", ")
                        ),
                    )
                    .with_hint(
                        "restore the missing immutable image file before retrying; Firestone preserved the remaining files",
                    ));
                }
            }

            let base = self.paths.image_base(&id)?;
            let sidecar = self.paths.image_metadata(&id)?;
            let sidecar_temp = sidecar.with_file_name(format!("{id}.json.tmp"));
            if presence.sidecar_temp {
                let _ = read_owned_bounded(
                    &self.paths,
                    &sidecar_temp,
                    "image sidecar temporary file",
                    SIDECAR_FILE_MODE,
                    MAX_SIDECAR_BYTES,
                )?;
                fs::remove_file(&sidecar_temp)
                    .map_err(|source| image_file_error("remove stale", &sidecar_temp, source))?;
                changed = true;
            }
            if complete {
                continue;
            }
            if presence.sidecar {
                let _ = read_owned_bounded(
                    &self.paths,
                    &sidecar,
                    "incomplete image sidecar",
                    SIDECAR_FILE_MODE,
                    MAX_SIDECAR_BYTES,
                )?;
                fs::remove_file(&sidecar)
                    .map_err(|source| image_file_error("remove incomplete", &sidecar, source))?;
                changed = true;
            }
            if presence.base {
                self.paths.validate_owned_data_file(
                    &base,
                    "incomplete image base",
                    BASE_FILE_MODE,
                    false,
                )?;
                fs::remove_file(&base)
                    .map_err(|source| image_file_error("remove incomplete", &base, source))?;
                changed = true;
            }
        }
        if changed {
            sync_directory(&self.paths.images_dir(), "images directory")?;
        }
        Ok(())
    }
    fn complete_stale_removal(&self, id: &str) -> Result<(), FirestoneError> {
        validate_image_id(id)?;
        let base = self.paths.image_base(id)?;
        let sidecar = self.paths.image_metadata(id)?;
        let base_tombstone = self.paths.image_base_removal(id)?;
        let sidecar_tombstone = self.paths.image_metadata_removal(id)?;
        self.move_to_tombstone(
            &sidecar,
            &sidecar_tombstone,
            "image sidecar",
            SIDECAR_FILE_MODE,
        )?;
        self.move_to_tombstone(&base, &base_tombstone, "image base", BASE_FILE_MODE)?;
        sync_directory(&self.paths.images_dir(), "images directory")?;
        self.remove_tombstone(
            &sidecar_tombstone,
            "image sidecar removal tombstone",
            SIDECAR_FILE_MODE,
        )?;
        self.remove_tombstone(
            &base_tombstone,
            "image base removal tombstone",
            BASE_FILE_MODE,
        )?;
        sync_directory(&self.paths.images_dir(), "images directory")
    }

    fn move_to_tombstone(
        &self,
        source: &Path,
        tombstone: &Path,
        label: &str,
        mode: u32,
    ) -> Result<(), FirestoneError> {
        let source_exists = path_exists_without_following(source)?;
        let tombstone_exists = path_exists_without_following(tombstone)?;
        match (source_exists, tombstone_exists) {
            (false, false) => Ok(()),
            (false, true) => self.paths.validate_owned_data_file(
                tombstone,
                &format!("{label} removal tombstone"),
                mode,
                false,
            ),
            (true, false) => {
                self.paths
                    .validate_owned_data_file(source, label, mode, false)?;
                fs::rename(source, tombstone)
                    .map_err(|error| image_file_error("stage stale removal of", source, error))
            }
            (true, true) => Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "both {label} '{}' and removal tombstone '{}' exist",
                    source.display(),
                    tombstone.display()
                ),
            )
            .with_hint("move the conflicting image files aside and retry")),
        }
    }

    fn remove_tombstone(&self, path: &Path, label: &str, mode: u32) -> Result<(), FirestoneError> {
        if !path_exists_without_following(path)? {
            return Ok(());
        }
        self.paths
            .validate_owned_data_file(path, label, mode, false)?;
        fs::remove_file(path).map_err(|source| image_file_error("remove", path, source))
    }
}

struct ImageStoreLock {
    _file: Flock<File>,
}

impl ImageStoreLock {
    fn acquire(
        paths: &Paths,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, FirestoneError> {
        let path = paths.image_store_lock()?;
        let create = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(LOCK_FILE_MODE)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
            .open(&path);
        let file = match create {
            Ok(file) => {
                file.set_permissions(fs::Permissions::from_mode(LOCK_FILE_MODE))
                    .map_err(|source| image_lock_error("set mode on", &path, source))?;
                file.sync_all()
                    .map_err(|source| image_lock_error("fsync", &path, source))?;
                file
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
                .read(true)
                .write(true)
                .truncate(false)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
                .open(&path)
                .map_err(|source| image_lock_error("open existing", &path, source))?,
            Err(source) => return Err(image_lock_error("create", &path, source)),
        };
        paths.validate_owned_data_file_handle(&path, "image store lock", LOCK_FILE_MODE, &file)?;
        let started = Instant::now();
        let mut file = file;
        loop {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(file) => return Ok(Self { _file: file }),
                Err((returned, error)) if error == Errno::EWOULDBLOCK || error == Errno::EAGAIN => {
                    file = returned;
                }
                Err((_, error)) => {
                    return Err(FirestoneError::new(
                        ErrorKind::Generic,
                        format!(
                            "cannot acquire image store lock '{}': {error}",
                            path.display()
                        ),
                    )
                    .with_hint("check the image store permissions"));
                }
            }
            if started.elapsed() >= timeout {
                return Err(FirestoneError::new(
                    ErrorKind::Busy,
                    "image store is busy with another mutation",
                )
                .with_hint("wait for the other image operation to finish and retry"));
            }
            thread::sleep(poll_interval.min(timeout.saturating_sub(started.elapsed())));
        }
    }
}

struct CleanupGuard {
    paths: Vec<PathBuf>,
    armed: bool,
}

impl CleanupGuard {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            armed: true,
        }
    }

    fn track(&mut self, path: PathBuf) {
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.paths.clear();
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for path in self.paths.iter().rev() {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    tracing::error!(path = %path.display(), error = %source, "cannot clean image operation file");
                }
            }
        }
    }
}

#[derive(Debug)]
struct StagedSource {
    source_sha256: String,
    source_sha512: Option<String>,
    size: u64,
    detected_format: ImageFormat,
}

fn stream_source(
    input: &mut dyn Read,
    output: &Path,
    expected_length: Option<u64>,
    source_error_kind: ErrorKind,
    verification_algorithm: Option<ChecksumAlgorithm>,
    events: &mut dyn EventSink,
) -> Result<StagedSource, FirestoneError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SIDECAR_FILE_MODE)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(output)
        .map_err(|source| image_file_error("create", output, source))?;
    file.set_permissions(fs::Permissions::from_mode(SIDECAR_FILE_MODE))
        .map_err(|source| image_file_error("set mode on", output, source))?;
    let mut sha256 = Sha256::new();
    let mut sha512 = (verification_algorithm == Some(ChecksumAlgorithm::Sha512)).then(Sha512::new);
    let mut buffer = vec![0_u8; IMAGE_BUFFER_SIZE];
    let mut size = 0_u64;
    let mut header = [0_u8; 4];
    let mut header_length = 0_usize;

    loop {
        let read = input.read(&mut buffer).map_err(|source| {
            FirestoneError::new(
                source_error_kind,
                format!(
                    "cannot read image source while writing '{}'",
                    output.display()
                ),
            )
            .with_hint("retry the pull")
            .with_source(source)
        })?;
        if read == 0 {
            break;
        }
        let next_size = size.checked_add(read as u64).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "image source size overflowed u64")
        })?;
        if let Some(expected) = expected_length {
            if next_size > expected {
                return Err(FirestoneError::new(
                    source_error_kind,
                    format!("image source exceeded declared Content-Length of {expected} bytes"),
                )
                .with_hint("retry the pull; the remote response was inconsistent"));
            }
        }
        file.write_all(&buffer[..read])
            .map_err(|source| image_file_error("write", output, source))?;
        sha256.update(&buffer[..read]);
        if let Some(hasher) = &mut sha512 {
            hasher.update(&buffer[..read]);
        }
        if header_length < header.len() {
            let copy = (header.len() - header_length).min(read);
            header[header_length..header_length + copy].copy_from_slice(&buffer[..copy]);
            header_length += copy;
        }
        size = next_size;
        events.emit(Event::Progress {
            id: StepId::from("image"),
            done: size,
            total: expected_length,
            unit: Unit::Bytes,
        })?;
    }

    if let Some(expected) = expected_length {
        if size != expected {
            return Err(FirestoneError::new(
                source_error_kind,
                format!(
                    "image source ended after {size} bytes; Content-Length declared {expected}"
                ),
            )
            .with_hint("retry the pull; the remote response was partial"));
        }
    }
    file.sync_all()
        .map_err(|source| image_file_error("fsync", output, source))?;
    let detected_format = if header_length == QCOW2_MAGIC.len() && header == QCOW2_MAGIC {
        ImageFormat::Qcow2
    } else {
        ImageFormat::Raw
    };
    Ok(StagedSource {
        source_sha256: digest_hex(sha256.finalize().as_slice()),
        source_sha512: sha512.map(|hasher| digest_hex(hasher.finalize().as_slice())),
        size,
        detected_format,
    })
}

fn parse_checksum_manifest(
    manifest: &str,
    algorithm: ChecksumAlgorithm,
    filename: &str,
) -> Result<String, FirestoneError> {
    let mut matches = BTreeSet::new();
    for line in manifest.lines() {
        if let Some((entry_filename, digest)) = parse_manifest_line(line, algorithm) {
            let normalized = entry_filename.strip_prefix("./").unwrap_or(&entry_filename);
            if normalized == filename {
                matches.insert(digest);
            }
        }
    }
    match matches.len() {
        0 => Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!("checksum manifest has no entry for '{filename}'"),
        )
        .with_hint("check that checksum_url and image URL name the same release file")),
        1 => matches.into_iter().next().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Checksum,
                format!("checksum manifest has no entry for '{filename}'"),
            )
        }),
        _ => Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!("checksum manifest has conflicting entries for '{filename}'"),
        )
        .with_hint("use an unambiguous checksum manifest")),
    }
}

fn parse_manifest_line(line: &str, algorithm: ChecksumAlgorithm) -> Option<(String, String)> {
    let line = line.trim_end_matches('\r').trim();
    let digest_length = digest_length(algorithm);
    let bytes = line.as_bytes();
    if bytes.len() > digest_length {
        let candidate_bytes = bytes.get(..digest_length)?;
        let rest_bytes = bytes.get(digest_length..)?;
        if candidate_bytes.iter().all(u8::is_ascii_hexdigit)
            && rest_bytes.first().is_some_and(u8::is_ascii_whitespace)
        {
            let candidate = std::str::from_utf8(candidate_bytes).ok()?;
            let rest = std::str::from_utf8(rest_bytes).ok()?;
            let mut filename = rest.trim_start();
            if let Some(stripped) = filename.strip_prefix('*') {
                filename = stripped;
            }
            if !filename.is_empty() {
                return Some((filename.to_owned(), candidate.to_ascii_lowercase()));
            }
        }
    }

    let label = algorithm_name(algorithm);
    let prefix = line.get(..label.len())?;
    if !prefix.eq_ignore_ascii_case(label) {
        return None;
    }
    let after_label = line.get(label.len()..)?.trim_start();
    let inside = after_label.strip_prefix('(')?;
    let (filename, after_filename) = inside.rsplit_once(')')?;
    let digest = after_filename.trim_start().strip_prefix('=')?.trim();
    if !is_hex(digest, digest_length) {
        return None;
    }
    Some((filename.to_owned(), digest.to_ascii_lowercase()))
}

fn image_filename(url: &str) -> Result<String, FirestoneError> {
    let parsed = parse_https_url(url).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("image URL '{url}' is not strict HTTPS"),
        )
    })?;
    let encoded = parsed
        .path_segments()
        .and_then(Iterator::last)
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("image URL '{url}' has no file name"),
            )
            .with_hint("use a URL whose path ends in the checksum manifest file name")
        })?;
    percent_decode_utf8(encoded).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("image URL '{url}' has a non-UTF-8 file name"),
        )
        .with_hint("use a UTF-8 image file name")
    })
}

fn percent_decode_utf8(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn metadata_matches_cache_source(metadata: &ImageMetadata, source: &ResolvedImageSource) -> bool {
    if metadata.source_ref != source.source_ref
        || metadata.architecture != source.architecture
        || source
            .source_format
            .is_some_and(|format| format != metadata.source_format)
    {
        return false;
    }
    match &source.checksum {
        ExpectedChecksum::None => {
            metadata.source_url == source.source_url
                && metadata.verification_algorithm.is_none()
                && metadata.verification_digest.is_none()
        }
        ExpectedChecksum::Digest(verification) => {
            metadata.source_url == source.source_url
                && metadata.verification().as_ref() == Some(verification)
        }
        ExpectedChecksum::Manifest { algorithm, .. } => {
            metadata.verification_algorithm == Some(*algorithm)
                && metadata.verification_digest.is_some()
        }
    }
}

fn metadata_matches_source(
    metadata: &ImageMetadata,
    source: &ResolvedImageSource,
    require_verification: bool,
) -> bool {
    metadata.source_ref == source.source_ref
        && metadata.source_url == source.source_url
        && metadata.architecture == source.architecture
        && metadata.firmware == source.firmware
        && source
            .source_format
            .is_none_or(|format| format == metadata.source_format)
        && (!require_verification || metadata.verification() == source.verification)
}

fn stable_image_id(
    source_ref: &str,
    source_url: Option<&str>,
    architecture: Arch,
    source_sha256: &str,
) -> String {
    let mut hasher = Sha256::new();
    for component in [
        "firestone-image-v1",
        source_ref,
        source_url.unwrap_or(""),
        architecture.as_str(),
        source_sha256,
    ] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component.as_bytes());
    }
    format!(
        "{IMAGE_ID_PREFIX}{}",
        digest_hex(hasher.finalize().as_slice())
    )
}

fn operation_key(source: &ResolvedImageSource) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.source_ref.as_bytes());
    if let Some(url) = &source.source_url {
        hasher.update([0]);
        hasher.update(url.as_bytes());
    }
    digest_hex(hasher.finalize().as_slice())
}

fn validate_image_id(id: &str) -> Result<(), FirestoneError> {
    let Some(digest) = id.strip_prefix(IMAGE_ID_PREFIX) else {
        return Err(invalid_image_id(id));
    };
    if !is_lower_hex(digest, IMAGE_ID_HEX_LENGTH) {
        return Err(invalid_image_id(id));
    }
    Ok(())
}

fn invalid_image_id(id: &str) -> FirestoneError {
    FirestoneError::new(ErrorKind::InvalidSpec, format!("invalid image id '{id}'"))
        .with_hint("use the complete stable id shown by `firestone images ls`")
}

fn validate_sha256(value: &str) -> Result<String, FirestoneError> {
    if !is_hex(value, 64) {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "--sha256 must contain exactly 64 hexadecimal characters",
        )
        .with_hint("copy the complete SHA-256 digest, not an abbreviated prefix"));
    }
    Ok(value.to_ascii_lowercase())
}

fn digest_length(algorithm: ChecksumAlgorithm) -> usize {
    match algorithm {
        ChecksumAlgorithm::Sha256 => 64,
        ChecksumAlgorithm::Sha512 => 128,
    }
}

fn algorithm_name(algorithm: ChecksumAlgorithm) -> &'static str {
    match algorithm {
        ChecksumAlgorithm::Sha256 => "SHA256",
        ChecksumAlgorithm::Sha512 => "SHA512",
    }
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn local_source_snapshot(metadata: &fs::Metadata) -> LocalSourceSnapshot {
    LocalSourceSnapshot {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn try_open_local_source(path: &Path) -> Result<Option<OpenedLocalSource>, FirestoneError> {
    let parent = path.parent().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "local image path '{}' has no parent directory",
                path.display()
            ),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("local image path '{}' has no file name", path.display()),
        )
    })?;
    let canonical_parent = match fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!(
                    "cannot canonicalize local image parent '{}'",
                    parent.display()
                ),
            )
            .with_hint("check every parent path component")
            .with_source(source));
        }
    };
    let parent_file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(&canonical_parent)
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!(
                    "cannot open local image parent '{}'",
                    canonical_parent.display()
                ),
            )
            .with_source(source)
        })?;
    let descriptor = match openat(
        &parent_file,
        Path::new(file_name),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::ENOENT) => return Ok(None),
        Err(Errno::ELOOP) => {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("local image '{}' is a symlink", path.display()),
            )
            .with_hint("use a regular raw or qcow2 file, not a final-component symlink"));
        }
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot open local image '{}'", path.display()),
            )
            .with_hint("check that the image is a readable regular file")
            .with_source(io::Error::from(source)));
        }
    };
    let file = File::from(descriptor);
    let metadata = file.metadata().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot inspect opened local image '{}'", path.display()),
        )
        .with_source(source)
    })?;
    if !metadata.is_file() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("local image '{}' is not a regular file", path.display()),
        )
        .with_hint("use a regular raw or qcow2 file, not a FIFO, device, or socket"));
    }
    let uid = metadata.uid();
    let current_uid = nix::unistd::getuid().as_raw();
    if uid != current_uid && uid != 0 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("local image '{}' is owned by uid {uid}", path.display()),
        )
        .with_hint("use an image owned by the current user or root"));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "local image '{}' is group- or world-writable",
                path.display()
            ),
        )
        .with_hint("remove group/world write permissions before pulling the image"));
    }
    let canonical_path = canonical_parent.join(file_name);
    Ok(Some(OpenedLocalSource {
        path: canonical_path,
        file: Arc::new(file),
        snapshot: local_source_snapshot(&metadata),
    }))
}

fn validate_created_regular_file(path: &Path, label: &str) -> Result<(), FirestoneError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| image_file_error("inspect", path, source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} '{}' is not a regular file", path.display()),
        )
        .with_hint("check qemu-img and the Firestone data directory"));
    }
    Ok(())
}

fn validate_regular_nofollow(path: &Path, label: &str) -> Result<(), FirestoneError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| image_file_error("inspect", path, source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} '{}' is not a regular file", path.display()),
        )
        .with_hint("replace the symlink or special file with a regular owned file"));
    }
    Ok(())
}

fn read_owned_bounded(
    paths: &Paths,
    path: &Path,
    label: &str,
    mode: u32,
    limit: u64,
) -> Result<Vec<u8>, FirestoneError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot open {label} '{}'", path.display()),
            )
            .with_hint("check the file permissions")
            .with_source(source)
        })?;
    paths.validate_owned_data_file_handle(path, label, mode, &file)?;
    bounded::read_to_end(&mut file, limit).map_err(|error| match error {
        BoundedReadError::Io(source) => FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot read {label} '{}'", path.display()),
        )
        .with_hint("check the file permissions")
        .with_source(source),
        BoundedReadError::LimitExceeded => FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{label} '{}' exceeds the {limit} byte limit",
                path.display()
            ),
        )
        .with_hint("replace the oversized file with bounded strict metadata"),
    })
}

fn hash_file_sha256(path: &Path) -> Result<String, FirestoneError> {
    hash_file_with_size(path).map(|(digest, _)| digest)
}

fn hash_file_with_size(path: &Path) -> Result<(String, u64), FirestoneError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|source| image_file_error("open", path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IMAGE_BUFFER_SIZE];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| image_file_error("read", path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.checked_add(read as u64).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "image file size overflowed u64")
        })?;
    }
    Ok((digest_hex(hasher.finalize().as_slice()), size))
}

fn set_file_mode(path: &Path, mode: u32, label: &str) -> Result<(), FirestoneError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot set mode {mode:04o} on {label} '{}'", path.display()),
        )
        .with_hint("check the Firestone data directory permissions")
        .with_source(source)
    })
}

fn sync_file(path: &Path, label: &str) -> Result<(), FirestoneError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot fsync {label} '{}'", path.display()),
            )
            .with_hint("check the data filesystem and free space")
            .with_source(source)
        })
}

fn sync_directory(path: &Path, label: &str) -> Result<(), FirestoneError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot fsync {label} '{}'", path.display()),
            )
            .with_hint("check the data filesystem and free space")
            .with_source(source)
        })
}

fn publish_no_replace(source: &Path, destination: &Path) -> Result<(), FirestoneError> {
    fs::hard_link(source, destination).map_err(|source_error| {
        let kind = if source_error.kind() == io::ErrorKind::AlreadyExists {
            ErrorKind::Conflict
        } else {
            ErrorKind::Generic
        };
        FirestoneError::new(
            kind,
            format!(
                "cannot publish '{}' as '{}'",
                source.display(),
                destination.display()
            ),
        )
        .with_hint("check for a conflicting image file and available space")
        .with_source(source_error)
    })?;
    fs::remove_file(source).map_err(|source_error| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!(
                "cannot remove published staging file '{}'",
                source.display()
            ),
        )
        .with_hint("remove the staging file and retry")
        .with_source(source_error)
    })?;
    let parent = destination.parent().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("published file '{}' has no parent", destination.display()),
        )
    })?;
    sync_directory(parent, "publication directory")
}

fn ensure_pair_absent(base: &Path, sidecar: &Path, id: &str) -> Result<(), FirestoneError> {
    if path_exists_without_following(base)? || path_exists_without_following(sidecar)? {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("image `{id}` appeared while it was being pulled"),
        )
        .with_hint("retry after the other image operation finishes"));
    }
    Ok(())
}

fn remove_stale_partial(path: &Path) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|source| image_file_error("remove stale", path, source))
        }
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("partial path '{}' is not a regular file", path.display()),
        )
        .with_hint("move the symlink or special file aside and retry")),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(image_file_error("inspect", path, source)),
    }
}

fn path_exists_without_following(path: &Path) -> Result<bool, FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(image_file_error("inspect", path, source)),
    }
}

fn removal_id_from_name(name: &str) -> Option<&str> {
    let id = name
        .strip_suffix(".qcow2.removing")
        .or_else(|| name.strip_suffix(".json.removing"))?;
    validate_image_id(id).is_ok().then_some(id)
}

fn is_known_partial_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".pull-") else {
        return false;
    };
    let digest = rest
        .strip_suffix(".source.partial")
        .or_else(|| rest.strip_suffix(".stored.partial"));
    digest.is_some_and(|value| is_lower_hex(value, 64))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageArtifact {
    Base,
    Sidecar,
    SidecarTemp,
}

#[derive(Debug, Default)]
struct ImagePairPresence {
    base: bool,
    sidecar: bool,
    sidecar_temp: bool,
}

fn image_artifact_from_name(name: &str) -> Result<Option<(String, ImageArtifact)>, FirestoneError> {
    let parsed = name
        .strip_suffix(".json.tmp")
        .map(|id| (id, ImageArtifact::SidecarTemp))
        .or_else(|| {
            name.strip_suffix(".json")
                .map(|id| (id, ImageArtifact::Sidecar))
        })
        .or_else(|| {
            name.strip_suffix(".qcow2")
                .map(|id| (id, ImageArtifact::Base))
        });
    let Some((id, artifact)) = parsed else {
        return Ok(None);
    };
    validate_image_id(id).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("images directory contains invalid image artifact name '{name}'"),
        )
        .with_hint("move the malformed image artifact out of the images directory")
        .with_source(source)
    })?;
    Ok(Some((id.to_owned(), artifact)))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Qcow2FormatSpecific {
    corrupt: Option<bool>,
    data_file: Option<String>,
    data_file_raw: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QemuInfo {
    format: String,
    virtual_size: u64,
    backing_filename: Option<String>,
    backing_filename_format: Option<String>,
    full_backing_filename: Option<String>,
    dirty_flag: Option<bool>,
    corrupt: Option<bool>,
    data_file: Option<String>,
    data_file_raw: Option<bool>,
}

fn required_qemu_string(
    value: &serde_json::Value,
    field: &str,
    path: &Path,
) -> Result<String, FirestoneError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| missing_qemu_info_field(path, field))
}

fn optional_qemu_string(
    value: &serde_json::Value,
    field: &str,
    path: &Path,
) -> Result<Option<String>, FirestoneError> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| missing_qemu_info_field(path, field)),
    }
}

fn optional_qemu_bool(
    value: &serde_json::Value,
    field: &str,
    path: &Path,
) -> Result<Option<bool>, FirestoneError> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| missing_qemu_info_field(path, field)),
    }
}

fn parse_qcow2_format_specific(
    value: &serde_json::Value,
    path: &Path,
) -> Result<Qcow2FormatSpecific, FirestoneError> {
    let format_specific = value
        .get("format-specific")
        .ok_or_else(|| missing_qemu_info_field(path, "format-specific"))?;
    if format_specific.is_null() {
        return Err(missing_qemu_info_field(path, "format-specific"));
    }
    let object = format_specific
        .as_object()
        .ok_or_else(|| missing_qemu_info_field(path, "format-specific"))?;
    if object.get("type").and_then(serde_json::Value::as_str) != Some("qcow2") {
        return Err(missing_qemu_info_field(path, "format-specific.type"));
    }
    let data = object
        .get("data")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| missing_qemu_info_field(path, "format-specific.data"))?;
    for key in data.keys() {
        let normalized = key.replace('_', "-");
        if normalized.contains("data-file") && key != "data-file" && key != "data-file-raw" {
            return Err(missing_qemu_info_field(
                path,
                "format-specific.data external-file field",
            ));
        }
    }
    let data_value = serde_json::Value::Object(data.clone());
    Ok(Qcow2FormatSpecific {
        corrupt: optional_qemu_bool(&data_value, "corrupt", path)?,
        data_file: optional_qemu_string(&data_value, "data-file", path)?,
        data_file_raw: optional_qemu_bool(&data_value, "data-file-raw", path)?,
    })
}
fn reject_hidden_qemu_dependencies(
    value: &serde_json::Value,
    path: &Path,
) -> Result<(), FirestoneError> {
    let object = value
        .as_object()
        .ok_or_else(|| missing_qemu_info_field(path, "top-level object"))?;
    for key in object.keys() {
        let normalized = key.replace('_', "-");
        let known_backing = matches!(
            key.as_str(),
            "backing-filename" | "backing-filename-format" | "full-backing-filename"
        );
        if (normalized.contains("backing") && !known_backing) || normalized.contains("data-file") {
            return Err(missing_qemu_info_field(
                path,
                "unknown field that may hide an external dependency",
            ));
        }
    }
    Ok(())
}
fn validate_qcow2_health(label: &str, info: &QemuInfo) -> Result<(), FirestoneError> {
    if info.format != "qcow2" {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} has qemu format '{}'", info.format),
        ));
    }
    if info.dirty_flag != Some(false) || info.corrupt != Some(false) {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} omitted health fields or is marked dirty/corrupt by qemu-img"),
        )
        .with_hint("repair or replace the qcow2 image before retrying"));
    }
    if info.data_file.is_some() || info.data_file_raw == Some(true) {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} uses an external qcow2 data file"),
        )
        .with_hint("flatten the image so every guest byte is stored in the owned qcow2 file"));
    }
    Ok(())
}

fn validate_base_info(id: &str, info: &QemuInfo) -> Result<(), FirestoneError> {
    validate_qcow2_health(&format!("stored image `{id}`"), info)?;
    if info.backing_filename.is_some()
        || info.backing_filename_format.is_some()
        || info.full_backing_filename.is_some()
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("stored image `{id}` unexpectedly has a backing file"),
        )
        .with_hint("flatten the source or pull a standalone cloud image"));
    }
    Ok(())
}

fn validate_overlay_info(
    overlay: &Path,
    base: &Path,
    requested_size: u64,
    info: &QemuInfo,
) -> Result<(), FirestoneError> {
    validate_qcow2_health(&format!("overlay '{}'", overlay.display()), info)?;
    let expected = base.to_str().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("base image path '{}' is not UTF-8", base.display()),
        )
    })?;
    if info.backing_filename.as_deref() != Some(expected)
        || info.full_backing_filename.as_deref() != Some(expected)
        || info.backing_filename_format.as_deref() != Some("qcow2")
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "overlay '{}' does not reference exact qcow2 base '{}'",
                overlay.display(),
                base.display()
            ),
        )
        .with_hint("remove the invalid overlay and retry start"));
    }
    if info.virtual_size != requested_size {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "overlay '{}' has virtual size {}; expected {requested_size}",
                overlay.display(),
                info.virtual_size
            ),
        )
        .with_hint("remove the invalid overlay and retry start"));
    }
    Ok(())
}

fn missing_qemu_info_field(path: &Path, field: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "qemu-img info for '{}' omitted or invalidated field '{field}'",
            path.display()
        ),
    )
    .with_hint("install qemu-img 8.2.2 or a compatible release")
}

fn invalid_sidecar(id: &str, detail: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("invalid image sidecar for `{id}`: {detail}"),
    )
    .with_hint("remove the invalid image pair and pull it again")
}

fn image_not_found(id: &str) -> FirestoneError {
    FirestoneError::new(ErrorKind::NotFound, format!("no image with id `{id}`"))
        .with_hint("run `firestone images ls` to list stored images")
}

fn image_lock_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot {operation} image store lock '{}'", path.display()),
    )
    .with_hint("replace a symlink, special, or insecure lock file with a protected regular file")
    .with_source(source)
}

fn image_file_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    let kind = if source.kind() == io::ErrorKind::NotFound {
        ErrorKind::NotFound
    } else {
        ErrorKind::Generic
    };
    FirestoneError::new(
        kind,
        format!("cannot {operation} image file '{}'", path.display()),
    )
    .with_hint("check the Firestone data directory permissions and free space")
    .with_source(source)
}

fn download_error(url: &Url, source: reqwest::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot download HTTPS image resource '{url}'"),
    )
    .with_hint("check network access and the image URL, then retry")
    .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        env, fs,
        io::Cursor,
        os::unix::fs::{PermissionsExt, symlink},
        process::{Command, Stdio},
        sync::{Mutex, MutexGuard},
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{MachineStatus, PathInputs, StateVersion};

    const FIXED_TIME: &str = "2026-08-28T09:00:00Z";
    const LOCK_HELPER_ENV: &str = "FIRESTONE_IMAGE_LOCK_HELPER";
    const LOCK_READY_ENV: &str = "FIRESTONE_IMAGE_LOCK_READY";
    const LOCK_RELEASE_ENV: &str = "FIRESTONE_IMAGE_LOCK_RELEASE";
    static IMAGE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> String {
            FIXED_TIME.to_owned()
        }
    }
    struct StaticClock(&'static str);

    impl Clock for StaticClock {
        fn now(&self) -> String {
            self.0.to_owned()
        }
    }

    #[derive(Clone)]
    struct FakeReply {
        bytes: Vec<u8>,
        content_length: Option<u64>,
        content_type: Option<String>,
    }

    #[derive(Default)]
    struct ScriptedHttp {
        replies: Mutex<BTreeMap<String, VecDeque<FakeReply>>>,
    }

    impl ScriptedHttp {
        fn push(
            &self,
            url: &str,
            bytes: Vec<u8>,
            content_length: Option<u64>,
            content_type: Option<&str>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let mut replies = self
                .replies
                .lock()
                .map_err(|_| io::Error::other("scripted HTTP mutex poisoned"))?;
            replies
                .entry(url.to_owned())
                .or_default()
                .push_back(FakeReply {
                    bytes,
                    content_length,
                    content_type: content_type.map(ToOwned::to_owned),
                });
            Ok(())
        }
    }

    impl HttpSource for ScriptedHttp {
        fn get(&self, url: &Url) -> Result<HttpResponse, FirestoneError> {
            let mut replies = self.replies.lock().map_err(|_| {
                FirestoneError::new(ErrorKind::Generic, "scripted HTTP mutex poisoned")
            })?;
            let reply = replies
                .get_mut(url.as_str())
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        format!("no scripted HTTP response for '{url}'"),
                    )
                })?;
            Ok(HttpResponse {
                body: Box::new(Cursor::new(reply.bytes)),
                content_length: reply.content_length,
                content_type: reply.content_type,
            })
        }
    }

    struct Fixture {
        _test_lock: MutexGuard<'static, ()>,
        _directory: TempDir,
        root: PathBuf,
        paths: Paths,
        qemu_log: PathBuf,
        http: Arc<ScriptedHttp>,
        store: ImageStore,
    }

    impl Fixture {
        fn new(fail_convert: bool) -> Result<Self, Box<dyn std::error::Error>> {
            let test_lock = IMAGE_TEST_LOCK
                .lock()
                .map_err(|_| io::Error::other("image test lock poisoned"))?;
            let directory = tempfile::tempdir()?;
            let root = fs::canonicalize(directory.path())?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
            let paths = test_paths(&root, root.join("firestone"))?;
            let qemu_img = root.join("qemu-img");
            let qemu_log = root.join("qemu.log");
            write_fake_qemu(&qemu_img, &qemu_log, fail_convert)?;
            let http = Arc::new(ScriptedHttp::default());
            let store = ImageStore {
                paths: paths.clone(),
                catalog: Catalog::built_in()?,
                architecture: Arch::X86_64,
                qemu_img,
                http: http.clone(),
                clock: Arc::new(FixedClock),
            };
            Ok(Self {
                _test_lock: test_lock,
                _directory: directory,
                root,
                paths,
                qemu_log,
                http,
                store,
            })
        }

        fn write_source(
            &self,
            name: &str,
            bytes: &[u8],
        ) -> Result<PathBuf, Box<dyn std::error::Error>> {
            let path = self.root.join(name);
            fs::write(&path, bytes)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok(path)
        }
    }

    fn test_paths(root: &Path, firestone_home: PathBuf) -> Result<Paths, FirestoneError> {
        Paths::from_inputs(&PathInputs {
            current_dir: root.to_path_buf(),
            home_dir: Some(root.to_path_buf()),
            firestone_home: Some(firestone_home),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: nix::unistd::getuid().as_raw(),
        })
    }

    fn write_fake_qemu(
        path: &Path,
        log: &Path,
        fail_convert: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let log_literal = serde_json::to_string(&log.to_string_lossy())?;
        let convert = if fail_convert {
            "sys.exit(17)"
        } else {
            "pathlib.Path(args[6]).write_bytes(bytes([81, 70, 73, 251]) + b'CONVERTED' + pathlib.Path(args[5]).read_bytes())"
        };
        let script = r#"#!/usr/bin/env python3
import json
import pathlib
import sys

args = sys.argv[1:]
log = pathlib.Path(__LOG__)
with log.open("a", encoding="utf-8") as output:
    output.write(" ".join(args) + "\n")

if args[0] == "convert":
    __CONVERT__
elif args[0] == "create":
    data = bytes([81, 70, 73, 251]) + b"OVERLAY\n" + args[6].encode() + b"\n" + args[8].encode() + b"\n"
    pathlib.Path(args[7]).write_bytes(data)
elif args[0] == "info":
    data = pathlib.Path(args[4]).read_bytes()
    info = {"format": "qcow2", "virtual-size": 4, "dirty-flag": False, "format-specific": {"type": "qcow2", "data": {"corrupt": False}}}
    if data[4:].startswith(b"OVERLAY\n"):
        lines = data[4:].splitlines()
        info["virtual-size"] = int(lines[2])
        info["backing-filename"] = lines[1].decode()
        info["backing-filename-format"] = "qcow2"
        info["full-backing-filename"] = lines[1].decode()
    print(json.dumps(info, separators=(",", ":")))
else:
    sys.exit(23)
"#
        .replace("__LOG__", &log_literal)
        .replace("__CONVERT__", convert);
        fs::write(path, script)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        digest_hex(Sha256::digest(bytes).as_slice())
    }

    fn sha512_bytes(bytes: &[u8]) -> String {
        digest_hex(Sha512::digest(bytes).as_slice())
    }

    fn url_request(url: &str, digest: Option<String>, source_base: &Path) -> ImagePullRequest {
        ImagePullRequest {
            image: ImageRef::new(url),
            sha256: digest,
            source_base: source_base.to_path_buf(),
        }
    }

    fn local_request(path: &Path, source_base: &Path) -> ImagePullRequest {
        ImagePullRequest::new(
            ImageRef::new(path.to_string_lossy().into_owned()),
            source_base,
        )
    }
    fn custom_catalog(
        root: &Path,
        label: &str,
        source: &str,
    ) -> Result<Catalog, Box<dyn std::error::Error>> {
        let path = root.join(format!("{label}.toml"));
        fs::write(&path, source)?;
        Ok(Catalog::load(&root.join("missing-catalog.toml"), &[path])?)
    }

    fn store_with_catalog(
        fixture: &Fixture,
        catalog: Catalog,
        clock: Arc<dyn Clock>,
    ) -> ImageStore {
        ImageStore {
            paths: fixture.paths.clone(),
            catalog,
            architecture: Arch::X86_64,
            qemu_img: fixture.store.qemu_img.clone(),
            http: fixture.http.clone(),
            clock,
        }
    }

    fn assert_no_image_artifacts(paths: &Paths) -> Result<(), Box<dyn std::error::Error>> {
        let entries = fs::read_dir(paths.images_dir())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        let offending = entries
            .into_iter()
            .filter_map(|name| name.into_string().ok())
            .filter(|name| {
                name.ends_with(".partial") || name.ends_with(".qcow2") || name.ends_with(".json")
            })
            .collect::<Vec<_>>();
        assert!(
            offending.is_empty(),
            "unexpected image artifacts: {offending:?}"
        );
        Ok(())
    }

    fn machine_state(
        paths: &Paths,
        name: &str,
        image: StateImage,
    ) -> Result<MachineState, FirestoneError> {
        Ok(MachineState {
            version: StateVersion,
            status: MachineStatus::Created,
            image,
            mac: None,
            cid: 3,
            instance_id: None,
            shim_pid: None,
            vmm_pid: None,
            sidecar_pids: BTreeMap::new(),
            runtime_dir: paths.machine_runtime_dir(name)?,
            started_at: None,
            forwards: Vec::new(),
            degraded: Vec::new(),
            last_exit: None,
        })
    }

    fn create_machine(
        paths: &Paths,
        name: &str,
        state: &MachineState,
        events: &mut Vec<Event>,
    ) -> Result<MachineLock, FirestoneError> {
        paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
        paths.ensure_owned_data_directory(&paths.machines_dir(), "machines directory", false)?;
        paths.ensure_owned_data_directory(&paths.machine_dir(name)?, "machine directory", false)?;
        let lock = MachineLock::acquire(name, &paths.machine_lock(name)?, events)?;
        StateStore::new(paths.machine_state(name)?).write_from_locked_action(state, &lock)?;
        Ok(lock)
    }

    #[test]
    fn image_sidecar_verified_url_has_exact_version_one_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let bytes = b"QFI\xFBDATA".to_vec();
        let source_sha256 = sha256_bytes(&bytes);
        let url = "https://images.example.invalid/base.qcow2";
        fixture.http.push(
            url,
            bytes.clone(),
            Some(bytes.len() as u64),
            Some("application/octet-stream"),
        )?;
        let mut events = Vec::new();
        let pulled = fixture.store.pull(
            &url_request(url, Some(source_sha256.clone()), &fixture.root),
            &mut events,
        )?;
        let id = stable_image_id(url, Some(url), Arch::X86_64, &source_sha256);
        assert_eq!(pulled.metadata.id, id);
        assert_eq!(id.len(), IMAGE_ID_PREFIX.len() + IMAGE_ID_HEX_LENGTH);
        assert_eq!(pulled.metadata.source_sha256, source_sha256);
        assert_eq!(pulled.metadata.stored_sha256, source_sha256);
        assert_eq!(pulled.metadata.source_format, ImageFormat::Qcow2);
        assert_eq!(pulled.metadata.stored_format, ImageFormat::Qcow2);

        let actual = fs::read_to_string(fixture.paths.image_metadata(&id)?)?;
        let mut expected = format!(
            r#"{{
  "version": 1,
  "id": "{id}",
  "generation": 1,
  "source_ref": "{url}",
  "source_url": "{url}",
  "source_sha256": "{source_sha256}",
  "stored_sha256": "{source_sha256}",
  "architecture": "x86_64",
  "firmware": null,
  "source_format": "qcow2",
  "stored_format": "qcow2",
  "verification_algorithm": "sha256",
  "verification_digest": "{source_sha256}",
  "size": 8,
  "pulled_at": "{time}"
}}"#,
            id = id,
            url = url,
            source_sha256 = source_sha256,
            time = FIXED_TIME,
        );
        expected.push(char::from(10));
        assert_eq!(actual.as_bytes(), expected.as_bytes());
        assert_eq!(
            fs::symlink_metadata(fixture.paths.image_base(&id)?)?
                .permissions()
                .mode()
                & 0o7777,
            BASE_FILE_MODE
        );
        assert_eq!(
            fs::symlink_metadata(fixture.paths.image_metadata(&id)?)?
                .permissions()
                .mode()
                & 0o7777,
            SIDECAR_FILE_MODE
        );
        let inspected = fixture.store.inspect(&id)?;
        assert_eq!(inspected.image.metadata.id, id);
        assert_eq!(inspected.virtual_size, 4);
        Ok(())
    }

    #[test]
    fn image_pull_corrupted_checksum_removes_every_partial()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let bytes = b"QFI\xFBCORRUPT".to_vec();
        let url = "https://images.example.invalid/corrupt.qcow2";
        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let mut events = Vec::new();
        let error = fixture
            .store
            .pull(
                &url_request(url, Some("0".repeat(64)), &fixture.root),
                &mut events,
            )
            .err()
            .ok_or("expected checksum error")?;
        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert_no_image_artifacts(&fixture.paths)
    }

    #[test]
    fn image_pull_partial_http_body_removes_every_partial() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new(false)?;
        let bytes = b"QFI\xFBSHORT".to_vec();
        let url = "https://images.example.invalid/partial.qcow2";
        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64 + 5), None)?;
        let mut events = Vec::new();
        let error = fixture
            .store
            .pull(
                &url_request(url, Some(sha256_bytes(&bytes)), &fixture.root),
                &mut events,
            )
            .err()
            .ok_or("expected partial response error")?;
        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert!(error.message().contains("Content-Length"));
        assert_no_image_artifacts(&fixture.paths)
    }

    #[test]
    fn image_pull_unverified_url_warns_once_and_records_no_verifier()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let bytes = b"QFI\xFBUNCHECKED".to_vec();
        let url = "https://images.example.invalid/unchecked.qcow2";
        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let mut events = Vec::new();
        let pulled = fixture
            .store
            .pull(&url_request(url, None, &fixture.root), &mut events)?;
        assert_eq!(pulled.metadata.verification_algorithm, None);
        assert_eq!(pulled.metadata.verification_digest, None);
        let warnings = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::Log {
                        level: Level::Warn,
                        message,
                    } if message.contains("--sha256")
                )
            })
            .count();
        assert_eq!(warnings, 1);
        Ok(())
    }

    #[test]
    fn checksum_manifest_conflict_and_missing_filename_return_checksum()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = "1".repeat(64);
        let second = "2".repeat(64);
        let ambiguous = format!(
            "{first}  image.qcow2
{second} *image.qcow2
{first}  other.qcow2
"
        );
        let conflict =
            parse_checksum_manifest(&ambiguous, ChecksumAlgorithm::Sha256, "image.qcow2")
                .err()
                .ok_or("expected ambiguous manifest error")?;
        assert_eq!(conflict.kind(), ErrorKind::Checksum);
        assert!(conflict.message().contains("conflicting"));

        let missing = parse_checksum_manifest(
            &format!(
                "{first}  other.qcow2
"
            ),
            ChecksumAlgorithm::Sha256,
            "image.qcow2",
        )
        .err()
        .ok_or("expected missing manifest error")?;
        assert_eq!(missing.kind(), ErrorKind::Checksum);
        assert!(missing.message().contains("no entry"));

        let duplicate = parse_checksum_manifest(
            &format!(
                "{first}  image.qcow2
{first} *./image.qcow2
"
            ),
            ChecksumAlgorithm::Sha256,
            "image.qcow2",
        )?;
        assert_eq!(duplicate, first);
        let bsd = parse_checksum_manifest(
            &format!("SHA256 (image.qcow2) = {first}\n"),
            ChecksumAlgorithm::Sha256,
            "image.qcow2",
        )?;
        assert_eq!(bsd, first);
        let non_ascii =
            parse_checksum_manifest("éééééééé", ChecksumAlgorithm::Sha256, "image.qcow2")
                .err()
                .ok_or("expected non-ASCII manifest miss")?;
        assert_eq!(non_ascii.kind(), ErrorKind::Checksum);
        Ok(())
    }

    #[test]
    fn checksum_manifest_sha512_entry_verifies_exact_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let bytes = b"QFI\xFBDEBIAN".to_vec();
        let digest = sha512_bytes(&bytes);
        let image_url = "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2";
        let manifest_url = "https://cloud.debian.org/images/cloud/bookworm/latest/SHA512SUMS";
        fixture.http.push(
            manifest_url,
            format!(
                "{digest}  debian-12-genericcloud-amd64.qcow2
"
            )
            .into_bytes(),
            None,
            Some("text/plain"),
        )?;
        fixture
            .http
            .push(image_url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let mut events = Vec::new();
        let pulled = fixture.store.pull(
            &ImagePullRequest::new(ImageRef::new("debian:12"), &fixture.root),
            &mut events,
        )?;
        assert_eq!(
            pulled.metadata.verification_algorithm,
            Some(ChecksumAlgorithm::Sha512)
        );
        assert_eq!(pulled.metadata.verification_digest, Some(digest));
        assert_eq!(pulled.metadata.source_sha256, sha256_bytes(&bytes));
        Ok(())
    }

    #[test]
    fn checksum_manifest_oversize_is_rejected_before_unbounded_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://images.example.invalid/SHA256SUMS";
        fixture.http.push(
            url,
            vec![b'x'; MAX_MANIFEST_BYTES as usize + 1],
            None,
            Some("text/plain"),
        )?;
        let parsed = parse_https_url(url).ok_or("invalid test URL")?;
        let error = fixture
            .store
            .fetch_manifest(&parsed)
            .err()
            .ok_or("expected oversized manifest error")?;
        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert!(error.message().contains("exceeds"));
        Ok(())
    }

    #[test]
    fn raw_source_conversion_preserves_source_and_stored_identities_and_exact_argv()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let raw = b"raw-root-filesystem";
        let source = fixture.write_source("root.raw", raw)?;
        let request = local_request(&source, &fixture.root);
        let resolved = fixture
            .store
            .resolve(&request.image, None, &request.source_base)?;
        let operation = operation_key(&resolved);
        let mut events = Vec::new();
        let pulled = fixture.store.pull(&request, &mut events)?;
        assert_eq!(pulled.metadata.source_format, ImageFormat::Raw);
        assert_eq!(pulled.metadata.source_sha256, sha256_bytes(raw));
        assert_ne!(pulled.metadata.source_sha256, pulled.metadata.stored_sha256);
        assert_eq!(
            pulled.metadata.id,
            stable_image_id(
                &source.to_string_lossy(),
                None,
                Arch::X86_64,
                &sha256_bytes(raw)
            )
        );

        let log = fs::read_to_string(&fixture.qemu_log)?;
        let expected = format!(
            "convert -f raw -O qcow2 {} {}",
            fixture.paths.image_source_partial(&operation)?.display(),
            fixture.paths.image_stored_partial(&operation)?.display()
        );
        assert!(log.lines().any(|line| line == expected));
        let expected_info = format!(
            "info --output=json -f qcow2 {}",
            fixture.paths.image_stored_partial(&operation)?.display()
        );
        assert!(log.lines().any(|line| line == expected_info));
        assert!(!fixture.paths.image_source_partial(&operation)?.exists());
        assert!(!fixture.paths.image_stored_partial(&operation)?.exists());
        fs::remove_file(&source)?;
        let owned = fixture.store.inspect(&pulled.metadata.id)?;
        assert_eq!(owned.image.metadata.source_sha256, sha256_bytes(raw));
        Ok(())
    }

    #[test]
    fn raw_conversion_failure_removes_source_and_conversion_partials()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(true)?;
        let source = fixture.write_source("broken.raw", b"raw")?;
        let mut events = Vec::new();
        let error = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut events)
            .err()
            .ok_or("expected conversion error")?;
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_no_image_artifacts(&fixture.paths)
    }

    #[test]
    fn machine_prepare_pins_source_sha_before_exact_overlay_and_inspects_backing()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source_bytes = b"QFI\xFBBASE";
        let source = fixture.write_source("base.qcow2", source_bytes)?;
        let name = "demo";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: source.to_string_lossy().into_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let mut lock_events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut lock_events)?;
        let mut events = Vec::new();
        let disk_size = ByteSize::from_mib(1)?;
        let prepared = fixture.store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            disk_size,
            &lock,
            &mut events,
        )?;
        assert_eq!(
            state.image.id.as_deref(),
            Some(prepared.image.metadata.id.as_str())
        );
        assert_eq!(
            state.image.sha256.as_deref(),
            Some(sha256_bytes(source_bytes).as_str())
        );
        let persisted = StateStore::new(fixture.paths.machine_state(name)?).read()?;
        assert_eq!(persisted.image, state.image);
        assert_eq!(prepared.overlay.backing_path, prepared.image.path);
        assert!(prepared.overlay.path.exists());

        let log = fs::read_to_string(&fixture.qemu_log)?;
        let expected_create = format!(
            "create -f qcow2 -F qcow2 -b {} {} {}",
            prepared.image.path.display(),
            fixture.paths.machine_disk_partial(name)?.display(),
            disk_size.as_bytes()
        );
        assert!(log.lines().any(|line| line == expected_create));
        let expected_info = format!(
            "info --output=json -f qcow2 {}",
            fixture.paths.machine_disk_partial(name)?.display()
        );
        assert!(log.lines().any(|line| line == expected_info));
        Ok(())
    }

    #[test]
    fn machine_prepare_changed_catalog_override_pulls_new_identity_instead_of_old_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let old_bytes = b"QFI\xFBOLD-UBUNTU".to_vec();
        let old_sha256 = sha256_bytes(&old_bytes);
        let old_image_url =
            "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img";
        let old_manifest_url = "https://cloud-images.ubuntu.com/noble/current/SHA256SUMS";
        fixture.http.push(
            old_manifest_url,
            format!("{old_sha256}  noble-server-cloudimg-amd64.img\n").into_bytes(),
            None,
            Some("text/plain"),
        )?;
        fixture.http.push(
            old_image_url,
            old_bytes.clone(),
            Some(old_bytes.len() as u64),
            None,
        )?;
        let old = fixture.store.pull(
            &ImagePullRequest::new(ImageRef::new("ubuntu:24.04"), &fixture.root),
            &mut Vec::new(),
        )?;

        let replacement_url = "https://override.example.invalid/ubuntu-24.04.raw";
        let replacement_bytes = b"replacement raw source".to_vec();
        let replacement_sha256 = sha256_bytes(&replacement_bytes);
        let override_path = fixture.root.join("override.toml");
        fs::write(
            &override_path,
            format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"ubuntu\"\n",
                    "version = \"24.04\"\n",
                    "aliases = [\"noble\"]\n",
                    "default = true\n",
                    "firmware = \"edk2\"\n",
                    "format = \"raw\"\n\n",
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "sha256 = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                replacement_url, replacement_sha256,
            ),
        )?;
        fixture.http.push(
            replacement_url,
            replacement_bytes.clone(),
            Some(replacement_bytes.len() as u64),
            None,
        )?;
        let overridden = ImageStore {
            paths: fixture.paths.clone(),
            catalog: Catalog::load(&fixture.root.join("missing-catalog.toml"), &[override_path])?,
            architecture: Arch::X86_64,
            qemu_img: fixture.store.qemu_img.clone(),
            http: fixture.http.clone(),
            clock: Arc::new(FixedClock),
        };
        let name = "changed-override";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: "ubuntu:24.04".to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let mut lock_events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut lock_events)?;
        let prepared = overridden.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert!(!prepared.image.cached);
        assert_ne!(prepared.image.metadata.id, old.metadata.id);
        assert_eq!(
            prepared.image.metadata.source_url.as_deref(),
            Some(replacement_url)
        );
        assert_eq!(prepared.image.metadata.source_format, ImageFormat::Raw);
        assert_eq!(
            prepared.image.metadata.verification_digest.as_deref(),
            Some(replacement_sha256.as_str())
        );
        assert_eq!(
            state.image.id.as_deref(),
            Some(prepared.image.metadata.id.as_str())
        );
        Ok(())
    }

    #[test]
    fn machine_prepare_alias_uses_canonical_cache_without_manifest_refresh()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let bytes = b"QFI\xFBCACHED-DEBIAN".to_vec();
        let digest = sha512_bytes(&bytes);
        let image_url = "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2";
        let manifest_url = "https://cloud.debian.org/images/cloud/bookworm/latest/SHA512SUMS";
        fixture.http.push(
            manifest_url,
            format!("{digest}  debian-12-genericcloud-amd64.qcow2\n").into_bytes(),
            None,
            Some("text/plain"),
        )?;
        fixture
            .http
            .push(image_url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let pulled = fixture.store.pull(
            &ImagePullRequest::new(ImageRef::new("debian:12"), &fixture.root),
            &mut Vec::new(),
        )?;

        let name = "alias-cache";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: "debian:bookworm".to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let mut lock_events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut lock_events)?;
        let prepared = fixture.store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert!(prepared.image.cached);
        assert_eq!(prepared.image.metadata.id, pulled.metadata.id);
        assert_eq!(state.image.r#ref, "debian:12");
        assert_eq!(prepared.image.firmware, Some(CatalogFirmware::Rhf));
        Ok(())
    }

    #[test]
    fn machine_overlay_wrong_architecture_and_source_ref_are_rejected_before_qemu()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("identity.qcow2", b"QFI\xFBIDENTITY")?;
        let pulled = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut Vec::new())?;
        let pinned = StateImage {
            r#ref: pulled.metadata.source_ref.clone(),
            id: Some(pulled.metadata.id.clone()),
            sha256: Some(pulled.metadata.source_sha256.clone()),
        };

        let arch_name = "wrong-arch";
        let arch_state = machine_state(&fixture.paths, arch_name, pinned.clone())?;
        let mut arch_events = Vec::new();
        let arch_lock = create_machine(&fixture.paths, arch_name, &arch_state, &mut arch_events)?;
        fs::write(&fixture.qemu_log, b"")?;
        let aarch_store = ImageStore {
            paths: fixture.paths.clone(),
            catalog: Catalog::built_in()?,
            architecture: Arch::Aarch64,
            qemu_img: fixture.store.qemu_img.clone(),
            http: fixture.http.clone(),
            clock: Arc::new(FixedClock),
        };
        let architecture_error = aarch_store
            .create_overlay(arch_name, &pinned, ByteSize::from_mib(1)?, &arch_lock)
            .err()
            .ok_or("expected cross-architecture rejection")?;
        assert_eq!(architecture_error.kind(), ErrorKind::InvalidSpec);
        assert!(fs::read_to_string(&fixture.qemu_log)?.is_empty());

        let ref_name = "wrong-ref";
        let stale = StateImage {
            r#ref: fixture
                .root
                .join("other.qcow2")
                .to_string_lossy()
                .into_owned(),
            ..pinned
        };
        let ref_state = machine_state(&fixture.paths, ref_name, stale.clone())?;
        let mut ref_events = Vec::new();
        let ref_lock = create_machine(&fixture.paths, ref_name, &ref_state, &mut ref_events)?;
        let reference_error = fixture
            .store
            .create_overlay(ref_name, &stale, ByteSize::from_mib(1)?, &ref_lock)
            .err()
            .ok_or("expected stale reference rejection")?;
        assert_eq!(reference_error.kind(), ErrorKind::Conflict);
        assert!(fs::read_to_string(&fixture.qemu_log)?.is_empty());
        Ok(())
    }

    #[test]
    fn machine_overlay_same_size_corruption_fails_before_qemu()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("corrupted-base.qcow2", b"QFI\xFBORIGINAL")?;
        let pulled = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut Vec::new())?;
        let base = fixture.paths.image_base(&pulled.metadata.id)?;
        let mut corrupted = fs::read(&base)?;
        let last = corrupted.last_mut().ok_or("empty test base")?;
        *last ^= 0xff;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o600))?;
        fs::write(&base, corrupted)?;
        fs::set_permissions(&base, fs::Permissions::from_mode(BASE_FILE_MODE))?;

        let name = "corrupt-base";
        let image = StateImage {
            r#ref: pulled.metadata.source_ref.clone(),
            id: Some(pulled.metadata.id.clone()),
            sha256: Some(pulled.metadata.source_sha256.clone()),
        };
        let state = machine_state(&fixture.paths, name, image.clone())?;
        let mut lock_events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut lock_events)?;
        fs::write(&fixture.qemu_log, b"")?;
        let error = fixture
            .store
            .create_overlay(name, &image, ByteSize::from_mib(1)?, &lock)
            .err()
            .ok_or("expected corrupted stored image rejection")?;
        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert!(fs::read_to_string(&fixture.qemu_log)?.is_empty());
        Ok(())
    }

    #[test]
    fn image_removal_tombstones_recover_each_interruption_boundary()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let first_source = fixture.write_source("remove-one.qcow2", b"QFI\xFBREMOVE-ONE")?;
        let first = fixture.store.pull(
            &local_request(&first_source, &fixture.root),
            &mut Vec::new(),
        )?;
        let first_sidecar = fixture.paths.image_metadata(&first.metadata.id)?;
        let first_sidecar_tombstone = fixture.paths.image_metadata_removal(&first.metadata.id)?;
        fs::rename(&first_sidecar, &first_sidecar_tombstone)?;
        let recovered = fixture.store.prune()?;
        assert!(recovered.removed.is_empty());
        assert!(!fixture.paths.image_base(&first.metadata.id)?.exists());
        assert!(!first_sidecar_tombstone.exists());

        let second_source = fixture.write_source("remove-two.qcow2", b"QFI\xFBREMOVE-TWO")?;
        let second_request = local_request(&second_source, &fixture.root);
        let second = fixture.store.pull(&second_request, &mut Vec::new())?;
        let second_base = fixture.paths.image_base(&second.metadata.id)?;
        let second_sidecar = fixture.paths.image_metadata(&second.metadata.id)?;
        let second_base_tombstone = fixture.paths.image_base_removal(&second.metadata.id)?;
        let second_sidecar_tombstone = fixture.paths.image_metadata_removal(&second.metadata.id)?;
        fs::rename(&second_sidecar, &second_sidecar_tombstone)?;
        fs::rename(&second_base, &second_base_tombstone)?;
        let repulled = fixture.store.pull(&second_request, &mut Vec::new())?;
        assert_eq!(repulled.metadata.id, second.metadata.id);
        assert!(second_base.exists());
        assert!(second_sidecar.exists());
        assert!(!second_base_tombstone.exists());
        assert!(!second_sidecar_tombstone.exists());
        Ok(())
    }

    #[test]
    fn image_reference_refuses_remove_force_removes_and_prune_keeps_reference()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let first_source = fixture.write_source("first.qcow2", b"QFI\xFBFIRST")?;
        let second_source = fixture.write_source("second.qcow2", b"QFI\xFBSECOND")?;
        let mut events = Vec::new();
        let first = fixture
            .store
            .pull(&local_request(&first_source, &fixture.root), &mut events)?;
        let second = fixture
            .store
            .pull(&local_request(&second_source, &fixture.root), &mut events)?;

        let state = machine_state(
            &fixture.paths,
            "kept",
            StateImage {
                r#ref: first_source.to_string_lossy().into_owned(),
                id: Some(first.metadata.id.clone()),
                sha256: Some(first.metadata.source_sha256.clone()),
            },
        )?;
        let mut lock_events = Vec::new();
        let _lock = create_machine(&fixture.paths, "kept", &state, &mut lock_events)?;

        let pruned = fixture.store.prune()?;
        assert_eq!(pruned.removed, vec![second.metadata.id.clone()]);
        assert!(fixture.paths.image_base(&first.metadata.id)?.exists());
        assert!(!fixture.paths.image_base(&second.metadata.id)?.exists());

        let refusal = fixture
            .store
            .remove(&first.metadata.id, false)
            .err()
            .ok_or("expected referenced image refusal")?;
        assert_eq!(refusal.kind(), ErrorKind::Conflict);
        assert!(refusal.message().contains("kept"));
        let forced = fixture.store.remove(&first.metadata.id, true)?;
        assert_eq!(forced.referenced_by, vec!["kept"]);
        assert!(!fixture.paths.image_base(&first.metadata.id)?.exists());
        assert!(!fixture.paths.image_metadata(&first.metadata.id)?.exists());
        Ok(())
    }

    #[test]
    fn image_store_lock_contention_returns_busy() -> Result<(), Box<dyn std::error::Error>> {
        if env::var_os(LOCK_HELPER_ENV).is_some() {
            return Ok(());
        }
        let fixture = Fixture::new(false)?;
        fixture.store.ensure_store()?;
        drop(ImageStoreLock::acquire(
            &fixture.paths,
            Duration::from_secs(1),
            Duration::from_millis(5),
        )?);
        let ready = fixture.root.join("lock-ready");
        let release = fixture.root.join("lock-release");
        let current = env::current_exe()?;
        let mut child = Command::new(current)
            .arg("--exact")
            .arg("image::tests::image_store_lock_helper_process")
            .arg("--nocapture")
            .env(LOCK_HELPER_ENV, fixture.paths.image_store_lock()?)
            .env(LOCK_READY_ENV, &ready)
            .env(LOCK_RELEASE_ENV, &release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        wait_for_path(&ready)?;
        let error = ImageStoreLock::acquire(
            &fixture.paths,
            Duration::from_millis(80),
            Duration::from_millis(5),
        )
        .err()
        .ok_or("expected image store contention")?;
        assert_eq!(error.kind(), ErrorKind::Busy);
        fs::write(&release, b"release")?;
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn image_store_lock_helper_process() -> Result<(), Box<dyn std::error::Error>> {
        let Some(lock_path) = env::var_os(LOCK_HELPER_ENV).map(PathBuf::from) else {
            return Ok(());
        };
        let ready = PathBuf::from(env::var_os(LOCK_READY_ENV).ok_or("missing ready path")?);
        let release = PathBuf::from(env::var_os(LOCK_RELEASE_ENV).ok_or("missing release path")?);
        let file = OpenOptions::new().read(true).write(true).open(lock_path)?;
        let _lock = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, error)| io::Error::from(error))?;
        fs::write(ready, b"ready")?;
        wait_for_path(&release)?;
        Ok(())
    }

    fn wait_for_path(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if path.exists() {
            Ok(())
        } else {
            Err(format!("timed out waiting for '{}'", path.display()).into())
        }
    }

    #[test]
    fn image_storage_symlink_mode_and_ancestry_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        fixture.paths.ensure_owned_data_directory(
            fixture.paths.data_dir(),
            "data directory",
            true,
        )?;
        let outside = fixture.root.join("outside-images");
        fs::create_dir(&outside)?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700))?;
        symlink(&outside, fixture.paths.images_dir())?;
        let source = fixture.write_source("source.qcow2", b"QFI\xFBSOURCE")?;
        let mut events = Vec::new();
        let symlink_error = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut events)
            .err()
            .ok_or("expected symlinked images directory rejection")?;
        assert_eq!(symlink_error.kind(), ErrorKind::Dependency);

        drop(fixture);
        let mode_fixture = Fixture::new(false)?;
        let mode_source = mode_fixture.write_source("mode.qcow2", b"QFI\xFBMODE")?;
        let pulled = mode_fixture.store.pull(
            &local_request(&mode_source, &mode_fixture.root),
            &mut Vec::new(),
        )?;
        fs::set_permissions(
            mode_fixture.paths.image_metadata(&pulled.metadata.id)?,
            fs::Permissions::from_mode(0o644),
        )?;
        let mode_error = mode_fixture
            .store
            .list()
            .err()
            .ok_or("expected permissive sidecar mode rejection")?;
        assert_eq!(mode_error.kind(), ErrorKind::Dependency);

        let ancestry_directory = tempfile::tempdir()?;
        let ancestry_root = fs::canonicalize(ancestry_directory.path())?;
        fs::set_permissions(&ancestry_root, fs::Permissions::from_mode(0o700))?;
        let safe = ancestry_root.join("safe");
        fs::create_dir(&safe)?;
        fs::set_permissions(&safe, fs::Permissions::from_mode(0o700))?;
        let linked = ancestry_root.join("linked");
        symlink(&safe, &linked)?;
        let paths = test_paths(&ancestry_root, linked.join("home"))?;
        let store = ImageStore {
            paths,
            catalog: Catalog::built_in()?,
            architecture: Arch::X86_64,
            qemu_img: PathBuf::from("/bin/false"),
            http: Arc::new(ScriptedHttp::default()),
            clock: Arc::new(FixedClock),
        };
        let ancestry_error = store
            .list()
            .err()
            .ok_or("expected symlink ancestry rejection")?;
        assert_eq!(ancestry_error.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[test]
    fn local_image_symlink_and_writable_mode_are_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new(false)?;
        let target = fixture.write_source("target.qcow2", b"QFI\xFBTARGET")?;
        let link = fixture.root.join("linked.qcow2");
        symlink(&target, &link)?;
        let link_error = fixture
            .store
            .pull(&local_request(&link, &fixture.root), &mut Vec::new())
            .err()
            .ok_or("expected local symlink rejection")?;
        assert_eq!(link_error.kind(), ErrorKind::InvalidSpec);

        let writable = fixture.write_source("writable.qcow2", b"QFI\xFBWRITE")?;
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o622))?;
        let mode_error = fixture
            .store
            .pull(&local_request(&writable, &fixture.root), &mut Vec::new())
            .err()
            .ok_or("expected writable local source rejection")?;
        assert_eq!(mode_error.kind(), ErrorKind::Dependency);
        assert_no_image_artifacts(&fixture.paths)
    }

    #[test]
    fn image_sidecar_unknown_version_and_field_are_rejected_strictly()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("strict.qcow2", b"QFI\xFBSTRICT")?;
        let pulled = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut Vec::new())?;
        let sidecar = fixture.paths.image_metadata(&pulled.metadata.id)?;
        let mut version = serde_json::to_value(&pulled.metadata)?;
        version["version"] = serde_json::Value::from(2);
        atomic::write_json_with_mode(&sidecar, &version, SIDECAR_FILE_MODE)?;
        let version_error = fixture
            .store
            .list()
            .err()
            .ok_or("expected unknown sidecar version rejection")?;
        assert_eq!(version_error.kind(), ErrorKind::Dependency);

        let mut value = serde_json::to_value(&pulled.metadata)?;
        value
            .as_object_mut()
            .ok_or("sidecar JSON was not an object")?
            .insert("extra".to_owned(), serde_json::Value::Bool(true));
        atomic::write_json_with_mode(&sidecar, &value, SIDECAR_FILE_MODE)?;
        let error = fixture
            .store
            .list()
            .err()
            .ok_or("expected unknown sidecar field rejection")?;
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("cannot parse"));
        let pristine = serde_json::to_value(&pulled.metadata)?;
        for key in [
            "version",
            "id",
            "generation",
            "source_ref",
            "source_url",
            "source_sha256",
            "stored_sha256",
            "architecture",
            "firmware",
            "source_format",
            "stored_format",
            "verification_algorithm",
            "verification_digest",
            "size",
            "pulled_at",
        ] {
            let mut missing = pristine.clone();
            missing
                .as_object_mut()
                .ok_or("sidecar JSON was not an object")?
                .remove(key);
            atomic::write_json_with_mode(&sidecar, &missing, SIDECAR_FILE_MODE)?;
            let missing_error = fixture
                .store
                .list()
                .err()
                .ok_or_else(|| format!("expected missing sidecar key '{key}' rejection"))?;
            assert_eq!(missing_error.kind(), ErrorKind::Dependency);
        }
        for key in [
            "source_url",
            "firmware",
            "verification_algorithm",
            "verification_digest",
        ] {
            assert!(pristine.get(key).is_some_and(serde_json::Value::is_null));
        }
        let encoded = serde_json::to_vec(&pulled.metadata)?;
        let round_trip: ImageMetadata = serde_json::from_slice(&encoded)?;
        assert_eq!(round_trip, pulled.metadata);
        Ok(())
    }
    #[test]
    fn redirect_policy_allows_five_and_rejects_six() {
        assert!(!redirect_limit_exceeded(5));
        assert!(redirect_limit_exceeded(6));
    }

    #[test]
    fn manifest_parser_handles_multibyte_digest_boundaries_without_panicking() {
        let sha256_boundary = format!("{}é  image.qcow2", "a".repeat(63));
        assert!(
            parse_checksum_manifest(&sha256_boundary, ChecksumAlgorithm::Sha256, "image.qcow2",)
                .is_err()
        );
        let sha512_boundary = format!("{}é  image.qcow2", "a".repeat(127));
        assert!(
            parse_checksum_manifest(&sha512_boundary, ChecksumAlgorithm::Sha512, "image.qcow2",)
                .is_err()
        );
        assert!(
            parse_checksum_manifest(
                "éSHA256 (image.qcow2) = bad",
                ChecksumAlgorithm::Sha256,
                "image.qcow2"
            )
            .is_err()
        );
    }

    #[test]
    fn qemu_validation_rejects_external_dirty_corrupt_and_tampered_dependencies()
    -> Result<(), Box<dyn std::error::Error>> {
        let healthy = || QemuInfo {
            format: "qcow2".to_owned(),
            virtual_size: 4096,
            backing_filename: None,
            backing_filename_format: None,
            full_backing_filename: None,
            dirty_flag: Some(false),
            corrupt: Some(false),
            data_file: None,
            data_file_raw: None,
        };
        let mut dirty = healthy();
        dirty.dirty_flag = Some(true);
        assert_eq!(
            validate_base_info("dirty", &dirty)
                .err()
                .ok_or("dirty accepted")?
                .kind(),
            ErrorKind::Dependency
        );
        let mut corrupt = healthy();
        corrupt.corrupt = Some(true);
        assert_eq!(
            validate_base_info("corrupt", &corrupt)
                .err()
                .ok_or("corrupt accepted")?
                .kind(),
            ErrorKind::Dependency
        );
        let mut external = healthy();
        external.data_file = Some("outside.raw".to_owned());
        assert_eq!(
            validate_base_info("external", &external)
                .err()
                .ok_or("external data accepted")?
                .kind(),
            ErrorKind::Dependency
        );
        let mut raw_external = healthy();
        raw_external.data_file_raw = Some(true);
        assert!(validate_base_info("external-raw", &raw_external).is_err());

        let base = Path::new("/owned/images/base.qcow2");
        let overlay = Path::new("/owned/machines/demo/disk.qcow2");
        let mut valid_overlay = healthy();
        valid_overlay.backing_filename = Some(base.to_string_lossy().into_owned());
        valid_overlay.full_backing_filename = Some(base.to_string_lossy().into_owned());
        valid_overlay.backing_filename_format = Some("qcow2".to_owned());
        validate_overlay_info(overlay, base, 4096, &valid_overlay)?;
        let mut wrong_path = valid_overlay.clone();
        wrong_path.full_backing_filename = Some("/other/base.qcow2".to_owned());
        assert!(validate_overlay_info(overlay, base, 4096, &wrong_path).is_err());
        let mut wrong_format = valid_overlay;
        wrong_format.backing_filename_format = Some("raw".to_owned());
        assert!(validate_overlay_info(overlay, base, 4096, &wrong_format).is_err());

        let hidden_top =
            serde_json::json!({"format":"qcow2","virtual-size":4,"backing_file":"outside"});
        assert!(reject_hidden_qemu_dependencies(&hidden_top, overlay).is_err());
        let hidden_data = serde_json::json!({
            "format-specific": {"type":"qcow2","data":{"data_file":"outside"}}
        });
        assert!(parse_qcow2_format_specific(&hidden_data, overlay).is_err());
        assert!(parse_qcow2_format_specific(&serde_json::json!({}), overlay).is_err());
        Ok(())
    }

    #[test]
    fn local_source_descriptor_dedupes_aliases_and_detects_swap_and_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source_dir = fixture.root.join("sources");
        fs::create_dir(&source_dir)?;
        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o700))?;
        let source = source_dir.join("base.qcow2");
        fs::write(&source, b"QFI\xFBORIGINAL")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        let aliased = source_dir.join("..").join("sources").join("base.qcow2");
        let first = fixture
            .store
            .pull(&local_request(&aliased, &fixture.root), &mut Vec::new())?;
        let second = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut Vec::new())?;
        assert_eq!(first.metadata.id, second.metadata.id);
        assert_eq!(first.metadata.source_ref, source.to_string_lossy());

        fixture.store.ensure_store()?;
        {
            let _lock = fixture.store.acquire_lock()?;
            let held_path = fixture.write_source("held.qcow2", b"QFI\xFBHELD")?;
            let resolved = fixture.store.resolve(
                &ImageRef::new(held_path.to_string_lossy().into_owned()),
                None,
                &fixture.root,
            )?;
            let moved = fixture.root.join("held-original.qcow2");
            fs::rename(&held_path, &moved)?;
            fs::write(&held_path, b"QFI\xFBREPLACEMENT")?;
            fs::set_permissions(&held_path, fs::Permissions::from_mode(0o600))?;
            let error = fixture
                .store
                .pull_locked(resolved, &mut Vec::new())
                .err()
                .ok_or("expected pathname swap rejection")?;
            assert_eq!(error.kind(), ErrorKind::Checksum);
        }
        {
            let _lock = fixture.store.acquire_lock()?;
            let changing = fixture.write_source("changing.qcow2", b"QFI\xFBSTART")?;
            let resolved = fixture.store.resolve(
                &ImageRef::new(changing.to_string_lossy().into_owned()),
                None,
                &fixture.root,
            )?;
            OpenOptions::new()
                .append(true)
                .open(&changing)?
                .write_all(b"changed")?;
            let error = fixture
                .store
                .pull_locked(resolved, &mut Vec::new())
                .err()
                .ok_or("expected mutation rejection")?;
            assert_eq!(error.kind(), ErrorKind::Generic);
        }

        let fifo = fixture.root.join("source.fifo");
        nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600))?;
        let fifo_error = fixture
            .store
            .resolve(
                &ImageRef::new(fifo.to_string_lossy().into_owned()),
                None,
                &fixture.root,
            )
            .err()
            .ok_or("expected FIFO rejection")?;
        assert_eq!(fifo_error.kind(), ErrorKind::InvalidSpec);
        Ok(())
    }

    #[test]
    fn oversized_sidecar_is_rejected_by_descriptor_bounded_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("bounded.qcow2", b"QFI\xFBBOUNDED")?;
        let pulled = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut Vec::new())?;
        let sidecar = fixture.paths.image_metadata(&pulled.metadata.id)?;
        fs::write(&sidecar, vec![b'x'; MAX_SIDECAR_BYTES as usize + 1])?;
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(SIDECAR_FILE_MODE))?;
        let error = fixture
            .store
            .list()
            .err()
            .ok_or("expected oversized sidecar rejection")?;
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("exceeds"));
        Ok(())
    }
    #[test]
    fn moved_manifest_catalog_uses_warm_cache_but_explicit_digest_mismatch_does_not()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let old_url = "https://cache.example.invalid/old/base.qcow2";
        let old_manifest = "https://cache.example.invalid/old/SHA256SUMS";
        let bytes = b"QFI\xFBWARM-CACHE".to_vec();
        let digest = sha256_bytes(&bytes);
        let old_catalog = custom_catalog(
            &fixture.root,
            "old-moving",
            &format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"moving\"\n",
                    "version = \"1\"\n",
                    "aliases = []\n",
                    "default = true\n",
                    "firmware = \"rhf\"\n",
                    "format = \"qcow2\"\n\n",
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "checksum_url = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                old_url, old_manifest,
            ),
        )?;
        let old_store = store_with_catalog(&fixture, old_catalog, Arc::new(FixedClock));
        fixture.http.push(
            old_manifest,
            format!("{digest}  base.qcow2\n").into_bytes(),
            None,
            Some("text/plain"),
        )?;
        fixture
            .http
            .push(old_url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let old = old_store.pull(
            &ImagePullRequest::new(ImageRef::new("moving:1"), &fixture.root),
            &mut Vec::new(),
        )?;

        let new_url = "https://cache.example.invalid/new/base.qcow2";
        let new_manifest = "https://cache.example.invalid/new/SHA256SUMS";
        let new_catalog = custom_catalog(
            &fixture.root,
            "new-moving",
            &format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"moving\"\n",
                    "version = \"1\"\n",
                    "aliases = []\n",
                    "default = true\n",
                    "firmware = \"edk2\"\n",
                    "format = \"qcow2\"\n\n",
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "checksum_url = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                new_url, new_manifest,
            ),
        )?;
        let new_store = store_with_catalog(&fixture, new_catalog, Arc::new(FixedClock));
        let name = "moving-cache";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: "moving:1".to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let lock = create_machine(&fixture.paths, name, &state, &mut Vec::new())?;
        let prepared = new_store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert!(prepared.image.cached);
        assert_eq!(prepared.image.metadata.id, old.metadata.id);
        assert_eq!(prepared.image.metadata.source_url.as_deref(), Some(old_url));
        assert_eq!(prepared.image.firmware, Some(CatalogFirmware::Rhf));

        let direct_url = "https://cache.example.invalid/direct.qcow2";
        fixture
            .http
            .push(direct_url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let direct = fixture.store.pull(
            &url_request(direct_url, Some(digest.clone()), &fixture.root),
            &mut Vec::new(),
        )?;
        fixture
            .http
            .push(direct_url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let wrong = "0".repeat(64);
        let mismatch = fixture
            .store
            .pull(
                &url_request(direct_url, Some(wrong), &fixture.root),
                &mut Vec::new(),
            )
            .err()
            .ok_or("expected explicit digest mismatch")?;
        assert_eq!(mismatch.kind(), ErrorKind::Checksum);
        assert_eq!(
            fixture
                .store
                .inspect(&direct.metadata.id)?
                .image
                .metadata
                .id,
            direct.metadata.id
        );
        Ok(())
    }

    #[test]
    fn verifier_provenance_upgrades_without_identity_change_and_never_downgrades()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://verify.example.invalid/base.qcow2";
        let bytes = b"QFI\xFBPROVENANCE".to_vec();
        let sha256 = sha256_bytes(&bytes);
        let sha512 = sha512_bytes(&bytes);

        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let unchecked = fixture
            .store
            .pull(&url_request(url, None, &fixture.root), &mut Vec::new())?;
        assert_eq!(unchecked.metadata.generation, 1);
        assert!(unchecked.metadata.verification().is_none());

        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let checked = fixture.store.pull(
            &url_request(url, Some(sha256.clone()), &fixture.root),
            &mut Vec::new(),
        )?;
        assert_eq!(checked.metadata.id, unchecked.metadata.id);
        assert_eq!(checked.metadata.generation, 2);
        assert_eq!(
            checked.metadata.verification_algorithm,
            Some(ChecksumAlgorithm::Sha256)
        );

        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        fixture.store.ensure_store()?;
        let sha512_checked = {
            let _lock = fixture.store.acquire_lock()?;
            fixture.store.cleanup_stale_partials()?;
            fixture.store.pull_locked(
                ResolvedImageSource {
                    source_ref: url.to_owned(),
                    source_url: Some(url.to_owned()),
                    architecture: Arch::X86_64,
                    source_format: Some(ImageFormat::Qcow2),
                    firmware: None,
                    verification: Some(ImageVerification {
                        algorithm: ChecksumAlgorithm::Sha512,
                        digest: sha512.clone(),
                    }),
                    location: ImageSourceLocation::Https(url.to_owned()),
                    checksum: ExpectedChecksum::Digest(ImageVerification {
                        algorithm: ChecksumAlgorithm::Sha512,
                        digest: sha512.clone(),
                    }),
                    local_source: None,
                },
                &mut Vec::new(),
            )?
        };
        assert_eq!(sha512_checked.metadata.id, unchecked.metadata.id);
        assert_eq!(sha512_checked.metadata.generation, 3);
        assert_eq!(
            sha512_checked.metadata.verification_algorithm,
            Some(ChecksumAlgorithm::Sha512)
        );

        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let downgraded_request = fixture
            .store
            .pull(&url_request(url, None, &fixture.root), &mut Vec::new())?;
        assert_eq!(downgraded_request.metadata.id, unchecked.metadata.id);
        assert_eq!(downgraded_request.metadata.generation, 3);
        assert_eq!(
            downgraded_request.metadata.verification_algorithm,
            Some(ChecksumAlgorithm::Sha512)
        );
        assert_eq!(
            downgraded_request.metadata.verification_digest.as_deref(),
            Some(sha512.as_str())
        );
        Ok(())
    }

    #[test]
    fn generation_selects_unique_max_despite_equal_or_backward_timestamps_and_checks_overflow()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://generation.example.invalid/base.qcow2";
        let first_bytes = b"QFI\xFBGENERATION-ONE".to_vec();
        fixture.http.push(
            url,
            first_bytes.clone(),
            Some(first_bytes.len() as u64),
            None,
        )?;
        let first = fixture
            .store
            .pull(&url_request(url, None, &fixture.root), &mut Vec::new())?;

        let backward_store = store_with_catalog(
            &fixture,
            Catalog::built_in()?,
            Arc::new(StaticClock("2020-01-01T00:00:00Z")),
        );
        let second_bytes = b"QFI\xFBGENERATION-TWO".to_vec();
        fixture.http.push(
            url,
            second_bytes.clone(),
            Some(second_bytes.len() as u64),
            None,
        )?;
        let second =
            backward_store.pull(&url_request(url, None, &fixture.root), &mut Vec::new())?;
        assert_eq!(first.metadata.generation, 1);
        assert_eq!(second.metadata.generation, 2);
        assert!(second.metadata.pulled_at < first.metadata.pulled_at);

        {
            let _lock = backward_store.acquire_lock()?;
            let resolved = backward_store.resolve_persisted(url)?;
            let latest = backward_store
                .find_latest_for_source(&resolved)?
                .ok_or("missing latest generation")?;
            assert_eq!(latest.metadata.id, second.metadata.id);
        }

        let first_sidecar = fixture.paths.image_metadata(&first.metadata.id)?;
        let mut duplicate = first.metadata.clone();
        duplicate.generation = 2;
        atomic::write_json_with_mode(&first_sidecar, &duplicate, SIDECAR_FILE_MODE)?;
        {
            let _lock = backward_store.acquire_lock()?;
            let resolved = backward_store.resolve_persisted(url)?;
            let ambiguous = backward_store
                .find_latest_for_source(&resolved)
                .err()
                .ok_or("expected duplicate maximum generation rejection")?;
            assert_eq!(ambiguous.kind(), ErrorKind::Conflict);
        }

        duplicate.generation = 1;
        atomic::write_json_with_mode(&first_sidecar, &duplicate, SIDECAR_FILE_MODE)?;
        let second_sidecar = fixture.paths.image_metadata(&second.metadata.id)?;
        let mut maximum = second.metadata.clone();
        maximum.generation = u64::MAX;
        atomic::write_json_with_mode(&second_sidecar, &maximum, SIDECAR_FILE_MODE)?;
        let third_bytes = b"QFI\xFBGENERATION-THREE".to_vec();
        fixture.http.push(
            url,
            third_bytes.clone(),
            Some(third_bytes.len() as u64),
            None,
        )?;
        let overflow = backward_store
            .pull(&url_request(url, None, &fixture.root), &mut Vec::new())
            .err()
            .ok_or("expected generation overflow")?;
        assert_eq!(overflow.kind(), ErrorKind::Conflict);
        let third_id = stable_image_id(url, Some(url), Arch::X86_64, &sha256_bytes(&third_bytes));
        assert!(!fixture.paths.image_base(&third_id)?.exists());
        assert!(!fixture.paths.image_metadata(&third_id)?.exists());
        Ok(())
    }

    #[test]
    fn catalog_firmware_is_durable_across_warm_cache_upgrade_and_catalog_removal()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://firmware.example.invalid/base.qcow2";
        let bytes = b"QFI\xFBFIRMWARE".to_vec();
        let digest = sha256_bytes(&bytes);
        let catalog_source = |firmware: &str| {
            format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"firm\"\n",
                    "version = \"1\"\n",
                    "aliases = []\n",
                    "default = true\n",
                    "firmware = \"{}\"\n",
                    "format = \"qcow2\"\n\n",
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "sha256 = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                firmware, url, digest,
            )
        };
        let old_catalog = custom_catalog(&fixture.root, "firm-rhf", &catalog_source("rhf"))?;
        let old_store = store_with_catalog(&fixture, old_catalog, Arc::new(FixedClock));
        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let old = old_store.pull(
            &ImagePullRequest::new(ImageRef::new("firm:1"), &fixture.root),
            &mut Vec::new(),
        )?;
        assert_eq!(old.metadata.firmware, Some(CatalogFirmware::Rhf));

        let new_catalog = custom_catalog(&fixture.root, "firm-edk2", &catalog_source("edk2"))?;
        let new_store = store_with_catalog(&fixture, new_catalog, Arc::new(FixedClock));
        let name = "firmware-cache";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: "firm:1".to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let lock = create_machine(&fixture.paths, name, &state, &mut Vec::new())?;
        let warm = new_store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert_eq!(warm.image.metadata.id, old.metadata.id);
        assert_eq!(warm.image.firmware, Some(CatalogFirmware::Rhf));
        assert_eq!(warm.image.metadata.generation, 1);

        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let upgraded = new_store.pull(
            &ImagePullRequest::new(ImageRef::new("firm:1"), &fixture.root),
            &mut Vec::new(),
        )?;
        assert_eq!(upgraded.metadata.id, old.metadata.id);
        assert_eq!(upgraded.metadata.firmware, Some(CatalogFirmware::Edk2));
        assert_eq!(upgraded.metadata.generation, 2);

        let catalog_removed =
            store_with_catalog(&fixture, Catalog::built_in()?, Arc::new(FixedClock));
        let pinned = catalog_removed.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert_eq!(pinned.image.firmware, Some(CatalogFirmware::Edk2));
        assert_eq!(pinned.image.metadata.generation, 2);
        Ok(())
    }
    #[test]
    fn incomplete_pair_recovery_removes_unreferenced_crash_files_and_preserves_referenced_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("recovery.qcow2", b"QFI\xFBRECOVERY")?;
        let request = local_request(&source, &fixture.root);

        let first = fixture.store.pull(&request, &mut Vec::new())?;
        let base = fixture.paths.image_base(&first.metadata.id)?;
        let sidecar = fixture.paths.image_metadata(&first.metadata.id)?;
        fs::remove_file(&sidecar)?;
        assert!(fixture.store.list()?.is_empty());
        assert!(!base.exists());

        let second = fixture.store.pull(&request, &mut Vec::new())?;
        let base = fixture.paths.image_base(&second.metadata.id)?;
        let sidecar = fixture.paths.image_metadata(&second.metadata.id)?;
        let temp = sidecar.with_file_name(format!("{}.json.tmp", second.metadata.id));
        fs::write(&temp, b"stale sidecar temp")?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(SIDECAR_FILE_MODE))?;
        assert_eq!(fixture.store.list()?.len(), 1);
        assert!(!temp.exists());
        assert!(base.exists());
        assert!(sidecar.exists());

        fs::remove_file(&base)?;
        assert!(fixture.store.list()?.is_empty());
        assert!(!sidecar.exists());

        let third = fixture.store.pull(&request, &mut Vec::new())?;
        let base = fixture.paths.image_base(&third.metadata.id)?;
        let sidecar = fixture.paths.image_metadata(&third.metadata.id)?;
        let temp = sidecar.with_file_name(format!("{}.json.tmp", third.metadata.id));
        let sidecar_bytes = fs::read(&sidecar)?;
        fs::remove_file(&sidecar)?;
        fs::write(&temp, sidecar_bytes)?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(SIDECAR_FILE_MODE))?;
        assert!(fixture.store.list()?.is_empty());
        assert!(!base.exists());
        assert!(!temp.exists());

        let referenced = fixture.store.pull(&request, &mut Vec::new())?;
        let name = "kept-incomplete";
        let state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: referenced.metadata.source_ref.clone(),
                id: Some(referenced.metadata.id.clone()),
                sha256: Some(referenced.metadata.source_sha256.clone()),
            },
        )?;
        let _machine_lock = create_machine(&fixture.paths, name, &state, &mut Vec::new())?;
        let referenced_base = fixture.paths.image_base(&referenced.metadata.id)?;
        let referenced_sidecar = fixture.paths.image_metadata(&referenced.metadata.id)?;
        fs::remove_file(&referenced_sidecar)?;
        let error = fixture
            .store
            .list()
            .err()
            .ok_or("expected referenced incomplete pair rejection")?;
        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert!(error.message().contains(name));
        assert!(referenced_base.exists());
        assert!(!referenced_sidecar.exists());
        Ok(())
    }

    #[test]
    fn lock_and_directory_creation_override_restrictive_umask_without_repairing_insecure_existing_lock()
    -> Result<(), Box<dyn std::error::Error>> {
        if env::var_os("FIRESTONE_IMAGE_UMASK_ROOT").is_some() {
            return Ok(());
        }
        let fixture = Fixture::new(false)?;
        let current = env::current_exe()?;
        let status = Command::new(current)
            .arg("--exact")
            .arg("image::tests::image_store_restrictive_umask_helper")
            .arg("--nocapture")
            .env("FIRESTONE_IMAGE_UMASK_ROOT", &fixture.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        assert!(status.success());
        assert_eq!(
            fs::symlink_metadata(fixture.paths.data_dir())?.mode() & 0o7777,
            OWNED_DIRECTORY_MODE,
        );
        assert_eq!(
            fs::symlink_metadata(fixture.paths.images_dir())?.mode() & 0o7777,
            OWNED_DIRECTORY_MODE,
        );
        let lock_path = fixture.paths.image_store_lock()?;
        assert_eq!(
            fs::symlink_metadata(&lock_path)?.mode() & 0o7777,
            LOCK_FILE_MODE
        );

        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644))?;
        let error = ImageStoreLock::acquire(
            &fixture.paths,
            Duration::from_millis(20),
            Duration::from_millis(5),
        )
        .err()
        .ok_or("expected insecure existing lock rejection")?;
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(fs::symlink_metadata(&lock_path)?.mode() & 0o7777, 0o644);
        Ok(())
    }

    #[test]
    fn image_store_restrictive_umask_helper() -> Result<(), Box<dyn std::error::Error>> {
        let Some(root) = env::var_os("FIRESTONE_IMAGE_UMASK_ROOT").map(PathBuf::from) else {
            return Ok(());
        };
        nix::sys::stat::umask(Mode::from_bits_truncate(0o777));
        let paths = test_paths(&root, root.join("firestone"))?;
        paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
        paths.ensure_owned_data_directory(&paths.images_dir(), "images directory", false)?;
        drop(ImageStoreLock::acquire(
            &paths,
            Duration::from_secs(1),
            Duration::from_millis(5),
        )?);
        Ok(())
    }

    #[test]
    fn persisted_resolution_never_probes_relative_shadow_files_and_deleted_catalog_uses_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        fs::write(fixture.root.join("ubuntu:24.04"), b"shadow")?;
        let catalog = fixture.store.resolve_persisted("ubuntu:24.04")?;
        assert_eq!(catalog.source_ref, "ubuntu:24.04");
        assert!(matches!(catalog.location, ImageSourceLocation::Https(_)));

        let malformed_dir = fixture.root.join("https:");
        fs::create_dir(&malformed_dir)?;
        fs::set_permissions(&malformed_dir, fs::Permissions::from_mode(0o700))?;
        fs::write(malformed_dir.join("shadow"), b"shadow")?;
        assert!(fixture.store.resolve_persisted("https:/shadow").is_err());

        let url = "https://gone.example.invalid/base.qcow2";
        let bytes = b"QFI\xFBGONE-CATALOG".to_vec();
        let digest = sha256_bytes(&bytes);
        let gone_catalog = custom_catalog(
            &fixture.root,
            "gone",
            &format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"gone\"\n",
                    "version = \"1\"\n",
                    "aliases = []\n",
                    "default = true\n",
                    "firmware = \"edk2\"\n",
                    "format = \"qcow2\"\n\n",
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "sha256 = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                url, digest,
            ),
        )?;
        let old_store = store_with_catalog(&fixture, gone_catalog, Arc::new(FixedClock));
        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let pulled = old_store.pull(
            &ImagePullRequest::new(ImageRef::new("gone:1"), &fixture.root),
            &mut Vec::new(),
        )?;
        let removed_catalog =
            store_with_catalog(&fixture, Catalog::built_in()?, Arc::new(FixedClock));
        let name = "gone-cache";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: "gone:1".to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let lock = create_machine(&fixture.paths, name, &state, &mut Vec::new())?;
        let prepared = removed_catalog.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert!(prepared.image.cached);
        assert_eq!(prepared.image.metadata.id, pulled.metadata.id);
        assert_eq!(prepared.image.firmware, Some(CatalogFirmware::Edk2));
        Ok(())
    }
    #[test]
    fn pinned_local_image_prepares_after_original_source_is_deleted()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("ephemeral-local.qcow2", b"QFI\xFBEPHEMERAL")?;
        let name = "deleted-local";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: source.to_string_lossy().into_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let lock = create_machine(&fixture.paths, name, &state, &mut Vec::new())?;
        let first = fixture.store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert_eq!(
            state.image.id.as_deref(),
            Some(first.image.metadata.id.as_str())
        );
        fs::remove_file(&source)?;

        let second = fixture.store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert!(second.image.cached);
        assert_eq!(second.image.metadata.id, first.image.metadata.id);
        assert!(!source.exists());
        Ok(())
    }
}
