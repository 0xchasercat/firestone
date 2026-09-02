//! Canonical merge of OCI image layers into one deterministic tar stream.
//!
//! The merge is pure logic: it never touches the network and never extracts
//! anything onto the host. Layers are read twice.
//!
//! * Pass one walks every layer in order and builds a path map of the surviving
//!   entries while applying the overlay whiteout rules.
//! * Pass two re-reads only the members that survived and streams them out
//!   sorted by path, which places every parent directory before its children
//!   because a parent path is a byte-wise prefix of its children.
//!
//! Members that need no repair are copied out block for block, so mode, owner,
//! mtime, symlink targets, PAX extended headers (including `SCHILY.xattr.*`
//! records), GNU long names, and character or block device entries survive
//! byte-exactly. Device entries are kept deliberately: the merged tar is never
//! extracted on the host, it is handed to the static `mkfs.ext4` helper as
//! input.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;

use flate2::read::GzDecoder;
use serde::{Deserialize, Deserializer, Serialize};
use tar::{EntryType, Header};

use crate::error::{ErrorKind, FirestoneError};

/// Docker's gzip-compressed rootfs diff layer.
pub const MEDIA_TYPE_DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
/// The OCI equivalent of [`MEDIA_TYPE_DOCKER_LAYER_GZIP`].
pub const MEDIA_TYPE_OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
/// The zstd-compressed OCI layer, which this version does not support.
pub const MEDIA_TYPE_OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// Default cap on the total uncompressed bytes read from all layers.
pub const DEFAULT_MAX_UNCOMPRESSED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Default cap on the number of tar members read from all layers.
pub const DEFAULT_MAX_ENTRIES: u64 = 1_000_000;

/// Path of the injected init binary inside the merged tar.
pub const FIRESTONE_INIT_PATH: &str = "sbin/firestone-init";
/// Path of the injected marker file inside the merged tar.
pub const FIRESTONE_OCI_MARKER_PATH: &str = "etc/firestone-oci";
/// Bytes the SPEC §8.5 sizing rule charges for one directory, symlink, or hard
/// link in the merged tree.
pub const METADATA_ENTRY_BYTES: u64 = 4096;

/// Whiteout prefix defined by the OCI image layer specification.
const WHITEOUT_PREFIX: &[u8] = b".wh.";
/// Opaque-directory marker defined by the OCI image layer specification.
const OPAQUE_MARKER: &[u8] = b".wh..wh..opq";

/// Size of one tar block.
const BLOCK: usize = 512;
/// Largest PAX or GNU extension payload accepted for a single member.
const MAX_EXTENSION_BYTES: u64 = 1024 * 1024;
/// Largest link target that still fits the ustar `linkname` field.
const USTAR_LINK_NAME_LIMIT: usize = 100;
/// Hint repeated by every error that means the layer bytes are not usable.
const REPULL_HINT: &str = "re-pull the image; the layer tar is malformed";

/// Compression accepted for an OCI layer blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCompression {
    /// A gzip-compressed tar stream.
    Gzip,
}

/// Classifies a layer descriptor media type.
///
/// # Errors
///
/// Returns a `dependency` error for zstd and for every media type this version
/// cannot read.
pub fn classify_layer_media_type(media_type: &str) -> Result<LayerCompression, FirestoneError> {
    match media_type {
        MEDIA_TYPE_DOCKER_LAYER_GZIP | MEDIA_TYPE_OCI_LAYER_GZIP => Ok(LayerCompression::Gzip),
        MEDIA_TYPE_OCI_LAYER_ZSTD => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("unsupported OCI layer media type {media_type}"),
        )
        .with_hint("firestone reads gzip layers only; pull a gzip-compressed copy of the image")),
        other => {
            let hint = format!(
                "supported layer media types are {MEDIA_TYPE_DOCKER_LAYER_GZIP} and {MEDIA_TYPE_OCI_LAYER_GZIP}"
            );
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("unsupported OCI layer media type {other}"),
            )
            .with_hint(hint))
        }
    }
}

/// The runtime fields Firestone keeps from an OCI image configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct OciImageConfig {
    /// `Env` entries in `KEY=VALUE` form.
    #[serde(default, deserialize_with = "nullable_string_list")]
    pub env: Vec<String>,
    /// `Entrypoint` argument vector.
    #[serde(default, deserialize_with = "nullable_string_list")]
    pub entrypoint: Vec<String>,
    /// `Cmd` argument vector.
    #[serde(default, deserialize_with = "nullable_string_list")]
    pub cmd: Vec<String>,
    /// `WorkingDir`, when the image declares one.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// `User`, when the image declares one.
    #[serde(default)]
    pub user: Option<String>,
}

/// Accepts an explicit JSON `null` for a string list, which OCI configs emit.
fn nullable_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Hardening caps applied while reading layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeLimits {
    /// Maximum uncompressed bytes read across all layers.
    pub max_uncompressed_bytes: u64,
    /// Maximum tar members read across all layers.
    pub max_entries: u64,
}

impl Default for MergeLimits {
    fn default() -> Self {
        Self {
            max_uncompressed_bytes: DEFAULT_MAX_UNCOMPRESSED_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

/// A layer blob that can be opened more than once.
///
/// The merge reads every layer twice, so a source hands out a fresh reader over
/// the same compressed bytes on each call.
pub trait LayerSource {
    /// Opens the compressed layer bytes.
    ///
    /// # Errors
    ///
    /// Returns a `dependency` error when the blob cannot be opened.
    fn open(&self) -> Result<Box<dyn Read + '_>, FirestoneError>;

    /// Short identifier used in error context.
    fn label(&self) -> String;
}

/// A layer blob stored as a file.
#[derive(Debug, Clone)]
pub struct FileLayer {
    path: PathBuf,
}

impl FileLayer {
    /// Wraps an already resolved blob path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl LayerSource for FileLayer {
    fn open(&self) -> Result<Box<dyn Read + '_>, FirestoneError> {
        let file = std::fs::File::open(&self.path).map_err(|error| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("cannot open layer blob {}", self.path.display()),
            )
            .with_hint("re-pull the image so the layer blob is written again")
            .with_source(error)
        })?;
        Ok(Box::new(file))
    }

    fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// A layer blob held in memory.
#[derive(Debug, Clone)]
pub struct BytesLayer {
    label: String,
    bytes: Vec<u8>,
}

impl BytesLayer {
    /// Wraps compressed layer bytes under a caller-chosen label.
    #[must_use]
    pub fn new(label: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            label: label.into(),
            bytes,
        }
    }
}

impl LayerSource for BytesLayer {
    fn open(&self) -> Result<Box<dyn Read + '_>, FirestoneError> {
        Ok(Box::new(io::Cursor::new(self.bytes.as_slice())))
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

/// Everything the merge needs about one image.
pub struct MergeRequest<'a> {
    /// Layers ordered from the lowest to the highest.
    pub layers: &'a [&'a dyn LayerSource],
    /// The parsed image configuration, carried through to the summary.
    pub config: &'a OciImageConfig,
    /// Hardening caps.
    pub limits: MergeLimits,
    /// When set, the `firestone-init` bytes injected into the merged tar.
    pub injected_init: Option<&'a [u8]>,
}

impl<'a> MergeRequest<'a> {
    /// Builds a request with the default limits and no injection.
    #[must_use]
    pub fn new(layers: &'a [&'a dyn LayerSource], config: &'a OciImageConfig) -> Self {
        Self {
            layers,
            config,
            limits: MergeLimits::default(),
            injected_init: None,
        }
    }

    /// Replaces the hardening caps.
    #[must_use]
    pub fn with_limits(mut self, limits: MergeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Injects `/sbin/firestone-init` and the `/etc/firestone-oci` marker in
    /// canonical position instead of appending them after the merge.
    #[must_use]
    pub fn with_injected_init(mut self, init_binary: &'a [u8]) -> Self {
        self.injected_init = Some(init_binary);
        self
    }
}

/// What the merge produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSummary {
    /// Members written to the output stream.
    pub entries_written: u64,
    /// Uncompressed bytes read while planning the merge.
    pub uncompressed_bytes_read: u64,
    /// The merged tree's size for the SPEC §8.5 ext4 sizing rule: every
    /// emitted regular file's size plus [`METADATA_ENTRY_BYTES`] for each
    /// emitted directory, symlink, and hard link.
    pub unpacked_bytes: u64,
    /// The image configuration the caller supplied, carried through so one
    /// value describes the merged rootfs.
    pub config: OciImageConfig,
}

/// Merges the layers of one image into a single canonical tar stream.
///
/// # Errors
///
/// Returns a `dependency` error when a layer is unreadable or malformed, when
/// an entry path is absolute, contains `..`, or would escape the root, or when
/// a hardening cap is exceeded.
pub fn merge_layers<W: Write>(
    request: &MergeRequest<'_>,
    output: W,
) -> Result<MergeSummary, FirestoneError> {
    let plan = build_plan(request)?;
    let mut writer = BufWriter::new(output);
    let (entries_written, unpacked_bytes) = write_merged(request, &plan, &mut writer)?;
    writer.write_all(&[0u8; BLOCK * 2]).map_err(write_error)?;
    writer.flush().map_err(write_error)?;

    Ok(MergeSummary {
        entries_written,
        uncompressed_bytes_read: plan.bytes_read,
        unpacked_bytes,
        config: request.config.clone(),
    })
}

/// Appends the injected `/sbin/firestone-init` and `/etc/firestone-oci` members
/// to a tar stream that has not been finished yet.
///
/// [`merge_layers`] places the same two members in canonical sorted position
/// when [`MergeRequest::with_injected_init`] is used; this entry point exists
/// for a caller that already holds an unfinished merged stream.
///
/// # Errors
///
/// Returns a `dependency` error when the underlying writer fails.
pub fn append_firestone_init<W: Write>(
    output: &mut W,
    init_binary: &[u8],
) -> Result<(), FirestoneError> {
    write_injected_marker(output, FIRESTONE_OCI_MARKER_PATH.as_bytes())?;
    write_injected_init(output, FIRESTONE_INIT_PATH.as_bytes(), init_binary)
}

/// A surviving member, keyed in the plan by its normalized path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergedEntry {
    layer: usize,
    ordinal: u64,
    kind: MergedKind,
    link_target: Option<Vec<u8>>,
    local_target: Option<(u64, u64)>,
    /// A symlink's target exactly as the layer recorded it, kept so the
    /// injection can resolve its own parent through the tree's own links.
    symlink_target: Option<Vec<u8>>,
}

/// The classes of member the overlay rules distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergedKind {
    Directory,
    Regular,
    Symlink,
    HardLink,
    Other,
}

/// How pass two emits a member whose linkage the merge had to repair.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EmitMode {
    /// Re-point the hardlink at a surviving member.
    Relink(Vec<u8>),
    /// Turn the hardlink into a regular file carrying the shadowed content.
    Promote { ordinal: u64, size: u64 },
}

/// The result of pass one.
struct MergePlan {
    entries: BTreeMap<Vec<u8>, MergedEntry>,
    overrides: HashMap<Vec<u8>, EmitMode>,
    bytes_read: u64,
}

/// Pass one: walks every layer and applies the overlay rules.
fn build_plan(request: &MergeRequest<'_>) -> Result<MergePlan, FirestoneError> {
    let mut entries: BTreeMap<Vec<u8>, MergedEntry> = BTreeMap::new();
    let mut bytes_read = 0u64;
    let mut members_read = 0u64;

    for (layer, source) in request.layers.iter().enumerate() {
        let mut cursor = LayerCursor::new(layer, *source);
        let label = cursor.label();
        let mut local_files: HashMap<Vec<u8>, (u64, u64)> = HashMap::new();

        while let Some(member) = cursor.read_header()? {
            members_read += 1;
            if members_read > request.limits.max_entries {
                return Err(cap_error(format!(
                    "the OCI layers hold more than {} entries",
                    request.limits.max_entries
                )));
            }
            let projected = bytes_read
                .saturating_add(cursor.bytes_read)
                .saturating_add(member.data_size);
            if projected > request.limits.max_uncompressed_bytes {
                return Err(cap_error(format!(
                    "the OCI layers expand beyond {} uncompressed bytes",
                    request.limits.max_uncompressed_bytes
                )));
            }

            let Some(path) = normalize_entry_path(&member.path, &label)? else {
                // The archive's own root directory entry: nothing to merge,
                // but its payload still has to be stepped over.
                cursor.skip_data()?;
                continue;
            };
            let (parent, name) = split_last_component(&path);

            if name == OPAQUE_MARKER {
                drop_lower_subtree(&mut entries, parent, layer, false);
            } else if let Some(removed) = name.strip_prefix(WHITEOUT_PREFIX) {
                if removed.is_empty() {
                    return Err(layer_error(
                        format!("layer {label} has an empty whiteout name"),
                        REPULL_HINT,
                    ));
                }
                let target = join_path(parent, removed);
                drop_lower_subtree(&mut entries, &target, layer, true);
            } else {
                let kind = classify_entry(member.entry_type);
                if matches!(
                    member.entry_type,
                    EntryType::Regular | EntryType::Continuous
                ) {
                    local_files.insert(path.clone(), (member.ordinal, member.data_size));
                }
                if kind != MergedKind::Directory {
                    drop_lower_subtree(&mut entries, &path, layer, false);
                }
                let link_target = if kind == MergedKind::HardLink {
                    Some(normalize_link_target(
                        member.link.as_deref().unwrap_or_default(),
                        &label,
                    )?)
                } else {
                    None
                };
                let local_target = link_target
                    .as_ref()
                    .and_then(|target| local_files.get(target).copied());
                let symlink_target = (kind == MergedKind::Symlink)
                    .then(|| member.link.clone())
                    .flatten();
                entries.insert(
                    path,
                    MergedEntry {
                        layer,
                        ordinal: member.ordinal,
                        kind,
                        link_target,
                        local_target,
                        symlink_target,
                    },
                );
            }

            cursor.skip_data()?;
        }

        bytes_read = bytes_read.saturating_add(cursor.bytes_read);
    }

    let overrides = plan_hardlinks(&entries)?;
    Ok(MergePlan {
        entries,
        overrides,
        bytes_read,
    })
}

/// Decides how every hardlink whose target was shadowed is emitted.
fn plan_hardlinks(
    entries: &BTreeMap<Vec<u8>, MergedEntry>,
) -> Result<HashMap<Vec<u8>, EmitMode>, FirestoneError> {
    let mut overrides: HashMap<Vec<u8>, EmitMode> = HashMap::new();
    let mut representatives: HashMap<(usize, u64), Vec<u8>> = HashMap::new();

    for (path, entry) in entries {
        if entry.kind != MergedKind::HardLink {
            continue;
        }
        let target = entry.link_target.as_ref().ok_or_else(|| {
            layer_error(
                format!("hardlink {} has no target", String::from_utf8_lossy(path)),
                REPULL_HINT,
            )
        })?;
        let winner = entries.get(target);
        if winner.is_some_and(|found| found.layer == entry.layer) {
            continue;
        }
        if let Some((ordinal, size)) = entry.local_target {
            let key = (entry.layer, ordinal);
            let mode = match representatives.get(&key) {
                Some(representative) if representative.len() <= USTAR_LINK_NAME_LIMIT => {
                    EmitMode::Relink(representative.clone())
                }
                Some(_) => EmitMode::Promote { ordinal, size },
                None => {
                    representatives.insert(key, path.clone());
                    EmitMode::Promote { ordinal, size }
                }
            };
            overrides.insert(path.clone(), mode);
            continue;
        }
        if winner.is_some_and(|found| found.kind == MergedKind::Regular) {
            continue;
        }
        return Err(layer_error(
            format!(
                "hardlink {} points at the missing entry {}",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(target)
            ),
            REPULL_HINT,
        ));
    }

    Ok(overrides)
}

/// Pass two: streams the surviving members out in sorted order.
///
/// Returns the number of members written and the merged tree's unpacked size
/// for the §8.5 sizing rule.
fn write_merged<W: Write>(
    request: &MergeRequest<'_>,
    plan: &MergePlan,
    output: &mut W,
) -> Result<(u64, u64), FirestoneError> {
    let mut cursors: Vec<LayerCursor<'_>> = request
        .layers
        .iter()
        .enumerate()
        .map(|(index, source)| LayerCursor::new(index, *source))
        .collect();
    let init_path = resolve_injection_path(&plan.entries, FIRESTONE_INIT_PATH.as_bytes());
    let marker_path = resolve_injection_path(&plan.entries, FIRESTONE_OCI_MARKER_PATH.as_bytes());
    let mut injected: Vec<&[u8]> = Vec::new();
    if request.injected_init.is_some() {
        injected.push(marker_path.as_slice());
        injected.push(init_path.as_slice());
        injected.sort_unstable();
    }
    let init_binary = request.injected_init.unwrap_or_default();
    let mut injected_index = 0usize;
    let mut written = 0u64;
    let mut unpacked = 0u64;

    for (path, entry) in &plan.entries {
        let mut shadowed = false;
        while injected_index < injected.len() && injected[injected_index] <= path.as_slice() {
            shadowed |= injected[injected_index] == path.as_slice();
            unpacked = unpacked.saturating_add(write_injected(
                output,
                injected[injected_index],
                &init_path,
                init_binary,
            )?);
            injected_index += 1;
            written += 1;
        }
        if shadowed {
            continue;
        }
        unpacked = unpacked.saturating_add(write_entry(
            request,
            plan,
            &mut cursors,
            path,
            entry,
            output,
        )?);
        written += 1;
    }
    while injected_index < injected.len() {
        unpacked = unpacked.saturating_add(write_injected(
            output,
            injected[injected_index],
            &init_path,
            init_binary,
        )?);
        injected_index += 1;
        written += 1;
    }

    Ok((written, unpacked))
}

/// Streams one surviving member, repairing its header when the plan says so.
///
/// Returns the member's contribution to the §8.5 unpacked size.
fn write_entry<W: Write>(
    request: &MergeRequest<'_>,
    plan: &MergePlan,
    cursors: &mut [LayerCursor<'_>],
    path: &[u8],
    entry: &MergedEntry,
    output: &mut W,
) -> Result<u64, FirestoneError> {
    let cursor = cursors.get_mut(entry.layer).ok_or_else(|| {
        layer_error(
            format!("layer index {} is out of range", entry.layer),
            "report this as a firestone bug",
        )
    })?;
    let member = cursor.seek_to(entry.ordinal)?;
    let label = cursor.label();
    let observed = normalize_entry_path(&member.path, &label)?;
    if observed.as_deref() != Some(path) {
        return Err(layer_error(
            format!("layer {label} changed while it was being merged"),
            "re-pull the image and merge it again",
        ));
    }

    // §8.5 sizing: an emitted regular file counts its data, and every emitted
    // directory, symlink, and hard link counts one metadata block. A promoted
    // hard link is emitted as a regular file, so it counts its content.
    let unpacked = match plan.overrides.get(path) {
        Some(EmitMode::Promote { size, .. }) => *size,
        Some(EmitMode::Relink(_)) => METADATA_ENTRY_BYTES,
        None => match entry.kind {
            MergedKind::Regular => member.data_size,
            MergedKind::Directory | MergedKind::Symlink | MergedKind::HardLink => {
                METADATA_ENTRY_BYTES
            }
            MergedKind::Other => 0,
        },
    };

    match plan.overrides.get(path) {
        None => {
            for extension in &member.extensions {
                write_extension(output, extension)?;
            }
            output
                .write_all(member.header.as_bytes())
                .map_err(write_error)?;
            cursor.copy_data(output)?;
        }
        Some(EmitMode::Relink(target)) => {
            for extension in filter_extensions(&member.extensions, false)? {
                write_extension(output, &extension)?;
            }
            let mut header = member.header.clone();
            header.set_link_name_literal(target).map_err(|error| {
                layer_error(
                    format!("cannot re-point hardlink {}", String::from_utf8_lossy(path)),
                    REPULL_HINT,
                )
                .with_source(error)
            })?;
            header.set_cksum();
            output.write_all(header.as_bytes()).map_err(write_error)?;
            cursor.skip_data()?;
        }
        Some(EmitMode::Promote { ordinal, size }) => {
            for extension in filter_extensions(&member.extensions, true)? {
                write_extension(output, &extension)?;
            }
            let mut header = member.header.clone();
            header.set_entry_type(EntryType::Regular);
            header.set_size(*size);
            header.set_link_name_literal("").map_err(|error| {
                layer_error(
                    format!("cannot promote hardlink {}", String::from_utf8_lossy(path)),
                    REPULL_HINT,
                )
                .with_source(error)
            })?;
            header.set_cksum();
            output.write_all(header.as_bytes()).map_err(write_error)?;
            cursor.skip_data()?;

            let source = request.layers.get(entry.layer).ok_or_else(|| {
                layer_error(
                    format!("layer index {} is out of range", entry.layer),
                    "report this as a firestone bug",
                )
            })?;
            let mut content = LayerCursor::new(entry.layer, *source);
            let target = content.seek_to(*ordinal)?;
            if !matches!(
                target.entry_type,
                EntryType::Regular | EntryType::Continuous
            ) || target.data_size != *size
            {
                return Err(layer_error(
                    format!(
                        "hardlink {} has an unusable content source",
                        String::from_utf8_lossy(path)
                    ),
                    REPULL_HINT,
                ));
            }
            content.copy_data(output)?;
        }
    }

    Ok(unpacked)
}

/// Writes one injected member and returns its §8.5 unpacked contribution.
fn write_injected<W: Write>(
    output: &mut W,
    path: &[u8],
    init_path: &[u8],
    init_binary: &[u8],
) -> Result<u64, FirestoneError> {
    if path == init_path {
        write_injected_init(output, path, init_binary)?;
        Ok(init_binary.len() as u64)
    } else {
        write_injected_marker(output, path)?;
        Ok(0)
    }
}

/// Writes the empty `/etc/firestone-oci` marker.
fn write_injected_marker<W: Write>(output: &mut W, path: &[u8]) -> Result<(), FirestoneError> {
    let header = injected_header(path, 0o644, 0)?;
    output.write_all(header.as_bytes()).map_err(write_error)
}

/// Writes the `/sbin/firestone-init` regular file.
fn write_injected_init<W: Write>(
    output: &mut W,
    path: &[u8],
    init_binary: &[u8],
) -> Result<(), FirestoneError> {
    let size = init_binary.len() as u64;
    let header = injected_header(path, 0o755, size)?;
    output.write_all(header.as_bytes()).map_err(write_error)?;
    output.write_all(init_binary).map_err(write_error)?;
    write_padding(output, size)
}

/// Builds a deterministic ustar header for an injected member.
fn injected_header(path: &[u8], mode: u32, size: u64) -> Result<Header, FirestoneError> {
    let rendered = String::from_utf8_lossy(path).into_owned();
    let path = rendered.as_str();
    let mut header = Header::new_ustar();
    header
        .set_path(path)
        .map_err(|error| injection_error(path, error))?;
    header
        .set_username("root")
        .map_err(|error| injection_error(path, error))?;
    header
        .set_groupname("root")
        .map_err(|error| injection_error(path, error))?;
    header.set_entry_type(EntryType::Regular);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(size);
    header.set_cksum();
    Ok(header)
}

/// Removes every lower-layer entry under `path`, and `path` itself for a
/// whiteout.
fn drop_lower_subtree(
    entries: &mut BTreeMap<Vec<u8>, MergedEntry>,
    path: &[u8],
    layer: usize,
    include_self: bool,
) {
    let mut doomed: Vec<Vec<u8>> = Vec::new();
    if include_self
        && !path.is_empty()
        && entries.get(path).is_some_and(|entry| entry.layer < layer)
    {
        doomed.push(path.to_vec());
    }
    let mut prefix = path.to_vec();
    if !prefix.is_empty() {
        prefix.push(b'/');
    }
    for (key, entry) in entries.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        if entry.layer < layer {
            doomed.push(key.clone());
        }
    }
    for key in doomed {
        entries.remove(&key);
    }
}

/// Maps a tar entry type onto the classes the overlay rules distinguish.
fn classify_entry(entry_type: EntryType) -> MergedKind {
    match entry_type {
        EntryType::Directory => MergedKind::Directory,
        EntryType::Regular | EntryType::Continuous => MergedKind::Regular,
        EntryType::Symlink => MergedKind::Symlink,
        EntryType::Link => MergedKind::HardLink,
        _ => MergedKind::Other,
    }
}

/// Splits a normalized path into its parent and last component.
fn split_last_component(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(index) => (&path[..index], &path[index + 1..]),
        None => (&[], path),
    }
}

/// How many symlinks one injection path may traverse before it gives up.
const MAX_INJECTION_LINK_HOPS: usize = 8;

/// Lexically joins a target onto a base, resolving `.` and `..` inside the root.
///
/// An absolute target starts again from the root, and a `..` that would leave
/// the root is clamped there, so the result always names a path inside the
/// merged tree.
fn lexical_join(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut components: Vec<&[u8]> = Vec::new();
    let absolute = target.first() == Some(&b'/');
    let sources: [&[u8]; 2] = if absolute {
        [b"", target]
    } else {
        [base, target]
    };
    for source in sources {
        for component in source.split(|byte| *byte == b'/') {
            match component {
                b"" | b"." => {}
                b".." => {
                    components.pop();
                }
                other => components.push(other),
            }
        }
    }
    components.join(&b'/')
}

/// Resolves one injected member's path through the merged tree's own symlinks.
///
/// SPEC §8.5 injects `/sbin/firestone-init`, and every usrmerged image — Debian,
/// Ubuntu, and the long tail built on them, `nginx:latest` included — ships
/// `sbin` as a symlink to `usr/sbin`. A tar member written under a symlinked
/// parent is not a file in the directory that link names, and the pinned
/// `mkfs.ext4` refuses the archive rather than guessing. Resolving the parent
/// here puts the payload in the real directory; the guest kernel's
/// `init=/sbin/firestone-init` still reaches it, because it follows the same
/// link.
fn resolve_injection_path(entries: &BTreeMap<Vec<u8>, MergedEntry>, path: &[u8]) -> Vec<u8> {
    let (parent, name) = split_last_component(path);
    let mut resolved: Vec<u8> = Vec::new();
    for component in parent.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        let mut candidate = join_path(&resolved, component);
        for _ in 0..MAX_INJECTION_LINK_HOPS {
            let Some(target) = entries
                .get(&candidate)
                .and_then(|entry| entry.symlink_target.as_deref())
            else {
                break;
            };
            let (link_parent, _) = split_last_component(&candidate);
            let next = lexical_join(link_parent, target);
            if next.is_empty() || next == candidate {
                break;
            }
            candidate = next;
        }
        resolved = candidate;
    }
    join_path(&resolved, name)
}

/// Joins a normalized parent with one component.
fn join_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return name.to_vec();
    }
    let mut joined = Vec::with_capacity(parent.len() + 1 + name.len());
    joined.extend_from_slice(parent);
    joined.push(b'/');
    joined.extend_from_slice(name);
    joined
}

/// Normalizes and hardens an entry path.
///
/// `Ok(None)` means the member is the archive root itself. `./` and `.` are the
/// root directory entry that GNU tar writes first, and a large share of real
/// registry layers carry one; it names no content, cannot escape anything, and
/// `mkfs.ext4 -d` owns the root directory's own metadata, so the merge skips it
/// rather than refusing the whole image (SPEC §8.5).
fn normalize_entry_path(raw: &[u8], layer: &str) -> Result<Option<Vec<u8>>, FirestoneError> {
    if raw.first() == Some(&b'/') {
        return Err(traversal_error(raw, layer, "the path is absolute"));
    }
    normalize_relative(raw, layer)
}

/// Normalizes and hardens a hardlink target, tolerating a leading separator.
///
/// A link that resolves to the archive root names no member, so it stays an
/// error here even though the root *entry* is now skipped.
fn normalize_link_target(raw: &[u8], layer: &str) -> Result<Vec<u8>, FirestoneError> {
    let trimmed = raw.strip_prefix(b"/").unwrap_or(raw);
    normalize_relative(trimmed, layer)?
        .ok_or_else(|| traversal_error(raw, layer, "the path resolves to the archive root"))
}

/// Shared normalization: drops `.` and empty components, rejects `..` and NUL.
fn normalize_relative(raw: &[u8], layer: &str) -> Result<Option<Vec<u8>>, FirestoneError> {
    if raw.is_empty() {
        return Err(traversal_error(raw, layer, "the path is empty"));
    }
    if raw.contains(&0) {
        return Err(traversal_error(raw, layer, "the path contains a NUL byte"));
    }
    let mut components: Vec<&[u8]> = Vec::new();
    for component in raw.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                return Err(traversal_error(
                    raw,
                    layer,
                    "the path contains a `..` component",
                ));
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return Ok(None);
    }
    Ok(Some(components.join(&b'/')))
}

/// A PAX or GNU extension header captured verbatim.
#[derive(Debug, Clone)]
struct ExtensionHeader {
    header: Header,
    data: Vec<u8>,
}

/// One tar member with its extension headers already read.
#[derive(Debug, Clone)]
struct Member {
    header: Header,
    extensions: Vec<ExtensionHeader>,
    path: Vec<u8>,
    link: Option<Vec<u8>>,
    entry_type: EntryType,
    data_size: u64,
    ordinal: u64,
}

/// Writes one extension header plus its padded payload.
fn write_extension<W: Write>(
    output: &mut W,
    extension: &ExtensionHeader,
) -> Result<(), FirestoneError> {
    output
        .write_all(extension.header.as_bytes())
        .map_err(write_error)?;
    output.write_all(&extension.data).map_err(write_error)?;
    write_padding(output, extension.data.len() as u64)
}

/// Drops the extension records a repaired header would contradict.
fn filter_extensions(
    extensions: &[ExtensionHeader],
    promoted: bool,
) -> Result<Vec<ExtensionHeader>, FirestoneError> {
    let mut kept: Vec<ExtensionHeader> = Vec::new();
    for extension in extensions {
        match extension.header.entry_type() {
            EntryType::GNULongLink => {}
            EntryType::XHeader => {
                let mut data = Vec::with_capacity(extension.data.len());
                for record in parse_pax_records(&extension.data)? {
                    let key = record
                        .split(|byte| *byte == b'=')
                        .next()
                        .unwrap_or_default();
                    if key == b"linkpath" || (promoted && key == b"size") {
                        continue;
                    }
                    data.extend_from_slice(&record_bytes(record));
                }
                if data.is_empty() {
                    continue;
                }
                let mut header = extension.header.clone();
                header.set_size(data.len() as u64);
                header.set_cksum();
                kept.push(ExtensionHeader { header, data });
            }
            _ => kept.push(extension.clone()),
        }
    }
    Ok(kept)
}

/// Re-serializes one PAX record with its self-referential length prefix.
fn record_bytes(record: &[u8]) -> Vec<u8> {
    let mut length = record.len() + 2;
    loop {
        let candidate = length.to_string().len() + 1 + record.len() + 1;
        if candidate == length {
            break;
        }
        length = candidate;
    }
    let mut bytes = Vec::with_capacity(length);
    bytes.extend_from_slice(length.to_string().as_bytes());
    bytes.push(b' ');
    bytes.extend_from_slice(record);
    bytes.push(b'\n');
    bytes
}

/// Splits a PAX payload into its `key=value` records without the framing.
fn parse_pax_records(data: &[u8]) -> Result<Vec<&[u8]>, FirestoneError> {
    let mut records: Vec<&[u8]> = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|byte| *byte == b' ').ok_or_else(|| {
            layer_error(
                "a PAX extended header record has no length prefix",
                REPULL_HINT,
            )
        })?;
        let length: usize = std::str::from_utf8(&rest[..space])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                layer_error(
                    "a PAX extended header record has an unreadable length",
                    REPULL_HINT,
                )
            })?;
        if length <= space + 1 || length > rest.len() {
            return Err(layer_error(
                "a PAX extended header record has an out-of-range length",
                REPULL_HINT,
            ));
        }
        records.push(&rest[space + 1..length - 1]);
        rest = &rest[length..];
    }
    Ok(records)
}

/// Extracts one PAX record value by key.
fn pax_value<'a>(records: &[&'a [u8]], key: &[u8]) -> Option<&'a [u8]> {
    records.iter().copied().find_map(|record| {
        let separator = record.iter().position(|byte| *byte == b'=')?;
        (&record[..separator] == key).then(|| &record[separator + 1..])
    })
}

/// A restartable forward reader over one layer's decompressed tar stream.
struct LayerCursor<'a> {
    index: usize,
    source: &'a dyn LayerSource,
    reader: Option<GzDecoder<Box<dyn Read + 'a>>>,
    next_ordinal: u64,
    pending_data: Option<u64>,
    bytes_read: u64,
}

impl<'a> LayerCursor<'a> {
    fn new(index: usize, source: &'a dyn LayerSource) -> Self {
        Self {
            index,
            source,
            reader: None,
            next_ordinal: 0,
            pending_data: None,
            bytes_read: 0,
        }
    }

    fn label(&self) -> String {
        format!("{} (index {})", self.source.label(), self.index)
    }

    fn reopen(&mut self) -> Result<(), FirestoneError> {
        self.reader = Some(GzDecoder::new(self.source.open()?));
        self.next_ordinal = 0;
        self.pending_data = None;
        Ok(())
    }

    /// Positions the cursor on the member with the given ordinal.
    fn seek_to(&mut self, ordinal: u64) -> Result<Member, FirestoneError> {
        if self.reader.is_none() || self.next_ordinal > ordinal {
            self.reopen()?;
        }
        loop {
            let label = self.label();
            let member = self.read_header()?.ok_or_else(|| {
                layer_error(
                    format!("layer {label} ended before entry {ordinal}"),
                    "re-pull the image and merge it again",
                )
            })?;
            if member.ordinal == ordinal {
                return Ok(member);
            }
            self.skip_data()?;
        }
    }

    /// Reads the next member header, consuming any extension headers first.
    fn read_header(&mut self) -> Result<Option<Member>, FirestoneError> {
        if self.reader.is_none() {
            self.reopen()?;
        }
        if self.pending_data.is_some() {
            self.skip_data()?;
        }
        let label = self.label();
        let mut extensions: Vec<ExtensionHeader> = Vec::new();
        let mut long_name: Option<Vec<u8>> = None;
        let mut long_link: Option<Vec<u8>> = None;
        let mut pax_path: Option<Vec<u8>> = None;
        let mut pax_link: Option<Vec<u8>> = None;

        loop {
            let mut block = [0u8; BLOCK];
            if !self.read_block(&mut block)? {
                if extensions.is_empty() {
                    return Ok(None);
                }
                return Err(layer_error(
                    format!("layer {label} ends after an extension header"),
                    "re-pull the image; the layer tar is truncated",
                ));
            }
            if block.iter().all(|byte| *byte == 0) {
                return Ok(None);
            }
            let mut header = Header::new_old();
            header.as_mut_bytes().copy_from_slice(&block);
            let stored = header.cksum().map_err(|error| {
                layer_error(
                    format!("layer {label} has an unreadable header checksum"),
                    REPULL_HINT,
                )
                .with_source(error)
            })?;
            if !checksum_matches(&block, stored) {
                return Err(layer_error(
                    format!("layer {label} has a header checksum mismatch"),
                    REPULL_HINT,
                ));
            }
            let entry_type = header.entry_type();
            if entry_type == EntryType::GNUSparse {
                return Err(layer_error(
                    format!("layer {label} holds a GNU sparse entry"),
                    "rebuild the image without sparse tar entries",
                ));
            }
            let size = header.entry_size().map_err(|error| {
                layer_error(
                    format!("layer {label} has an unreadable entry size"),
                    REPULL_HINT,
                )
                .with_source(error)
            })?;

            if matches!(
                entry_type,
                EntryType::GNULongName
                    | EntryType::GNULongLink
                    | EntryType::XHeader
                    | EntryType::XGlobalHeader
            ) {
                if size > MAX_EXTENSION_BYTES {
                    return Err(cap_error(format!(
                        "layer {label} has an extension header above {MAX_EXTENSION_BYTES} bytes"
                    )));
                }
                let data = self.read_exact_padded(size)?;
                match entry_type {
                    EntryType::GNULongName => long_name = Some(trim_nul(&data).to_vec()),
                    EntryType::GNULongLink => long_link = Some(trim_nul(&data).to_vec()),
                    EntryType::XHeader => {
                        let records = parse_pax_records(&data)?;
                        pax_path = pax_value(&records, b"path").map(<[u8]>::to_vec);
                        pax_link = pax_value(&records, b"linkpath").map(<[u8]>::to_vec);
                    }
                    _ => {}
                }
                if entry_type != EntryType::XGlobalHeader {
                    extensions.push(ExtensionHeader { header, data });
                }
                continue;
            }

            let path = pax_path
                .or(long_name)
                .unwrap_or_else(|| header.path_bytes().into_owned());
            let link = pax_link
                .or(long_link)
                .or_else(|| header.link_name_bytes().map(std::borrow::Cow::into_owned));
            let ordinal = self.next_ordinal;
            self.next_ordinal += 1;
            self.pending_data = Some(size);
            return Ok(Some(Member {
                header,
                extensions,
                path,
                link,
                entry_type,
                data_size: size,
                ordinal,
            }));
        }
    }

    /// Discards the current member's payload.
    fn skip_data(&mut self) -> Result<(), FirestoneError> {
        let size = self.take_pending()?;
        self.read_exact_padded(size).map(|_| ())
    }

    /// Streams the current member's payload, padded to a whole block.
    fn copy_data<W: Write>(&mut self, output: &mut W) -> Result<(), FirestoneError> {
        let size = self.take_pending()?;
        let label = self.label();
        let reader = self.reader_mut()?;
        let copied = io::copy(&mut reader.by_ref().take(size), output).map_err(|error| {
            FirestoneError::new(ErrorKind::Dependency, "cannot copy an OCI layer entry")
                .with_hint("re-pull the image and merge it again")
                .with_source(error)
        })?;
        self.bytes_read = self.bytes_read.saturating_add(copied);
        if copied != size {
            return Err(layer_error(
                format!("layer {label} is truncated inside an entry"),
                "re-pull the image; the layer tar is truncated",
            ));
        }
        self.skip_padding(size)?;
        write_padding(output, size)
    }

    fn take_pending(&mut self) -> Result<u64, FirestoneError> {
        let label = self.label();
        self.pending_data.take().ok_or_else(|| {
            layer_error(
                format!("layer {label} has no entry payload pending"),
                "report this as a firestone bug",
            )
        })
    }

    fn reader_mut(&mut self) -> Result<&mut GzDecoder<Box<dyn Read + 'a>>, FirestoneError> {
        self.reader.as_mut().ok_or_else(|| {
            FirestoneError::new(ErrorKind::Dependency, "an OCI layer reader is not open")
                .with_hint("report this as a firestone bug")
        })
    }

    /// Reads exactly one block, reporting a clean end of stream as `false`.
    fn read_block(&mut self, block: &mut [u8; BLOCK]) -> Result<bool, FirestoneError> {
        let label = self.label();
        let reader = self.reader_mut()?;
        let mut filled = 0usize;
        while filled < BLOCK {
            match reader.read(&mut block[filled..]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(read_error(error)),
            }
        }
        self.bytes_read = self.bytes_read.saturating_add(filled as u64);
        if filled == 0 {
            return Ok(false);
        }
        if filled < BLOCK {
            return Err(layer_error(
                format!("layer {label} ends inside a tar header"),
                "re-pull the image; the layer tar is truncated",
            ));
        }
        Ok(true)
    }

    /// Reads `size` bytes plus the block padding that follows them.
    fn read_exact_padded(&mut self, size: u64) -> Result<Vec<u8>, FirestoneError> {
        let label = self.label();
        let reader = self.reader_mut()?;
        let mut data = Vec::new();
        let copied = reader
            .by_ref()
            .take(size)
            .read_to_end(&mut data)
            .map_err(read_error)?;
        self.bytes_read = self.bytes_read.saturating_add(copied as u64);
        if copied as u64 != size {
            return Err(layer_error(
                format!("layer {label} is truncated inside an entry"),
                "re-pull the image; the layer tar is truncated",
            ));
        }
        self.skip_padding(size)?;
        Ok(data)
    }

    /// Consumes the zero padding that follows a payload of `size` bytes.
    fn skip_padding(&mut self, size: u64) -> Result<(), FirestoneError> {
        let padding = padding_for(size);
        if padding == 0 {
            return Ok(());
        }
        let label = self.label();
        let reader = self.reader_mut()?;
        let mut discard = [0u8; BLOCK];
        let mut filled = 0usize;
        while filled < padding {
            match reader.read(&mut discard[filled..padding]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(read_error(error)),
            }
        }
        self.bytes_read = self.bytes_read.saturating_add(filled as u64);
        if filled < padding {
            return Err(layer_error(
                format!("layer {label} ends inside entry padding"),
                "re-pull the image; the layer tar is truncated",
            ));
        }
        Ok(())
    }
}

/// Bytes needed to pad `size` up to a whole block.
const fn padding_for(size: u64) -> usize {
    let remainder = (size % BLOCK as u64) as usize;
    if remainder == 0 { 0 } else { BLOCK - remainder }
}

/// Writes the zero padding that follows a payload of `size` bytes.
fn write_padding<W: Write>(output: &mut W, size: u64) -> Result<(), FirestoneError> {
    let padding = padding_for(size);
    if padding == 0 {
        return Ok(());
    }
    output
        .write_all(&[0u8; BLOCK][..padding])
        .map_err(write_error)
}

/// Verifies a header checksum against both the unsigned and the signed sum.
fn checksum_matches(block: &[u8; BLOCK], stored: u32) -> bool {
    let mut unsigned = 0u32;
    let mut signed = 0i32;
    for (index, byte) in block.iter().enumerate() {
        let value = if (148..156).contains(&index) {
            b' '
        } else {
            *byte
        };
        unsigned = unsigned.wrapping_add(u32::from(value));
        signed = signed.wrapping_add(i32::from(value as i8));
    }
    unsigned == stored || signed == stored as i32
}

/// Trims the NUL padding GNU long-name payloads carry.
fn trim_nul(data: &[u8]) -> &[u8] {
    match data.iter().position(|byte| *byte == 0) {
        Some(index) => &data[..index],
        None => data,
    }
}

fn layer_error(message: impl Into<String>, hint: &str) -> FirestoneError {
    FirestoneError::new(ErrorKind::Dependency, message).with_hint(hint.to_owned())
}

fn cap_error(message: impl Into<String>) -> FirestoneError {
    FirestoneError::new(ErrorKind::Dependency, message)
        .with_hint("raise the OCI merge limits or use a smaller image")
}

fn traversal_error(raw: &[u8], layer: &str, reason: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!(
            "layer {layer} holds the unsafe entry path `{}`: {reason}",
            String::from_utf8_lossy(raw)
        ),
    )
    .with_hint("firestone refuses layer entries that could escape the image root")
}

fn injection_error(path: &str, error: io::Error) -> FirestoneError {
    layer_error(
        format!("cannot build the injected header for /{path}"),
        "report this as a firestone bug",
    )
    .with_source(error)
}

fn read_error(error: io::Error) -> FirestoneError {
    FirestoneError::new(ErrorKind::Dependency, "cannot read an OCI layer")
        .with_hint("re-pull the image and merge it again")
        .with_source(error)
}

fn write_error(error: io::Error) -> FirestoneError {
    FirestoneError::new(ErrorKind::Dependency, "cannot write the merged OCI tar")
        .with_hint("check free space in the firestone image store")
        .with_source(error)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Archive, Builder};

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    /// One entry written into a synthetic layer tar.
    struct TestEntry {
        path: String,
        entry_type: EntryType,
        mode: u32,
        link: Option<String>,
        data: Vec<u8>,
        device: Option<(u32, u32)>,
        raw_name: bool,
    }

    fn make_entry(path: &str, entry_type: EntryType) -> TestEntry {
        TestEntry {
            path: path.to_owned(),
            entry_type,
            mode: 0o644,
            link: None,
            data: Vec::new(),
            device: None,
            raw_name: false,
        }
    }

    fn file_entry(path: &str, data: &str) -> TestEntry {
        let mut entry = make_entry(path, EntryType::Regular);
        entry.data = data.as_bytes().to_vec();
        entry
    }

    fn dir_entry(path: &str) -> TestEntry {
        let mut entry = make_entry(path, EntryType::Directory);
        entry.mode = 0o755;
        entry
    }

    fn symlink_entry(path: &str, target: &str) -> TestEntry {
        let mut entry = make_entry(path, EntryType::Symlink);
        entry.mode = 0o777;
        entry.link = Some(target.to_owned());
        entry
    }

    fn hardlink_entry(path: &str, target: &str) -> TestEntry {
        let mut entry = make_entry(path, EntryType::Link);
        entry.link = Some(target.to_owned());
        entry
    }

    fn raw_name_entry(path: &str) -> TestEntry {
        let mut entry = make_entry(path, EntryType::Regular);
        entry.raw_name = true;
        entry
    }

    fn build_layer(entries: &[TestEntry]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut builder = Builder::new(Vec::new());
        for entry in entries {
            let mut header = Header::new_ustar();
            let declared = if entry.raw_name {
                "placeholder"
            } else {
                entry.path.as_str()
            };
            header.set_path(declared)?;
            header.set_entry_type(entry.entry_type);
            header.set_mode(entry.mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            if let Some(link) = &entry.link {
                header.set_link_name(link)?;
            }
            if let Some((major, minor)) = entry.device {
                header.set_device_major(major)?;
                header.set_device_minor(minor)?;
            }
            header.set_size(entry.data.len() as u64);
            if entry.raw_name {
                let name = entry.path.as_bytes();
                let old = header.as_old_mut();
                old.name = [0u8; 100];
                old.name[..name.len()].copy_from_slice(name);
            }
            header.set_cksum();
            builder.append(&header, entry.data.as_slice())?;
        }
        let uncompressed = builder.into_inner()?;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&uncompressed)?;
        Ok(encoder.finish()?)
    }

    /// One entry read back out of a merged tar.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadEntry {
        path: String,
        entry_type: EntryType,
        mode: u32,
        uid: u64,
        gid: u64,
        link: Option<String>,
        data: Vec<u8>,
        device: Option<(Option<u32>, Option<u32>)>,
    }

    fn read_back(merged: &[u8]) -> Result<Vec<ReadEntry>, Box<dyn Error>> {
        let mut archive = Archive::new(merged);
        let mut collected = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let header = entry.header().clone();
            let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let link = entry
                .link_name_bytes()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            let device = match header.entry_type() {
                EntryType::Char | EntryType::Block => {
                    Some((header.device_major()?, header.device_minor()?))
                }
                _ => None,
            };
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            collected.push(ReadEntry {
                path,
                entry_type: header.entry_type(),
                mode: header.mode()?,
                uid: header.uid()?,
                gid: header.gid()?,
                link,
                data,
                device,
            });
        }
        Ok(collected)
    }

    fn paths(entries: &[ReadEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.path.as_str()).collect()
    }

    fn find<'a>(entries: &'a [ReadEntry], path: &str) -> Option<&'a ReadEntry> {
        entries.iter().find(|entry| entry.path == path)
    }

    fn merge(blobs: &[Vec<u8>]) -> Result<Vec<u8>, FirestoneError> {
        merge_with(blobs, MergeLimits::default(), None)
    }

    fn merge_with(
        blobs: &[Vec<u8>],
        limits: MergeLimits,
        injected: Option<&[u8]>,
    ) -> Result<Vec<u8>, FirestoneError> {
        let sources: Vec<BytesLayer> = blobs
            .iter()
            .enumerate()
            .map(|(index, bytes)| BytesLayer::new(format!("layer-{index}"), bytes.clone()))
            .collect();
        let layers: Vec<&dyn LayerSource> = sources
            .iter()
            .map(|source| source as &dyn LayerSource)
            .collect();
        let config = OciImageConfig::default();
        let mut request = MergeRequest::new(&layers, &config).with_limits(limits);
        request.injected_init = injected;
        let mut merged = Vec::new();
        merge_layers(&request, &mut merged)?;
        Ok(merged)
    }

    #[test]
    fn classify_layer_media_type_gzip_variants_return_gzip() -> TestResult {
        for media_type in [MEDIA_TYPE_DOCKER_LAYER_GZIP, MEDIA_TYPE_OCI_LAYER_GZIP] {
            assert_eq!(
                classify_layer_media_type(media_type)?,
                LayerCompression::Gzip
            );
        }
        Ok(())
    }

    #[test]
    fn classify_layer_media_type_unsupported_returns_dependency_error() {
        for media_type in [
            MEDIA_TYPE_OCI_LAYER_ZSTD,
            "application/vnd.oci.image.layer.v1.tar+gzip+estargz",
            "application/octet-stream",
        ] {
            let Err(error) = classify_layer_media_type(media_type) else {
                panic!("{media_type} must not be supported");
            };
            assert_eq!(error.kind(), ErrorKind::Dependency);
            assert!(error.message().contains(media_type));
            assert!(error.hint().is_some());
        }
    }

    #[test]
    fn merge_layers_whiteout_removes_lower_file_and_subtree() -> TestResult {
        let lower = build_layer(&[
            dir_entry("app"),
            file_entry("app/keep", "keep"),
            file_entry("app/gone", "gone"),
            dir_entry("app/tree"),
            file_entry("app/tree/deep", "deep"),
        ])?;
        let upper = build_layer(&[
            file_entry("app/.wh.gone", ""),
            file_entry("app/.wh.tree", ""),
        ])?;

        let entries = read_back(&merge(&[lower, upper])?)?;

        assert_eq!(paths(&entries), vec!["app", "app/keep"]);
        Ok(())
    }

    #[test]
    fn merge_layers_opaque_marker_masks_lower_tree_and_keeps_upper() -> TestResult {
        let lower = build_layer(&[
            dir_entry("data"),
            file_entry("data/old", "old"),
            dir_entry("data/sub"),
            file_entry("data/sub/deep", "deep"),
            file_entry("outside", "outside"),
        ])?;
        let upper = build_layer(&[
            dir_entry("data"),
            file_entry("data/new", "new"),
            file_entry("data/.wh..wh..opq", ""),
        ])?;

        let entries = read_back(&merge(&[lower, upper])?)?;

        assert_eq!(paths(&entries), vec!["data", "data/new", "outside"]);
        Ok(())
    }

    #[test]
    fn merge_layers_file_over_directory_drops_the_old_subtree() -> TestResult {
        let lower = build_layer(&[
            dir_entry("x"),
            file_entry("x/a", "a"),
            file_entry("x/b", "b"),
        ])?;
        let upper = build_layer(&[file_entry("x", "now a file")])?;

        let entries = read_back(&merge(&[lower, upper])?)?;

        assert_eq!(paths(&entries), vec!["x"]);
        assert_eq!(entries[0].entry_type, EntryType::Regular);
        assert_eq!(entries[0].data, b"now a file");
        Ok(())
    }

    #[test]
    fn merge_layers_directory_over_file_keeps_the_new_subtree() -> TestResult {
        let lower = build_layer(&[file_entry("y", "was a file")])?;
        let upper = build_layer(&[dir_entry("y"), file_entry("y/z", "z")])?;

        let entries = read_back(&merge(&[lower, upper])?)?;

        assert_eq!(paths(&entries), vec!["y", "y/z"]);
        assert_eq!(entries[0].entry_type, EntryType::Directory);
        Ok(())
    }

    #[test]
    fn merge_layers_intact_hardlink_stays_a_hardlink() -> TestResult {
        let layer = build_layer(&[
            file_entry("bin/real", "content"),
            hardlink_entry("bin/link", "bin/real"),
        ])?;

        let entries = read_back(&merge(&[layer])?)?;

        let link = find(&entries, "bin/link").ok_or("bin/link is missing")?;
        assert_eq!(link.entry_type, EntryType::Link);
        assert_eq!(link.link.as_deref(), Some("bin/real"));
        Ok(())
    }

    #[test]
    fn merge_layers_hardlink_to_shadowed_target_promotes_and_relinks() -> TestResult {
        let lower = build_layer(&[
            file_entry("bin/real", "lower"),
            hardlink_entry("bin/link", "bin/real"),
            hardlink_entry("bin/link2", "bin/real"),
        ])?;
        let upper = build_layer(&[file_entry("bin/real", "upper")])?;

        let entries = read_back(&merge(&[lower, upper])?)?;

        assert_eq!(paths(&entries), vec!["bin/link", "bin/link2", "bin/real"]);
        let promoted = find(&entries, "bin/link").ok_or("bin/link is missing")?;
        assert_eq!(promoted.entry_type, EntryType::Regular);
        assert_eq!(promoted.data, b"lower");
        let relinked = find(&entries, "bin/link2").ok_or("bin/link2 is missing")?;
        assert_eq!(relinked.entry_type, EntryType::Link);
        assert_eq!(relinked.link.as_deref(), Some("bin/link"));
        let winner = find(&entries, "bin/real").ok_or("bin/real is missing")?;
        assert_eq!(winner.data, b"upper");
        Ok(())
    }

    #[test]
    fn merge_layers_hardlink_to_whiteouted_target_promotes_content() -> TestResult {
        let lower = build_layer(&[
            file_entry("bin/real", "lower"),
            hardlink_entry("bin/link", "bin/real"),
        ])?;
        let upper = build_layer(&[file_entry("bin/.wh.real", "")])?;

        let entries = read_back(&merge(&[lower, upper])?)?;

        assert_eq!(paths(&entries), vec!["bin/link"]);
        assert_eq!(entries[0].entry_type, EntryType::Regular);
        assert_eq!(entries[0].data, b"lower");
        Ok(())
    }

    #[test]
    fn merge_layers_symlink_is_preserved_without_following_it() -> TestResult {
        let layer = build_layer(&[
            file_entry("etc/target", "target"),
            symlink_entry("etc/link", "../../etc/target"),
        ])?;

        let entries = read_back(&merge(&[layer])?)?;

        let link = find(&entries, "etc/link").ok_or("etc/link is missing")?;
        assert_eq!(link.entry_type, EntryType::Symlink);
        assert_eq!(link.link.as_deref(), Some("../../etc/target"));
        assert!(link.data.is_empty());
        Ok(())
    }

    #[test]
    fn merge_layers_unsafe_entry_paths_are_rejected() -> TestResult {
        for path in ["/etc/passwd", "../escape", "a/../../escape"] {
            let layer = build_layer(&[raw_name_entry(path)])?;
            let Err(error) = merge(&[layer]) else {
                panic!("{path} must be rejected");
            };
            assert_eq!(error.kind(), ErrorKind::Dependency);
            assert!(error.message().contains("unsafe entry path"));
            assert!(error.hint().is_some());
        }
        Ok(())
    }

    /// GNU tar writes the archive root as its first member, and a large share
    /// of registry layers — `nginx:latest` among them — carry one. It names no
    /// content and cannot escape, so it is skipped, not refused (SPEC §8.5).
    #[test]
    fn merge_layers_root_directory_entry_is_skipped_not_refused() -> TestResult {
        for root in ["./", "."] {
            let layer = build_layer(&[raw_name_entry(root), file_entry("etc/hostname", "app")])?;

            let entries = read_back(&merge(&[layer])?)?;

            assert_eq!(paths(&entries), vec!["etc/hostname"]);
        }
        Ok(())
    }

    /// A hard link that resolves to the archive root still names no member.
    #[test]
    fn merge_layers_hardlink_to_the_archive_root_is_rejected() -> TestResult {
        let layer = build_layer(&[
            file_entry("bin/real", "real"),
            hardlink_entry("bin/link", "./"),
        ])?;

        let Err(error) = merge(&[layer]) else {
            panic!("a hard link to the archive root must be rejected");
        };
        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(
            error.message().contains("archive root"),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn merge_layers_entry_count_above_the_cap_is_rejected() -> TestResult {
        let layer = build_layer(&[file_entry("a", "a"), file_entry("b", "b")])?;
        let limits = MergeLimits {
            max_entries: 1,
            ..MergeLimits::default()
        };

        let Err(error) = merge_with(&[layer], limits, None) else {
            panic!("the entry cap must be enforced");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("more than 1 entries"));
        Ok(())
    }

    #[test]
    fn merge_layers_uncompressed_size_above_the_cap_is_rejected() -> TestResult {
        let payload = "x".repeat(4096);
        let layer = build_layer(&[file_entry("big", &payload)])?;
        let limits = MergeLimits {
            max_uncompressed_bytes: 1024,
            ..MergeLimits::default()
        };

        let Err(error) = merge_with(&[layer], limits, None) else {
            panic!("the size cap must be enforced");
        };

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("1024 uncompressed bytes"));
        Ok(())
    }

    #[test]
    fn merge_layers_injection_writes_init_and_marker_as_root() -> TestResult {
        let layer = build_layer(&[dir_entry("bin"), file_entry("zzz", "zzz")])?;
        let init = b"init-binary-bytes".as_slice();

        let entries = read_back(&merge_with(&[layer], MergeLimits::default(), Some(init))?)?;

        assert_eq!(
            paths(&entries),
            vec!["bin", "etc/firestone-oci", "sbin/firestone-init", "zzz"]
        );
        let marker = find(&entries, FIRESTONE_OCI_MARKER_PATH).ok_or("the marker is missing")?;
        assert_eq!(marker.entry_type, EntryType::Regular);
        assert_eq!(marker.mode, 0o644);
        assert_eq!((marker.uid, marker.gid), (0, 0));
        assert!(marker.data.is_empty());
        let injected = find(&entries, FIRESTONE_INIT_PATH).ok_or("firestone-init is missing")?;
        assert_eq!(injected.entry_type, EntryType::Regular);
        assert_eq!(injected.mode, 0o755);
        assert_eq!((injected.uid, injected.gid), (0, 0));
        assert_eq!(injected.data, init);
        Ok(())
    }

    /// Every usrmerged image ships `sbin` as a symlink to `usr/sbin`, and a tar
    /// member under a symlinked parent is not a file in the directory the link
    /// names. The injection follows the tree's own link instead (SPEC §8.5).
    #[test]
    fn merge_layers_injection_follows_a_usrmerged_sbin_symlink() -> TestResult {
        let layer = build_layer(&[
            symlink_entry("sbin", "usr/sbin"),
            dir_entry("usr"),
            dir_entry("usr/sbin"),
        ])?;
        let init = b"init-binary-bytes".as_slice();

        let entries = read_back(&merge_with(&[layer], MergeLimits::default(), Some(init))?)?;

        assert_eq!(
            paths(&entries),
            vec![
                "etc/firestone-oci",
                "sbin",
                "usr",
                "usr/sbin",
                "usr/sbin/firestone-init"
            ]
        );
        let injected =
            find(&entries, "usr/sbin/firestone-init").ok_or("firestone-init is missing")?;
        assert_eq!(injected.entry_type, EntryType::Regular);
        assert_eq!(injected.mode, 0o755);
        assert_eq!(injected.data, init);
        let link = find(&entries, "sbin").ok_or("the sbin symlink is missing")?;
        assert_eq!(link.entry_type, EntryType::Symlink);
        assert_eq!(link.link.as_deref(), Some("usr/sbin"));
        Ok(())
    }

    /// An absolute link, a relative one with `..`, and a chain all resolve.
    #[test]
    fn merge_layers_injection_resolves_absolute_relative_and_chained_links() -> TestResult {
        for (target, expected) in [
            ("/usr/sbin", "usr/sbin/firestone-init"),
            ("../usr/sbin", "usr/sbin/firestone-init"),
        ] {
            let layer = build_layer(&[
                symlink_entry("sbin", target),
                dir_entry("usr"),
                dir_entry("usr/sbin"),
            ])?;
            let entries = read_back(&merge_with(
                &[layer],
                MergeLimits::default(),
                Some(b"init".as_slice()),
            )?)?;
            assert!(
                find(&entries, expected).is_some(),
                "{target} did not resolve to {expected}: {:?}",
                paths(&entries)
            );
        }
        // A chain resolves link by link, and the second hop is relative to the
        // link's own directory, exactly as the kernel resolves it.
        let chained = build_layer(&[
            symlink_entry("sbin", "usr/sbin"),
            dir_entry("usr"),
            dir_entry("usr/bin"),
            symlink_entry("usr/sbin", "bin"),
        ])?;
        let entries = read_back(&merge_with(
            &[chained],
            MergeLimits::default(),
            Some(b"init".as_slice()),
        )?)?;
        assert!(
            find(&entries, "usr/bin/firestone-init").is_some(),
            "a chained link did not resolve: {:?}",
            paths(&entries)
        );
        Ok(())
    }

    #[test]
    fn merge_layers_injection_replaces_a_layer_entry_at_the_same_path() -> TestResult {
        let layer = build_layer(&[file_entry("sbin/firestone-init", "stale")])?;
        let init = b"fresh".as_slice();

        let entries = read_back(&merge_with(&[layer], MergeLimits::default(), Some(init))?)?;

        assert_eq!(
            paths(&entries),
            vec!["etc/firestone-oci", "sbin/firestone-init"]
        );
        let injected = find(&entries, FIRESTONE_INIT_PATH).ok_or("firestone-init is missing")?;
        assert_eq!(injected.data, init);
        Ok(())
    }

    #[test]
    fn append_firestone_init_writes_both_entries_into_an_open_stream() -> TestResult {
        let mut stream = Vec::new();
        append_firestone_init(&mut stream, b"bytes")?;
        stream.extend_from_slice(&[0u8; BLOCK * 2]);

        let entries = read_back(&stream)?;

        assert_eq!(
            paths(&entries),
            vec!["etc/firestone-oci", "sbin/firestone-init"]
        );
        assert_eq!(entries[1].data, b"bytes");
        assert_eq!(entries[1].mode, 0o755);
        Ok(())
    }

    #[test]
    fn merge_layers_same_input_twice_produces_identical_bytes() -> TestResult {
        let lower = build_layer(&[
            dir_entry("srv"),
            file_entry("srv/b", "b"),
            file_entry("srv/a", "a"),
            hardlink_entry("srv/c", "srv/a"),
            symlink_entry("srv/d", "a"),
        ])?;
        let upper = build_layer(&[file_entry("srv/.wh.b", ""), file_entry("srv/e", "e")])?;
        let blobs = vec![lower, upper];
        let init = b"init".as_slice();

        let first = merge_with(&blobs, MergeLimits::default(), Some(init))?;
        let second = merge_with(&blobs, MergeLimits::default(), Some(init))?;

        assert_eq!(first, second);
        assert!(!first.is_empty());
        Ok(())
    }

    #[test]
    fn merge_layers_out_of_order_input_is_sorted_with_parents_first() -> TestResult {
        let layer = build_layer(&[
            file_entry("usr/lib/z", "z"),
            file_entry("usr/bin/a", "a"),
            dir_entry("usr/lib"),
            dir_entry("usr"),
            dir_entry("usr/bin"),
        ])?;

        let entries = read_back(&merge(&[layer])?)?;

        assert_eq!(
            paths(&entries),
            vec!["usr", "usr/bin", "usr/bin/a", "usr/lib", "usr/lib/z"]
        );
        Ok(())
    }

    #[test]
    fn merge_layers_metadata_and_device_entries_are_preserved() -> TestResult {
        let mut node = make_entry("dev/null", EntryType::Char);
        node.mode = 0o666;
        node.device = Some((1, 3));
        let mut script = file_entry("usr/bin/run", "#!/bin/sh\n");
        script.mode = 0o750;
        let layer = build_layer(&[node, script])?;

        let entries = read_back(&merge(&[layer])?)?;

        let device = find(&entries, "dev/null").ok_or("dev/null is missing")?;
        assert_eq!(device.entry_type, EntryType::Char);
        assert_eq!(device.mode, 0o666);
        assert_eq!(device.device, Some((Some(1), Some(3))));
        let executable = find(&entries, "usr/bin/run").ok_or("usr/bin/run is missing")?;
        assert_eq!(executable.mode, 0o750);
        assert_eq!(executable.data, b"#!/bin/sh\n");
        Ok(())
    }

    #[test]
    fn merge_layers_long_path_keeps_its_extension_header() -> TestResult {
        let long = format!("usr/share/{}/file", "d".repeat(160));
        let mut builder = Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(4);
        builder.append_data(&mut header, &long, b"data".as_slice())?;
        let uncompressed = builder.into_inner()?;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&uncompressed)?;
        let layer = encoder.finish()?;

        let entries = read_back(&merge(&[layer])?)?;

        assert_eq!(paths(&entries), vec![long.as_str()]);
        assert_eq!(entries[0].data, b"data");
        Ok(())
    }

    #[test]
    fn merge_layers_summary_reports_counts_and_carries_the_config() -> TestResult {
        let layer = build_layer(&[dir_entry("opt"), file_entry("opt/a", "a")])?;
        let source = BytesLayer::new("layer-0", layer);
        let layers: Vec<&dyn LayerSource> = vec![&source];
        let config = OciImageConfig {
            env: vec!["PATH=/usr/bin".to_owned()],
            entrypoint: vec!["/opt/a".to_owned()],
            cmd: Vec::new(),
            working_dir: Some("/opt".to_owned()),
            user: Some("root".to_owned()),
        };
        let request = MergeRequest::new(&layers, &config);
        let mut merged = Vec::new();

        let summary = merge_layers(&request, &mut merged)?;

        assert_eq!(summary.entries_written, 2);
        assert!(summary.uncompressed_bytes_read > 0);
        // One directory plus a one-byte file (SPEC §8.5 sizing input).
        assert_eq!(summary.unpacked_bytes, METADATA_ENTRY_BYTES + 1);
        assert_eq!(summary.config, config);
        Ok(())
    }

    /// SPEC §8.5 sizing counts the members the canonical tar emits: file data
    /// for regular files and one metadata block for every directory, symlink,
    /// and hard link, with a promoted hard link counting as the file it became.
    #[test]
    fn merge_layers_unpacked_bytes_counts_emitted_members_by_kind() -> TestResult {
        let lower = build_layer(&[
            dir_entry("bin"),
            file_entry("bin/real", "0123456789"),
            hardlink_entry("bin/link", "bin/real"),
            hardlink_entry("bin/link2", "bin/real"),
            symlink_entry("bin/alias", "real"),
            file_entry("bin/dropped", "gone"),
        ])?;
        let upper = build_layer(&[
            file_entry("bin/real", "upper"),
            file_entry("bin/.wh.dropped", ""),
        ])?;
        let sources = [
            BytesLayer::new("layer-0", lower),
            BytesLayer::new("layer-1", upper),
        ];
        let layers: Vec<&dyn LayerSource> = sources
            .iter()
            .map(|source| source as &dyn LayerSource)
            .collect();
        let config = OciImageConfig::default();
        let init = b"init-bytes";
        let request = MergeRequest::new(&layers, &config).with_injected_init(init);
        let mut merged = Vec::new();

        let summary = merge_layers(&request, &mut merged)?;

        // The `bin` directory, the `bin/alias` symlink and the `bin/link2`
        // relinked hard link cost one metadata block each; the promoted
        // `bin/link` carries the shadowed ten-byte content, `bin/real` carries
        // the five upper bytes, the injected init carries its own, and the
        // empty `/etc/firestone-oci` marker carries none. `bin/dropped` was
        // whiteouted, so it is not emitted and not counted.
        let expected = 3 * METADATA_ENTRY_BYTES + 10 + 5 + init.len() as u64;
        assert_eq!(summary.unpacked_bytes, expected);
        Ok(())
    }

    #[test]
    fn oci_image_config_null_lists_deserialize_as_empty() -> TestResult {
        let parsed: OciImageConfig = serde_json::from_str(
            r#"{"Env":["A=1"],"Cmd":null,"Entrypoint":null,"WorkingDir":"/srv","ExposedPorts":{}}"#,
        )?;

        assert_eq!(parsed.env, vec!["A=1".to_owned()]);
        assert!(parsed.cmd.is_empty());
        assert!(parsed.entrypoint.is_empty());
        assert_eq!(parsed.working_dir.as_deref(), Some("/srv"));
        assert_eq!(parsed.user, None);
        Ok(())
    }
}
