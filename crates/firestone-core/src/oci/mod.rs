//! OCI image reference classification, parsing, and normalization.
//!
//! Firestone accepts container images alongside catalog references, HTTPS URLs,
//! and local files. This module owns the syntax half of that surface: deciding
//! whether a user-supplied string is an OCI reference at all, and turning the
//! ones that are into a canonical `registry/repository:tag` or
//! `registry/repository@sha256:…` value. Nothing here performs I/O.

use std::fmt;

use crate::error::{ErrorKind, FirestoneError};

/// Explicit scheme prefix that always selects the OCI branch.
pub const OCI_SCHEME_PREFIX: &str = "oci://";
/// Docker-flavored alias of [`OCI_SCHEME_PREFIX`].
pub const DOCKER_SCHEME_PREFIX: &str = "docker://";
/// Registry assumed when a reference carries no registry host.
pub const DEFAULT_REGISTRY: &str = "docker.io";
/// Legacy spelling of [`DEFAULT_REGISTRY`] that normalizes to it.
pub const DEFAULT_REGISTRY_ALIAS: &str = "index.docker.io";
/// Namespace prefixed to single-component Docker Hub repositories.
pub const DEFAULT_NAMESPACE: &str = "library";
/// Tag assumed when a reference carries neither a tag nor a digest.
pub const DEFAULT_TAG: &str = "latest";
/// Digest algorithm Firestone accepts in a reference.
pub const DIGEST_ALGORITHM: &str = "sha256";

const MAX_TAG_BYTES: usize = 128;
const MAX_REPOSITORY_BYTES: usize = 255;
const MAX_HOST_LABEL_BYTES: usize = 63;
const DIGEST_HEX_LENGTH: usize = 64;

/// Why [`classify`] selected the OCI branch for a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OciClassification {
    /// The reference began with `oci://` or `docker://`.
    Scheme,
    /// The first `/`-separated component looked like a registry host.
    RegistryHost,
}

impl OciClassification {
    /// Reports whether the classification came from an explicit scheme prefix.
    #[must_use]
    pub const fn is_explicit(self) -> bool {
        matches!(self, Self::Scheme)
    }
}

/// The tag or digest half of an [`OciReference`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OciTagOrDigest {
    /// A mutable tag such as `latest`.
    Tag(String),
    /// An immutable `sha256:…` digest.
    Digest(String),
}

impl OciTagOrDigest {
    /// Returns the tag when this reference is tag-addressed.
    #[must_use]
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Tag(tag) => Some(tag),
            Self::Digest(_) => None,
        }
    }

    /// Returns the `sha256:…` digest when this reference is digest-addressed.
    #[must_use]
    pub fn digest(&self) -> Option<&str> {
        match self {
            Self::Tag(_) => None,
            Self::Digest(digest) => Some(digest),
        }
    }
}

impl fmt::Display for OciTagOrDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag(tag) => write!(formatter, ":{tag}"),
            Self::Digest(digest) => write!(formatter, "@{digest}"),
        }
    }
}

/// A normalized OCI image reference.
///
/// Construct one with [`OciReference::parse`]; the fields are always canonical,
/// so [`fmt::Display`] round-trips back through `parse` unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OciReference {
    registry: String,
    repository: String,
    reference: OciTagOrDigest,
}

impl OciReference {
    /// Parses and normalizes one reference, with or without a scheme prefix.
    pub fn parse(value: &str) -> Result<Self, FirestoneError> {
        let body = strip_scheme(value).unwrap_or(value);
        if body.is_empty() {
            return Err(invalid(
                value,
                "the reference is empty",
                "write a reference such as `docker://nginx:latest`",
            ));
        }

        let (name, reference) = split_reference(value, body)?;
        if name.is_empty() {
            return Err(invalid(
                value,
                "the repository name is empty",
                "write a reference such as `docker://nginx:latest`",
            ));
        }

        let (registry, repository) = match name.split_once('/') {
            Some((first, rest)) if looks_like_registry_host(first) => {
                (normalize_registry(first), rest.to_owned())
            }
            _ => (DEFAULT_REGISTRY.to_owned(), name.to_owned()),
        };

        validate_registry_host_for(value, &registry)?;

        let repository = if registry == DEFAULT_REGISTRY && !repository.contains('/') {
            format!("{DEFAULT_NAMESPACE}/{repository}")
        } else {
            repository
        };
        validate_repository(value, &repository)?;

        Ok(Self {
            registry,
            repository,
            reference,
        })
    }

    /// The normalized registry host, `host` or `host:port`.
    #[must_use]
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The normalized repository path, always at least one component.
    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// The tag or digest this reference addresses.
    #[must_use]
    pub fn reference(&self) -> &OciTagOrDigest {
        &self.reference
    }

    /// Reports whether this reference pins an immutable digest.
    #[must_use]
    pub fn is_digest(&self) -> bool {
        matches!(self.reference, OciTagOrDigest::Digest(_))
    }
}

impl fmt::Display for OciReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}{}",
            self.registry, self.repository, self.reference
        )
    }
}

/// Decides whether a reference belongs to the OCI branch of image resolution.
///
/// A reference is OCI when it starts with `oci://` or `docker://`, or when it
/// contains `/` and its first `/`-separated component contains `.` or `:` or
/// equals `localhost`. The rule is stated without exceptions, so a `scheme://…`
/// URL is classified too; it never survives [`OciReference::parse`], because
/// the trailing colon of `https:` leaves the registry host without a port.
/// Callers still run local-file resolution first, and treat a classified
/// reference that fails to parse as a decline unless the classification was
/// [`OciClassification::Scheme`].
#[must_use]
pub fn classify(value: &str) -> Option<OciClassification> {
    if strip_scheme(value).is_some() {
        return Some(OciClassification::Scheme);
    }
    let (first, _) = value.split_once('/')?;
    if looks_like_registry_host(first) {
        Some(OciClassification::RegistryHost)
    } else {
        None
    }
}

/// Strips an `oci://` or `docker://` prefix, returning the remaining body.
#[must_use]
pub fn strip_scheme(value: &str) -> Option<&str> {
    value
        .strip_prefix(OCI_SCHEME_PREFIX)
        .or_else(|| value.strip_prefix(DOCKER_SCHEME_PREFIX))
}

/// Parses a reference and returns its canonical text.
pub fn normalize(value: &str) -> Result<String, FirestoneError> {
    Ok(OciReference::parse(value)?.to_string())
}

/// Validates one `host` or `host:port` registry entry with no scheme or path.
///
/// Used for `[images].insecure_registries` in the global configuration.
pub fn validate_registry_host(value: &str) -> Result<(), FirestoneError> {
    if let Err(detail) = check_registry_host(value) {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("invalid registry host '{value}': {detail}"),
        )
        .with_hint("write a bare 'host' or 'host:port' entry such as 'localhost:5000'"));
    }
    Ok(())
}

fn split_reference<'a>(
    value: &str,
    body: &'a str,
) -> Result<(&'a str, OciTagOrDigest), FirestoneError> {
    if let Some((name, digest)) = body.split_once('@') {
        return Ok((
            name,
            OciTagOrDigest::Digest(validate_digest(value, digest)?),
        ));
    }
    let last_component = body.rfind('/').map_or(0, |index| index + 1);
    if let Some(offset) = body[last_component..].rfind(':') {
        let split = last_component + offset;
        let (name, tag) = body.split_at(split);
        return Ok((name, OciTagOrDigest::Tag(validate_tag(value, &tag[1..])?)));
    }
    Ok((body, OciTagOrDigest::Tag(DEFAULT_TAG.to_owned())))
}

fn looks_like_registry_host(component: &str) -> bool {
    component.contains('.') || component.contains(':') || component == "localhost"
}

fn normalize_registry(component: &str) -> String {
    let lowered = component.to_ascii_lowercase();
    if lowered == DEFAULT_REGISTRY_ALIAS {
        DEFAULT_REGISTRY.to_owned()
    } else {
        lowered
    }
}

fn validate_registry_host_for(value: &str, registry: &str) -> Result<(), FirestoneError> {
    check_registry_host(registry).map_err(|detail| {
        invalid(
            value,
            format!("registry host '{registry}' is invalid: {detail}"),
            "write a bare 'host' or 'host:port' registry such as 'localhost:5000'",
        )
    })
}

fn check_registry_host(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("the host is empty".to_owned());
    }
    if value.contains("://") {
        return Err("a scheme is not allowed".to_owned());
    }
    if value.contains('/') {
        return Err("a path is not allowed".to_owned());
    }
    if value.contains('@') {
        return Err("credentials are not allowed".to_owned());
    }
    let (host, port) = match value.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (value, None),
    };
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("the port must be decimal digits".to_owned());
        }
        match port.parse::<u32>() {
            Ok(number) if (1..=65535).contains(&number) => {}
            _ => return Err("the port must be between 1 and 65535".to_owned()),
        }
    }
    if host.is_empty() {
        return Err("the host is empty".to_owned());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err("host labels must not be empty".to_owned());
        }
        if label.len() > MAX_HOST_LABEL_BYTES {
            return Err(format!(
                "host labels must be at most {MAX_HOST_LABEL_BYTES} characters"
            ));
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("host labels accept only ASCII letters, digits, and '-'".to_owned());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("host labels must not start or end with '-'".to_owned());
        }
    }
    Ok(())
}

fn validate_repository(value: &str, repository: &str) -> Result<(), FirestoneError> {
    if repository.len() > MAX_REPOSITORY_BYTES {
        return Err(invalid(
            value,
            format!("the repository exceeds {MAX_REPOSITORY_BYTES} characters"),
            "shorten the repository path",
        ));
    }
    for component in repository.split('/') {
        if let Err(detail) = check_repository_component(component) {
            return Err(invalid(
                value,
                format!("repository component '{component}' is invalid: {detail}"),
                "use lowercase repository components such as 'library/nginx'",
            ));
        }
    }
    Ok(())
}

fn check_repository_component(component: &str) -> Result<(), String> {
    if component.is_empty() {
        return Err("the component is empty".to_owned());
    }
    let bytes = component.as_bytes();
    if !bytes.iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return Err("only lowercase letters, digits, '.', '_', and '-' are accepted".to_owned());
    }
    let mut index = 0;
    let mut expect_alphanumeric = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            expect_alphanumeric = false;
            index += 1;
            continue;
        }
        if expect_alphanumeric {
            return Err("components must start and end with a letter or digit".to_owned());
        }
        let run_start = index;
        while index < bytes.len() && bytes[index] == byte {
            index += 1;
        }
        let run = index - run_start;
        let allowed = match byte {
            b'.' => run == 1,
            b'_' => run <= 2,
            b'-' => true,
            _ => false,
        };
        if !allowed {
            return Err("separator runs must be '.', '_', '__', or '-'".to_owned());
        }
        if index == bytes.len() {
            return Err("components must start and end with a letter or digit".to_owned());
        }
        expect_alphanumeric = true;
    }
    Ok(())
}

fn validate_tag(value: &str, tag: &str) -> Result<String, FirestoneError> {
    if tag.is_empty() {
        return Err(invalid(
            value,
            "the tag is empty",
            "write a tag such as ':latest' or omit it",
        ));
    }
    if tag.len() > MAX_TAG_BYTES {
        return Err(invalid(
            value,
            format!("the tag exceeds {MAX_TAG_BYTES} characters"),
            "shorten the tag",
        ));
    }
    let mut bytes = tag.bytes();
    let first = bytes.next().unwrap_or(b'.');
    if !(first.is_ascii_alphanumeric() || first == b'_') {
        return Err(invalid(
            value,
            "the tag must start with a letter, digit, or '_'",
            "write a tag such as ':24.04'",
        ));
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        return Err(invalid(
            value,
            "the tag accepts only letters, digits, '.', '_', and '-'",
            "write a tag such as ':24.04'",
        ));
    }
    Ok(tag.to_owned())
}

fn validate_digest(value: &str, digest: &str) -> Result<String, FirestoneError> {
    let Some(hex) = digest
        .strip_prefix(DIGEST_ALGORITHM)
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return Err(invalid(
            value,
            format!("the digest must use the '{DIGEST_ALGORITHM}:' algorithm prefix"),
            "pin the image with 'repository@sha256:<64 hex characters>'",
        ));
    };
    if hex.len() != DIGEST_HEX_LENGTH
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid(
            value,
            format!("the digest must carry {DIGEST_HEX_LENGTH} lowercase hexadecimal characters"),
            "pin the image with 'repository@sha256:<64 hex characters>'",
        ));
    }
    Ok(format!("{DIGEST_ALGORITHM}:{hex}"))
}

fn invalid(value: &str, detail: impl fmt::Display, hint: impl Into<String>) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::InvalidSpec,
        format!("invalid OCI image reference '{value}': {detail}"),
    )
    .with_hint(hint)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn classify_every_documented_reference_shape_matches_the_rule() {
        // (input, expected classification)
        let cases: &[(&str, Option<OciClassification>)] = &[
            // Explicit schemes always win.
            ("oci://nginx", Some(OciClassification::Scheme)),
            ("docker://nginx", Some(OciClassification::Scheme)),
            ("docker://nginx:1.27", Some(OciClassification::Scheme)),
            (
                "oci://ghcr.io/owner/app:v1",
                Some(OciClassification::Scheme),
            ),
            ("docker://", Some(OciClassification::Scheme)),
            ("oci://", Some(OciClassification::Scheme)),
            // The scheme prefix is case-sensitive and exact; a near miss falls
            // to the heuristic, which sees a colon in the first component.
            ("DOCKER://nginx", Some(OciClassification::RegistryHost)),
            ("oci:/nginx", Some(OciClassification::RegistryHost)),
            ("docker:/nginx", Some(OciClassification::RegistryHost)),
            ("oci:nginx", None),
            // Registry-host heuristic on the first component.
            ("ghcr.io/owner/app", Some(OciClassification::RegistryHost)),
            (
                "quay.io/fedora/fedora",
                Some(OciClassification::RegistryHost),
            ),
            ("localhost/app", Some(OciClassification::RegistryHost)),
            ("localhost:5000/app", Some(OciClassification::RegistryHost)),
            ("registry:5000/app", Some(OciClassification::RegistryHost)),
            (
                "docker.io/library/nginx",
                Some(OciClassification::RegistryHost),
            ),
            (
                "example.com/a/b/c:tag",
                Some(OciClassification::RegistryHost),
            ),
            // No slash: never OCI.
            ("nginx", None),
            ("ubuntu", None),
            ("ubuntu:24.04", None),
            ("debian:bookworm", None),
            ("fedora", None),
            ("ghcr.io", None),
            ("localhost", None),
            ("localhost:5000", None),
            // Slash present but the first component is not host-like.
            ("owner/app", None),
            ("library/nginx", None),
            ("myorg/myimage:1.0", None),
            ("images/base", None),
            ("~/images/base.qcow2", None),
            ("/absolute/path.qcow2", None),
            ("/", None),
            // Relative paths whose first component contains a dot are
            // classified, then decline to parse and fall through.
            ("./file.qcow2", Some(OciClassification::RegistryHost)),
            ("../file.qcow2", Some(OciClassification::RegistryHost)),
            ("dir.d/file.qcow2", Some(OciClassification::RegistryHost)),
            // HTTPS never reaches the classifier in resolution order, but the
            // rule is stated without exceptions.
            (
                "https://example.com/a.img",
                Some(OciClassification::RegistryHost),
            ),
            ("", None),
        ];

        for (input, expected) in cases {
            assert_eq!(classify(input), *expected, "classifying {input:?}");
        }
    }

    #[test]
    fn parse_docker_hub_short_name_expected_library_prefix_and_latest_tag() {
        let reference = OciReference::parse("docker://nginx").expect("parse");

        assert_eq!(reference.registry(), "docker.io");
        assert_eq!(reference.repository(), "library/nginx");
        assert_eq!(reference.reference().tag(), Some("latest"));
        assert!(!reference.is_digest());
        assert_eq!(reference.to_string(), "docker.io/library/nginx:latest");
    }

    #[test]
    fn parse_normalization_cases_expected_canonical_text() {
        let cases: &[(&str, &str)] = &[
            ("docker://nginx", "docker.io/library/nginx:latest"),
            ("oci://nginx", "docker.io/library/nginx:latest"),
            ("docker://nginx:1.27", "docker.io/library/nginx:1.27"),
            ("docker://library/nginx", "docker.io/library/nginx:latest"),
            ("docker://owner/app", "docker.io/owner/app:latest"),
            ("docker.io/nginx", "docker.io/library/nginx:latest"),
            ("index.docker.io/nginx", "docker.io/library/nginx:latest"),
            ("docker://Docker.IO/nginx", "docker.io/library/nginx:latest"),
            ("ghcr.io/owner/app", "ghcr.io/owner/app:latest"),
            ("ghcr.io/owner/app:v1.2.3", "ghcr.io/owner/app:v1.2.3"),
            ("localhost/app", "localhost/app:latest"),
            ("localhost:5000/app:dev", "localhost:5000/app:dev"),
            (
                "example.com:5000/nested/team/app:edge",
                "example.com:5000/nested/team/app:edge",
            ),
            (
                "docker://nginx@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "docker.io/library/nginx@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ),
            (
                "ghcr.io/owner/app@sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                "ghcr.io/owner/app@sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            ("docker://my__app", "docker.io/library/my__app:latest"),
            ("docker://my-app.v2", "docker.io/library/my-app.v2:latest"),
        ];

        for (input, expected) in cases {
            let parsed = OciReference::parse(input)
                .unwrap_or_else(|error| panic!("parsing {input:?}: {}", error.message()));
            assert_eq!(parsed.to_string(), *expected, "normalizing {input:?}");
            assert_eq!(normalize(input).expect("normalize"), *expected);
        }
    }

    #[test]
    fn parse_canonical_text_expected_idempotent_round_trip() {
        for input in [
            "docker://nginx",
            "ghcr.io/owner/app:v1",
            "localhost:5000/app:dev",
            "docker://nginx@sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            let once = normalize(input).expect("normalize once");
            let twice = normalize(&once).expect("normalize twice");
            assert_eq!(once, twice, "round-tripping {input:?}");
        }
    }

    #[test]
    fn parse_digest_reference_expected_digest_target() {
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let reference =
            OciReference::parse(&format!("ghcr.io/owner/app@{digest}")).expect("parse digest");

        assert!(reference.is_digest());
        assert_eq!(reference.reference().digest(), Some(digest));
        assert_eq!(reference.reference().tag(), None);
    }

    #[test]
    fn parse_malformed_reference_expected_invalid_spec_with_hint() {
        let cases: &[&str] = &[
            "docker://",
            "oci://",
            "docker://nginx:",
            "docker://nginx:-bad",
            "docker://nginx@sha256:short",
            "docker://nginx@sha512:0000000000000000000000000000000000000000000000000000000000000000",
            "docker://nginx@sha256:GGGG000000000000000000000000000000000000000000000000000000000000",
            "docker://NGINX",
            "docker://-nginx",
            "docker://nginx-",
            "docker://ng..inx",
            "docker://ng___inx",
            "docker://owner//app",
            "example..com/app",
            "example.com:/app",
            "example.com:abc/app",
            "-example.com/app",
            "./file.qcow2",
            "../file.qcow2",
        ];

        for input in cases {
            let error = OciReference::parse(input)
                .expect_err(&format!("expected {input:?} to be rejected"));
            assert_eq!(error.kind(), ErrorKind::InvalidSpec, "kind for {input:?}");
            assert!(
                error.message().contains("invalid OCI image reference"),
                "message for {input:?}: {}",
                error.message()
            );
            assert!(error.hint().is_some(), "hint for {input:?}");
        }
    }

    #[test]
    fn validate_registry_host_accepted_and_rejected_entries_expected_split() {
        for accepted in [
            "docker.io",
            "ghcr.io",
            "localhost",
            "localhost:5000",
            "registry.internal.example.com:443",
            "my-registry.example.com",
        ] {
            validate_registry_host(accepted)
                .unwrap_or_else(|error| panic!("{accepted}: {}", error.message()));
        }

        for rejected in [
            "",
            "https://ghcr.io",
            "ghcr.io/path",
            "user@ghcr.io",
            "ghcr.io:",
            "ghcr.io:0",
            "ghcr.io:70000",
            "ghcr.io:port",
            "-ghcr.io",
            "ghcr-.io",
            "ghcr..io",
        ] {
            let error = validate_registry_host(rejected)
                .expect_err(&format!("expected {rejected:?} to be rejected"));
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert!(error.hint().is_some());
        }
    }
}
