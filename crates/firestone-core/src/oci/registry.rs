//! Read-only OCI Registry V2 client (SPEC §8.5).
//!
//! This module speaks exactly the subset a pull needs: `GET /v2/<name>/manifests/<reference>`
//! and `GET /v2/<name>/blobs/<digest>`. Nothing is pushed, deleted, or mounted.
//! It resolves a normalized [`OciReference`] to the platform manifest for the
//! host architecture, reads the image configuration, and streams layer blobs
//! digest-verified into a caller-supplied writer.
//!
//! The client is side-effect free with respect to the image store: it never
//! takes the store lock, never writes a sidecar, and never publishes a base.
//! The pull pipeline owns all of that.
//!
//! Transport comes from the shared image HTTP seam, so identity encoding, the
//! 30 s connect timeout, the 30 min request timeout, and the five-redirect
//! strict-HTTPS redirect policy apply here unchanged.

use std::{
    collections::BTreeMap,
    fmt,
    fmt::Write as _,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{
    bounded::{self, BoundedReadError},
    error::{ErrorKind, FirestoneError},
    image::{HttpRequest, HttpSource, HttpStatusResponse, shared_http_source},
    oci::{
        DEFAULT_REGISTRY, DEFAULT_REGISTRY_ALIAS, DIGEST_ALGORITHM, OciReference, OciTagOrDigest,
        layers::{OciImageConfig, classify_layer_media_type},
        validate_registry_host,
    },
    spec::Arch,
};

/// Host that serves the Registry V2 API for the canonical `docker.io`.
pub const DOCKER_REGISTRY_ENDPOINT: &str = "registry-1.docker.io";
/// Legacy `~/.docker/config.json` key that holds Docker Hub credentials.
pub const DOCKER_CREDENTIAL_KEY: &str = "https://index.docker.io/v1/";

/// Largest manifest or index document accepted, in bytes.
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
/// Largest image configuration blob accepted, in bytes.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Largest token-endpoint response accepted, in bytes.
pub const MAX_TOKEN_BYTES: u64 = 64 * 1024;
/// Largest `~/.docker/config.json` accepted, in bytes.
pub const MAX_DOCKER_CONFIG_BYTES: u64 = 1024 * 1024;
/// Largest number of layers accepted in one manifest.
pub const MAX_LAYERS: usize = 128;

/// OCI image index media type.
pub const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
/// OCI image manifest media type.
pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// Docker manifest list media type.
pub const MEDIA_TYPE_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
/// Docker image manifest media type.
pub const MEDIA_TYPE_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Exact `Accept` header sent with every manifest request.
pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// Bytes accumulated between two progress callbacks.
const PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024;
/// Size of the layer streaming buffer.
const LAYER_BUFFER_BYTES: usize = 1024 * 1024;
/// Length of a `sha256` digest in hexadecimal characters.
const DIGEST_HEX_LENGTH: usize = 64;

/// One layer blob named by a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerDescriptor {
    /// `sha256:…` digest of the compressed blob.
    pub digest: String,
    /// Media type as the manifest spelled it.
    pub media_type: String,
    /// Compressed size in bytes, as the manifest declared it.
    pub size: u64,
}

/// Everything a pull needs after one manifest resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedManifest {
    /// The canonical reference this resolution started from.
    pub reference: String,
    /// Digest of the platform manifest actually selected. This is the cache key.
    pub manifest_digest: String,
    /// Digest of the index, when the reference resolved through one.
    pub index_digest: Option<String>,
    /// Media type of the selected manifest.
    pub manifest_media_type: String,
    /// Digest of the image configuration blob.
    pub config_digest: String,
    /// Runtime fields kept from the image configuration.
    pub config: OciImageConfig,
    /// Layers in manifest order, which is also apply order.
    pub layers: Vec<LayerDescriptor>,
}

/// How one [`RegistryClient`] is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryOptions {
    /// Host architecture used for index platform selection.
    pub architecture: Arch,
    /// `images.insecure_registries` entries reachable over plain HTTP.
    pub insecure_registries: Vec<String>,
    /// Path of `~/.docker/config.json`, when the home directory is known.
    pub docker_config: Option<PathBuf>,
}

impl RegistryOptions {
    /// Anonymous, HTTPS-only options for one architecture.
    #[must_use]
    pub const fn new(architecture: Arch) -> Self {
        Self {
            architecture,
            insecure_registries: Vec::new(),
            docker_config: None,
        }
    }

    /// Sets the plain-HTTP allow list.
    #[must_use]
    pub fn with_insecure_registries(mut self, registries: Vec<String>) -> Self {
        self.insecure_registries = registries;
        self
    }

    /// Sets the Docker CLI configuration file read for static credentials.
    #[must_use]
    pub fn with_docker_config(mut self, path: Option<PathBuf>) -> Self {
        self.docker_config = path;
        self
    }
}

/// Static `user:password` credentials for one registry host.
#[derive(Clone, PartialEq, Eq)]
pub struct BasicCredential {
    user: String,
    secret: String,
}

impl BasicCredential {
    /// The user half, which is safe to name in a message.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Renders the `Authorization` header value for these credentials.
    fn header_value(&self) -> String {
        format!(
            "Basic {}",
            base64_encode(format!("{}:{}", self.user, self.secret).as_bytes())
        )
    }
}

impl fmt::Debug for BasicCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BasicCredential")
            .field("user", &self.user)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Credentials read from `~/.docker/config.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerCredentials {
    entries: BTreeMap<String, BasicCredential>,
    warnings: Vec<String>,
}

impl DockerCredentials {
    /// Reads one Docker CLI configuration file.
    ///
    /// A missing, unreadable, oversized, or malformed file yields anonymous
    /// access plus, where the reason is worth surfacing, one warning. Only the
    /// `auths` object and only its base64 `auth` fields are read; `credsStore`,
    /// `credHelpers`, and `identitytoken` are ignored with one warning naming
    /// the host.
    #[must_use]
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let Ok(metadata) = fs::metadata(path) else {
            return Self::default();
        };
        if !metadata.is_file() {
            return Self::default();
        }
        if metadata.len() > MAX_DOCKER_CONFIG_BYTES {
            return Self {
                entries: BTreeMap::new(),
                warnings: vec![format!(
                    "ignoring '{}': the file is larger than {MAX_DOCKER_CONFIG_BYTES} bytes",
                    path.display()
                )],
            };
        }
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        Self::parse(&bytes, path)
    }

    /// Parses configuration bytes that were already read and bounded.
    #[must_use]
    pub fn parse(bytes: &[u8], path: &Path) -> Self {
        let mut warnings = Vec::new();
        let Ok(document) = serde_json::from_slice::<DockerConfigDocument>(bytes) else {
            warnings.push(format!(
                "ignoring '{}': the file is not valid Docker CLI configuration JSON",
                path.display()
            ));
            return Self {
                entries: BTreeMap::new(),
                warnings,
            };
        };
        if let Some(store) = document.creds_store.as_deref() {
            warnings.push(format!(
                "ignoring the '{store}' credential store in '{}'; firestone reads base64 'auth' entries only",
                path.display()
            ));
        }
        let mut entries = BTreeMap::new();
        for (key, entry) in document.auths {
            let host = credential_host(&key);
            if document.cred_helpers.contains_key(&key) {
                warnings.push(format!(
                    "ignoring the credential helper configured for {host}; firestone reads base64 'auth' entries only"
                ));
            }
            if entry.identity_token.is_some() {
                warnings.push(format!(
                    "ignoring the identity token stored for {host}; firestone reads base64 'auth' entries only"
                ));
            }
            let Some(auth) = entry.auth.as_deref().filter(|value| !value.is_empty()) else {
                continue;
            };
            match decode_basic_auth(auth) {
                Some(credential) => {
                    entries.insert(host, credential);
                }
                None => warnings.push(format!(
                    "ignoring the credentials stored for {host}; the 'auth' field is not base64 'user:password'"
                )),
            }
        }
        for key in document.cred_helpers.keys() {
            let host = credential_host(key);
            if !entries.contains_key(&host) {
                warnings.push(format!(
                    "ignoring the credential helper configured for {host}; firestone reads base64 'auth' entries only"
                ));
            }
        }
        warnings.sort();
        warnings.dedup();
        Self { entries, warnings }
    }

    /// Warnings the caller should surface once, none of which name a secret.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Looks credentials up for one normalized registry host.
    #[must_use]
    pub fn credential(&self, registry: &str) -> Option<&BasicCredential> {
        self.entries.get(registry)
    }
}

/// Authentication scheme named by a `WWW-Authenticate` challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// Token flow: fetch from `realm`, retry once.
    Bearer,
    /// Static credentials, which were already sent when they exist.
    Basic,
}

/// A parsed `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    scheme: AuthScheme,
    parameters: BTreeMap<String, String>,
}

impl AuthChallenge {
    /// The challenge scheme.
    #[must_use]
    pub const fn scheme(&self) -> AuthScheme {
        self.scheme
    }

    /// One lowercase-named challenge parameter.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(String::as_str)
    }
}

/// Parses one `WWW-Authenticate` header value.
///
/// Accepts quoted and unquoted parameters, backslash escapes inside quotes,
/// repeated parameters (first wins), and a case-insensitive scheme name.
///
/// # Errors
///
/// Returns a `dependency` error when the scheme is neither `Bearer` nor
/// `Basic`, which SPEC §8.5 does not implement.
pub fn parse_auth_challenge(value: &str) -> Result<AuthChallenge, FirestoneError> {
    let trimmed = value.trim();
    let (scheme_text, rest) = match trimmed.find(char::is_whitespace) {
        Some(index) => trimmed.split_at(index),
        None => (trimmed, ""),
    };
    let scheme = if scheme_text.eq_ignore_ascii_case("bearer") {
        AuthScheme::Bearer
    } else if scheme_text.eq_ignore_ascii_case("basic") {
        AuthScheme::Basic
    } else {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("registry requested unsupported authentication scheme '{scheme_text}'"),
        )
        .with_hint("firestone authenticates with anonymous Bearer tokens or Basic credentials"));
    };
    Ok(AuthChallenge {
        scheme,
        parameters: parse_challenge_parameters(rest),
    })
}

/// Splits the parameter list of a challenge into lowercase-keyed pairs.
fn parse_challenge_parameters(rest: &str) -> BTreeMap<String, String> {
    let mut parameters = BTreeMap::new();
    let bytes: Vec<char> = rest.chars().collect();
    let mut index = 0_usize;
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_whitespace() || bytes[index] == ',') {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && bytes[index] != '=' && bytes[index] != ',' {
            index += 1;
        }
        let name: String = bytes[start..index].iter().collect();
        let name = name.trim().to_ascii_lowercase();
        if index >= bytes.len() || bytes[index] != '=' {
            if !name.is_empty() {
                parameters.entry(name).or_insert_with(String::new);
            }
            index += 1;
            continue;
        }
        index += 1;
        let mut value = String::new();
        if index < bytes.len() && bytes[index] == '"' {
            index += 1;
            while index < bytes.len() {
                let character = bytes[index];
                if character == '\\' && index + 1 < bytes.len() {
                    value.push(bytes[index + 1]);
                    index += 2;
                    continue;
                }
                if character == '"' {
                    index += 1;
                    break;
                }
                value.push(character);
                index += 1;
            }
        } else {
            while index < bytes.len() && bytes[index] != ',' {
                value.push(bytes[index]);
                index += 1;
            }
        }
        if !name.is_empty() {
            parameters
                .entry(name)
                .or_insert_with(|| value.trim().to_owned());
        }
    }
    parameters
}

/// A bounded, read-only Registry V2 client.
pub struct RegistryClient {
    http: Arc<dyn HttpSource>,
    architecture: Arch,
    insecure_registries: Vec<String>,
    credentials: DockerCredentials,
}

impl RegistryClient {
    /// Builds a client on the shared strict-transport HTTP seam.
    ///
    /// # Errors
    ///
    /// Returns `invalid_spec` when an `images.insecure_registries` entry is not
    /// a bare `host` or `host:port`, or names Docker Hub, and `dependency` when
    /// the HTTPS client cannot be initialized.
    pub fn new(options: &RegistryOptions) -> Result<Self, FirestoneError> {
        Self::with_http(shared_http_source()?, options)
    }

    /// Builds a client on an explicit transport, which the tests script.
    pub(crate) fn with_http(
        http: Arc<dyn HttpSource>,
        options: &RegistryOptions,
    ) -> Result<Self, FirestoneError> {
        let mut insecure_registries = Vec::new();
        for entry in &options.insecure_registries {
            validate_registry_host(entry).map_err(|error| {
                FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!("images.insecure_registries: {}", error.message()),
                )
                .with_hint("write a bare 'host' or 'host:port' entry such as 'localhost:5000'")
            })?;
            let entry = entry.to_ascii_lowercase();
            if entry == DEFAULT_REGISTRY
                || entry == DEFAULT_REGISTRY_ALIAS
                || entry == DOCKER_REGISTRY_ENDPOINT
            {
                return Err(FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!(
                        "images.insecure_registries: '{entry}' cannot be reached over plain HTTP"
                    ),
                )
                .with_hint(
                    "remove the Docker Hub entry; Docker Hub is always contacted over HTTPS",
                ));
            }
            insecure_registries.push(entry);
        }
        insecure_registries.sort();
        insecure_registries.dedup();
        Ok(Self {
            http,
            architecture: options.architecture,
            insecure_registries,
            credentials: DockerCredentials::load(options.docker_config.as_deref()),
        })
    }

    /// Warnings produced while reading credentials; none names a secret.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        self.credentials.warnings()
    }

    /// The architecture used for index platform selection.
    #[must_use]
    pub const fn architecture(&self) -> Arch {
        self.architecture
    }

    /// Reports whether this registry may be contacted over plain HTTP.
    #[must_use]
    pub fn is_insecure(&self, registry: &str) -> bool {
        self.insecure_registries
            .iter()
            .any(|entry| entry == registry)
    }

    /// Scheme used for one registry: HTTPS unless it is explicitly allow-listed.
    fn scheme_for(&self, registry: &str) -> &'static str {
        if self.is_insecure(registry) {
            "http"
        } else {
            "https"
        }
    }

    /// Builds one Registry V2 endpoint URL.
    fn endpoint_url(&self, registry: &str, path: &str) -> Result<Url, FirestoneError> {
        let scheme = self.scheme_for(registry);
        let host = registry_endpoint_host(registry);
        Url::parse(&format!("{scheme}://{host}{path}")).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("cannot build a registry URL for '{registry}'"),
            )
            .with_hint("check the registry host in the image reference")
            .with_source(source)
        })
    }

    /// Resolves a reference to the platform manifest, configuration, and layers.
    ///
    /// # Errors
    ///
    /// Returns `checksum` for a digest mismatch, `not_found` for an unknown
    /// repository or reference, `dependency` for authentication failures,
    /// unsupported media types, and exceeded bounds.
    pub fn resolve(&self, reference: &OciReference) -> Result<ResolvedManifest, FirestoneError> {
        let target = match reference.reference() {
            OciTagOrDigest::Tag(tag) => tag.clone(),
            OciTagOrDigest::Digest(digest) => digest.clone(),
        };
        let top = self.fetch_manifest_document(reference, &target)?;
        let (index_digest, manifest_digest, media_type, manifest) = match parse_document(
            reference, &top,
        )? {
            RegistryDocument::Index(entries) => {
                let selected = select_platform(reference, &entries, self.architecture)?;
                let child = self.fetch_manifest_document(reference, &selected.digest)?;
                match parse_document(reference, &child)? {
                    RegistryDocument::Manifest(manifest) => (
                        Some(top.digest),
                        child.digest,
                        child.media_type.unwrap_or_else(|| {
                            selected
                                .media_type
                                .clone()
                                .unwrap_or_else(|| MEDIA_TYPE_OCI_MANIFEST.to_owned())
                        }),
                        manifest,
                    ),
                    RegistryDocument::Index(_) => {
                        return Err(FirestoneError::new(
                            ErrorKind::Dependency,
                            format!(
                                "registry returned a nested image index for '{reference}' at {}",
                                selected.digest
                            ),
                        )
                        .with_hint("firestone resolves one index level only; pull a platform digest directly"),
                        );
                    }
                }
            }
            RegistryDocument::Manifest(manifest) => (
                None,
                top.digest,
                top.media_type
                    .unwrap_or_else(|| MEDIA_TYPE_OCI_MANIFEST.to_owned()),
                manifest,
            ),
        };

        if manifest.layers.len() > MAX_LAYERS {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "manifest for '{reference}' declares {} layers, more than the {MAX_LAYERS} firestone reads",
                    manifest.layers.len()
                ),
            )
            .with_hint("pull a squashed image with at most 128 layers"));
        }
        if manifest.layers.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("manifest for '{reference}' declares no layers"),
            )
            .with_hint("pull an image that ships a root filesystem"));
        }

        let mut layers = Vec::with_capacity(manifest.layers.len());
        for descriptor in &manifest.layers {
            classify_layer_media_type(&descriptor.media_type)?;
            validate_digest(&descriptor.digest, "layer")?;
            layers.push(LayerDescriptor {
                digest: descriptor.digest.clone(),
                media_type: descriptor.media_type.clone(),
                size: descriptor.size,
            });
        }

        validate_digest(&manifest.config.digest, "image config")?;
        let config_bytes = self.fetch_blob_bytes(
            reference,
            &manifest.config.digest,
            MAX_CONFIG_BYTES,
            "image config",
        )?;
        let document: ImageConfigDocument =
            serde_json::from_slice(&config_bytes).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("cannot read the image configuration for '{reference}'"),
                )
                .with_hint("the registry returned a configuration firestone cannot parse")
                .with_source(source)
            })?;

        Ok(ResolvedManifest {
            reference: reference.to_string(),
            manifest_digest,
            index_digest,
            manifest_media_type: media_type,
            config_digest: manifest.config.digest.clone(),
            config: document.config,
            layers,
        })
    }

    /// Streams one layer blob to `writer`, verifying its digest and size.
    ///
    /// `progress` is called with `(done, Some(total))` at most once per accumulated
    /// mebibyte plus once at the end, matching the §8.3 pull progress contract.
    ///
    /// # Errors
    ///
    /// Returns `checksum` when the stream is short, long, or hashes to another
    /// digest, and `dependency` for an unsupported layer media type.
    pub fn fetch_layer(
        &self,
        reference: &OciReference,
        descriptor: &LayerDescriptor,
        writer: &mut dyn Write,
        progress: &mut dyn FnMut(u64, Option<u64>) -> Result<(), FirestoneError>,
    ) -> Result<(), FirestoneError> {
        classify_layer_media_type(&descriptor.media_type)?;
        validate_digest(&descriptor.digest, "layer")?;
        let url = self.endpoint_url(
            reference.registry(),
            &format!("/v2/{}/blobs/{}", reference.repository(), descriptor.digest),
        )?;
        let mut response = self.request(reference, &url, None)?;
        if let Some(length) = response.content_length {
            if length != descriptor.size {
                return Err(blob_length_error(reference, descriptor, length));
            }
        }

        let total = Some(descriptor.size);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; LAYER_BUFFER_BYTES];
        let mut done = 0_u64;
        let mut last_progress = 0_u64;
        loop {
            let read = response.body.read(&mut buffer).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot read layer {} of '{reference}'", descriptor.digest),
                )
                .with_hint("check network access and retry the pull")
                .with_source(source)
            })?;
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            done = done.saturating_add(read as u64);
            if done > descriptor.size {
                return Err(FirestoneError::new(
                    ErrorKind::Checksum,
                    format!(
                        "layer {} of '{reference}' exceeded its declared {} bytes",
                        descriptor.digest, descriptor.size
                    ),
                )
                .with_hint("retry the pull; the registry response was inconsistent"));
            }
            hasher.update(chunk);
            writer.write_all(chunk).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot write layer {} of '{reference}'", descriptor.digest),
                )
                .with_hint("check free space in the firestone images directory")
                .with_source(source)
            })?;
            if done.saturating_sub(last_progress) >= PROGRESS_INTERVAL_BYTES {
                progress(done, total)?;
                last_progress = done;
            }
        }
        if done != descriptor.size {
            return Err(FirestoneError::new(
                ErrorKind::Checksum,
                format!(
                    "layer {} of '{reference}' ended after {done} bytes; the manifest declared {}",
                    descriptor.digest, descriptor.size
                ),
            )
            .with_hint("retry the pull; the registry response was partial"));
        }
        let actual = hex_digest(hasher.finalize().as_slice());
        verify_digest(
            &descriptor.digest,
            &actual,
            &format!("layer of '{reference}'"),
        )?;
        progress(done, total)?;
        Ok(())
    }

    /// Fetches one manifest or index and verifies its bytes.
    fn fetch_manifest_document(
        &self,
        reference: &OciReference,
        target: &str,
    ) -> Result<FetchedDocument, FirestoneError> {
        let url = self.endpoint_url(
            reference.registry(),
            &format!("/v2/{}/manifests/{target}", reference.repository()),
        )?;
        let response = self.request(reference, &url, Some(MANIFEST_ACCEPT))?;
        let media_type = response
            .header("content-type")
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned());
        let content_digest = response
            .header("docker-content-digest")
            .map(ToOwned::to_owned);
        let bytes = read_bounded(
            response,
            MAX_MANIFEST_BYTES,
            &format!("manifest '{target}' of '{reference}'"),
        )?;
        let digest = format!(
            "{DIGEST_ALGORITHM}:{}",
            hex_digest(Sha256::digest(&bytes).as_slice())
        );
        if target.starts_with(DIGEST_ALGORITHM) && target.contains(':') {
            verify_digest(
                target,
                digest_hex(&digest),
                &format!("manifest of '{reference}'"),
            )?;
        }
        if let Some(advertised) = content_digest.as_deref() {
            if !advertised.eq_ignore_ascii_case(&digest) {
                return Err(FirestoneError::new(
                    ErrorKind::Checksum,
                    format!(
                        "registry reported manifest digest {advertised} for '{reference}' but the bytes hash to {digest}"
                    ),
                )
                .with_hint("retry the pull; the registry response did not match its own digest"));
            }
        }
        Ok(FetchedDocument {
            bytes,
            digest,
            media_type,
        })
    }

    /// Fetches one small blob whole, verifying its digest.
    fn fetch_blob_bytes(
        &self,
        reference: &OciReference,
        digest: &str,
        limit: u64,
        label: &str,
    ) -> Result<Vec<u8>, FirestoneError> {
        let url = self.endpoint_url(
            reference.registry(),
            &format!("/v2/{}/blobs/{digest}", reference.repository()),
        )?;
        let response = self.request(reference, &url, None)?;
        let bytes = read_bounded(response, limit, &format!("{label} of '{reference}'"))?;
        let actual = hex_digest(Sha256::digest(&bytes).as_slice());
        verify_digest(digest, &actual, &format!("{label} of '{reference}'"))?;
        Ok(bytes)
    }

    /// Issues one request, performing at most one token fetch and one retry.
    fn request(
        &self,
        reference: &OciReference,
        url: &Url,
        accept: Option<&str>,
    ) -> Result<HttpStatusResponse, FirestoneError> {
        let over_https = url.scheme() == "https";
        let credential = over_https
            .then(|| self.credentials.credential(reference.registry()))
            .flatten();
        let mut headers = Vec::new();
        if let Some(accept) = accept {
            headers.push(("accept", accept.to_owned()));
        }
        if let Some(credential) = credential {
            headers.push(("authorization", credential.header_value()));
        }
        let response = self.http.send(&HttpRequest {
            url,
            headers: &headers,
        })?;
        if response.status != 401 {
            return check_status(reference, url, response);
        }

        let challenge = response
            .header("www-authenticate")
            .map(parse_auth_challenge)
            .transpose()?;
        let Some(challenge) = challenge.filter(|challenge| challenge.scheme == AuthScheme::Bearer)
        else {
            return Err(authentication_error(reference, over_https, credential));
        };
        let token = self.fetch_token(reference, &challenge, over_https, credential)?;
        let mut retry_headers = Vec::new();
        if let Some(accept) = accept {
            retry_headers.push(("accept", accept.to_owned()));
        }
        retry_headers.push(("authorization", format!("Bearer {token}")));
        let retried = self.http.send(&HttpRequest {
            url,
            headers: &retry_headers,
        })?;
        if retried.status == 401 {
            return Err(authentication_error(reference, over_https, credential));
        }
        check_status(reference, url, retried)
    }

    /// Performs the single token fetch a Bearer challenge asks for.
    fn fetch_token(
        &self,
        reference: &OciReference,
        challenge: &AuthChallenge,
        over_https: bool,
        credential: Option<&BasicCredential>,
    ) -> Result<String, FirestoneError> {
        let realm = challenge.parameter("realm").ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "registry '{}' sent a Bearer challenge without a realm",
                    reference.registry()
                ),
            )
            .with_hint("the registry is not speaking the Registry V2 token protocol")
        })?;
        let mut url = Url::parse(realm).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "registry '{}' sent an unusable token realm",
                    reference.registry()
                ),
            )
            .with_hint("the registry is not speaking the Registry V2 token protocol")
            .with_source(source)
        })?;
        if !self.token_realm_allowed(&url, reference.registry()) {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "registry '{}' sent a plain-HTTP token realm '{url}'",
                    reference.registry()
                ),
            )
            .with_hint(
                "token realms must use HTTPS unless the registry is in images.insecure_registries",
            ));
        }
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = challenge.parameter("service") {
                query.append_pair("service", service);
            }
            let scope = challenge.parameter("scope").map_or_else(
                || format!("repository:{}:pull", reference.repository()),
                ToOwned::to_owned,
            );
            query.append_pair("scope", &scope);
        }
        let mut headers = Vec::new();
        if url.scheme() == "https" {
            if let Some(credential) = credential {
                headers.push(("authorization", credential.header_value()));
            }
        }
        let response = self.http.send(&HttpRequest {
            url: &url,
            headers: &headers,
        })?;
        if response.status != 200 {
            return Err(authentication_error(reference, over_https, credential));
        }
        let bytes = read_bounded(
            response,
            MAX_TOKEN_BYTES,
            &format!("token response for '{}'", reference.registry()),
        )?;
        let token: TokenResponse = serde_json::from_slice(&bytes).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "registry '{}' returned an unreadable token response",
                    reference.registry()
                ),
            )
            .with_hint("the registry is not speaking the Registry V2 token protocol")
            .with_source(source)
        })?;
        token
            .token
            .or(token.access_token)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "registry '{}' returned a token response without a token",
                        reference.registry()
                    ),
                )
                .with_hint("the registry is not speaking the Registry V2 token protocol")
            })
    }

    /// A token realm must be HTTPS, or plain HTTP on an allow-listed registry.
    fn token_realm_allowed(&self, realm: &Url, registry: &str) -> bool {
        match realm.scheme() {
            "https" => true,
            "http" => self.is_insecure(registry) && realm_matches_registry(realm, registry),
            _ => false,
        }
    }
}

/// One fetched manifest document plus the digest of its exact bytes.
struct FetchedDocument {
    bytes: Vec<u8>,
    digest: String,
    media_type: Option<String>,
}

/// The two document shapes a manifest request can return.
enum RegistryDocument {
    Index(Vec<IndexEntry>),
    Manifest(ImageManifest),
}

/// One entry of an image index or manifest list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    #[serde(default)]
    media_type: Option<String>,
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
    #[serde(default)]
    artifact_type: Option<String>,
}

/// The platform of one index entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Platform {
    #[serde(default)]
    os: String,
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default, rename = "os.features")]
    os_features: Option<Vec<String>>,
}

impl Platform {
    /// `os/architecture[/variant]`, as listed by a no-match error.
    fn label(&self) -> String {
        match self.variant.as_deref() {
            Some(variant) if !variant.is_empty() => {
                format!("{}/{}/{variant}", self.os, self.architecture)
            }
            _ => format!("{}/{}", self.os, self.architecture),
        }
    }
}

/// One image manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageManifest {
    config: BlobDescriptor,
    #[serde(default)]
    layers: Vec<BlobDescriptor>,
}

/// A blob descriptor inside a manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlobDescriptor {
    #[serde(default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
}

/// The manifest fields needed to tell an index from a manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentProbe {
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    manifests: Option<Vec<IndexEntry>>,
    #[serde(default)]
    config: Option<BlobDescriptor>,
}

/// The image configuration blob, of which Firestone keeps `config`.
#[derive(Debug, Clone, Default, Deserialize)]
struct ImageConfigDocument {
    #[serde(default)]
    config: OciImageConfig,
}

/// A token endpoint response.
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// `~/.docker/config.json`, of which Firestone reads only `auths`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DockerConfigDocument {
    #[serde(default)]
    auths: BTreeMap<String, DockerAuthEntry>,
    #[serde(default)]
    creds_store: Option<String>,
    #[serde(default)]
    cred_helpers: BTreeMap<String, String>,
}

/// One `auths` entry.
#[derive(Debug, Clone, Default, Deserialize)]
struct DockerAuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default, rename = "identitytoken")]
    identity_token: Option<String>,
}

/// Decides whether a fetched document is an index or a manifest.
fn parse_document(
    reference: &OciReference,
    document: &FetchedDocument,
) -> Result<RegistryDocument, FirestoneError> {
    let probe: DocumentProbe = serde_json::from_slice(&document.bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("registry returned an unreadable manifest for '{reference}'"),
        )
        .with_hint("retry the pull; the registry response was not a Registry V2 manifest")
        .with_source(source)
    })?;
    let media_type = probe
        .media_type
        .as_deref()
        .or(document.media_type.as_deref())
        .unwrap_or_default()
        .to_owned();
    match media_type.as_str() {
        MEDIA_TYPE_OCI_INDEX | MEDIA_TYPE_DOCKER_MANIFEST_LIST => {
            return Ok(RegistryDocument::Index(probe.manifests.unwrap_or_default()));
        }
        MEDIA_TYPE_OCI_MANIFEST | MEDIA_TYPE_DOCKER_MANIFEST => {
            return parse_manifest_body(reference, &document.bytes).map(RegistryDocument::Manifest);
        }
        _ => {}
    }
    if !media_type.is_empty() && media_type != "application/json" {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("registry returned media type '{media_type}' for '{reference}'"),
        )
        .with_hint(format!("firestone reads {MANIFEST_ACCEPT}")));
    }
    if probe.manifests.is_some() {
        return Ok(RegistryDocument::Index(probe.manifests.unwrap_or_default()));
    }
    if probe.config.is_some() {
        return parse_manifest_body(reference, &document.bytes).map(RegistryDocument::Manifest);
    }
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "registry returned a document for '{reference}' that is neither a manifest nor an index"
        ),
    )
    .with_hint(format!("firestone reads {MANIFEST_ACCEPT}")))
}

/// Parses one manifest body.
fn parse_manifest_body(
    reference: &OciReference,
    bytes: &[u8],
) -> Result<ImageManifest, FirestoneError> {
    serde_json::from_slice(bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("registry returned an unreadable manifest for '{reference}'"),
        )
        .with_hint("retry the pull; the registry response was not a Registry V2 manifest")
        .with_source(source)
    })
}

/// Selects the one index entry that matches the host platform.
fn select_platform<'entries>(
    reference: &OciReference,
    entries: &'entries [IndexEntry],
    architecture: Arch,
) -> Result<&'entries IndexEntry, FirestoneError> {
    let wanted = match architecture {
        Arch::X86_64 => "amd64",
        Arch::Aarch64 => "arm64",
    };
    let mut offered = Vec::new();
    let mut selected = None;
    for entry in entries {
        let Some(platform) = entry.platform.as_ref() else {
            continue;
        };
        offered.push(platform.label());
        if entry.artifact_type.is_some() {
            continue;
        }
        if let Some(media_type) = entry.media_type.as_deref() {
            if media_type != MEDIA_TYPE_OCI_MANIFEST
                && media_type != MEDIA_TYPE_DOCKER_MANIFEST
                && media_type != MEDIA_TYPE_OCI_INDEX
                && media_type != MEDIA_TYPE_DOCKER_MANIFEST_LIST
            {
                continue;
            }
        }
        if platform.os_features.is_some() {
            continue;
        }
        if platform.os != "linux" || platform.architecture != wanted {
            continue;
        }
        if architecture == Arch::Aarch64 {
            if let Some(variant) = platform.variant.as_deref() {
                if !variant.is_empty() && variant != "v8" {
                    continue;
                }
            }
        }
        if selected.is_none() {
            selected = Some(entry);
        }
    }
    selected.ok_or_else(|| {
        offered.sort();
        offered.dedup();
        let listed = if offered.is_empty() {
            "nothing".to_owned()
        } else {
            offered.join(", ")
        };
        FirestoneError::new(
            ErrorKind::NotFound,
            format!(
                "image '{reference}' has no linux/{wanted} manifest; the index offers {listed}"
            ),
        )
        .with_hint("pull an image published for this host architecture")
    })
}

/// Maps a registry host to the host that actually serves Registry V2.
#[must_use]
pub fn registry_endpoint_host(registry: &str) -> &str {
    if registry == DEFAULT_REGISTRY || registry == DEFAULT_REGISTRY_ALIAS {
        DOCKER_REGISTRY_ENDPOINT
    } else {
        registry
    }
}

/// Reports whether a plain-HTTP realm stays on the allow-listed registry.
fn realm_matches_registry(realm: &Url, registry: &str) -> bool {
    let Some(host) = realm.host_str() else {
        return false;
    };
    let authority = match realm.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    authority.eq_ignore_ascii_case(registry)
}

/// Maps one `~/.docker/config.json` key to a normalized registry host.
fn credential_host(key: &str) -> String {
    let trimmed = key
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let authority = trimmed.split('/').next().unwrap_or(trimmed);
    let host = authority.to_ascii_lowercase();
    if host == DEFAULT_REGISTRY_ALIAS || host == DOCKER_REGISTRY_ENDPOINT {
        DEFAULT_REGISTRY.to_owned()
    } else {
        host
    }
}

/// Decodes one base64 `user:password` field.
fn decode_basic_auth(value: &str) -> Option<BasicCredential> {
    let decoded = base64_decode(value.trim())?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, secret) = text.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some(BasicCredential {
        user: user.to_owned(),
        secret: secret.to_owned(),
    })
}

/// Standard base64 alphabet.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes bytes as standard padded base64.
fn base64_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0_u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let value = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        let indexes = [
            (value >> 18) & 0x3f,
            (value >> 12) & 0x3f,
            (value >> 6) & 0x3f,
            value & 0x3f,
        ];
        for (position, index) in indexes.iter().enumerate() {
            if position <= chunk.len() {
                let index = usize::try_from(*index).unwrap_or(0);
                output.push(char::from(BASE64_ALPHABET[index]));
            } else {
                output.push('=');
            }
        }
    }
    output
}

/// Decodes standard base64, with or without padding.
fn base64_decode(value: &str) -> Option<Vec<u8>> {
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    let mut output = Vec::with_capacity(value.len() / 4 * 3);
    for character in value.chars() {
        if character == '=' {
            break;
        }
        if character.is_whitespace() {
            continue;
        }
        let symbol = u8::try_from(character).ok()?;
        let index = BASE64_ALPHABET.iter().position(|entry| *entry == symbol)?;
        accumulator = (accumulator << 6) | u32::try_from(index).ok()?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            let byte = u8::try_from((accumulator >> bits) & 0xff).ok()?;
            output.push(byte);
        }
    }
    Some(output)
}

/// Renders bytes as lowercase hexadecimal.
fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

/// Returns the hexadecimal half of a `sha256:…` digest, or the whole string.
fn digest_hex(digest: &str) -> &str {
    digest.split_once(':').map_or(digest, |(_, hex)| hex)
}

/// Rejects a digest that is not `sha256:` plus 64 lowercase hex characters.
fn validate_digest(digest: &str, label: &str) -> Result<(), FirestoneError> {
    let valid = digest.split_once(':').is_some_and(|(algorithm, hex)| {
        algorithm == DIGEST_ALGORITHM
            && hex.len() == DIGEST_HEX_LENGTH
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    if valid {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Checksum,
        format!("{label} descriptor carries an unsupported digest '{digest}'"),
    )
    .with_hint("firestone accepts sha256 digests only"))
}

/// Compares an expected digest with the digest of the bytes that arrived.
fn verify_digest(expected: &str, actual_hex: &str, label: &str) -> Result<(), FirestoneError> {
    if digest_hex(expected).eq_ignore_ascii_case(actual_hex) {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Checksum,
        format!(
            "{label} hashes to {DIGEST_ALGORITHM}:{actual_hex} but the registry named {expected}"
        ),
    )
    .with_hint("retry the pull; the registry returned bytes that do not match their digest"))
}

/// Reads one response body under a byte cap.
fn read_bounded(
    mut response: HttpStatusResponse,
    limit: u64,
    label: &str,
) -> Result<Vec<u8>, FirestoneError> {
    bounded::read_to_end(&mut *response.body, limit).map_err(|error| match error {
        BoundedReadError::LimitExceeded => FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} is larger than the {limit} byte limit firestone reads"),
        )
        .with_hint("the registry returned an oversized document; pull a smaller image"),
        BoundedReadError::Io(source) => {
            FirestoneError::new(ErrorKind::Generic, format!("cannot read {label}"))
                .with_hint("check network access and retry the pull")
                .with_source(source)
        }
    })
}

/// Turns a non-success status into an actionable error.
fn check_status(
    reference: &OciReference,
    url: &Url,
    response: HttpStatusResponse,
) -> Result<HttpStatusResponse, FirestoneError> {
    match response.status {
        200..=299 => Ok(response),
        404 => Err(FirestoneError::new(
            ErrorKind::NotFound,
            format!(
                "registry '{}' has no '{}' matching '{reference}'",
                reference.registry(),
                reference.repository()
            ),
        )
        .with_hint("check the repository name and tag, or run `docker login` for a private image")),
        429 => Err(FirestoneError::new(
            ErrorKind::Busy,
            format!(
                "registry '{}' rate-limited the pull of '{reference}'",
                reference.registry()
            ),
        )
        .with_hint("wait and retry, or authenticate with `docker login`")),
        status => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("registry request for '{reference}' failed with HTTP {status}"),
        )
        .with_hint(format!(
            "the registry endpoint {} rejected a read-only Registry V2 request",
            redacted_endpoint(url)
        ))),
    }
}

/// The scheme and authority of a URL, with any query or credential dropped.
fn redacted_endpoint(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

/// The error raised after the one permitted authentication retry failed.
fn authentication_error(
    reference: &OciReference,
    over_https: bool,
    credential: Option<&BasicCredential>,
) -> FirestoneError {
    let hint = if over_https {
        match credential {
            Some(credential) => format!(
                "the credentials stored for {} as user '{}' were rejected; run `docker login {}`",
                reference.registry(),
                credential.user(),
                reference.registry()
            ),
            None => format!("run `docker login {}` and retry", reference.registry()),
        }
    } else {
        format!(
            "firestone never sends credentials over plain HTTP; serve {} over HTTPS to authenticate",
            reference.registry()
        )
    };
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "registry '{}' denied access to repository '{}'",
            reference.registry(),
            reference.repository()
        ),
    )
    .with_hint(hint)
}

/// The error raised when a blob advertises a length the manifest disagrees with.
fn blob_length_error(
    reference: &OciReference,
    descriptor: &LayerDescriptor,
    length: u64,
) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Checksum,
        format!(
            "layer {} of '{reference}' declared {length} bytes but the manifest declared {}",
            descriptor.digest, descriptor.size
        ),
    )
    .with_hint("retry the pull; the registry response was inconsistent")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::{self, Cursor},
        sync::Mutex,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::image::HttpResponse;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[derive(Clone)]
    struct ScriptedReply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        content_length: Option<u64>,
    }

    impl ScriptedReply {
        fn ok(body: Vec<u8>) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body,
                content_length: None,
            }
        }

        fn json(body: &str) -> Self {
            Self::ok(body.as_bytes().to_vec())
        }

        fn status(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: Vec::new(),
                content_length: None,
            }
        }

        fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }

        fn with_content_length(mut self, length: u64) -> Self {
            self.content_length = Some(length);
            self
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        url: String,
        headers: Vec<(String, String)>,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    #[derive(Default)]
    struct ScriptedHttp {
        replies: Mutex<BTreeMap<String, VecDeque<ScriptedReply>>>,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    impl ScriptedHttp {
        fn push(&self, url: &str, reply: ScriptedReply) -> TestResult {
            let mut replies = self
                .replies
                .lock()
                .map_err(|_| io::Error::other("scripted HTTP mutex poisoned"))?;
            replies.entry(url.to_owned()).or_default().push_back(reply);
            Ok(())
        }

        fn requests(&self) -> Result<Vec<RecordedRequest>, Box<dyn std::error::Error>> {
            let requests = self
                .requests
                .lock()
                .map_err(|_| io::Error::other("scripted HTTP mutex poisoned"))?;
            Ok(requests.clone())
        }
    }

    impl HttpSource for ScriptedHttp {
        fn get(&self, url: &Url) -> Result<HttpResponse, FirestoneError> {
            let response = self.send(&HttpRequest { url, headers: &[] })?;
            let content_type = response.header("content-type").map(ToOwned::to_owned);
            Ok(HttpResponse {
                body: response.body,
                content_length: response.content_length,
                content_type,
            })
        }

        fn send(&self, request: &HttpRequest<'_>) -> Result<HttpStatusResponse, FirestoneError> {
            {
                let mut requests = self.requests.lock().map_err(|_| {
                    FirestoneError::new(ErrorKind::Generic, "scripted HTTP mutex poisoned")
                })?;
                requests.push(RecordedRequest {
                    url: request.url.to_string(),
                    headers: request
                        .headers
                        .iter()
                        .map(|(name, value)| ((*name).to_owned(), value.clone()))
                        .collect(),
                });
            }
            let mut replies = self.replies.lock().map_err(|_| {
                FirestoneError::new(ErrorKind::Generic, "scripted HTTP mutex poisoned")
            })?;
            let reply = replies
                .get_mut(request.url.as_str())
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        format!("no scripted registry response for '{}'", request.url),
                    )
                })?;
            Ok(HttpStatusResponse {
                status: reply.status,
                headers: reply.headers,
                content_length: reply.content_length,
                body: Box::new(Cursor::new(reply.body)),
            })
        }
    }

    fn digest_of(bytes: &[u8]) -> String {
        format!(
            "{DIGEST_ALGORITHM}:{}",
            hex_digest(Sha256::digest(bytes).as_slice())
        )
    }

    fn config_body() -> String {
        r#"{"architecture":"amd64","os":"linux","config":{"Env":["PATH=/usr/bin"],"Entrypoint":["/entry.sh"],"Cmd":["run"],"WorkingDir":"/srv","User":"root"},"rootfs":{"type":"layers","diff_ids":[]}}"#
            .to_owned()
    }

    fn manifest_body(config_digest: &str, layer_digest: &str, layer_size: u64) -> String {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{MEDIA_TYPE_OCI_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":10}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer_digest}","size":{layer_size}}}]}}"#
        )
    }

    fn client(
        http: Arc<ScriptedHttp>,
        options: &RegistryOptions,
    ) -> Result<RegistryClient, FirestoneError> {
        RegistryClient::with_http(http, options)
    }

    struct ScriptedRegistry {
        http: Arc<ScriptedHttp>,
        reference: OciReference,
        manifest_digest: String,
        layer_digest: String,
    }

    fn scripted_registry(
        reference: &str,
        layer: &[u8],
    ) -> Result<ScriptedRegistry, Box<dyn std::error::Error>> {
        let parsed = OciReference::parse(reference)?;
        let config = config_body();
        let config_digest = digest_of(config.as_bytes());
        let layer_digest = digest_of(layer);
        let manifest = manifest_body(&config_digest, &layer_digest, layer.len() as u64);
        let manifest_digest = digest_of(manifest.as_bytes());
        let http = Arc::new(ScriptedHttp::default());
        let host = registry_endpoint_host(parsed.registry());
        let repository = parsed.repository().to_owned();
        let target = match parsed.reference() {
            OciTagOrDigest::Tag(tag) => tag.clone(),
            OciTagOrDigest::Digest(digest) => digest.clone(),
        };
        http.push(
            &format!("https://{host}/v2/{repository}/manifests/{target}"),
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        http.push(
            &format!("https://{host}/v2/{repository}/blobs/{config_digest}"),
            ScriptedReply::json(&config),
        )?;
        http.push(
            &format!("https://{host}/v2/{repository}/blobs/{layer_digest}"),
            ScriptedReply::ok(layer.to_vec()),
        )?;
        Ok(ScriptedRegistry {
            http,
            reference: parsed,
            manifest_digest,
            layer_digest,
        })
    }

    #[test]
    fn parse_auth_challenge_quoted_parameters_expected_all_parsed() -> TestResult {
        let cases = [
            (
                r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
                "https://auth.docker.io/token",
                Some("registry.docker.io"),
                Some("repository:library/nginx:pull"),
            ),
            (
                "bearer realm=https://auth.example.com/token,service=example",
                "https://auth.example.com/token",
                Some("example"),
                None,
            ),
            (
                r#"Bearer   realm="https://auth.example.com/to\"ken" , error="insufficient_scope""#,
                "https://auth.example.com/to\"ken",
                None,
                None,
            ),
        ];

        for (header, realm, service, scope) in cases {
            let challenge = parse_auth_challenge(header)?;
            assert_eq!(challenge.scheme(), AuthScheme::Bearer, "{header}");
            assert_eq!(challenge.parameter("realm"), Some(realm), "{header}");
            assert_eq!(challenge.parameter("service"), service, "{header}");
            assert_eq!(challenge.parameter("scope"), scope, "{header}");
        }
        Ok(())
    }

    #[test]
    fn parse_auth_challenge_unsupported_scheme_expected_dependency_error() {
        let Err(error) = parse_auth_challenge("Negotiate realm=\"https://auth.example.com\"")
        else {
            panic!("expected an unsupported scheme error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("Negotiate"), "{}", error.message());
    }

    #[test]
    fn parse_auth_challenge_basic_scheme_expected_basic() -> TestResult {
        let challenge = parse_auth_challenge("Basic realm=\"registry\"")?;

        assert_eq!(challenge.scheme(), AuthScheme::Basic);
        assert_eq!(challenge.parameter("realm"), Some("registry"));
        Ok(())
    }

    #[test]
    fn resolve_manifest_and_config_expected_descriptors() -> TestResult {
        let layer = b"layer-bytes".to_vec();
        let scripted = scripted_registry("docker://alpine", &layer)?;
        let (http, reference, manifest_digest, layer_digest) = (
            scripted.http,
            scripted.reference,
            scripted.manifest_digest,
            scripted.layer_digest,
        );
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let resolved = client.resolve(&reference)?;

        assert_eq!(resolved.reference, "docker.io/library/alpine:latest");
        assert_eq!(resolved.manifest_digest, manifest_digest);
        assert_eq!(resolved.index_digest, None);
        assert_eq!(resolved.layers.len(), 1);
        assert_eq!(resolved.layers[0].digest, layer_digest);
        assert_eq!(resolved.layers[0].size, layer.len() as u64);
        assert_eq!(resolved.config.entrypoint, vec!["/entry.sh".to_owned()]);
        assert_eq!(resolved.config.cmd, vec!["run".to_owned()]);
        assert_eq!(resolved.config.working_dir.as_deref(), Some("/srv"));
        Ok(())
    }

    #[test]
    fn resolve_docker_io_reference_expected_registry_1_endpoint() -> TestResult {
        let layer = b"layer".to_vec();
        let scripted = scripted_registry("docker://alpine", &layer)?;
        let (http, reference) = (scripted.http, scripted.reference);
        let client = client(http.clone(), &RegistryOptions::new(Arch::X86_64))?;

        client.resolve(&reference)?;

        let requests = http.requests()?;
        assert!(
            requests.iter().all(|request| request
                .url
                .starts_with("https://registry-1.docker.io/v2/library/alpine/")),
            "{requests:?}"
        );
        assert_eq!(
            requests[0].header("accept"),
            Some(MANIFEST_ACCEPT),
            "{requests:?}"
        );
        Ok(())
    }

    #[test]
    fn resolve_bearer_challenge_expected_token_fetch_and_single_retry() -> TestResult {
        let layer = b"layer".to_vec();
        let scripted = scripted_registry("ghcr.io/org/app:1", &layer)?;
        let (http, reference) = (scripted.http, scripted.reference);
        let manifest_url = "https://ghcr.io/v2/org/app/manifests/1";
        {
            let mut replies = http
                .replies
                .lock()
                .map_err(|_| io::Error::other("scripted HTTP mutex poisoned"))?;
            let queue = replies
                .get_mut(manifest_url)
                .ok_or_else(|| io::Error::other("missing scripted manifest"))?;
            queue.push_front(
                ScriptedReply::status(401).with_header(
                    "www-authenticate",
                    r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:org/app:pull""#,
                ),
            );
        }
        http.push(
            "https://ghcr.io/token?service=ghcr.io&scope=repository%3Aorg%2Fapp%3Apull",
            ScriptedReply::json(r#"{"token":"deadbeef"}"#),
        )?;
        let client = client(http.clone(), &RegistryOptions::new(Arch::X86_64))?;

        client.resolve(&reference)?;

        let requests = http.requests()?;
        assert_eq!(requests[0].header("authorization"), None, "{requests:?}");
        assert!(
            requests[1].url.starts_with("https://ghcr.io/token?"),
            "{requests:?}"
        );
        assert_eq!(requests[2].url, manifest_url, "{requests:?}");
        assert_eq!(
            requests[2].header("authorization"),
            Some("Bearer deadbeef"),
            "{requests:?}"
        );
        Ok(())
    }

    #[test]
    fn resolve_second_unauthorized_expected_dependency_error_naming_repository() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let http = Arc::new(ScriptedHttp::default());
        let manifest_url = "https://ghcr.io/v2/org/app/manifests/1";
        http.push(
            manifest_url,
            ScriptedReply::status(401).with_header(
                "www-authenticate",
                r#"Bearer realm="https://ghcr.io/token""#,
            ),
        )?;
        http.push(
            "https://ghcr.io/token?scope=repository%3Aorg%2Fapp%3Apull",
            ScriptedReply::json(r#"{"access_token":"token"}"#),
        )?;
        http.push(manifest_url, ScriptedReply::status(401))?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected an authentication error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("org/app"), "{}", error.message());
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("docker login ghcr.io")),
            "{:?}",
            error.hint()
        );
        Ok(())
    }

    #[test]
    fn resolve_with_docker_credentials_expected_basic_authorization_header() -> TestResult {
        let directory = TempDir::new()?;
        let config = directory.path().join("config.json");
        let encoded = base64_encode(b"alice:s3cret");
        fs::write(
            &config,
            format!(r#"{{"auths":{{"ghcr.io":{{"auth":"{encoded}"}}}}}}"#),
        )?;
        let layer = b"layer".to_vec();
        let scripted = scripted_registry("ghcr.io/org/app:1", &layer)?;
        let (http, reference) = (scripted.http, scripted.reference);
        let options = RegistryOptions::new(Arch::X86_64).with_docker_config(Some(config));
        let client = client(http.clone(), &options)?;

        client.resolve(&reference)?;

        let requests = http.requests()?;
        assert_eq!(
            requests[0].header("authorization"),
            Some(format!("Basic {encoded}").as_str()),
            "{requests:?}"
        );
        Ok(())
    }

    #[test]
    fn docker_credentials_helper_entries_expected_ignored_with_warnings() -> TestResult {
        let directory = TempDir::new()?;
        let path = directory.path().join("config.json");
        let encoded = base64_encode(b"bob:pw");
        let document = format!(
            r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{encoded}"}},"quay.io":{{"identitytoken":"t"}}}},"credsStore":"osxkeychain","credHelpers":{{"gcr.io":"gcloud"}}}}"#
        );
        fs::write(&path, document)?;

        let credentials = DockerCredentials::load(Some(&path));

        let docker = credentials
            .credential("docker.io")
            .ok_or_else(|| io::Error::other("missing docker.io credential"))?;
        assert_eq!(docker.user(), "bob");
        assert!(credentials.credential("gcr.io").is_none());
        assert!(
            credentials
                .warnings()
                .iter()
                .any(|warning| warning.contains("osxkeychain")),
            "{:?}",
            credentials.warnings()
        );
        assert!(
            credentials
                .warnings()
                .iter()
                .any(|warning| warning.contains("gcr.io")),
            "{:?}",
            credentials.warnings()
        );
        assert!(
            credentials
                .warnings()
                .iter()
                .any(|warning| warning.contains("quay.io")),
            "{:?}",
            credentials.warnings()
        );
        assert!(
            !credentials
                .warnings()
                .iter()
                .any(|warning| warning.contains("pw")),
            "{:?}",
            credentials.warnings()
        );
        Ok(())
    }

    #[test]
    fn docker_credentials_missing_file_expected_anonymous() {
        let credentials = DockerCredentials::load(Some(Path::new("/nonexistent/config.json")));

        assert!(credentials.credential("docker.io").is_none());
        assert!(credentials.warnings().is_empty());
        assert!(
            DockerCredentials::load(None)
                .credential("ghcr.io")
                .is_none()
        );
    }

    #[test]
    fn credential_debug_expected_secret_redacted() -> TestResult {
        let credential = decode_basic_auth(&base64_encode(b"alice:s3cret"))
            .ok_or_else(|| io::Error::other("credential did not decode"))?;

        let rendered = format!("{credential:?}");

        assert!(rendered.contains("alice"), "{rendered}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
        Ok(())
    }

    #[test]
    fn select_platform_index_expected_host_architecture_entry() -> TestResult {
        let entries: Vec<IndexEntry> = serde_json::from_str(
            r#"[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:aa","platform":{"os":"linux","architecture":"arm64","variant":"v8"}},
                {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:bb","platform":{"os":"linux","architecture":"amd64"}},
                {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:cc","platform":{"os":"windows","architecture":"amd64"}}]"#,
        )?;
        let reference = OciReference::parse("ghcr.io/org/app:1")?;

        let cases = [(Arch::X86_64, "sha256:bb"), (Arch::Aarch64, "sha256:aa")];
        for (architecture, expected) in cases {
            let selected = select_platform(&reference, &entries, architecture)?;
            assert_eq!(selected.digest, expected, "{architecture}");
        }
        Ok(())
    }

    #[test]
    fn select_platform_no_match_expected_not_found_listing_offers() -> TestResult {
        let entries: Vec<IndexEntry> = serde_json::from_str(
            r#"[{"digest":"sha256:aa","platform":{"os":"linux","architecture":"riscv64"}},
                {"digest":"sha256:bb","platform":{"os":"linux","architecture":"amd64","os.features":["sse"]}}]"#,
        )?;
        let reference = OciReference::parse("ghcr.io/org/app:1")?;

        let Err(error) = select_platform(&reference, &entries, Arch::X86_64) else {
            panic!("expected a platform selection error");
        };

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(
            error.message().contains("linux/amd64"),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("linux/riscv64"),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn resolve_index_expected_child_manifest_digest_recorded() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let layer = b"layer".to_vec();
        let config = config_body();
        let config_digest = digest_of(config.as_bytes());
        let layer_digest = digest_of(&layer);
        let manifest = manifest_body(&config_digest, &layer_digest, layer.len() as u64);
        let manifest_digest = digest_of(manifest.as_bytes());
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{MEDIA_TYPE_OCI_INDEX}","manifests":[{{"mediaType":"{MEDIA_TYPE_OCI_MANIFEST}","digest":"{manifest_digest}","size":100,"platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#
        );
        let index_digest = digest_of(index.as_bytes());
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::json(&index).with_header("content-type", MEDIA_TYPE_OCI_INDEX),
        )?;
        http.push(
            &format!("https://ghcr.io/v2/org/app/manifests/{manifest_digest}"),
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        http.push(
            &format!("https://ghcr.io/v2/org/app/blobs/{config_digest}"),
            ScriptedReply::json(&config),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let resolved = client.resolve(&reference)?;

        assert_eq!(resolved.manifest_digest, manifest_digest);
        assert_eq!(
            resolved.index_digest.as_deref(),
            Some(index_digest.as_str())
        );
        Ok(())
    }

    #[test]
    fn resolve_manifest_digest_mismatch_expected_checksum_error() -> TestResult {
        let reference = OciReference::parse(
            "ghcr.io/org/app@sha256:1111111111111111111111111111111111111111111111111111111111111111",
        )?;
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/sha256:1111111111111111111111111111111111111111111111111111111111111111",
            ScriptedReply::json(r#"{"schemaVersion":2}"#),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a manifest checksum error");
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        Ok(())
    }

    #[test]
    fn resolve_content_digest_header_mismatch_expected_checksum_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::json(r#"{"schemaVersion":2}"#).with_header(
                "docker-content-digest",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            ),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a content digest error");
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        Ok(())
    }

    #[test]
    fn resolve_config_digest_mismatch_expected_checksum_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let layer = b"layer".to_vec();
        let config_digest = digest_of(b"other-config");
        let manifest = manifest_body(&config_digest, &digest_of(&layer), layer.len() as u64);
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        http.push(
            &format!("https://ghcr.io/v2/org/app/blobs/{config_digest}"),
            ScriptedReply::json(&config_body()),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a config checksum error");
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        Ok(())
    }

    #[test]
    fn resolve_zstd_layer_expected_dependency_error_naming_media_type() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let config = config_body();
        let config_digest = digest_of(config.as_bytes());
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{MEDIA_TYPE_OCI_MANIFEST}","config":{{"digest":"{config_digest}","size":10}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+zstd","digest":"{}","size":4}}]}}"#,
            digest_of(b"zstd")
        );
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected an unsupported layer error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error
                .message()
                .contains("application/vnd.oci.image.layer.v1.tar+zstd"),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn resolve_unsupported_media_type_expected_dependency_error_naming_it() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::json(r#"{"schemaVersion":1,"mediaType":"application/vnd.docker.distribution.manifest.v1+prettyjws"}"#),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected an unsupported media type error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("prettyjws"), "{}", error.message());
        Ok(())
    }

    #[test]
    fn resolve_manifest_over_limit_expected_dependency_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let oversized = vec![b'x'; usize::try_from(MAX_MANIFEST_BYTES)? + 1];
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::ok(oversized),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a manifest size error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error.message().contains(&MAX_MANIFEST_BYTES.to_string()),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn resolve_too_many_layers_expected_dependency_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let config_digest = digest_of(config_body().as_bytes());
        let layer = digest_of(b"layer");
        let entries: Vec<String> = (0..=MAX_LAYERS)
            .map(|_| {
                format!(
                    r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer}","size":5}}"#
                )
            })
            .collect();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{MEDIA_TYPE_OCI_MANIFEST}","config":{{"digest":"{config_digest}","size":10}},"layers":[{}]}}"#,
            entries.join(",")
        );
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a layer count error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error.message().contains(&MAX_LAYERS.to_string()),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn fetch_layer_valid_blob_expected_bytes_and_bounded_progress() -> TestResult {
        let layer = vec![b'z'; usize::try_from(PROGRESS_INTERVAL_BYTES)? + 17];
        let scripted = scripted_registry("ghcr.io/org/app:1", &layer)?;
        let (http, reference, layer_digest) =
            (scripted.http, scripted.reference, scripted.layer_digest);
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;
        let descriptor = LayerDescriptor {
            digest: layer_digest,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_owned(),
            size: layer.len() as u64,
        };
        let mut written = Vec::new();
        let mut updates = Vec::new();

        client.fetch_layer(&reference, &descriptor, &mut written, &mut |done, total| {
            updates.push((done, total));
            Ok(())
        })?;

        assert_eq!(written, layer);
        assert!(updates.len() <= 3, "{updates:?}");
        assert_eq!(
            updates.last().copied(),
            Some((layer.len() as u64, Some(layer.len() as u64)))
        );
        Ok(())
    }

    #[test]
    fn fetch_layer_digest_mismatch_expected_checksum_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let digest = digest_of(b"expected");
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            &format!("https://ghcr.io/v2/org/app/blobs/{digest}"),
            ScriptedReply::ok(b"tampered".to_vec()),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;
        let descriptor = LayerDescriptor {
            digest: digest.clone(),
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_owned(),
            size: 8,
        };
        let mut written = Vec::new();

        let Err(error) =
            client.fetch_layer(&reference, &descriptor, &mut written, &mut |_, _| Ok(()))
        else {
            panic!("expected a layer checksum error");
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        assert!(error.message().contains(&digest), "{}", error.message());
        Ok(())
    }

    #[test]
    fn fetch_layer_short_or_long_stream_expected_checksum_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let cases: [(&[u8], u64); 2] = [(b"short", 32), (b"way-too-long", 4)];
        for (body, declared) in cases {
            let digest = digest_of(body);
            let http = Arc::new(ScriptedHttp::default());
            http.push(
                &format!("https://ghcr.io/v2/org/app/blobs/{digest}"),
                ScriptedReply::ok(body.to_vec()),
            )?;
            let client = client(http, &RegistryOptions::new(Arch::X86_64))?;
            let descriptor = LayerDescriptor {
                digest,
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_owned(),
                size: declared,
            };
            let mut written = Vec::new();

            let Err(error) =
                client.fetch_layer(&reference, &descriptor, &mut written, &mut |_, _| Ok(()))
            else {
                panic!("expected a layer size error");
            };

            assert_eq!(error.kind(), ErrorKind::Checksum);
        }
        Ok(())
    }

    #[test]
    fn fetch_layer_content_length_disagrees_expected_checksum_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let body = b"body".to_vec();
        let digest = digest_of(&body);
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            &format!("https://ghcr.io/v2/org/app/blobs/{digest}"),
            ScriptedReply::ok(body.clone()).with_content_length(99),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;
        let descriptor = LayerDescriptor {
            digest,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_owned(),
            size: body.len() as u64,
        };
        let mut written = Vec::new();

        let Err(error) =
            client.fetch_layer(&reference, &descriptor, &mut written, &mut |_, _| Ok(()))
        else {
            panic!("expected a declared length error");
        };

        assert_eq!(error.kind(), ErrorKind::Checksum);
        Ok(())
    }

    #[test]
    fn insecure_registry_listed_expected_plain_http_only_for_that_entry() -> TestResult {
        let options = RegistryOptions::new(Arch::X86_64)
            .with_insecure_registries(vec!["localhost:5000".to_owned()]);
        let http = Arc::new(ScriptedHttp::default());
        let client = client(http, &options)?;

        assert_eq!(client.scheme_for("localhost:5000"), "http");
        assert_eq!(client.scheme_for("localhost:5001"), "https");
        assert_eq!(client.scheme_for("ghcr.io"), "https");
        assert_eq!(
            client
                .endpoint_url("localhost:5000", "/v2/app/manifests/1")?
                .as_str(),
            "http://localhost:5000/v2/app/manifests/1"
        );
        assert_eq!(
            client
                .endpoint_url("ghcr.io", "/v2/org/app/manifests/1")?
                .as_str(),
            "https://ghcr.io/v2/org/app/manifests/1"
        );
        Ok(())
    }

    #[test]
    fn insecure_registry_docker_hub_or_malformed_expected_invalid_spec() {
        let cases = [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "https://host",
        ];
        for entry in cases {
            let options =
                RegistryOptions::new(Arch::X86_64).with_insecure_registries(vec![entry.to_owned()]);
            let Err(error) = RegistryClient::with_http(Arc::new(ScriptedHttp::default()), &options)
            else {
                panic!("expected '{entry}' to be rejected");
            };

            assert_eq!(error.kind(), ErrorKind::InvalidSpec, "{entry}");
            assert!(
                error.message().contains("images.insecure_registries"),
                "{}",
                error.message()
            );
        }
    }

    #[test]
    fn insecure_registry_plain_http_expected_no_credentials_sent() -> TestResult {
        let directory = TempDir::new()?;
        let config = directory.path().join("config.json");
        fs::write(
            &config,
            format!(
                r#"{{"auths":{{"localhost:5000":{{"auth":"{}"}}}}}}"#,
                base64_encode(b"alice:pw")
            ),
        )?;
        let layer = b"layer".to_vec();
        let config_body = config_body();
        let config_digest = digest_of(config_body.as_bytes());
        let layer_digest = digest_of(&layer);
        let manifest = manifest_body(&config_digest, &layer_digest, layer.len() as u64);
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "http://localhost:5000/v2/app/manifests/1",
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        http.push(
            &format!("http://localhost:5000/v2/app/blobs/{config_digest}"),
            ScriptedReply::json(&config_body),
        )?;
        let options = RegistryOptions::new(Arch::X86_64)
            .with_insecure_registries(vec!["localhost:5000".to_owned()])
            .with_docker_config(Some(config));
        let client = client(http.clone(), &options)?;

        client.resolve(&OciReference::parse("localhost:5000/app:1")?)?;

        let requests = http.requests()?;
        assert!(
            requests
                .iter()
                .all(|request| request.header("authorization").is_none()),
            "{requests:?}"
        );
        Ok(())
    }

    #[test]
    fn resolve_token_response_over_limit_expected_dependency_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::status(401).with_header(
                "www-authenticate",
                r#"Bearer realm="https://ghcr.io/token""#,
            ),
        )?;
        http.push(
            "https://ghcr.io/token?scope=repository%3Aorg%2Fapp%3Apull",
            ScriptedReply::ok(vec![b'x'; usize::try_from(MAX_TOKEN_BYTES)? + 1]),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a token size error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error.message().contains(&MAX_TOKEN_BYTES.to_string()),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn resolve_plain_http_token_realm_expected_dependency_error() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::status(401)
                .with_header("www-authenticate", r#"Bearer realm="http://ghcr.io/token""#),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a token realm error");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error.message().contains("plain-HTTP"),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn resolve_plain_http_token_realm_on_listed_registry_expected_accepted() -> TestResult {
        let reference = OciReference::parse("localhost:5000/app:1")?;
        let config = config_body();
        let config_digest = digest_of(config.as_bytes());
        let layer = b"layer".to_vec();
        let manifest = manifest_body(&config_digest, &digest_of(&layer), layer.len() as u64);
        let manifest_url = "http://localhost:5000/v2/app/manifests/1";
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            manifest_url,
            ScriptedReply::status(401).with_header(
                "www-authenticate",
                r#"Bearer realm="http://localhost:5000/token",service="local""#,
            ),
        )?;
        http.push(
            "http://localhost:5000/token?service=local&scope=repository%3Aapp%3Apull",
            ScriptedReply::json(r#"{"token":"local-token"}"#),
        )?;
        http.push(
            manifest_url,
            ScriptedReply::json(&manifest).with_header("content-type", MEDIA_TYPE_OCI_MANIFEST),
        )?;
        http.push(
            &format!("http://localhost:5000/v2/app/blobs/{config_digest}"),
            ScriptedReply::json(&config),
        )?;
        let options = RegistryOptions::new(Arch::X86_64)
            .with_insecure_registries(vec!["localhost:5000".to_owned()]);
        let client = client(http.clone(), &options)?;

        client.resolve(&reference)?;

        let requests = http.requests()?;
        assert_eq!(
            requests[2].header("authorization"),
            Some("Bearer local-token"),
            "{requests:?}"
        );
        Ok(())
    }

    #[test]
    fn resolve_missing_repository_expected_not_found() -> TestResult {
        let reference = OciReference::parse("ghcr.io/org/app:1")?;
        let http = Arc::new(ScriptedHttp::default());
        http.push(
            "https://ghcr.io/v2/org/app/manifests/1",
            ScriptedReply::status(404),
        )?;
        let client = client(http, &RegistryOptions::new(Arch::X86_64))?;

        let Err(error) = client.resolve(&reference) else {
            panic!("expected a not found error");
        };

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.message().contains("org/app"), "{}", error.message());
        Ok(())
    }

    #[test]
    fn base64_round_trip_expected_matching_bytes() -> TestResult {
        let cases: [&[u8]; 4] = [b"", b"a", b"ab", b"user:password"];
        for case in cases {
            let encoded = base64_encode(case);
            let decoded =
                base64_decode(&encoded).ok_or_else(|| io::Error::other("decode failed"))?;
            assert_eq!(decoded, case, "{encoded}");
        }
        assert_eq!(base64_encode(b"user:password"), "dXNlcjpwYXNzd29yZA==");
        assert!(base64_decode("not base64!").is_none());
        Ok(())
    }

    #[test]
    fn validate_digest_bad_forms_expected_checksum_error() {
        let cases = [
            "sha512:1111111111111111111111111111111111111111111111111111111111111111",
            "sha256:ZZ11111111111111111111111111111111111111111111111111111111111111",
            "sha256:11",
            "1111",
        ];
        for case in cases {
            let Err(error) = validate_digest(case, "layer") else {
                panic!("expected '{case}' to be rejected");
            };
            assert_eq!(error.kind(), ErrorKind::Checksum, "{case}");
        }
        assert!(
            validate_digest(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "layer"
            )
            .is_ok()
        );
    }
}
