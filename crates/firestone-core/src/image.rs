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
    Arch, ByteSize, Catalog, CatalogChecksum, CatalogFirmware, ChecksumAlgorithm, Cmd,
    DependencyArtifact, DependencyManifest, ErrorKind, Event, EventSink, FirestoneError,
    ImageFormat, ImageRef, Level, MachineLock, MachineState, Paths, StateImage, StateStore, StepId,
    Unit, atomic,
    bounded::{self, BoundedReadError},
    catalog::{SshdPath, parse_https_url},
    embedded_helpers::install_pinned_artifact_with,
    oci::{
        OciClassification, OciReference,
        layers::{FileLayer, LayerSource, MergeLimits, MergeRequest, OciImageConfig, merge_layers},
        registry::{LayerDescriptor, RegistryClient, RegistryOptions},
    },
};
const IMAGE_METADATA_VERSION: u32 = 1;
/// The only `oci.boot` value SPEC §8.5 defines in v0.2.
pub const FIRESTONE_INIT_BOOT: &str = "firestone-init";
const IMAGE_ID_PREFIX: &str = "image-";
const IMAGE_ID_HEX_LENGTH: usize = 64;
const IMAGE_BUFFER_SIZE: usize = 1024 * 1024;
const IMAGE_PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;
const MAX_DEPENDENCY_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const QEMU_INFO_TIMEOUT: Duration = Duration::from_secs(30);
const QEMU_CREATE_TIMEOUT: Duration = Duration::from_secs(60);
const QEMU_CONVERT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MKFS_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Block size the pinned `mkfs.ext4` writes an OCI rootfs with (SPEC §8.5).
const EXT4_BLOCK_BYTES: u64 = 4096;
/// Fixed headroom added by the §8.5 sizing rule.
const OCI_SIZE_HEADROOM_BYTES: u64 = 256 * 1024 * 1024;
/// Alignment the §8.5 sizing rule rounds up to.
const OCI_SIZE_ALIGNMENT_BYTES: u64 = 4 * 1024 * 1024;
/// The §8.5 bound on the merged tree, measured uncompressed.
const OCI_MAX_UNPACKED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
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

/// Which boot mode a stored image was built for (SPEC §9.5).
///
/// The field is absent from version-one sidecars, which are all disk images
/// booted through a firmware, so it deserializes to `Disk` by default and is
/// omitted when it is `Disk`: every existing sidecar keeps its exact bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageKind {
    /// A disk image booted through firmware and provisioned by cloud-init.
    #[default]
    Disk,
    /// An OCI-derived root filesystem booted through the pinned kernel (§8.5).
    Oci,
}

impl ImageKind {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Oci => "oci",
        }
    }

    #[must_use]
    pub const fn is_disk(&self) -> bool {
        matches!(self, Self::Disk)
    }

    #[must_use]
    pub const fn is_oci(&self) -> bool {
        matches!(self, Self::Oci)
    }
}

/// The image runtime configuration a sidecar `oci` object carries (SPEC §8.5).
///
/// This is the read side only: the OCI pull pipeline owns the write side. It is
/// what feeds the `firestone-init` config disk of §10.5, so an OCI machine boots
/// the entrypoint the image declared rather than a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciSidecar {
    pub registry_ref: String,
    pub manifest_digest: String,
    pub config_digest: String,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub boot: String,
}

impl OciSidecar {
    /// The runtime fields the config-disk writer consumes (§10.5).
    #[must_use]
    pub fn runtime_config(&self) -> OciImageConfig {
        OciImageConfig {
            env: self.env.clone(),
            entrypoint: self.entrypoint.clone(),
            cmd: self.cmd.clone(),
            working_dir: self.workdir.clone(),
            user: self.user.clone(),
        }
    }
}

/// Strict version-one contents of an image sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageMetadata {
    pub version: ImageMetadataVersion,
    pub id: String,
    /// Read side of the sidecar `kind` field: absent means `disk` (§9.5).
    #[serde(default, skip_serializing_if = "ImageKind::is_disk")]
    pub kind: ImageKind,
    /// Read side of the sidecar `oci` object; absent on every disk image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci: Option<OciSidecar>,
    pub generation: u64,
    pub source_ref: String,
    pub source_url: Option<String>,
    pub source_sha256: String,
    pub stored_sha256: String,
    pub architecture: Arch,
    pub firmware: Option<CatalogFirmware>,
    #[serde(default, skip_serializing_if = "SshdPath::is_default")]
    pub sshd_path: SshdPath,
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
            #[serde(default)]
            kind: ImageKind,
            #[serde(default)]
            oci: Option<OciSidecar>,
            generation: u64,
            source_ref: String,
            source_url: RequiredNullable<String>,
            source_sha256: String,
            stored_sha256: String,
            architecture: Arch,
            firmware: RequiredNullable<CatalogFirmware>,
            #[serde(default)]
            sshd_path: SshdPath,
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
            kind: wire.kind,
            oci: wire.oci,
            generation: wire.generation,
            source_ref: wire.source_ref,
            source_url: wire.source_url.0,
            source_sha256: wire.source_sha256,
            stored_sha256: wire.stored_sha256,
            architecture: wire.architecture,
            firmware: wire.firmware.0,
            sshd_path: wire.sshd_path,
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
        // §8.5: the `oci` object is present exactly when `kind` is `oci`. A
        // disk sidecar carrying one is corrupt. The reverse — an `oci` sidecar
        // with no runtime object — is tolerated here and refused at start,
        // because the pull pipeline that writes it lands separately.
        if let Some(oci) = &self.oci {
            if !self.kind.is_oci() {
                return Err(invalid_sidecar(
                    &self.id,
                    "a disk image must not carry an oci object",
                ));
            }
            if oci.boot != FIRESTONE_INIT_BOOT {
                return Err(invalid_sidecar(
                    &self.id,
                    "oci boot must be \"firestone-init\"",
                ));
            }
            if oci.registry_ref != self.source_ref {
                return Err(invalid_sidecar(
                    &self.id,
                    "oci registry_ref must equal source_ref",
                ));
            }
            for (field, digest) in [
                ("manifest_digest", &oci.manifest_digest),
                ("config_digest", &oci.config_digest),
            ] {
                if digest
                    .strip_prefix("sha256:")
                    .is_none_or(|hex| !is_lower_hex(hex, 64))
                {
                    return Err(invalid_sidecar(
                        &self.id,
                        &format!("oci {field} must be a sha256 digest"),
                    ));
                }
            }
        }
        if let Some(source_url) = &self.source_url {
            if self.kind.is_oci() {
                return Err(invalid_sidecar(
                    &self.id,
                    "an oci image must not carry a source_url",
                ));
            }
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
        } else if self.kind.is_oci() {
            // An OCI image (§8.5) has no source URL and no local path: its
            // immutable source is the normalized reference itself (§8.6).
            let normalized = OciReference::parse(&self.source_ref)
                .ok()
                .map(|reference| reference.to_string());
            if normalized.as_deref() != Some(self.source_ref.as_str()) {
                return Err(invalid_sidecar(
                    &self.id,
                    "an oci image source_ref must be a normalized OCI reference",
                ));
            }
            if self.firmware.is_some() {
                return Err(invalid_sidecar(&self.id, "oci image firmware must be null"));
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
    Oci(OciReference),
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
    pub sshd_path: SshdPath,
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

/// One image-store artifact system prune reported, with its measured size.
///
/// `id` is a stored image id for an unreferenced base pair and the partial
/// file's own name for an interrupted download (SPEC §26).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrunedImageArtifact {
    pub id: String,
    pub bytes: u64,
}

/// A validated machine overlay and its exact backing file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverlayInfo {
    pub path: PathBuf,
    pub backing_path: PathBuf,
    pub virtual_size: u64,
    pub cached: bool,
    /// True when this start grew an existing overlay to a larger spec `disk`.
    pub grown: bool,
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

pub(crate) struct HttpResponse {
    pub(crate) body: Box<dyn Read>,
    pub(crate) content_length: Option<u64>,
    pub(crate) content_type: Option<String>,
}

/// One request issued through the shared HTTP seam with explicit headers.
///
/// The image pull path only ever needs [`HttpSource::get`]; the OCI registry
/// client (§8.5) additionally needs request headers and an unmapped status
/// code, so both travel through this same transport rather than a second
/// client stack.
pub(crate) struct HttpRequest<'a> {
    pub(crate) url: &'a Url,
    pub(crate) headers: &'a [(&'static str, String)],
}

/// A response whose status line and headers survive the transport.
pub(crate) struct HttpStatusResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Box<dyn Read>,
    pub(crate) content_length: Option<u64>,
}

impl HttpStatusResponse {
    /// Returns the first value of one header, matched case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) trait HttpSource: Send + Sync {
    fn get(&self, url: &Url) -> Result<HttpResponse, FirestoneError>;

    /// Issues one request with explicit headers, preserving the status code.
    ///
    /// The default implementation delegates to [`HttpSource::get`], which maps
    /// every non-success status to an error, and therefore reports `200`.
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpStatusResponse, FirestoneError> {
        let response = self.get(request.url)?;
        let headers = response
            .content_type
            .map(|value| vec![("content-type".to_owned(), value)])
            .unwrap_or_default();
        Ok(HttpStatusResponse {
            status: 200,
            headers,
            body: response.body,
            content_length: response.content_length,
        })
    }
}

/// Builds the shared strict-transport HTTP client used by every image source.
pub(crate) fn shared_http_source() -> Result<Arc<dyn HttpSource>, FirestoneError> {
    Ok(Arc::new(ReqwestHttpSource::new()?))
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

    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpStatusResponse, FirestoneError> {
        let mut builder = self.client.get(request.url.clone());
        for (name, value) in request.headers {
            builder = builder.header(*name, value.as_str());
        }
        let response = builder
            .send()
            .map_err(|source| download_error(request.url, source))?;
        let status = response.status().as_u16();
        let content_length = response.content_length();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        Ok(HttpStatusResponse {
            status,
            headers,
            body: Box::new(response),
            content_length,
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
    /// `images.insecure_registries` from the global configuration (§7.3).
    insecure_registries: Vec<String>,
    /// Already-installed `mkfs.ext4`, which the tests point at a fake helper.
    /// Production leaves it `None` and installs the pinned artifact on demand.
    mkfs_ext4: Option<PathBuf>,
    /// Where the injected `firestone-init` payload comes from (§8.5, §10.5).
    init_payload: InitPayloadSource,
}

/// Supplies the `firestone-init` bytes injected into a merged OCI rootfs.
///
/// Production reads the payload embedded in a standalone release; the tests
/// install their own so both the success and the missing-payload paths are
/// exercised on every build configuration.
type InitPayloadSource = Arc<dyn Fn() -> Result<Vec<u8>, FirestoneError> + Send + Sync>;

fn embedded_init_payload_source() -> InitPayloadSource {
    Arc::new(|| crate::embedded_helpers::firestone_init_payload().map(<[u8]>::to_vec))
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
            insecure_registries: Vec::new(),
            mkfs_ext4: None,
            init_payload: embedded_init_payload_source(),
        })
    }

    /// Records the registries `images.insecure_registries` allows over plain
    /// HTTP; every other registry is contacted over HTTPS (SPEC §8.5).
    #[must_use]
    pub fn with_insecure_registries(mut self, registries: Vec<String>) -> Self {
        self.insecure_registries = registries;
        self
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

    /// Installs one pinned dependency with the image client's strict HTTPS
    /// transport and the shared secure artifact publisher.
    pub(crate) fn ensure_pinned_artifact(
        &self,
        artifact: &DependencyArtifact,
    ) -> Result<PathBuf, FirestoneError> {
        let url = parse_https_url(&artifact.url).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "dependency '{}' has an invalid HTTPS URL",
                    artifact.dependency
                ),
            )
            .with_hint("restore the bundled dependency manifest")
        })?;
        install_pinned_artifact_with(&self.paths, artifact, |output| {
            let mut response = self.http.get(&url).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "cannot download pinned dependency '{}' from {url}",
                        artifact.dependency
                    ),
                )
                .with_hint("check network access and retry start")
                .with_source(source)
            })?;
            if response
                .content_length
                .is_some_and(|length| length > MAX_DEPENDENCY_ARTIFACT_BYTES)
            {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "pinned dependency '{}' exceeds the {} byte download limit",
                        artifact.dependency, MAX_DEPENDENCY_ARTIFACT_BYTES
                    ),
                ));
            }
            let mut bounded = response
                .body
                .as_mut()
                .take(MAX_DEPENDENCY_ARTIFACT_BYTES.saturating_add(1));
            let copied = io::copy(&mut bounded, output).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("cannot store pinned dependency '{}'", artifact.dependency),
                )
                .with_hint("check the Firestone data directory permissions and free space")
                .with_source(source)
            })?;
            if copied > MAX_DEPENDENCY_ARTIFACT_BYTES {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "pinned dependency '{}' exceeds the {} byte download limit",
                        artifact.dependency, MAX_DEPENDENCY_ARTIFACT_BYTES
                    ),
                ));
            }
            Ok(())
        })
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
                sshd_path: SshdPath::default(),
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
                sshd_path: SshdPath::default(),
                verification: verification.clone(),
                location: ImageSourceLocation::Https(source_url),
                checksum: verification.map_or(ExpectedChecksum::None, ExpectedChecksum::Digest),
                local_source: None,
            });
        }

        if let Some(reference) = classify_oci_reference(value)? {
            if supplied_sha256.is_some() {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    format!("--sha256 is not accepted for OCI image reference '{reference}'"),
                )
                .with_hint(
                    "pin an OCI image with a repo@sha256:… digest reference instead of --sha256",
                ));
            }
            return Ok(self.oci_source(reference));
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
                    sshd_path: resolved.sshd_path,
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

    /// Builds the resolved source for an already-parsed OCI reference.
    fn oci_source(&self, reference: OciReference) -> ResolvedImageSource {
        ResolvedImageSource {
            source_ref: reference.to_string(),
            source_url: None,
            architecture: self.architecture,
            source_format: None,
            firmware: None,
            sshd_path: SshdPath::default(),
            verification: None,
            location: ImageSourceLocation::Oci(reference),
            checksum: ExpectedChecksum::None,
            local_source: None,
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
                sshd_path: SshdPath::default(),
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
                sshd_path: SshdPath::default(),
                verification: None,
                location: ImageSourceLocation::Local(opened.path.clone()),
                checksum: ExpectedChecksum::None,
                local_source: Some(opened),
            });
        }
        if let Some(oci) = classify_oci_reference(reference)? {
            return Ok(self.oci_source(oci));
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
            sshd_path: resolved.sshd_path,
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

    /// Reports, and unless `dry_run` removes, interrupted download artifacts.
    ///
    /// These are the `.pull-<digest>.{source,stored}.partial` files any locked
    /// image-store operation already discards; system prune measures them
    /// first so the reclaimed bytes can be reported (SPEC §26). Nothing else in
    /// the store is touched, so this is safe in the default prune scope.
    pub fn prune_partials(
        &self,
        dry_run: bool,
    ) -> Result<Vec<PrunedImageArtifact>, FirestoneError> {
        if !self.store_exists_for_read()? {
            return Ok(Vec::new());
        }
        let _lock = self.acquire_lock()?;
        let mut names = Vec::new();
        for entry in fs::read_dir(self.paths.images_dir())
            .map_err(|source| image_file_error("read", &self.paths.images_dir(), source))?
        {
            let entry =
                entry.map_err(|source| directory_entry_error(&self.paths.images_dir(), source))?;
            let name = entry.file_name().into_string().map_err(|_| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "images directory contains a non-UTF-8 file name",
                )
                .with_hint("move the unknown file out of the images directory")
            })?;
            if is_known_partial_name(&name) {
                names.push(name);
            }
        }
        names.sort();

        let mut pruned = Vec::with_capacity(names.len());
        let mut removed_any = false;
        for name in names {
            let path = self.paths.image_file(&name)?;
            validate_regular_nofollow(&path, "image partial")?;
            let bytes = crate::snapshot::allocated_bytes(&path)?;
            if !dry_run {
                fs::remove_file(&path)
                    .map_err(|source| image_file_error("remove stale", &path, source))?;
                removed_any = true;
            }
            pruned.push(PrunedImageArtifact { id: name, bytes });
        }
        if removed_any {
            sync_directory(&self.paths.images_dir(), "images directory")?;
        }
        Ok(pruned)
    }

    /// Reports, and unless `dry_run` removes, every unreferenced stored image.
    ///
    /// References are the same extended set `images rm` and `images prune`
    /// refuse to break: a machine's pinned `state.json` image and every
    /// published snapshot's `metadata.json` image (SPEC §23, §26). Sizes are
    /// the bytes the base and its sidecar occupy on disk, measured before the
    /// pair is unpublished, so a dry run and a real run report the same
    /// numbers for the same starting state.
    pub fn prune_unreferenced(
        &self,
        dry_run: bool,
    ) -> Result<Vec<PrunedImageArtifact>, FirestoneError> {
        if !self.store_exists_for_read()? {
            return Ok(Vec::new());
        }
        let _lock = self.acquire_lock()?;
        let references = self.image_references()?;
        let mut pruned = Vec::new();
        for image in self.list_locked()? {
            let id = image.metadata.id;
            if references.contains_key(&id) {
                continue;
            }
            let bytes = crate::snapshot::allocated_bytes(&image.path)?.saturating_add(
                crate::snapshot::allocated_bytes(&self.paths.image_metadata(&id)?)?,
            );
            if !dry_run {
                self.remove_pair(&id)?;
            }
            pruned.push(PrunedImageArtifact { id, bytes });
        }
        Ok(pruned)
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

    /// Copies one owned qcow2 overlay onto a new overlay that shares its qcow2 base.
    ///
    /// `dest_partial` must end in `.partial`; the copy is published, without
    /// replacing anything, at the same path with that suffix removed. The
    /// source overlay's backing chain must already resolve to exactly
    /// `backing`, and the published copy is validated the same way a freshly
    /// created overlay is (SPEC section 24).
    pub fn copy_overlay(
        &self,
        source: &Path,
        dest_partial: &Path,
        backing: &Path,
    ) -> Result<OverlayInfo, FirestoneError> {
        let published = published_overlay_path(dest_partial)?;
        let destination_dir = published.parent().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "overlay copy '{}' has no parent directory",
                    published.display()
                ),
            )
        })?;
        self.paths
            .validate_owned_data_directory(destination_dir, "machine directory", false)?;
        self.paths
            .validate_owned_data_file(source, "machine overlay", OVERLAY_FILE_MODE, false)?;
        let _lock = self.acquire_lock()?;
        self.paths
            .validate_owned_data_file(backing, "image base", BASE_FILE_MODE, false)?;
        let base_info = self.qemu_info(backing)?;
        validate_qcow2_structure(&format!("image base '{}'", backing.display()), &base_info)?;
        let source_info = self.qemu_info(source)?;
        validate_overlay_info(source, backing, source_info.virtual_size, &source_info)?;
        let virtual_size = source_info.virtual_size;

        remove_stale_partial(dest_partial)?;
        let mut cleanup = CleanupGuard::new();
        cleanup.track(dest_partial.to_path_buf());

        // Pinned qemu-img 8.2.2 overlay-copy argv. `-B` keeps the shared base
        // out of the copy and `-o backing_fmt=qcow2` records the backing format
        // explicitly, exactly as `create -F qcow2` does for a fresh overlay.
        Cmd::new(self.qemu_img.as_os_str())
            .arg("convert")
            .arg("-f")
            .arg("qcow2")
            .arg("-O")
            .arg("qcow2")
            .arg("-o")
            .arg("backing_fmt=qcow2")
            .arg("-B")
            .arg(backing.as_os_str())
            .arg(source.as_os_str())
            .arg(dest_partial.as_os_str())
            .timeout(QEMU_CONVERT_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;
        validate_created_regular_file(dest_partial, "machine overlay partial")?;
        set_file_mode(dest_partial, OVERLAY_FILE_MODE, "machine overlay partial")?;
        sync_file(dest_partial, "machine overlay partial")?;
        let info = self.qemu_info(dest_partial)?;
        validate_overlay_info(dest_partial, backing, virtual_size, &info)?;
        cleanup.track(published.clone());
        publish_no_replace(dest_partial, &published)?;
        sync_directory(destination_dir, "machine directory")?;
        cleanup.disarm();
        Ok(OverlayInfo {
            path: published,
            backing_path: backing.to_path_buf(),
            virtual_size: info.virtual_size,
            cached: false,
            grown: false,
        })
    }

    /// Pulls one OCI image and publishes it as an owned qcow2 base (SPEC §8.5).
    ///
    /// The manifest digest is the cache key: a re-pull of an unchanged tag hits
    /// the same stable id and skips, while a moved tag resolves to a different
    /// manifest and publishes a new generation. Every intermediate artifact is
    /// a tracked partial, so a failure at any step leaves the store untouched.
    fn pull_oci_locked(
        &self,
        source: &ResolvedImageSource,
        reference: &OciReference,
        events: &mut dyn EventSink,
    ) -> Result<PulledImage, FirestoneError> {
        let started = Instant::now();
        self.emit_image_start(&source.source_ref, events)?;

        // The guest init is injected into the merged rootfs, so a build that
        // carries no payload must fail before a single blob is downloaded.
        let init_payload = self.firestone_init_bytes(reference)?;

        let client = RegistryClient::with_http(
            Arc::clone(&self.http),
            &RegistryOptions::new(self.architecture)
                .with_insecure_registries(self.insecure_registries.clone())
                .with_docker_config(self.paths.docker_config_file()),
        )?;
        for warning in client.warnings() {
            events.emit(Event::Log {
                level: Level::Warn,
                message: warning.clone(),
            })?;
        }
        let resolved = client.resolve(reference)?;
        let manifest_sha256 = manifest_identity_digest(reference, &resolved.manifest_digest)?;

        // §8.5 identity: sidecar version, canonical reference, manifest digest,
        // and host architecture. There is no source file and no source URL.
        let id = stable_image_id(
            &source.source_ref,
            None,
            self.architecture,
            &manifest_sha256,
        );
        if let Some(cached) =
            self.existing_identity(&id, source, &manifest_sha256, ImageFormat::Raw)?
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

        let operation = operation_key(source);
        let source_partial = self.paths.image_source_partial(&operation)?;
        let stored_partial = self.paths.image_stored_partial(&operation)?;
        let tar_partial = self.paths.image_rootfs_tar_partial(&operation)?;
        let layer_partials = (0..resolved.layers.len())
            .map(|index| self.paths.image_layer_partial(&operation, index))
            .collect::<Result<Vec<_>, _>>()?;
        let mut cleanup = CleanupGuard::new();
        for path in [&source_partial, &stored_partial, &tar_partial]
            .into_iter()
            .chain(layer_partials.iter())
        {
            remove_stale_partial(path)?;
            cleanup.track(path.clone());
        }

        let total = total_layer_bytes(reference, &resolved.layers)?;
        let mut done = 0_u64;
        for (descriptor, path) in resolved.layers.iter().zip(&layer_partials) {
            let mut file = create_partial_file(path)?;
            let base = done;
            client.fetch_layer(reference, descriptor, &mut file, &mut |layer_done, _| {
                events.emit(Event::Progress {
                    id: StepId::from("image"),
                    done: base.saturating_add(layer_done),
                    total: Some(total),
                    unit: Unit::Bytes,
                })
            })?;
            file.sync_all()
                .map_err(|source| image_file_error("flush", path, source))?;
            done = base.saturating_add(descriptor.size);
        }
        events.emit(Event::Progress {
            id: StepId::from("image"),
            done: total,
            total: Some(total),
            unit: Unit::Bytes,
        })?;

        let mkfs_ext4 = self.mkfs_ext4_program()?;

        let layers = layer_partials
            .iter()
            .map(|path| FileLayer::new(path.clone()))
            .collect::<Vec<_>>();
        let sources = layers
            .iter()
            .map(|layer| layer as &dyn LayerSource)
            .collect::<Vec<_>>();
        let merge = MergeRequest::new(&sources, &resolved.config)
            .with_limits(MergeLimits {
                max_uncompressed_bytes: OCI_MAX_UNPACKED_BYTES,
                ..MergeLimits::default()
            })
            .with_injected_init(&init_payload);
        let summary = {
            let mut tar = create_partial_file(&tar_partial)?;
            let summary = merge_layers(&merge, &mut tar)?;
            tar.sync_all()
                .map_err(|source| image_file_error("flush", &tar_partial, source))?;
            summary
        };

        let rootfs_bytes = oci_rootfs_bytes(reference, summary.unpacked_bytes)?;
        create_partial_file(&source_partial)?
            .set_len(rootfs_bytes)
            .map_err(|source| image_file_error("size", &source_partial, source))?;
        // Pinned e2fsprogs 1.47.3 tar-input argv. With `-b 4096` the trailing
        // operand is a block count, and the §8.5 size is always a 4 MiB
        // multiple, so the division is exact.
        Cmd::new(mkfs_ext4.as_os_str())
            .arg("-F")
            .arg("-t")
            .arg("ext4")
            .arg("-d")
            .arg(tar_partial.as_os_str())
            .arg("-b")
            .arg(EXT4_BLOCK_BYTES.to_string())
            .arg(source_partial.as_os_str())
            .arg((rootfs_bytes / EXT4_BLOCK_BYTES).to_string())
            .timeout(MKFS_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;
        // The merged tar and the layer blobs are dead once the image is
        // packed; releasing them keeps peak store usage down.
        remove_stale_partial(&tar_partial)?;
        for path in &layer_partials {
            remove_stale_partial(path)?;
        }
        validate_created_regular_file(&source_partial, "staged OCI root filesystem")?;

        self.convert_raw(&source_partial, &stored_partial)?;
        validate_created_regular_file(&stored_partial, "staged qcow2 image")?;
        let info = self.qemu_info(&stored_partial)?;
        validate_base_info(&id, &info)?;
        let (stored_sha256, stored_size) = hash_file_with_size(&stored_partial)?;
        set_file_mode(&stored_partial, BASE_FILE_MODE, "staged qcow2 image")?;
        sync_file(&stored_partial, "staged qcow2 image")?;

        let base_path = self.paths.image_base(&id)?;
        let sidecar_path = self.paths.image_metadata(&id)?;
        let generation = self.next_generation(&source.source_ref, self.architecture)?;
        let metadata = ImageMetadata {
            version: ImageMetadataVersion,
            id: id.clone(),
            kind: ImageKind::Oci,
            oci: Some(OciSidecar {
                registry_ref: source.source_ref.clone(),
                manifest_digest: resolved.manifest_digest.clone(),
                config_digest: resolved.config_digest.clone(),
                entrypoint: resolved.config.entrypoint.clone(),
                cmd: resolved.config.cmd.clone(),
                env: resolved.config.env.clone(),
                workdir: resolved.config.working_dir.clone(),
                user: resolved.config.user.clone(),
                boot: FIRESTONE_INIT_BOOT.to_owned(),
            }),
            generation,
            source_ref: source.source_ref.clone(),
            source_url: None,
            source_sha256: manifest_sha256.clone(),
            stored_sha256,
            architecture: self.architecture,
            // §9.5: an OCI machine boots the pinned kernel directly, so the
            // firmware field carries no selection.
            firmware: None,
            sshd_path: SshdPath::default(),
            source_format: ImageFormat::Raw,
            stored_format: ImageFormat::Qcow2,
            verification_algorithm: Some(ChecksumAlgorithm::Sha256),
            verification_digest: Some(manifest_sha256),
            size: stored_size,
            pulled_at: self.clock.now(),
        };
        metadata.validate()?;
        let sidecar_bytes = serialize_image_metadata(&sidecar_path, &metadata)?;

        ensure_pair_absent(&base_path, &sidecar_path, &id)?;
        cleanup.track(base_path.clone());
        cleanup.track(sidecar_path.clone());
        publish_no_replace(&stored_partial, &base_path)?;
        atomic::write_with_mode(&sidecar_path, &sidecar_bytes, SIDECAR_FILE_MODE)?;
        self.paths.validate_owned_data_file(
            &sidecar_path,
            "image sidecar",
            SIDECAR_FILE_MODE,
            false,
        )?;
        remove_stale_partial(&source_partial)?;
        sync_directory(&self.paths.images_dir(), "images directory")?;
        cleanup.disarm();

        events.emit(Event::StepDone {
            id: StepId::from("image"),
            detail: Some(format!(
                "{} · {} · {stored_size} bytes",
                source.source_ref, self.architecture
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

    /// The `firestone-init` bytes injected at `/sbin/firestone-init` (§8.5).
    fn firestone_init_bytes(&self, reference: &OciReference) -> Result<Vec<u8>, FirestoneError> {
        match (self.init_payload)() {
            Ok(bytes) => Ok(bytes),
            Err(error) => Err(FirestoneError::new(
                error.kind(),
                format!(
                    "cannot pull OCI image '{reference}': the firestone-init guest payload is unavailable"
                ),
            )
            .with_hint(
                "this build embeds no firestone-init: pull with an x86_64 standalone release that \
                 embeds it, or publish the firestone-init release asset and pin it in deps.toml",
            )
            .with_source(error)),
        }
    }

    /// Resolves the pinned `mkfs.ext4`, installing it on demand (§8.5, §17.2).
    fn mkfs_ext4_program(&self) -> Result<PathBuf, FirestoneError> {
        if let Some(path) = &self.mkfs_ext4 {
            return Ok(path.clone());
        }
        let manifest = DependencyManifest::bundled()?;
        let artifact = manifest.mkfs_ext4(self.architecture.as_str())?;
        self.ensure_pinned_artifact(&artifact)
    }

    fn pull_locked(
        &self,
        mut source: ResolvedImageSource,
        events: &mut dyn EventSink,
    ) -> Result<PulledImage, FirestoneError> {
        if let ImageSourceLocation::Oci(reference) = &source.location {
            let reference = reference.clone();
            return self.pull_oci_locked(&source, &reference, events);
        }
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
        let generation = self.next_generation(&source.source_ref, source.architecture)?;
        let metadata = ImageMetadata {
            version: ImageMetadataVersion,
            id: id.clone(),
            // This path publishes disk images only; the OCI pull pipeline owns
            // the `oci` write side (§8.5).
            kind: ImageKind::Disk,
            oci: None,
            generation,
            source_ref: source.source_ref.clone(),
            source_url: source.source_url.clone(),
            source_sha256: staged.source_sha256,
            stored_sha256,
            architecture: source.architecture,
            firmware: source.firmware,
            sshd_path: source.sshd_path.clone(),
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
        let sidecar_bytes = serialize_image_metadata(&sidecar_path, &metadata)?;

        ensure_pair_absent(&base_path, &sidecar_path, &id)?;
        cleanup.track(base_path.clone());
        cleanup.track(sidecar_path.clone());
        publish_no_replace(candidate, &base_path)?;
        atomic::write_with_mode(&sidecar_path, &sidecar_bytes, SIDECAR_FILE_MODE)?;
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
                    ErrorKind::Checksum,
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
            ImageSourceLocation::Oci(reference) => Err(oci_pull_unsupported(reference)),
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
                ErrorKind::Generic,
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
                if source.source_format.is_some() && metadata.sshd_path != source.sshd_path {
                    metadata.sshd_path = source.sshd_path.clone();
                    changed = true;
                }
                if changed {
                    metadata.generation =
                        self.next_generation(&source.source_ref, source.architecture)?;
                    metadata.pulled_at = self.clock.now();
                    metadata.validate()?;
                    let sidecar_bytes = serialize_image_metadata(&sidecar, &metadata)?;
                    atomic::write_with_mode(&sidecar, &sidecar_bytes, SIDECAR_FILE_MODE)?;
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
    /// Grows one owned qcow2 overlay to `bytes` with pinned qemu-img 8.2.2.
    ///
    /// Only growth is ever requested: qcow2 shrink would discard guest data and
    /// is refused before this point. Growing the container leaves the guest
    /// partition untouched; cloud-init `growpart` extends it on the next boot.
    pub fn resize_overlay(&self, path: &Path, bytes: u64) -> Result<(), FirestoneError> {
        Cmd::new(self.qemu_img.as_os_str())
            .arg("resize")
            .arg("-f")
            .arg("qcow2")
            .arg(path.as_os_str())
            .arg(bytes.to_string())
            .timeout(QEMU_CREATE_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;
        Ok(())
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
        qemu_info_with(&self.qemu_img, path)
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
            let entry =
                entry.map_err(|source| directory_entry_error(&self.paths.images_dir(), source))?;
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
            .map_err(|source| directory_entry_error(&machines_dir, source))?;
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
            // A leading dot is reserved for Firestone's own working entries
            // (`.removing-<name>`, `.creating`). A removal tombstone is doomed
            // debris, not a machine, so the base it still names is not a
            // reference (SPEC §26).
            if name.starts_with('.') {
                continue;
            }
            let machine_dir = self.paths.machine_dir(&name)?;
            self.paths
                .validate_owned_data_directory(&machine_dir, "machine directory", false)?;
            // A snapshot pins its base image exactly like a live machine does,
            // so images rm and images prune must see it too (SPEC §23).
            for id in self.snapshot_image_references(&name)? {
                references.entry(id).or_default().push(name.clone());
            }
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

    /// Stored image ids pinned by one machine's published snapshots.
    ///
    /// Partial and removal directories are skipped: they are named with a
    /// leading dot and are never a published snapshot. A metadata document
    /// Firestone cannot read is a hard error, because silently dropping a
    /// reference would let `images rm` delete a base a snapshot still needs.
    fn snapshot_image_references(&self, name: &str) -> Result<Vec<String>, FirestoneError> {
        let snapshots_dir = self.paths.machine_snapshots_dir(name)?;
        if !path_exists_without_following(&snapshots_dir)? {
            return Ok(Vec::new());
        }
        self.paths.validate_owned_data_directory(
            &snapshots_dir,
            "machine snapshots directory",
            false,
        )?;
        let mut entries = fs::read_dir(&snapshots_dir)
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!(
                        "cannot read snapshots directory '{}'",
                        snapshots_dir.display()
                    ),
                )
                .with_hint("check the machine directory permissions")
                .with_source(source)
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| directory_entry_error(&snapshots_dir, source))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);

        let mut ids = Vec::new();
        for entry in entries {
            let Some(snapshot) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if snapshot.starts_with('.') {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|source| {
                image_file_error("inspect snapshot directory entry", &entry.path(), source)
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let snapshot_dir = self.paths.machine_snapshot_dir(name, &snapshot)?;
            let metadata_path = Paths::snapshot_metadata(&snapshot_dir);
            if !path_exists_without_following(&metadata_path)? {
                continue;
            }
            if let Some(id) = crate::snapshot::read_snapshot_metadata(&metadata_path)?.image_id {
                validate_image_id(&id)?;
                ids.push(id);
            }
        }
        Ok(ids)
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
        if metadata.kind.is_oci() {
            // §8.5: an OCI image's immutable source is its normalized
            // reference. It has no `source_url` and it is never a local path,
            // so it must not be resolved against the machine's source base.
            return Ok(OciReference::parse(reference)
                .map_or_else(|_| reference.to_owned(), |parsed| parsed.to_string()));
        }
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
            )
            .with_hint("recreate the machine to publish a complete image identity"));
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
                let mut info = self.qemu_info(&overlay)?;
                let requested = disk_size.as_bytes();
                let mut grown = false;
                if info.virtual_size > requested {
                    return Err(disk_shrink_error(name, info.virtual_size, requested));
                }
                if info.virtual_size < requested {
                    self.resize_overlay(&overlay, requested)?;
                    sync_file(&overlay, "machine overlay")?;
                    info = self.qemu_info(&overlay)?;
                    grown = true;
                }
                validate_overlay_info(&overlay, &stored.path, requested, &info)?;
                return Ok(OverlayInfo {
                    path: overlay,
                    backing_path: stored.path,
                    virtual_size: info.virtual_size,
                    cached: true,
                    grown,
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
            grown: false,
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
            let entry =
                entry.map_err(|source| directory_entry_error(&self.paths.images_dir(), source))?;
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
            let entry =
                entry.map_err(|source| directory_entry_error(&self.paths.images_dir(), source))?;
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
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Self::open_existing(paths, &path)?
            }
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

    fn open_existing(paths: &Paths, path: &Path) -> Result<File, FirestoneError> {
        paths.validate_owned_data_directory(
            &paths.images_dir(),
            "image store lock parent",
            false,
        )?;
        let metadata = fs::symlink_metadata(path)
            .map_err(|source| image_lock_error("inspect existing", path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "existing image store lock '{}' is not a regular no-follow file",
                    path.display()
                ),
            )
            .with_hint("replace the symlink or special file with a protected regular lock file"));
        }

        let expected_uid = nix::unistd::getuid().as_raw();
        let actual_uid = metadata.uid();
        let mode = metadata.mode() & 0o7777;
        if actual_uid != expected_uid || mode & !LOCK_FILE_MODE != 0 {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "existing image store lock '{}' is insecure: expected uid {expected_uid} and owner permissions no broader than 0600, found uid {actual_uid} and mode {mode:04o}",
                    path.display()
                ),
            )
            .with_hint("replace the lock with a current-user regular file without group/world permissions"));
        }

        let recovered = mode != LOCK_FILE_MODE;
        if recovered {
            fs::set_permissions(path, fs::Permissions::from_mode(LOCK_FILE_MODE))
                .map_err(|source| image_lock_error("recover mode on", path, source))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(false)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
            .open(path)
            .map_err(|source| image_lock_error("open existing", path, source))?;
        paths.validate_owned_data_file_handle(path, "image store lock", LOCK_FILE_MODE, &file)?;
        if recovered {
            file.sync_all()
                .map_err(|source| image_lock_error("fsync recovered", path, source))?;
        }
        Ok(file)
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
    integrity_error_kind: ErrorKind,
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
    let mut last_progress = 0_u64;
    let mut header = [0_u8; 4];
    let mut header_length = 0_usize;

    loop {
        let read = input.read(&mut buffer).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
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
                    integrity_error_kind,
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
        if size.saturating_sub(last_progress) >= IMAGE_PROGRESS_INTERVAL_BYTES {
            events.emit(Event::Progress {
                id: StepId::from("image"),
                done: size,
                total: expected_length,
                unit: Unit::Bytes,
            })?;
            last_progress = size;
        }
    }

    if let Some(expected) = expected_length {
        if size != expected {
            return Err(FirestoneError::new(
                integrity_error_kind,
                format!(
                    "image source ended after {size} bytes; Content-Length declared {expected}"
                ),
            )
            .with_hint("retry the pull; the remote response was partial"));
        }
    }
    events.emit(Event::Progress {
        id: StepId::from("image"),
        done: size,
        total: expected_length,
        unit: Unit::Bytes,
    })?;
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
        ExpectedChecksum::None => metadata.source_url == source.source_url,
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
        && metadata.sshd_path == source.sshd_path
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

/// Applies the §8.5 OCI classifier to one reference.
///
/// Returns `Ok(Some(reference))` when the value is an OCI reference that parses,
/// `Ok(None)` when the value is not OCI at all or when only the registry-host
/// heuristic matched and the value did not parse (so path and catalog
/// resolution keep their existing behavior), and `Err` when an explicit
/// `oci://` or `docker://` reference is malformed.
fn classify_oci_reference(value: &str) -> Result<Option<OciReference>, FirestoneError> {
    let Some(classification) = crate::oci::classify(value) else {
        return Ok(None);
    };
    match OciReference::parse(value) {
        Ok(reference) => Ok(Some(reference)),
        Err(error) if classification == OciClassification::Scheme => Err(error),
        Err(_) => Ok(None),
    }
}

/// The error returned when an OCI reference reaches a non-pull code path.
fn oci_pull_unsupported(reference: &OciReference) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("OCI reference '{reference}' cannot be staged as a file source"),
    )
    .with_hint("report this as a firestone bug; an OCI pull has its own pipeline")
}

/// The 64-hex identity digest of a resolved manifest (SPEC §8.5).
fn manifest_identity_digest(
    reference: &OciReference,
    digest: &str,
) -> Result<String, FirestoneError> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| is_lower_hex(hex, 64))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Checksum,
                format!("manifest digest '{digest}' for '{reference}' is not a sha256 digest"),
            )
            .with_hint("retry the pull; the registry returned an unusable manifest digest")
        })
}

/// The declared byte total of every layer, used as the progress denominator.
fn total_layer_bytes(
    reference: &OciReference,
    layers: &[LayerDescriptor],
) -> Result<u64, FirestoneError> {
    layers
        .iter()
        .try_fold(0_u64, |total, descriptor| {
            total.checked_add(descriptor.size)
        })
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("manifest for '{reference}' declares an impossible layer byte total"),
            )
            .with_hint("pull an image whose manifest declares plausible layer sizes")
        })
}

/// The §8.5 ext4 sizing rule: `unpacked × 23 / 20 + 256 MiB`, rounded up to a
/// 4 MiB multiple. Integer arithmetic only, so one manifest always yields the
/// same size on every host.
fn oci_rootfs_bytes(reference: &OciReference, unpacked_bytes: u64) -> Result<u64, FirestoneError> {
    unpacked_bytes
        .checked_mul(23)
        .map(|scaled| scaled / 20)
        .and_then(|scaled| scaled.checked_add(OCI_SIZE_HEADROOM_BYTES))
        .and_then(|sized| sized.checked_next_multiple_of(OCI_SIZE_ALIGNMENT_BYTES))
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "merged root filesystem for '{reference}' is too large to size ({unpacked_bytes} unpacked bytes)"
                ),
            )
            .with_hint("pull a smaller image")
        })
}

/// Creates one owner-only pull partial without following a symlink.
fn create_partial_file(path: &Path) -> Result<File, FirestoneError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SIDECAR_FILE_MODE)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| image_file_error("create", path, source))?;
    file.set_permissions(fs::Permissions::from_mode(SIDECAR_FILE_MODE))
        .map_err(|source| image_file_error("set mode on", path, source))?;
    Ok(file)
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
        Err(Errno::ENOENT | Errno::ENAMETOOLONG) => return Ok(None),
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

fn serialize_image_metadata(
    path: &Path,
    metadata: &ImageMetadata,
) -> Result<Vec<u8>, FirestoneError> {
    let mut bytes = serde_json::to_vec_pretty(metadata).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot serialize image sidecar '{}'", path.display()),
        )
        .with_hint("the image pair was not published")
        .with_source(source)
    })?;
    bytes.push(b'\n');
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SIDECAR_BYTES {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "image sidecar for `{}` exceeds the {} byte limit",
                metadata.id, MAX_SIDECAR_BYTES
            ),
        )
        .with_hint("shorten the canonical image reference before pulling"));
    }
    Ok(bytes)
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
    // `.source`, `.stored` and `.tar` are fixed; an OCI pull adds one
    // `.layer<N>` partial per manifest layer (SPEC §8.5).
    let digest = rest
        .strip_suffix(".source.partial")
        .or_else(|| rest.strip_suffix(".stored.partial"))
        .or_else(|| rest.strip_suffix(".tar.partial"))
        .or_else(|| {
            let body = rest.strip_suffix(".partial")?;
            let (digest, index) = body.rsplit_once(".layer")?;
            (!index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())).then_some(digest)
        });
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
    compat: Option<String>,
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
    compat: Option<String>,
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
        compat: optional_qemu_string(&data_value, "compat", path)?,
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
fn validate_qcow2_structure(label: &str, info: &QemuInfo) -> Result<(), FirestoneError> {
    if info.format != "qcow2" {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} has qemu format '{}'", info.format),
        ));
    }
    // qcow2 compat 0.10 has no incompatible-feature bitmap, so qemu-img 8.2
    // legitimately omits the v3-only corrupt flag while still reporting dirty-flag.
    let corrupt_field_is_healthy = match info.corrupt {
        Some(false) => true,
        None => info.compat.as_deref() == Some("0.10"),
        Some(true) => false,
    };
    if info.dirty_flag.is_none() || !corrupt_field_is_healthy {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} omitted health fields or is marked corrupt by qemu-img"),
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
    validate_qcow2_structure(&format!("stored image `{id}`"), info)?;
    if info.dirty_flag != Some(false) {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("stored image `{id}` is marked dirty by qemu-img"),
        )
        .with_hint("repair or replace the immutable base image before retrying"));
    }
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

fn published_overlay_path(dest_partial: &Path) -> Result<PathBuf, FirestoneError> {
    let published = dest_partial
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".partial"))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "overlay staging path '{}' does not end in '.partial'",
                    dest_partial.display()
                ),
            )
            .with_hint("stage an overlay copy at '<name>.partial' beside its published path")
        })?;
    Ok(dest_partial.with_file_name(published))
}

fn validate_overlay_info(
    overlay: &Path,
    base: &Path,
    requested_size: u64,
    info: &QemuInfo,
) -> Result<(), FirestoneError> {
    validate_qcow2_structure(&format!("overlay '{}'", overlay.display()), info)?;
    let expected = base.to_str().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("base image path '{}' is not UTF-8", base.display()),
        )
    })?;
    // Cloud Hypervisor's qcow2 writer can omit the optional backing-format
    // extension after a forced stop. The base was independently validated as
    // qcow2 above, so an absent hint is safe; an explicit different format is not.
    if info.backing_filename.as_deref() != Some(expected)
        || info.full_backing_filename.as_deref() != Some(expected)
        || info
            .backing_filename_format
            .as_deref()
            .is_some_and(|format| format != "qcow2")
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

/// Uses the pinned qemu-img 8.2.2 JSON inspection argv from verify 4/5.
fn qemu_info_with(qemu_img: &Path, path: &Path) -> Result<QemuInfo, FirestoneError> {
    let output = Cmd::new(qemu_img.as_os_str())
        .arg("info")
        .arg("--output=json")
        .arg("-f")
        .arg("qcow2")
        .arg(path.as_os_str())
        .timeout(QEMU_INFO_TIMEOUT)
        .error_kind(ErrorKind::Dependency)
        .run()?;
    let value = serde_json::from_slice::<serde_json::Value>(output.stdout()).map_err(|source| {
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
    let backing_filename_format = optional_qemu_string(&value, "backing-filename-format", path)?;
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
        compat: format_specific.compat,
        data_file: format_specific.data_file,
        data_file_raw: format_specific.data_file_raw,
    })
}

/// Reports one machine overlay's virtual size, or `None` when the machine
/// has never been started and therefore owns no overlay yet.
///
/// Spec validation needs this before an image store exists, so it takes the
/// resolved qemu-img program directly.
pub fn overlay_virtual_size(
    qemu_img: &Path,
    overlay: &Path,
) -> Result<Option<u64>, FirestoneError> {
    match fs::symlink_metadata(overlay) {
        Ok(_) => Ok(Some(qemu_info_with(qemu_img, overlay)?.virtual_size)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(image_file_error("inspect", overlay, source)),
    }
}

/// The one refusal shared by spec validation and the start-time grow path.
///
/// qcow2 shrink truncates the guest filesystem, so Firestone never performs it.
pub fn disk_shrink_error(name: &str, current: u64, requested: u64) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!(
            "disk shrink is not supported: machine `{name}` already has a {current}-byte overlay and disk requests {requested} bytes"
        ),
    )
    .with_hint(format!(
        "set disk to {current} bytes or more, or create a new machine with the smaller disk"
    ))
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

fn directory_entry_error(directory: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!(
            "cannot read an entry in directory '{}'",
            directory.display()
        ),
    )
    .with_hint("check the Firestone data directory permissions and retry")
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
    use crate::{
        CloudInitSpec, DependencyManifest, Firmware, MachineSpec, MachineStatus, NetMode,
        NetworkPlan, NetworkSpec, PathInputs, ShimTimeouts, StateVersion, VmConfigInput, VmmSpec,
        deps::DIRECT_BOOT_KERNEL_DEPENDENCY, prepare_start, publish_vm_config,
    };

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

        /// Removes the next queued reply for one URL, so a test can leave a
        /// request unanswered or replace its body.
        fn take(&self, url: &str) -> Result<(), Box<dyn std::error::Error>> {
            let mut replies = self
                .replies
                .lock()
                .map_err(|_| io::Error::other("scripted HTTP mutex poisoned"))?;
            replies
                .get_mut(url)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| io::Error::other(format!("no scripted reply queued for '{url}'")))?;
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
        mkfs_log: PathBuf,
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
            let mkfs_ext4 = root.join("mkfs.ext4");
            let mkfs_log = root.join("mkfs.log");
            write_fake_mkfs(&mkfs_ext4, &mkfs_log, &root.join("mkfs-input.tar"))?;
            let http = Arc::new(ScriptedHttp::default());
            let store = ImageStore {
                paths: paths.clone(),
                catalog: Catalog::built_in()?,
                architecture: Arch::X86_64,
                qemu_img,
                http: http.clone(),
                clock: Arc::new(FixedClock),
                insecure_registries: Vec::new(),
                mkfs_ext4: Some(mkfs_ext4),
                init_payload: test_init_payload_source(),
            };
            Ok(Self {
                _test_lock: test_lock,
                _directory: directory,
                root,
                paths,
                qemu_log,
                mkfs_log,
                http,
                store,
            })
        }

        /// Everything the fake `mkfs.ext4` was invoked with, one line per run.
        fn mkfs_invocations(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
            match fs::read_to_string(&self.mkfs_log) {
                Ok(text) => Ok(text.lines().map(ToOwned::to_owned).collect()),
                Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(source) => Err(source.into()),
            }
        }

        /// The canonical tar the last `mkfs.ext4` run consumed.
        fn mkfs_input_tar(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            Ok(fs::read(self.root.join("mkfs-input.tar"))?)
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

    /// Bytes the tests inject in place of the embedded `firestone-init`.
    const TEST_INIT_PAYLOAD: &[u8] = b"\x7fELF-fake-firestone-init";
    /// Creating this file next to the fake `mkfs.ext4` makes it exit non-zero.
    const MKFS_FAILURE_MARKER: &str = "mkfs.fail";

    fn test_init_payload_source() -> InitPayloadSource {
        Arc::new(|| Ok(TEST_INIT_PAYLOAD.to_vec()))
    }

    /// A build that carries no `firestone-init`, which every plain `cargo
    /// build` is until the standalone release embeds one.
    fn missing_init_payload_source() -> InitPayloadSource {
        Arc::new(|| {
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                "this build carries no embedded firestone-init payload",
            ))
        })
    }

    /// Writes a recording stand-in for the pinned static `mkfs.ext4`.
    ///
    /// The real helper is Linux-only, so the unit path drives it through the
    /// same `Cmd` seam: the fake logs its argv, copies the tar it was handed
    /// aside for inspection, and writes a small deterministic image so the
    /// fake `qemu-img convert` has bytes to read.
    fn write_fake_mkfs(
        path: &Path,
        log: &Path,
        tar_copy: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let log_literal = serde_json::to_string(&log.to_string_lossy())?;
        let copy_literal = serde_json::to_string(&tar_copy.to_string_lossy())?;
        let fail_literal = serde_json::to_string(
            &path
                .with_file_name(MKFS_FAILURE_MARKER)
                .to_string_lossy()
                .into_owned(),
        )?;
        let script = r#"#!/usr/bin/env python3
import pathlib
import shutil
import sys

args = sys.argv[1:]
log = pathlib.Path(__LOG__)
with log.open("a", encoding="utf-8") as output:
    output.write(" ".join(args) + "\n")

if pathlib.Path(__FAIL__).exists():
    sys.exit(9)
shutil.copyfile(args[4], __COPY__)
pathlib.Path(args[7]).write_bytes(b"EXT4FAKE " + args[8].encode() + b"\n")
"#
        .replace("__LOG__", &log_literal)
        .replace("__COPY__", &copy_literal)
        .replace("__FAIL__", &fail_literal);
        fs::write(path, script)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
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
    if b"DIRTY" in data:
        info["dirty-flag"] = True
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

    /// One gzip layer blob built from `(path, contents)` pairs.
    ///
    /// A `None` body writes a directory entry, so a fixture can describe a
    /// small but realistic root filesystem.
    fn gzip_layer(entries: &[(&str, Option<&str>)]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in entries {
            let body = contents.unwrap_or_default().as_bytes();
            let mut header = tar::Header::new_ustar();
            header.set_path(path)?;
            header.set_entry_type(if contents.is_some() {
                tar::EntryType::Regular
            } else {
                tar::EntryType::Directory
            });
            header.set_mode(if contents.is_some() { 0o644 } else { 0o755 });
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_size(body.len() as u64);
            header.set_cksum();
            builder.append(&header, body)?;
        }
        let uncompressed = builder.into_inner()?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&uncompressed)?;
        Ok(encoder.finish()?)
    }

    /// What one scripted registry image published, for later assertions.
    struct FakeOciImage {
        manifest_digest: String,
        config_digest: String,
        layer_digests: Vec<String>,
    }

    fn oci_config_json() -> String {
        r#"{"architecture":"amd64","os":"linux","config":{"Env":["PATH=/usr/bin"],"Entrypoint":["/entry.sh"],"Cmd":["serve"],"WorkingDir":"/srv","User":"app"},"rootfs":{"type":"layers","diff_ids":[]}}"#
            .to_owned()
    }

    /// Queues the manifest, config, and layer replies for one image pull.
    ///
    /// The reference is parsed and normalized exactly as the pull will, so the
    /// scripted URLs are the ones the registry client asks for.
    fn script_oci_image(
        fixture: &Fixture,
        reference: &str,
        layers: &[Vec<u8>],
        config: &str,
    ) -> Result<FakeOciImage, Box<dyn std::error::Error>> {
        let parsed = OciReference::parse(reference)?;
        let host = crate::oci::registry::registry_endpoint_host(parsed.registry()).to_owned();
        let repository = parsed.repository().to_owned();
        let config_digest = format!("sha256:{}", sha256_bytes(config.as_bytes()));
        let layer_digests = layers
            .iter()
            .map(|layer| format!("sha256:{}", sha256_bytes(layer)))
            .collect::<Vec<_>>();
        let descriptors = layers
            .iter()
            .zip(&layer_digests)
            .map(|(layer, digest)| {
                format!(
                    r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{digest}","size":{}}}"#,
                    layer.len()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{descriptors}]}}"#,
            crate::oci::registry::MEDIA_TYPE_OCI_MANIFEST,
            config.len()
        );
        let manifest_digest = format!("sha256:{}", sha256_bytes(manifest.as_bytes()));
        let target = match parsed.reference() {
            crate::oci::OciTagOrDigest::Tag(tag) => tag.clone(),
            crate::oci::OciTagOrDigest::Digest(digest) => digest.clone(),
        };
        fixture.http.push(
            &format!("https://{host}/v2/{repository}/manifests/{target}"),
            manifest.clone().into_bytes(),
            None,
            Some(crate::oci::registry::MEDIA_TYPE_OCI_MANIFEST),
        )?;
        fixture.http.push(
            &format!("https://{host}/v2/{repository}/blobs/{config_digest}"),
            config.as_bytes().to_vec(),
            None,
            Some("application/vnd.oci.image.config.v1+json"),
        )?;
        for (layer, digest) in layers.iter().zip(&layer_digests) {
            fixture.http.push(
                &format!("https://{host}/v2/{repository}/blobs/{digest}"),
                layer.clone(),
                Some(layer.len() as u64),
                None,
            )?;
        }
        Ok(FakeOciImage {
            manifest_digest,
            config_digest,
            layer_digests,
        })
    }

    fn oci_request(reference: &str, source_base: &Path) -> ImagePullRequest {
        ImagePullRequest::new(ImageRef::new(reference), source_base)
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
            insecure_registries: Vec::new(),
            mkfs_ext4: fixture.store.mkfs_ext4.clone(),
            init_payload: Arc::clone(&fixture.store.init_payload),
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

    fn direct_boot_kernel_manifest(
        url: &str,
        bytes: &[u8],
        firmware: Option<(&str, &str, &[u8])>,
    ) -> Result<DependencyManifest, FirestoneError> {
        let mut text = format!(
            "manifest_version = 1\n\n[dependency.cloud-hypervisor-kernel]\nversion = \"ch-test\"\navailability = \"binary\"\n[dependency.cloud-hypervisor-kernel.x86_64]\nasset = \"bzImage-x86_64\"\ninstall_name = \"bzImage-ch-test\"\nurl = \"{url}\"\nsha256 = \"{}\"\n",
            sha256_bytes(bytes)
        );
        if let Some((install_name, firmware_url, firmware_bytes)) = firmware {
            let _ = write!(
                text,
                "\n[dependency.cloud-hypervisor-edk2]\nversion = \"ch-test\"\navailability = \"binary\"\n[dependency.cloud-hypervisor-edk2.x86_64]\nasset = \"CLOUDHV.fd\"\ninstall_name = \"{install_name}\"\nurl = \"{firmware_url}\"\nsha256 = \"{}\"\n",
                sha256_bytes(firmware_bytes)
            );
        }
        DependencyManifest::parse(&text)
    }

    /// SPEC §9.5: an OCI start publishes the pinned kernel through the same
    /// locked, hash-verified publisher as a firmware, mode 0644, before
    /// `vmconfig.json` exists. This drives the exact sequence the shim runs
    /// (§9.3 step 6c) without KVM; the OCI pull itself lands with M6-15.
    #[test]
    fn oci_start_publishes_the_pinned_kernel_before_vmconfig()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let kernel = b"pinned-direct-boot-kernel";
        let url = "https://kernel.example.invalid/bzImage-x86_64";
        let manifest = direct_boot_kernel_manifest(url, kernel, None)?;
        fixture.http.push(
            url,
            kernel.to_vec(),
            Some(kernel.len() as u64),
            Some("application/octet-stream"),
        )?;
        let name = "oci-boot";
        let state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: "docker.io/library/nginx:latest".to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let mut events = Vec::new();
        let _lock = create_machine(&fixture.paths, name, &state, &mut events)?;
        let spec = MachineSpec {
            image: ImageRef::new("docker.io/library/nginx:latest"),
            arch: Some(Arch::X86_64),
            network: NetworkSpec {
                mode: NetMode::None,
                ..NetworkSpec::default()
            },
            ..MachineSpec::default()
        };
        let input = VmConfigInput {
            name,
            spec: &spec,
            state: &state,
            network: &NetworkPlan::None,
            filesystems: &[],
            architecture: Arch::X86_64,
            catalog_firmware: None,
            image_kind: ImageKind::Oci,
        };

        // Without the install, the direct-boot payload cannot be published.
        let error = publish_vm_config(&fixture.paths, &manifest, input)
            .err()
            .ok_or("an uninstalled kernel must refuse vmconfig publication")?;
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(!fixture.paths.machine_vmconfig(name)?.exists());

        let artifact = crate::vmm::selected_pinned_boot_artifact(
            &manifest,
            &spec.vmm.firmware,
            Arch::X86_64,
            None,
            ImageKind::Oci,
        )?
        .ok_or("an OCI start must select the pinned kernel")?;
        assert_eq!(artifact.dependency, DIRECT_BOOT_KERNEL_DEPENDENCY);
        let installed = fixture.store.ensure_pinned_artifact(&artifact)?;

        assert_eq!(fs::read(&installed)?, kernel);
        assert_eq!(
            fs::symlink_metadata(&installed)?.permissions().mode() & 0o7777,
            0o644
        );
        let config = publish_vm_config(&fixture.paths, &manifest, input)?;
        let published: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.paths.machine_vmconfig(name)?)?)?;
        assert_eq!(
            published["payload"]["kernel"],
            installed.to_string_lossy().as_ref()
        );
        assert_eq!(
            published["payload"]["cmdline"],
            "console=hvc0 console=ttyS0 root=/dev/vda rw init=/sbin/firestone-init"
        );
        assert!(published["payload"].get("firmware").is_none());
        assert_eq!(
            fs::read(fixture.paths.machine_vmconfig(name)?)?,
            config.as_bytes()
        );
        Ok(())
    }

    /// A firmware machine never fetches the direct-boot kernel: the scripted
    /// transport would fail the start if it tried (§9.5).
    #[test]
    fn firmware_start_never_installs_the_direct_boot_kernel()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let kernel = b"unused-direct-boot-kernel";
        let kernel_url = "https://kernel.example.invalid/bzImage-x86_64";
        let firmware = b"disk-machine-edk2";
        let firmware_url = "https://firmware.example.invalid/CLOUDHV.fd";
        let manifest = direct_boot_kernel_manifest(
            kernel_url,
            kernel,
            Some(("CLOUDHV-disk-start.fd", firmware_url, firmware)),
        )?;
        fixture.http.push(
            firmware_url,
            firmware.to_vec(),
            Some(firmware.len() as u64),
            Some("application/octet-stream"),
        )?;

        prepare_firmware_start(&fixture, "disk-start", Firmware::EDK2, &manifest)?;

        let kernel_artifact = manifest.direct_boot_kernel("x86_64")?;
        assert!(
            !fixture
                .paths
                .binary_file(&kernel_artifact.install_name)?
                .exists()
        );
        let vmconfig: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.paths.machine_vmconfig("disk-start")?)?)?;
        assert_eq!(
            vmconfig["payload"]["firmware"],
            fixture
                .paths
                .binary_file("CLOUDHV-disk-start.fd")?
                .to_string_lossy()
                .as_ref()
        );
        assert!(vmconfig["payload"].get("cmdline").is_none());
        Ok(())
    }

    /// A version-one sidecar has no `kind`, reads as a disk image, and keeps
    /// its exact bytes; an `oci` sidecar round-trips and validates (§9.5).
    #[test]
    fn sidecar_without_kind_reads_as_disk_and_an_oci_kind_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = sha256_bytes(b"oci-rootfs");
        let reference = "docker.io/library/nginx:latest";
        let local = "/srv/images/base.qcow2";
        let sidecar = |kind: &str, source_ref: &str, source_url: &str, firmware: &str| {
            let id = stable_image_id(
                source_ref,
                (source_url != "null").then_some(source_url),
                Arch::X86_64,
                &digest,
            );
            format!(
                r#"{{"version":1,{kind}"id":"{id}","generation":1,"source_ref":"{source_ref}","source_url":{source_url},"source_sha256":"{digest}","stored_sha256":"{digest}","architecture":"x86_64","firmware":{firmware},"source_format":"qcow2","stored_format":"qcow2","verification_algorithm":null,"verification_digest":null,"size":8,"pulled_at":"{FIXED_TIME}"}}"#
            )
        };

        let disk = serde_json::from_str::<ImageMetadata>(&sidecar("", local, "null", "null"))?;
        assert_eq!(disk.kind, ImageKind::Disk);
        assert_eq!(disk.kind.as_str(), "disk");
        disk.validate()?;
        assert!(!String::from_utf8(serde_json::to_vec(&disk)?)?.contains("kind"));

        let oci = serde_json::from_str::<ImageMetadata>(&sidecar(
            r#""kind":"oci","#,
            reference,
            "null",
            "null",
        ))?;
        assert_eq!(oci.kind, ImageKind::Oci);
        assert_eq!(oci.kind.as_str(), "oci");
        oci.validate()?;
        assert!(String::from_utf8(serde_json::to_vec(&oci)?)?.contains(r#""kind":"oci""#));

        for (kind, source_ref, source_url, firmware, reason) in [
            (
                r#""kind":"oci","#,
                local,
                "null",
                "null",
                "an oci image source_ref must be a normalized OCI reference",
            ),
            (
                r#""kind":"oci","#,
                "nginx:latest",
                "null",
                "null",
                "an oci image source_ref must be a normalized OCI reference",
            ),
            (
                r#""kind":"oci","#,
                reference,
                r#""https://images.example.invalid/base.qcow2""#,
                "null",
                "an oci image must not carry a source_url",
            ),
            (
                r#""kind":"oci","#,
                reference,
                "null",
                r#""edk2""#,
                "oci image firmware must be null",
            ),
        ] {
            let metadata = serde_json::from_str::<ImageMetadata>(&sidecar(
                kind, source_ref, source_url, firmware,
            ))?;
            let error = metadata
                .validate()
                .err()
                .ok_or("an inconsistent oci sidecar must be rejected")?;
            assert_eq!(error.kind(), ErrorKind::Dependency);
            assert!(
                error.message().contains(reason),
                "unexpected message: {}",
                error.message()
            );
        }
        Ok(())
    }

    /// Publishes one OCI image pair straight into the store, the way the pull
    /// pipeline will once M6-15 lands, so `prepare_start` can be driven through
    /// its OCI branch with the fake VMM today (SPEC §8.5, §10.5).
    fn write_stored_oci_image(
        fixture: &Fixture,
        reference: &str,
        oci: Option<OciSidecar>,
    ) -> Result<(String, String), Box<dyn std::error::Error>> {
        let bytes = b"QFI\xFBOCI-ROOTFS".to_vec();
        let digest = sha256_bytes(&bytes);
        let id = stable_image_id(reference, None, Arch::X86_64, &digest);
        let metadata = ImageMetadata {
            version: ImageMetadataVersion,
            id: id.clone(),
            kind: ImageKind::Oci,
            oci,
            generation: 1,
            source_ref: reference.to_owned(),
            source_url: None,
            source_sha256: digest.clone(),
            stored_sha256: digest.clone(),
            architecture: Arch::X86_64,
            firmware: None,
            sshd_path: SshdPath::default(),
            source_format: ImageFormat::Qcow2,
            stored_format: ImageFormat::Qcow2,
            verification_algorithm: None,
            verification_digest: None,
            size: bytes.len() as u64,
            pulled_at: FIXED_TIME.to_owned(),
        };
        fixture.paths.ensure_owned_data_directory(
            fixture.paths.data_dir(),
            "data directory",
            true,
        )?;
        fixture.paths.ensure_owned_data_directory(
            &fixture.paths.images_dir(),
            "images directory",
            false,
        )?;
        let base = fixture.paths.image_base(&id)?;
        fs::write(&base, &bytes)?;
        fs::set_permissions(&base, fs::Permissions::from_mode(BASE_FILE_MODE))?;
        let sidecar = fixture.paths.image_metadata(&id)?;
        fs::write(&sidecar, serde_json::to_vec(&metadata)?)?;
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(SIDECAR_FILE_MODE))?;
        Ok((id, digest))
    }

    fn oci_sidecar(reference: &str) -> OciSidecar {
        OciSidecar {
            registry_ref: reference.to_owned(),
            manifest_digest: format!("sha256:{}", "1".repeat(64)),
            config_digest: format!("sha256:{}", "2".repeat(64)),
            entrypoint: vec!["/docker-entrypoint.sh".to_owned()],
            cmd: vec![
                "nginx".to_owned(),
                "-g".to_owned(),
                "daemon off;".to_owned(),
            ],
            env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_owned()],
            workdir: Some("/".to_owned()),
            user: Some("root".to_owned()),
            boot: crate::image::FIRESTONE_INIT_BOOT.to_owned(),
        }
    }

    /// SPEC §10.5: `prepare_start` branches on the pinned image's kind. An OCI
    /// machine publishes the `firestone-init` config disk into the §9.2 seed
    /// slot and never renders a cloud-init seed.
    #[test]
    fn prepare_start_oci_image_publishes_the_config_disk_instead_of_a_seed()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let kernel = b"pinned-direct-boot-kernel";
        let url = "https://kernel.example.invalid/bzImage-x86_64";
        let manifest = direct_boot_kernel_manifest(url, kernel, None)?;
        fixture.http.push(
            url,
            kernel.to_vec(),
            Some(kernel.len() as u64),
            Some("application/octet-stream"),
        )?;
        let reference = "docker.io/library/nginx:latest";
        let (id, digest) =
            write_stored_oci_image(&fixture, reference, Some(oci_sidecar(reference)))?;
        let name = "oci-config";
        let state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: reference.to_owned(),
                id: Some(id),
                sha256: Some(digest),
            },
        )?;
        let spec = MachineSpec {
            image: ImageRef::new(reference),
            arch: Some(Arch::X86_64),
            network: NetworkSpec {
                mode: NetMode::None,
                ..NetworkSpec::default()
            },
            vmm: VmmSpec {
                binary: Some(fixture.store.qemu_img.clone()),
                ..VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let mut events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut events)?;

        let prepared = prepare_start(
            &fixture.paths,
            &fixture.store,
            &manifest,
            name,
            &spec,
            state,
            &fixture.root,
            &lock,
            &mut events,
            ShimTimeouts::default(),
        )?;

        let config_image = fixture.paths.machine_config_image(name)?;
        assert!(config_image.exists());
        assert!(!fixture.paths.machine_seed_image(name)?.exists());
        assert!(!fixture.paths.machine_seed_dir(name)?.exists());
        let document = fixture.paths.machine_config_file(name, "config.json")?;
        let inspected: serde_json::Value = serde_json::from_slice(&fs::read(&document)?)?;
        assert_eq!(inspected["hostname"], name);
        assert_eq!(inspected["network"], "none");
        assert_eq!(inspected["entrypoint"][0], "/docker-entrypoint.sh");

        let decoded = firestone_initproto::decode_frame(&fs::read(&config_image)?)?;
        assert_eq!(decoded.hostname, name);
        assert_eq!(decoded.user.as_deref(), Some("root"));

        let identity = prepared
            .state()
            .instance_id
            .clone()
            .ok_or("an OCI start records a config identity")?;
        assert!(identity.starts_with("iid-oci-config-"));
        assert!(prepared.seed_rewritten());

        let vmconfig: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.paths.machine_vmconfig(name)?)?)?;
        assert_eq!(
            vmconfig["disks"][1]["path"],
            config_image.to_string_lossy().as_ref()
        );
        assert_eq!(vmconfig["disks"][1]["readonly"], true);
        assert_eq!(vmconfig["disks"][1]["image_type"], "Raw");
        assert_eq!(vmconfig["disks"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    /// An `oci` sidecar with no runtime object cannot describe a child, so the
    /// start fails with a dependency error naming the re-pull rather than
    /// booting a machine with an empty entrypoint (SPEC §8.5, §10.5).
    #[test]
    fn prepare_start_oci_image_without_a_runtime_config_reports_a_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let kernel = b"pinned-direct-boot-kernel";
        let url = "https://kernel.example.invalid/bzImage-x86_64";
        let manifest = direct_boot_kernel_manifest(url, kernel, None)?;
        fixture.http.push(
            url,
            kernel.to_vec(),
            Some(kernel.len() as u64),
            Some("application/octet-stream"),
        )?;
        let reference = "docker.io/library/alpine:3.22";
        let (id, digest) = write_stored_oci_image(&fixture, reference, None)?;
        let name = "oci-bare";
        let state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: reference.to_owned(),
                id: Some(id),
                sha256: Some(digest),
            },
        )?;
        let spec = MachineSpec {
            image: ImageRef::new(reference),
            arch: Some(Arch::X86_64),
            network: NetworkSpec {
                mode: NetMode::None,
                ..NetworkSpec::default()
            },
            vmm: VmmSpec {
                binary: Some(fixture.store.qemu_img.clone()),
                ..VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let mut events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut events)?;

        let error = prepare_start(
            &fixture.paths,
            &fixture.store,
            &manifest,
            name,
            &spec,
            state,
            &fixture.root,
            &lock,
            &mut events,
            ShimTimeouts::default(),
        )
        .err()
        .ok_or("an OCI image without a runtime config must refuse to start")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error.message().contains("no OCI runtime configuration"),
            "{}",
            error.message()
        );
        assert!(!fixture.paths.machine_config_image(name)?.exists());
        assert!(!fixture.paths.machine_vmconfig(name)?.exists());
        Ok(())
    }

    fn firmware_manifest(
        dependency: &str,
        install_name: &str,
        url: &str,
        bytes: &[u8],
    ) -> Result<DependencyManifest, FirestoneError> {
        DependencyManifest::parse(&format!(
            "manifest_version = 1\n\n[dependency.{dependency}]\nversion = \"test\"\navailability = \"binary\"\n[dependency.{dependency}.x86_64]\nasset = \"firmware\"\ninstall_name = \"{install_name}\"\nurl = \"{url}\"\nsha256 = \"{}\"\n",
            sha256_bytes(bytes)
        ))
    }

    fn prepare_firmware_start(
        fixture: &Fixture,
        name: &str,
        firmware: Firmware,
        manifest: &DependencyManifest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let source = fixture.write_source(&format!("{name}.raw"), b"raw-machine-image")?;
        let state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: source.to_string_lossy().into_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let spec = MachineSpec {
            image: ImageRef::new(source.to_string_lossy().into_owned()),
            arch: Some(Arch::X86_64),
            network: NetworkSpec {
                mode: NetMode::None,
                ..NetworkSpec::default()
            },
            cloud_init: CloudInitSpec {
                provisioning: false,
                ..CloudInitSpec::default()
            },
            vmm: VmmSpec {
                binary: Some(fixture.store.qemu_img.clone()),
                firmware,
                ..VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let mut events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut events)?;
        let prepared = prepare_start(
            &fixture.paths,
            &fixture.store,
            manifest,
            name,
            &spec,
            state,
            &fixture.root,
            &lock,
            &mut events,
            ShimTimeouts::default(),
        )?;
        assert_eq!(prepared.state().status, MachineStatus::Created);
        Ok(())
    }

    #[test]
    fn first_start_missing_named_firmware_is_securely_installed_before_vmconfig()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let firmware = b"first-start-edk2";
        let url = "https://firmware.example.invalid/CLOUDHV.fd";
        let manifest = firmware_manifest(
            "cloud-hypervisor-edk2",
            "CLOUDHV-first-start.fd",
            url,
            firmware,
        )?;
        fixture.http.push(
            url,
            firmware.to_vec(),
            Some(firmware.len() as u64),
            Some("application/octet-stream"),
        )?;

        prepare_firmware_start(&fixture, "first-firmware", Firmware::EDK2, &manifest)?;

        let artifact = manifest.artifact("cloud-hypervisor-edk2", "x86_64")?;
        let installed = fixture.paths.binary_file(&artifact.install_name)?;
        assert_eq!(fs::read(&installed)?, firmware);
        assert_eq!(
            fs::metadata(&installed)?.permissions().mode() & 0o7777,
            0o644
        );
        let vmconfig: serde_json::Value = serde_json::from_slice(&fs::read(
            fixture.paths.machine_vmconfig("first-firmware")?,
        )?)?;
        assert_eq!(
            vmconfig["payload"]["firmware"],
            installed.to_string_lossy().as_ref()
        );
        Ok(())
    }

    #[test]
    fn first_start_custom_firmware_does_not_install_or_modify_pinned_artifacts()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let custom = fixture.write_source("custom.fd", b"custom firmware")?;
        let manifest = firmware_manifest(
            "cloud-hypervisor-edk2",
            "CLOUDHV-unused.fd",
            "https://firmware.example.invalid/unused.fd",
            b"unused pinned firmware",
        )?;

        prepare_firmware_start(
            &fixture,
            "custom-firmware",
            Firmware::path(custom.clone())?,
            &manifest,
        )?;

        assert_eq!(fs::read(&custom)?, b"custom firmware");
        assert!(!fixture.paths.bin_dir().exists());
        let vmconfig: serde_json::Value = serde_json::from_slice(&fs::read(
            fixture.paths.machine_vmconfig("custom-firmware")?,
        )?)?;
        assert_eq!(
            vmconfig["payload"]["firmware"],
            custom.to_string_lossy().as_ref()
        );
        Ok(())
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
        let image_url = "https://cloud.debian.org/images/cloud/bookworm/20260821-2577/debian-12-genericcloud-amd64-20260821-2577.qcow2";
        let manifest_url =
            "https://cloud.debian.org/images/cloud/bookworm/20260821-2577/SHA512SUMS";
        fixture.http.push(
            manifest_url,
            format!("{digest}  debian-12-genericcloud-amd64-20260821-2577.qcow2\n").into_bytes(),
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
        let old_image_url = "https://cloud-images.ubuntu.com/releases/noble/release-20260826/ubuntu-24.04-server-cloudimg-amd64.img";
        let old_manifest_url =
            "https://cloud-images.ubuntu.com/releases/noble/release-20260826/SHA256SUMS";
        fixture.http.push(
            old_manifest_url,
            format!("{old_sha256}  ubuntu-24.04-server-cloudimg-amd64.img\n").into_bytes(),
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
            insecure_registries: Vec::new(),
            mkfs_ext4: None,
            init_payload: test_init_payload_source(),
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
        let image_url = "https://cloud.debian.org/images/cloud/bookworm/20260821-2577/debian-12-genericcloud-amd64-20260821-2577.qcow2";
        let manifest_url =
            "https://cloud.debian.org/images/cloud/bookworm/20260821-2577/SHA512SUMS";
        fixture.http.push(
            manifest_url,
            format!("{digest}  debian-12-genericcloud-amd64-20260821-2577.qcow2\n").into_bytes(),
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
        assert_eq!(prepared.image.firmware, Some(CatalogFirmware::Edk2));
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
            insecure_registries: Vec::new(),
            mkfs_ext4: None,
            init_payload: test_init_payload_source(),
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
    fn image_reference_snapshot_metadata_pins_the_base_like_a_machine_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let source = fixture.write_source("pinned.qcow2", b"QFI\xFBPINNED")?;
        let mut events = Vec::new();
        let image = fixture
            .store
            .pull(&local_request(&source, &fixture.root), &mut events)?;

        // The machine itself pins nothing: only its snapshot does.
        let state = machine_state(
            &fixture.paths,
            "rolled",
            StateImage {
                r#ref: source.to_string_lossy().into_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let mut lock_events = Vec::new();
        let _lock = create_machine(&fixture.paths, "rolled", &state, &mut lock_events)?;

        let snapshots = fixture.paths.machine_snapshots_dir("rolled")?;
        crate::snapshot::ensure_snapshot_directory(&snapshots)?;
        for snapshot in ["snap-20260902-123456", ".partial-snap-20260902-125959"] {
            let directory = snapshots.join(snapshot);
            crate::snapshot::ensure_snapshot_directory(&directory)?;
            crate::atomic::write_json_with_mode(
                &Paths::snapshot_metadata(&directory),
                &crate::snapshot::SnapshotMetadata {
                    schema_version: crate::snapshot::SNAPSHOT_SCHEMA_VERSION,
                    kind: crate::snapshot::SnapshotKind::Cold,
                    created_at: "2026-09-02T12:34:56Z".to_owned(),
                    image_id: Some(image.metadata.id.clone()),
                    firestone_version: "0.1.4".to_owned(),
                    disk_bytes: 4096,
                    memory_bytes: None,
                },
                0o600,
            )?;
        }

        assert_eq!(
            fixture.store.referencing_machines(&image.metadata.id)?,
            vec!["rolled".to_owned()]
        );
        let pruned = fixture.store.prune()?;
        assert!(pruned.removed.is_empty(), "{pruned:?}");
        assert!(fixture.paths.image_base(&image.metadata.id)?.exists());

        let refusal = fixture
            .store
            .remove(&image.metadata.id, false)
            .err()
            .ok_or("expected a snapshot-referenced image refusal")?;
        assert_eq!(refusal.kind(), ErrorKind::Conflict);
        assert!(refusal.message().contains("rolled"), "{refusal}");
        Ok(())
    }

    #[test]
    fn prune_unreferenced_keeps_machine_and_snapshot_pinned_images_in_both_modes()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let mut events = Vec::new();
        let pinned_source = fixture.write_source("pinned.qcow2", b"QFI\xFBSTATE-PINNED")?;
        let snapshotted_source =
            fixture.write_source("snapshotted.qcow2", b"QFI\xFBSNAP-PINNED")?;
        let loose_source = fixture.write_source("loose.qcow2", b"QFI\xFBLOOSE")?;
        let pinned = fixture
            .store
            .pull(&local_request(&pinned_source, &fixture.root), &mut events)?;
        let snapshotted = fixture.store.pull(
            &local_request(&snapshotted_source, &fixture.root),
            &mut events,
        )?;
        let loose = fixture
            .store
            .pull(&local_request(&loose_source, &fixture.root), &mut events)?;

        let pinned_state = machine_state(
            &fixture.paths,
            "pinned",
            StateImage {
                r#ref: pinned_source.to_string_lossy().into_owned(),
                id: Some(pinned.metadata.id.clone()),
                sha256: Some(pinned.metadata.source_sha256.clone()),
            },
        )?;
        let mut lock_events = Vec::new();
        let _pinned_lock =
            create_machine(&fixture.paths, "pinned", &pinned_state, &mut lock_events)?;
        let rolled_state = machine_state(
            &fixture.paths,
            "rolled",
            StateImage {
                r#ref: snapshotted_source.to_string_lossy().into_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let _rolled_lock =
            create_machine(&fixture.paths, "rolled", &rolled_state, &mut lock_events)?;
        let snapshots = fixture.paths.machine_snapshots_dir("rolled")?;
        crate::snapshot::ensure_snapshot_directory(&snapshots)?;
        let snapshot_dir = snapshots.join("snap-20260902-123456");
        crate::snapshot::ensure_snapshot_directory(&snapshot_dir)?;
        crate::atomic::write_json_with_mode(
            &Paths::snapshot_metadata(&snapshot_dir),
            &crate::snapshot::SnapshotMetadata {
                schema_version: crate::snapshot::SNAPSHOT_SCHEMA_VERSION,
                kind: crate::snapshot::SnapshotKind::Cold,
                created_at: "2026-09-02T12:34:56Z".to_owned(),
                image_id: Some(snapshotted.metadata.id.clone()),
                firestone_version: "0.1.4".to_owned(),
                disk_bytes: 4096,
                memory_bytes: None,
            },
            0o600,
        )?;

        let planned = fixture.store.prune_unreferenced(true)?;
        assert_eq!(
            planned
                .iter()
                .map(|artifact| artifact.id.as_str())
                .collect::<Vec<_>>(),
            vec![loose.metadata.id.as_str()]
        );
        assert!(planned.iter().all(|artifact| artifact.bytes > 0));
        assert!(fixture.paths.image_base(&loose.metadata.id)?.exists());

        let acted = fixture.store.prune_unreferenced(false)?;
        assert_eq!(acted, planned);
        assert!(!fixture.paths.image_base(&loose.metadata.id)?.exists());
        assert!(fixture.paths.image_base(&pinned.metadata.id)?.exists());
        assert!(fixture.paths.image_base(&snapshotted.metadata.id)?.exists());
        assert!(fixture.store.prune_unreferenced(false)?.is_empty());
        Ok(())
    }

    #[test]
    fn prune_partials_measures_interrupted_downloads_before_removing_them()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        fixture.store.ensure_store()?;
        let digest = "a".repeat(64);
        let partial = fixture
            .paths
            .image_file(&format!(".pull-{digest}.stored.partial"))?;
        fs::write(&partial, vec![7_u8; 8192])?;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o600))?;
        let kept = fixture.paths.image_file("keep-me.txt")?;
        fs::write(&kept, b"not a partial")?;
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o600))?;

        let planned = fixture.store.prune_partials(true)?;
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].id, format!(".pull-{digest}.stored.partial"));
        assert!(planned[0].bytes >= 8192, "{planned:?}");
        assert!(partial.exists());

        let acted = fixture.store.prune_partials(false)?;
        assert_eq!(acted, planned);
        assert!(!partial.exists());
        assert!(kept.exists());
        assert!(fixture.store.prune_partials(false)?.is_empty());
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
            insecure_registries: Vec::new(),
            mkfs_ext4: None,
            init_payload: test_init_payload_source(),
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
    fn published_overlay_path_requires_a_partial_suffix() -> Result<(), FirestoneError> {
        assert_eq!(
            super::published_overlay_path(Path::new("/data/machines/dev/disk.qcow2.partial"))?,
            PathBuf::from("/data/machines/dev/disk.qcow2")
        );
        assert_eq!(
            super::published_overlay_path(Path::new(
                "/data/machines/dev/snapshots/a.qcow2.partial"
            ))?,
            PathBuf::from("/data/machines/dev/snapshots/a.qcow2")
        );
        for rejected in ["/data/machines/dev/disk.qcow2", "/data/dev/.partial", "/"] {
            assert!(
                super::published_overlay_path(Path::new(rejected)).is_err(),
                "accepted {rejected}"
            );
        }
        Ok(())
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
            compat: Some("1.1".to_owned()),
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
        let mut legacy = healthy();
        legacy.compat = Some("0.10".to_owned());
        legacy.corrupt = None;
        validate_base_info("legacy", &legacy)?;
        let mut missing_corrupt = healthy();
        missing_corrupt.corrupt = None;
        assert_eq!(
            validate_base_info("missing-corrupt", &missing_corrupt)
                .err()
                .ok_or("missing v3 corrupt flag accepted")?
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
        let mut dirty_overlay = valid_overlay.clone();
        dirty_overlay.dirty_flag = Some(true);
        validate_overlay_info(overlay, base, 4096, &dirty_overlay)?;
        let mut inferred_backing_format = valid_overlay.clone();
        inferred_backing_format.backing_filename_format = None;
        validate_overlay_info(overlay, base, 4096, &inferred_backing_format)?;
        let mut corrupt_overlay = valid_overlay.clone();
        corrupt_overlay.corrupt = Some(true);
        assert!(validate_overlay_info(overlay, base, 4096, &corrupt_overlay).is_err());
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
            assert_eq!(error.kind(), ErrorKind::Checksum);
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
                    sshd_path: SshdPath::default(),
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
    fn catalog_guest_boot_metadata_is_durable_across_cache_upgrade_and_catalog_removal()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://firmware.example.invalid/base.qcow2";
        let bytes = b"QFI\xFBFIRMWARE".to_vec();
        let digest = sha256_bytes(&bytes);
        let catalog_source = |firmware: &str, sshd_path: &str| {
            format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"firm\"\n",
                    "version = \"1\"\n",
                    "aliases = []\n",
                    "default = true\n",
                    r#"firmware = "{}"
format = "qcow2"
sshd_path = "{}"

"#,
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "sha256 = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                firmware, sshd_path, url, digest,
            )
        };
        let old_catalog = custom_catalog(
            &fixture.root,
            "firm-rhf",
            &catalog_source("rhf", "/usr/sbin/sshd"),
        )?;
        let old_store = store_with_catalog(&fixture, old_catalog, Arc::new(FixedClock));
        fixture
            .http
            .push(url, bytes.clone(), Some(bytes.len() as u64), None)?;
        let old = old_store.pull(
            &ImagePullRequest::new(ImageRef::new("firm:1"), &fixture.root),
            &mut Vec::new(),
        )?;
        assert_eq!(old.metadata.firmware, Some(CatalogFirmware::Rhf));
        assert_eq!(old.metadata.sshd_path.as_str(), "/usr/sbin/sshd");

        let new_catalog = custom_catalog(
            &fixture.root,
            "firm-edk2",
            &catalog_source("edk2", "/usr/libexec/openssh/sshd"),
        )?;
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
        assert_eq!(warm.image.metadata.sshd_path.as_str(), "/usr/sbin/sshd");
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
        assert_eq!(
            upgraded.metadata.sshd_path.as_str(),
            "/usr/libexec/openssh/sshd"
        );
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
        assert_eq!(
            pinned.image.metadata.sshd_path.as_str(),
            "/usr/libexec/openssh/sshd"
        );
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
            .env("FIRESTONE_IMAGE_UMASK_INTERRUPT", "1")
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
        assert_eq!(fs::symlink_metadata(&lock_path)?.mode() & 0o7777, 0o000);
        drop(ImageStoreLock::acquire(
            &fixture.paths,
            Duration::from_secs(1),
            Duration::from_millis(5),
        )?);
        assert_eq!(
            fs::symlink_metadata(&lock_path)?.mode() & 0o7777,
            LOCK_FILE_MODE
        );

        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o400))?;
        drop(ImageStoreLock::acquire(
            &fixture.paths,
            Duration::from_secs(1),
            Duration::from_millis(5),
        )?);
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
        if env::var_os("FIRESTONE_IMAGE_UMASK_INTERRUPT").is_some() {
            let lock_path = paths.image_store_lock()?;
            drop(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(LOCK_FILE_MODE)
                    .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                    .open(lock_path)?,
            );
            return Ok(());
        }
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
    #[test]
    fn verified_direct_url_is_an_offline_warm_cache_and_unchecked_refresh_tracks_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://mutable.example.invalid/base.qcow2";
        let original_bytes = b"QFI\xFBMUTABLE-ONE".to_vec();
        let original_sha = sha256_bytes(&original_bytes);
        fixture.http.push(
            url,
            original_bytes.clone(),
            Some(original_bytes.len() as u64),
            None,
        )?;
        let verified = fixture.store.pull(
            &url_request(url, Some(original_sha), &fixture.root),
            &mut Vec::new(),
        )?;
        assert_eq!(verified.metadata.generation, 1);
        assert_eq!(
            verified.metadata.verification_algorithm,
            Some(ChecksumAlgorithm::Sha256)
        );

        let first_name = "verified-url-cache";
        let mut first_state = machine_state(
            &fixture.paths,
            first_name,
            StateImage {
                r#ref: url.to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let first_lock = create_machine(&fixture.paths, first_name, &first_state, &mut Vec::new())?;
        let offline = fixture.store.prepare_machine_image(
            first_name,
            &mut first_state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &first_lock,
            &mut Vec::new(),
        )?;
        assert!(offline.image.cached);
        assert_eq!(offline.image.metadata.id, verified.metadata.id);
        assert_eq!(
            offline.image.metadata.verification_algorithm,
            Some(ChecksumAlgorithm::Sha256)
        );

        let changed_bytes = b"QFI\xFBMUTABLE-TWO".to_vec();
        fixture.http.push(
            url,
            changed_bytes.clone(),
            Some(changed_bytes.len() as u64),
            None,
        )?;
        let refreshed = fixture
            .store
            .pull(&url_request(url, None, &fixture.root), &mut Vec::new())?;
        assert!(!refreshed.cached);
        assert_ne!(refreshed.metadata.id, verified.metadata.id);
        assert_eq!(refreshed.metadata.generation, 2);
        assert!(refreshed.metadata.verification().is_none());

        let second_name = "refreshed-url-cache";
        let mut second_state = machine_state(
            &fixture.paths,
            second_name,
            StateImage {
                r#ref: url.to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let second_lock =
            create_machine(&fixture.paths, second_name, &second_state, &mut Vec::new())?;
        let latest = fixture.store.prepare_machine_image(
            second_name,
            &mut second_state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &second_lock,
            &mut Vec::new(),
        )?;
        assert!(latest.image.cached);
        assert_eq!(latest.image.metadata.id, refreshed.metadata.id);
        assert_eq!(
            first_state.image.id.as_deref(),
            Some(verified.metadata.id.as_str())
        );
        Ok(())
    }

    #[test]
    fn dirty_base_is_rejected_but_cached_dirty_overlay_is_restartable()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let dirty_base = fixture.write_source("dirty-base.qcow2", b"QFI\xFBDIRTY-BASE")?;
        let base_error = fixture
            .store
            .pull(&local_request(&dirty_base, &fixture.root), &mut Vec::new())
            .err()
            .ok_or("expected dirty immutable base rejection")?;
        assert_eq!(base_error.kind(), ErrorKind::Dependency);
        assert_no_image_artifacts(&fixture.paths)?;

        let clean = fixture.write_source("clean-base.qcow2", b"QFI\xFBCLEAN-BASE")?;
        let name = "dirty-overlay";
        let mut state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: clean.to_string_lossy().into_owned(),
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
        OpenOptions::new()
            .append(true)
            .open(&first.overlay.path)?
            .write_all(b"\nDIRTY\n")?;

        let restarted = fixture.store.prepare_machine_image(
            name,
            &mut state,
            &fixture.root,
            ByteSize::from_mib(1)?,
            &lock,
            &mut Vec::new(),
        )?;
        assert!(restarted.image.cached);
        assert!(restarted.overlay.cached);
        assert_eq!(restarted.overlay.path, first.overlay.path);
        Ok(())
    }
    #[test]
    fn directory_entry_failure_names_parent_and_has_hint() {
        let directory = Path::new("/var/lib/firestone/images");
        let error = directory_entry_error(directory, std::io::Error::other("injected"));

        assert_eq!(error.kind(), ErrorKind::Generic);
        assert!(error.message().contains(&directory.display().to_string()));
        assert!(error.hint().is_some());
    }

    #[test]
    fn source_read_failure_is_transport_not_checksum() -> Result<(), Box<dyn std::error::Error>> {
        struct FailingReader;

        impl std::io::Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected transport failure"))
            }
        }

        let directory = tempfile::tempdir()?;
        let output = directory.path().join("failing-source.partial");
        let mut reader = FailingReader;
        let mut events = Vec::new();
        let error = stream_source(
            &mut reader,
            &output,
            None,
            ErrorKind::Checksum,
            None,
            &mut events,
        )
        .err()
        .ok_or("failing source read succeeded")?;

        assert_eq!(error.kind(), ErrorKind::Generic);
        assert!(error.message().contains("cannot read image source"));
        assert!(error.hint().is_some());
        Ok(())
    }

    #[test]
    fn one_byte_source_frames_have_bounded_progress_and_final_total()
    -> Result<(), Box<dyn std::error::Error>> {
        struct OneByteReader {
            bytes: Vec<u8>,
            offset: usize,
        }

        impl Read for OneByteReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.offset == self.bytes.len() || buffer.is_empty() {
                    return Ok(0);
                }
                buffer[0] = self.bytes[self.offset];
                self.offset += 1;
                Ok(1)
            }
        }

        let length = IMAGE_PROGRESS_INTERVAL_BYTES as usize + 17;
        let mut bytes = vec![b'x'; length];
        bytes[..QCOW2_MAGIC.len()].copy_from_slice(&QCOW2_MAGIC);
        let mut reader = OneByteReader { bytes, offset: 0 };
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("one-byte-source.partial");
        let mut events = Vec::new();
        let staged = stream_source(
            &mut reader,
            &output,
            Some(length as u64),
            ErrorKind::Checksum,
            None,
            &mut events,
        )?;
        let progress = events
            .iter()
            .filter_map(|event| match event {
                Event::Progress { done, total, .. } => Some((*done, *total)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(progress.len() <= length / IMAGE_PROGRESS_INTERVAL_BYTES as usize + 1);
        assert_eq!(progress.last(), Some(&(length as u64, Some(length as u64))));
        assert_eq!(staged.size, length as u64);
        Ok(())
    }

    #[test]
    fn sidecar_limit_is_prevalidated_before_base_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let url = "https://long-reference.example.invalid/base.qcow2";
        let image_bytes = b"QFI\xFBLONG-REFERENCE".to_vec();
        let digest = sha256_bytes(&image_bytes);
        let metadata_for = |version: &str| {
            let source_ref = format!("longref:{version}");
            let id = stable_image_id(&source_ref, Some(url), Arch::X86_64, &digest);
            ImageMetadata {
                version: ImageMetadataVersion,
                id,
                kind: ImageKind::Disk,
                oci: None,
                generation: 1,
                source_ref,
                source_url: Some(url.to_owned()),
                source_sha256: digest.clone(),
                stored_sha256: digest.clone(),
                architecture: Arch::X86_64,
                firmware: Some(CatalogFirmware::Rhf),
                sshd_path: SshdPath::default(),
                source_format: ImageFormat::Qcow2,
                stored_format: ImageFormat::Qcow2,
                verification_algorithm: Some(ChecksumAlgorithm::Sha256),
                verification_digest: Some(digest.clone()),
                size: image_bytes.len() as u64,
                pulled_at: FIXED_TIME.to_owned(),
            }
        };

        let template = metadata_for("a");
        let template_path = fixture.paths.image_metadata(&template.id)?;
        let template_length = serialize_image_metadata(&template_path, &template)?.len();
        let padding = (MAX_SIDECAR_BYTES as usize)
            .checked_sub(template_length)
            .ok_or("sidecar template unexpectedly exceeded limit")?
            + 1;
        let exact_version = "a".repeat(padding);
        let exact_metadata = metadata_for(&exact_version);
        let exact_sidecar = fixture.paths.image_metadata(&exact_metadata.id)?;
        assert_eq!(
            serialize_image_metadata(&exact_sidecar, &exact_metadata)?.len(),
            MAX_SIDECAR_BYTES as usize
        );

        let catalog_source = |version: &str| {
            format!(
                concat!(
                    "[[image]]\n",
                    "distro = \"longref\"\n",
                    "version = \"{}\"\n",
                    "aliases = []\n",
                    "default = true\n",
                    "firmware = \"rhf\"\n",
                    "format = \"qcow2\"\n\n",
                    "[image.arch.x86_64]\n",
                    "url = \"{}\"\n",
                    "sha256 = \"{}\"\n",
                    "checksum_alg = \"sha256\"\n"
                ),
                version, url, digest,
            )
        };
        let exact_catalog = custom_catalog(
            &fixture.root,
            "longref-exact",
            &catalog_source(&exact_version),
        )?;
        let exact_store = store_with_catalog(&fixture, exact_catalog, Arc::new(FixedClock));
        fixture.http.push(
            url,
            image_bytes.clone(),
            Some(image_bytes.len() as u64),
            None,
        )?;
        let exact = exact_store.pull(
            &ImagePullRequest::new(
                ImageRef::new(exact_metadata.source_ref.clone()),
                &fixture.root,
            ),
            &mut Vec::new(),
        )?;
        assert_eq!(exact.metadata.id, exact_metadata.id);
        assert_eq!(fs::read(&exact_sidecar)?.len(), MAX_SIDECAR_BYTES as usize);
        assert_eq!(exact_store.list()?.len(), 1);

        let over_version = format!("{exact_version}a");
        let over_metadata = metadata_for(&over_version);
        let over_sidecar = fixture.paths.image_metadata(&over_metadata.id)?;
        let preflight = serialize_image_metadata(&over_sidecar, &over_metadata)
            .err()
            .ok_or("expected over-limit sidecar preflight rejection")?;
        assert_eq!(preflight.kind(), ErrorKind::InvalidSpec);
        assert!(preflight.message().contains("exceeds"));

        let over_catalog = custom_catalog(
            &fixture.root,
            "longref-over",
            &catalog_source(&over_version),
        )?;
        let over_store = store_with_catalog(&fixture, over_catalog, Arc::new(FixedClock));
        fixture.http.push(
            url,
            image_bytes.clone(),
            Some(image_bytes.len() as u64),
            None,
        )?;
        let error = over_store
            .pull(
                &ImagePullRequest::new(
                    ImageRef::new(over_metadata.source_ref.clone()),
                    &fixture.root,
                ),
                &mut Vec::new(),
            )
            .err()
            .ok_or("expected over-limit pull rejection")?;
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("exceeds"));
        assert!(!fixture.paths.image_base(&over_metadata.id)?.exists());
        assert!(!over_sidecar.exists());
        for entry in fs::read_dir(fixture.paths.images_dir())? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".partial"), "stale partial: {name}");
        }
        assert_eq!(over_store.list()?.len(), 1);
        Ok(())
    }

    #[test]
    fn resolve_oci_reference_expected_normalized_oci_location()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let cases: &[(&str, &str)] = &[
            ("docker://nginx", "docker.io/library/nginx:latest"),
            ("oci://nginx:1.27", "docker.io/library/nginx:1.27"),
            ("ghcr.io/owner/app:v1", "ghcr.io/owner/app:v1"),
            ("localhost:5000/app", "localhost:5000/app:latest"),
        ];

        for (input, canonical) in cases {
            let resolved = fixture
                .store
                .resolve(&ImageRef::new(*input), None, &fixture.root)?;
            assert_eq!(resolved.source_ref, *canonical, "resolving {input}");
            assert_eq!(resolved.source_url, None);
            assert_eq!(resolved.firmware, None);
            assert_eq!(resolved.verification, None);
            let ImageSourceLocation::Oci(reference) = &resolved.location else {
                return Err(format!("{input} did not resolve to an OCI location").into());
            };
            assert_eq!(reference.to_string(), *canonical);
        }
        Ok(())
    }

    #[test]
    fn resolve_non_oci_references_expected_unchanged_after_the_oci_branch()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let local = fixture.write_source("local.qcow2", b"QFI\xFBLOCAL")?;

        let catalog = fixture
            .store
            .resolve(&ImageRef::new("ubuntu:24.04"), None, &fixture.root)?;
        assert_eq!(catalog.source_ref, "ubuntu:24.04");
        assert!(matches!(catalog.location, ImageSourceLocation::Https(_)));

        let url = "https://example.invalid/base.qcow2";
        let direct = fixture
            .store
            .resolve(&ImageRef::new(url), None, &fixture.root)?;
        assert_eq!(direct.location, ImageSourceLocation::Https(url.to_owned()));

        let relative =
            fixture
                .store
                .resolve(&ImageRef::new("./local.qcow2"), None, &fixture.root)?;
        assert_eq!(relative.location, ImageSourceLocation::Local(local.clone()));

        let absolute = fixture.store.resolve(
            &ImageRef::new(local.to_string_lossy().as_ref()),
            None,
            &fixture.root,
        )?;
        assert_eq!(absolute.location, ImageSourceLocation::Local(local));

        let missing = fixture
            .store
            .resolve(&ImageRef::new("./missing.qcow2"), None, &fixture.root)
            .err()
            .ok_or("expected a missing relative path to fail")?;
        assert_eq!(missing.kind(), ErrorKind::NotFound);
        assert!(missing.message().contains("local image path"));

        let bare = fixture
            .store
            .resolve(&ImageRef::new("nginx"), None, &fixture.root)
            .err()
            .ok_or("expected a bare name to stay a catalog error")?;
        assert_eq!(bare.kind(), ErrorKind::NotFound);
        assert!(bare.message().contains("unknown image 'nginx'"));
        assert!(
            bare.hint()
                .is_some_and(|hint| hint.contains("docker://nginx")),
            "bare-name hint should name docker://nginx"
        );

        let namespaced = fixture
            .store
            .resolve(&ImageRef::new("owner/app"), None, &fixture.root)
            .err()
            .ok_or("expected a registry-less namespaced name to stay a path error")?;
        assert_eq!(namespaced.kind(), ErrorKind::NotFound);
        assert!(namespaced.message().contains("local image path"));
        Ok(())
    }

    #[test]
    fn resolve_oci_reference_with_supplied_sha256_expected_usage_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let error = fixture
            .store
            .resolve(
                &ImageRef::new("docker://nginx"),
                Some(&"a".repeat(64)),
                &fixture.root,
            )
            .err()
            .ok_or("expected --sha256 to be rejected for an OCI reference")?;

        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("docker.io/library/nginx:latest"));
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("repo@sha256:")),
            "hint should point at digest references"
        );
        Ok(())
    }

    #[test]
    fn resolve_malformed_oci_scheme_reference_expected_invalid_spec()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let error = fixture
            .store
            .resolve(&ImageRef::new("docker://NGINX"), None, &fixture.root)
            .err()
            .ok_or("expected an explicit malformed OCI reference to fail")?;

        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert!(error.message().contains("invalid OCI image reference"));
        Ok(())
    }

    /// A full scripted pull publishes a real qcow2 pair, a version-two sidecar
    /// carrying the complete `oci` object, and the §8.5 packing argv.
    #[test]
    fn pull_oci_image_expected_published_base_and_oci_sidecar()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let layer = gzip_layer(&[
            ("etc/", None),
            ("etc/os-release", Some("NAME=fake\n")),
            ("entry.sh", Some("#!/bin/sh\nexec true\n")),
        ])?;
        let scripted = script_oci_image(
            &fixture,
            "docker.io/library/alpine:3.20",
            std::slice::from_ref(&layer),
            &oci_config_json(),
        )?;

        let mut events = Vec::new();
        let pulled = fixture.store.pull(
            &oci_request("docker.io/library/alpine:3.20", &fixture.root),
            &mut events,
        )?;

        assert!(!pulled.cached);
        let metadata = &pulled.metadata;
        assert_eq!(metadata.kind, ImageKind::Oci);
        assert_eq!(metadata.source_ref, "docker.io/library/alpine:3.20");
        assert_eq!(metadata.source_url, None);
        assert_eq!(metadata.firmware, None);
        assert_eq!(metadata.source_format, ImageFormat::Raw);
        assert_eq!(metadata.stored_format, ImageFormat::Qcow2);
        assert_eq!(metadata.generation, 1);
        assert_eq!(
            format!("sha256:{}", metadata.source_sha256),
            scripted.manifest_digest
        );
        assert_eq!(
            metadata.id,
            stable_image_id(
                "docker.io/library/alpine:3.20",
                None,
                Arch::X86_64,
                &metadata.source_sha256,
            )
        );
        let oci = metadata
            .oci
            .as_ref()
            .ok_or("an oci image must publish an oci object")?;
        assert_eq!(oci.registry_ref, "docker.io/library/alpine:3.20");
        assert_eq!(oci.manifest_digest, scripted.manifest_digest);
        assert_eq!(oci.config_digest, scripted.config_digest);
        assert_eq!(oci.entrypoint, vec!["/entry.sh".to_owned()]);
        assert_eq!(oci.cmd, vec!["serve".to_owned()]);
        assert_eq!(oci.env, vec!["PATH=/usr/bin".to_owned()]);
        assert_eq!(oci.workdir.as_deref(), Some("/srv"));
        assert_eq!(oci.user.as_deref(), Some("app"));
        assert_eq!(oci.boot, FIRESTONE_INIT_BOOT);

        // The published pair round-trips through the strict sidecar reader,
        // and its bytes stay version one with the optional `kind` written.
        let listed = fixture.store.list()?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].metadata, *metadata);
        let sidecar: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.paths.image_metadata(&metadata.id)?)?)?;
        assert_eq!(sidecar["version"], 1);
        assert_eq!(sidecar["kind"], "oci");
        assert!(sidecar["source_url"].is_null());
        assert!(sidecar["firmware"].is_null());
        assert_eq!(sidecar["source_format"], "raw");
        assert_eq!(sidecar["verification_algorithm"], "sha256");
        assert_eq!(sidecar["verification_digest"], sidecar["source_sha256"]);
        assert_eq!(
            fs::symlink_metadata(&pulled.path)?.permissions().mode() & 0o7777,
            BASE_FILE_MODE
        );

        // §8.5 packing: one `mkfs.ext4 -F -t ext4 -d <tar> -b 4096` run whose
        // trailing operand is the sized block count.
        let invocations = fixture.mkfs_invocations()?;
        assert_eq!(invocations.len(), 1);
        let argv = invocations[0].split(' ').collect::<Vec<_>>();
        assert_eq!(argv[0..4], ["-F", "-t", "ext4", "-d"]);
        assert_eq!(argv[5..7], ["-b", "4096"]);
        assert!(argv[4].ends_with(".tar.partial"), "{}", invocations[0]);
        assert!(argv[7].ends_with(".source.partial"), "{}", invocations[0]);
        let blocks = argv[8].parse::<u64>()?;
        assert_eq!(blocks * EXT4_BLOCK_BYTES % OCI_SIZE_ALIGNMENT_BYTES, 0);
        assert!(blocks * EXT4_BLOCK_BYTES >= OCI_SIZE_HEADROOM_BYTES);

        // The tar handed to mkfs carries the injected guest init.
        let packed = fixture.mkfs_input_tar()?;
        let mut archive = tar::Archive::new(packed.as_slice());
        let mut injected = None;
        let mut paths = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            if path == crate::oci::layers::FIRESTONE_INIT_PATH {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                injected = Some(bytes);
            }
            paths.push(path);
        }
        assert_eq!(injected.as_deref(), Some(TEST_INIT_PAYLOAD));
        assert!(paths.iter().any(|path| path == "etc/os-release"));
        assert!(
            paths
                .iter()
                .any(|path| path == crate::oci::layers::FIRESTONE_OCI_MARKER_PATH)
        );

        // §8.3 events: one start, byte progress against the layer total, one
        // done. No partial survives.
        let total = layer.len() as u64;
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Progress { done, total: Some(declared), unit: Unit::Bytes, .. }
                if *done == total && *declared == total
        )));
        assert!(events.iter().any(
            |event| matches!(event, Event::StepDone { detail: Some(detail), .. }
                    if detail.contains("docker.io/library/alpine:3.20"))
        ));
        let remaining = fs::read_dir(fixture.paths.images_dir())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".partial"))
            .count();
        assert_eq!(remaining, 0);
        Ok(())
    }

    /// Re-pulling an unchanged tag resolves the same manifest digest and hits
    /// the exact cache, exactly as an unchanged HTTPS source does.
    #[test]
    fn pull_oci_image_unchanged_tag_expected_cached_skip() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new(false)?;
        let layer = gzip_layer(&[("etc/", None), ("etc/hostname", Some("fake\n"))])?;
        let config = oci_config_json();
        let reference = "docker.io/library/alpine:3.20";
        script_oci_image(&fixture, reference, std::slice::from_ref(&layer), &config)?;
        let first = fixture
            .store
            .pull(&oci_request(reference, &fixture.root), &mut Vec::new())?;

        // The second pull re-reads the manifest and stops there.
        script_oci_image(&fixture, reference, std::slice::from_ref(&layer), &config)?;
        let mut events = Vec::new();
        let second = fixture
            .store
            .pull(&oci_request(reference, &fixture.root), &mut events)?;

        assert!(second.cached);
        assert_eq!(second.metadata.id, first.metadata.id);
        assert_eq!(second.metadata.generation, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::StepSkip { reason, .. } if reason == "cached"
        )));
        assert_eq!(fixture.mkfs_invocations()?.len(), 1);
        assert_eq!(fixture.store.list()?.len(), 1);
        Ok(())
    }

    /// A moved tag resolves to another manifest digest, so it publishes a new
    /// id and generation and leaves the earlier base in place.
    #[test]
    fn pull_oci_image_moved_tag_expected_second_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let reference = "ghcr.io/owner/app:edge";
        let first_layer = gzip_layer(&[("etc/", None), ("etc/build", Some("one\n"))])?;
        let first_scripted = script_oci_image(
            &fixture,
            reference,
            std::slice::from_ref(&first_layer),
            &oci_config_json(),
        )?;
        let first = fixture
            .store
            .pull(&oci_request(reference, &fixture.root), &mut Vec::new())?;

        let second_layer = gzip_layer(&[("etc/", None), ("etc/build", Some("two\n"))])?;
        let second_scripted = script_oci_image(
            &fixture,
            reference,
            std::slice::from_ref(&second_layer),
            &oci_config_json(),
        )?;
        let second = fixture
            .store
            .pull(&oci_request(reference, &fixture.root), &mut Vec::new())?;

        assert_ne!(
            first_scripted.manifest_digest,
            second_scripted.manifest_digest
        );
        assert!(!second.cached);
        assert_ne!(second.metadata.id, first.metadata.id);
        assert_eq!(second.metadata.generation, 2);
        assert_eq!(fixture.store.list()?.len(), 2);
        assert!(fs::symlink_metadata(&first.path).is_ok());
        Ok(())
    }

    /// A digest reference selects that exact manifest and is persisted, and
    /// the layer digests it names are the ones fetched.
    #[test]
    fn pull_oci_digest_reference_expected_exact_manifest_selection()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let layer = gzip_layer(&[("etc/", None), ("etc/pinned", Some("yes\n"))])?;
        let config = oci_config_json();
        let tagged = script_oci_image(
            &fixture,
            "ghcr.io/owner/app:edge",
            std::slice::from_ref(&layer),
            &config,
        )?;
        let pinned = format!("ghcr.io/owner/app@{}", tagged.manifest_digest);
        let scripted = script_oci_image(&fixture, &pinned, std::slice::from_ref(&layer), &config)?;
        assert_eq!(scripted.manifest_digest, tagged.manifest_digest);

        let pulled = fixture
            .store
            .pull(&oci_request(&pinned, &fixture.root), &mut Vec::new())?;

        assert_eq!(pulled.metadata.source_ref, pinned);
        let oci = pulled
            .metadata
            .oci
            .as_ref()
            .ok_or("an oci image must publish an oci object")?;
        assert_eq!(oci.registry_ref, pinned);
        assert_eq!(oci.manifest_digest, tagged.manifest_digest);
        assert_eq!(scripted.layer_digests.len(), 1);
        Ok(())
    }

    /// The `firestone-init` payload is checked before a byte is downloaded: a
    /// build without one fails with the scripted blobs still untouched.
    #[test]
    fn pull_oci_image_without_init_payload_expected_dependency_before_download()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::new(false)?;
        let layer = gzip_layer(&[("etc/", None), ("etc/hostname", Some("fake\n"))])?;
        let reference = "docker.io/library/alpine:3.20";
        script_oci_image(
            &fixture,
            reference,
            std::slice::from_ref(&layer),
            &oci_config_json(),
        )?;
        fixture.store.init_payload = missing_init_payload_source();

        let error = fixture
            .store
            .pull(&oci_request(reference, &fixture.root), &mut Vec::new())
            .err()
            .ok_or("expected a missing firestone-init payload to fail the pull")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("firestone-init"));
        assert!(error.message().contains(reference));
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("standalone") && hint.contains("deps.toml")),
            "the hint must name the release limitation: {:?}",
            error.hint()
        );
        assert!(fixture.mkfs_invocations()?.is_empty());
        assert_no_image_artifacts(&fixture.paths)?;

        // Nothing was requested: the queued replies still serve a full pull.
        fixture.store.init_payload = test_init_payload_source();
        let pulled = fixture
            .store
            .pull(&oci_request(reference, &fixture.root), &mut Vec::new())?;
        assert!(!pulled.cached);
        Ok(())
    }

    /// A zstd layer is refused with the media type named, before any blob is
    /// downloaded, because §8.5 decompresses gzip only in v0.2.
    #[test]
    fn pull_oci_zstd_layer_expected_dependency_naming_the_media_type()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let config = oci_config_json();
        let config_digest = format!("sha256:{}", sha256_bytes(config.as_bytes()));
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"{}","digest":"sha256:{}","size":10}}]}}"#,
            crate::oci::registry::MEDIA_TYPE_OCI_MANIFEST,
            config.len(),
            crate::oci::layers::MEDIA_TYPE_OCI_LAYER_ZSTD,
            "b".repeat(64),
        );
        fixture.http.push(
            "https://registry-1.docker.io/v2/library/alpine/manifests/3.20",
            manifest.into_bytes(),
            None,
            Some(crate::oci::registry::MEDIA_TYPE_OCI_MANIFEST),
        )?;

        let error = fixture
            .store
            .pull(
                &oci_request("docker.io/library/alpine:3.20", &fixture.root),
                &mut Vec::new(),
            )
            .err()
            .ok_or("expected a zstd layer to be refused")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error
                .message()
                .contains(crate::oci::layers::MEDIA_TYPE_OCI_LAYER_ZSTD)
        );
        assert!(error.hint().is_some_and(|hint| hint.contains("gzip")));
        assert_no_image_artifacts(&fixture.paths)?;
        Ok(())
    }

    /// Every failure injection point leaves the store exactly as it was: no
    /// partial, no half-published pair.
    #[test]
    fn pull_oci_image_failure_at_each_step_expected_clean_store()
    -> Result<(), Box<dyn std::error::Error>> {
        let layer = gzip_layer(&[("etc/", None), ("etc/hostname", Some("fake\n"))])?;
        let reference = "docker.io/library/alpine:3.20";

        // 1. The layer blob is unavailable.
        {
            let fixture = Fixture::new(false)?;
            let config = oci_config_json();
            let scripted =
                script_oci_image(&fixture, reference, std::slice::from_ref(&layer), &config)?;
            let host = "registry-1.docker.io";
            let blob = format!(
                "https://{host}/v2/library/alpine/blobs/{}",
                scripted.layer_digests[0]
            );
            fixture.http.take(&blob)?;
            let error = fixture
                .store
                .pull(&oci_request(reference, &fixture.root), &mut Vec::new())
                .err()
                .ok_or("expected an unavailable layer blob to fail the pull")?;
            assert!(error.message().contains("no scripted HTTP response"));
            assert_no_image_artifacts(&fixture.paths)?;
        }

        // 2. A truncated layer blob fails digest verification.
        {
            let fixture = Fixture::new(false)?;
            let config = oci_config_json();
            let scripted =
                script_oci_image(&fixture, reference, std::slice::from_ref(&layer), &config)?;
            let blob = format!(
                "https://registry-1.docker.io/v2/library/alpine/blobs/{}",
                scripted.layer_digests[0]
            );
            fixture.http.take(&blob)?;
            fixture
                .http
                .push(&blob, layer[..layer.len() - 1].to_vec(), None, None)?;
            let error = fixture
                .store
                .pull(&oci_request(reference, &fixture.root), &mut Vec::new())
                .err()
                .ok_or("expected a truncated layer blob to fail the pull")?;
            assert_eq!(error.kind(), ErrorKind::Checksum);
            assert_no_image_artifacts(&fixture.paths)?;
        }

        // 3. `mkfs.ext4` exits non-zero.
        {
            let fixture = Fixture::new(false)?;
            script_oci_image(
                &fixture,
                reference,
                std::slice::from_ref(&layer),
                &oci_config_json(),
            )?;
            fs::write(fixture.root.join(MKFS_FAILURE_MARKER), b"")?;
            let error = fixture
                .store
                .pull(&oci_request(reference, &fixture.root), &mut Vec::new())
                .err()
                .ok_or("expected a failing mkfs.ext4 to fail the pull")?;
            assert_eq!(error.kind(), ErrorKind::Dependency);
            assert_eq!(fixture.mkfs_invocations()?.len(), 1);
            assert_no_image_artifacts(&fixture.paths)?;
        }

        // 4. `qemu-img convert` exits non-zero.
        {
            let fixture = Fixture::new(true)?;
            script_oci_image(
                &fixture,
                reference,
                std::slice::from_ref(&layer),
                &oci_config_json(),
            )?;
            let error = fixture
                .store
                .pull(&oci_request(reference, &fixture.root), &mut Vec::new())
                .err()
                .ok_or("expected a failing qemu-img convert to fail the pull")?;
            assert_eq!(error.kind(), ErrorKind::Dependency);
            assert_no_image_artifacts(&fixture.paths)?;
        }
        Ok(())
    }

    /// `create NAME <oci ref>` then `start NAME`: an unpinned OCI reference
    /// pulls through the registry, pins immutable identity in `state.json`, and
    /// reaches the §9.5 direct-kernel payload with no firmware in sight.
    #[test]
    fn prepare_start_unpinned_oci_reference_expected_pull_then_direct_kernel_boot()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let kernel = b"pinned-direct-boot-kernel";
        let kernel_url = "https://kernel.example.invalid/bzImage-x86_64";
        let manifest = direct_boot_kernel_manifest(kernel_url, kernel, None)?;
        fixture.http.push(
            kernel_url,
            kernel.to_vec(),
            Some(kernel.len() as u64),
            Some("application/octet-stream"),
        )?;
        let reference = "docker.io/library/alpine:3.20";
        let layer = gzip_layer(&[("etc/", None), ("etc/hostname", Some("packed\n"))])?;
        let scripted = script_oci_image(
            &fixture,
            reference,
            std::slice::from_ref(&layer),
            &oci_config_json(),
        )?;
        let name = "oci-created";
        let state = machine_state(
            &fixture.paths,
            name,
            StateImage {
                r#ref: reference.to_owned(),
                id: None,
                sha256: None,
            },
        )?;
        let spec = MachineSpec {
            image: ImageRef::new(reference),
            arch: Some(Arch::X86_64),
            network: NetworkSpec {
                mode: NetMode::None,
                ..NetworkSpec::default()
            },
            vmm: VmmSpec {
                binary: Some(fixture.store.qemu_img.clone()),
                ..VmmSpec::default()
            },
            ..MachineSpec::default()
        };
        let mut events = Vec::new();
        let lock = create_machine(&fixture.paths, name, &state, &mut events)?;

        let prepared = prepare_start(
            &fixture.paths,
            &fixture.store,
            &manifest,
            name,
            &spec,
            state,
            &fixture.root,
            &lock,
            &mut events,
            ShimTimeouts::default(),
        )?;

        // The pull pinned the manifest digest as the machine's image identity.
        let pinned = &prepared.state().image;
        assert_eq!(pinned.r#ref, reference);
        assert_eq!(
            pinned
                .sha256
                .as_deref()
                .map(|digest| format!("sha256:{digest}")),
            Some(scripted.manifest_digest.clone())
        );
        let id = pinned
            .id
            .clone()
            .ok_or("a started machine pins an image id")?;
        assert_eq!(fixture.mkfs_invocations()?.len(), 1);

        // §9.5: the payload is the pinned kernel plus the fixed command line.
        let vmconfig: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.paths.machine_vmconfig(name)?)?)?;
        assert_eq!(
            vmconfig["payload"]["cmdline"],
            "console=hvc0 console=ttyS0 root=/dev/vda rw init=/sbin/firestone-init"
        );
        assert!(vmconfig["payload"]["firmware"].is_null());
        assert!(
            vmconfig["payload"]["kernel"]
                .as_str()
                .is_some_and(|path| path.ends_with("bzImage-ch-test"))
        );
        // §10.5: an OCI machine boots the config disk, never a cloud-init seed,
        // and its entrypoint comes from the sidecar the pull just wrote.
        assert!(!fixture.paths.machine_seed_image(name)?.exists());
        let document = fixture.paths.machine_config_file(name, "config.json")?;
        let inspected: serde_json::Value = serde_json::from_slice(&fs::read(&document)?)?;
        assert_eq!(inspected["entrypoint"][0], "/entry.sh");
        assert_eq!(inspected["cmd"][0], "serve");

        let stored = fixture.store.inspect(&id)?;
        assert_eq!(stored.image.metadata.kind, ImageKind::Oci);
        Ok(())
    }

    /// The pinned static `mkfs.ext4` accepts the exact §8.5 argv and tar input.
    ///
    /// The helper is a Linux x86_64 binary, so this runs only there and only
    /// when `FIRESTONE_TEST_MKFS_EXT4` names an installed copy; every other
    /// host proves the same pipeline through the recording fake above.
    #[cfg(target_os = "linux")]
    #[test]
    fn mkfs_ext4_real_helper_packs_the_merged_tar_expected_ext4_superblock()
    -> Result<(), Box<dyn std::error::Error>> {
        let Some(helper) = env::var_os("FIRESTONE_TEST_MKFS_EXT4").map(PathBuf::from) else {
            return Ok(());
        };
        if !helper.is_file() {
            return Ok(());
        }
        let fixture = Fixture::new(false)?;
        let reference = OciReference::parse("docker://alpine")?;
        let layer = gzip_layer(&[
            ("etc/", None),
            ("etc/os-release", Some("NAME=fake\n")),
            ("bin/", None),
        ])?;
        let blob = fixture.root.join("layer.tar.gz");
        fs::write(&blob, &layer)?;
        let source = FileLayer::new(blob);
        let sources: [&dyn LayerSource; 1] = [&source];
        let config = OciImageConfig::default();
        let request = MergeRequest::new(&sources, &config).with_injected_init(TEST_INIT_PAYLOAD);
        let tar = fixture.root.join("rootfs.tar");
        let summary = merge_layers(&request, &mut File::create(&tar)?)?;

        let raw = fixture.root.join("rootfs.raw");
        let bytes = oci_rootfs_bytes(&reference, summary.unpacked_bytes)?;
        File::create(&raw)?.set_len(bytes)?;
        Cmd::new(helper.as_os_str())
            .arg("-F")
            .arg("-t")
            .arg("ext4")
            .arg("-d")
            .arg(tar.as_os_str())
            .arg("-b")
            .arg(EXT4_BLOCK_BYTES.to_string())
            .arg(raw.as_os_str())
            .arg((bytes / EXT4_BLOCK_BYTES).to_string())
            .timeout(MKFS_TIMEOUT)
            .error_kind(ErrorKind::Dependency)
            .run()?;

        assert_eq!(fs::symlink_metadata(&raw)?.len(), bytes);
        // The ext2/3/4 superblock magic lives at byte 1080 of the image.
        let mut head = [0_u8; 1082];
        File::open(&raw)?.read_exact(&mut head)?;
        assert_eq!(&head[1080..], &[0x53, 0xef], "ext4 superblock magic");
        Ok(())
    }

    /// The §8.5 sizing rule is pure integer arithmetic, so one manifest yields
    /// the same ext4 size on every host.
    #[test]
    fn oci_rootfs_bytes_documented_table_expected_exact_sizes()
    -> Result<(), Box<dyn std::error::Error>> {
        let reference = OciReference::parse("docker://alpine")?;
        let mib = 1024 * 1024;
        // (unpacked bytes, expected ext4 bytes)
        let cases: &[(u64, u64)] = &[
            // An empty tree is exactly the headroom, which is already aligned.
            (0, 256 * mib),
            // One byte past it rounds up a whole 4 MiB step.
            (1, 260 * mib),
            (4096, 260 * mib),
            (100 * mib, 372 * mib),
            (1024 * mib, 1436 * mib),
            (3 * 1024 * mib + 12345, 3792 * mib),
        ];
        for (unpacked, expected) in cases {
            let sized = oci_rootfs_bytes(&reference, *unpacked)?;
            assert_eq!(sized, *expected, "sizing {unpacked} unpacked bytes");
            assert_eq!(sized % OCI_SIZE_ALIGNMENT_BYTES, 0);
            assert!(sized >= unpacked.saturating_add(OCI_SIZE_HEADROOM_BYTES));
        }

        let overflow = oci_rootfs_bytes(&reference, u64::MAX)
            .err()
            .ok_or("expected an impossible tree size to be refused")?;
        assert_eq!(overflow.kind(), ErrorKind::Dependency);
        assert!(overflow.hint().is_some());
        Ok(())
    }

    #[test]
    fn resolve_persisted_references_expected_oci_only_for_classified_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(false)?;
        let local = fixture.write_source("persisted.qcow2", b"QFI\xFBPERSIST")?;

        let oci = fixture
            .store
            .resolve_persisted("docker.io/library/nginx:latest")?;
        assert_eq!(oci.source_ref, "docker.io/library/nginx:latest");
        assert!(matches!(oci.location, ImageSourceLocation::Oci(_)));

        let catalog = fixture.store.resolve_persisted("ubuntu:24.04")?;
        assert!(matches!(catalog.location, ImageSourceLocation::Https(_)));

        let url = "https://example.invalid/persisted.qcow2";
        let direct = fixture.store.resolve_persisted(url)?;
        assert_eq!(direct.location, ImageSourceLocation::Https(url.to_owned()));

        let absolute = fixture
            .store
            .resolve_persisted(local.to_string_lossy().as_ref())?;
        assert_eq!(absolute.location, ImageSourceLocation::Local(local));

        let relative = fixture
            .store
            .resolve_persisted("./persisted.qcow2")
            .err()
            .ok_or("persisted resolution must not probe relative paths")?;
        assert_eq!(relative.kind(), ErrorKind::NotFound);
        Ok(())
    }
}
