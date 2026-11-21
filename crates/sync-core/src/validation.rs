//! Pure validation rules for local roots, cloud collection ancestry, and
//! cross-platform file and directory names.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use unicase::UniCase;
use unicode_normalization::UnicodeNormalization;

use crate::model::{CloudCollection, DeviceId, LocalBinding, MappingId};

/// Maximum supported UTF-8 byte length for a single file or directory name component
/// on APFS and ext4 filesystems.
pub const MAX_COMPONENT_BYTES: usize = 255;

/// Characters rejected as reserved or unsupported across macOS, Linux, and
/// removable-disk filesystems (in addition to ASCII control characters and NUL).
pub const RESERVED_FILENAME_CHARS: &[char] = &[':', '\\', '<', '>', '"', '|', '?', '*'];

/// Error returned when a [`PathResolver`] cannot canonicalize a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathResolutionError {
    /// Path that failed canonicalization.
    pub path: PathBuf,
    /// Underlying diagnostic message.
    pub message: String,
}

impl fmt::Display for PathResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to resolve path `{}`: {}",
            self.path.display(),
            self.message
        )
    }
}

impl std::error::Error for PathResolutionError {}

/// Injected filesystem path canonicalization interface.
pub trait PathResolver {
    /// Resolves `path` to its canonical absolute form.
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, PathResolutionError>;
}

/// Real [`PathResolver`] implementation backed by [`std::fs::canonicalize`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StdPathResolver;

impl PathResolver for StdPathResolver {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, PathResolutionError> {
        std::fs::canonicalize(path).map_err(|err| PathResolutionError {
            path: path.to_path_buf(),
            message: err.to_string(),
        })
    }
}

/// Machine-readable classification for a [`ValidationError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationErrorKind {
    /// A deletion limit percentage fell outside `0..=100`.
    InvalidMaxDeletePercent {
        /// The rejected percentage value.
        value: u8,
    },
    /// A required identifier or path field was empty.
    EmptyField {
        /// Name of the empty field.
        field: String,
    },
    /// Path canonicalization failed via [`PathResolver`].
    CanonicalizationFailed {
        /// Path that could not be canonicalized.
        path: PathBuf,
    },
    /// A local binding attempted to use a volume root or escape its volume.
    VolumeRootDisallowed {
        /// Rejected path.
        path: PathBuf,
    },
    /// Two local mapping roots overlap (equal, ancestor, or descendant).
    LocalRootOverlap {
        /// Canonical path of the first mapping root.
        first: PathBuf,
        /// Canonical path of the second mapping root.
        second: PathBuf,
    },
    /// A recovery directory overlaps a synchronized tree.
    RecoveryDirOverlap {
        /// Canonical synchronized tree root.
        sync_root: PathBuf,
        /// Canonical recovery directory.
        recovery_dir: PathBuf,
    },
    /// The application state directory overlaps a synchronized tree.
    AppStateDirOverlap {
        /// Canonical synchronized tree root.
        sync_root: PathBuf,
        /// Canonical application state directory.
        app_state_dir: PathBuf,
    },
    /// The same cloud collection was mapped twice on the same device.
    CloudDuplicateCollectionOnSameDevice {
        /// Device on which the duplicate collection appears.
        device_id: DeviceId,
        /// Duplicate Google Drive folder ID.
        folder_id: String,
    },
    /// Two distinct cloud collections nest inside each other.
    CloudCollectionNesting {
        /// First collection's folder ID.
        first_folder_id: String,
        /// Second collection's folder ID.
        second_folder_id: String,
    },
    /// Two distinct cloud folder IDs share the same display path.
    CloudDuplicateDisplayPath {
        /// Colliding normalized display path.
        display_path: String,
        /// First collection's folder ID.
        first_folder_id: String,
        /// Second collection's folder ID.
        second_folder_id: String,
    },
}

/// Typed validation error with a plain-language cause and actionable next step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    /// Structured error category.
    pub kind: ValidationErrorKind,
    /// Human-readable cause explaining why validation failed.
    pub cause: String,
    /// Actionable next step for resolving the problem.
    pub next_step: String,
}

impl ValidationError {
    /// Constructs a new validation error.
    pub fn new(
        kind: ValidationErrorKind,
        cause: impl Into<String>,
        next_step: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            cause: cause.into(),
            next_step: next_step.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Next step: {}", self.cause, self.next_step)
    }
}

impl std::error::Error for ValidationError {}

/// Local layout entry describing one mapping's local root and recovery root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalLayoutEntry {
    /// Mapping identifier.
    pub mapping_id: MappingId,
    /// Local volume binding for the synchronized directory.
    pub local_binding: LocalBinding,
    /// Local recovery root for this mapping.
    pub local_recovery_root: PathBuf,
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn canonicalize_checked(
    path: &Path,
    resolver: &dyn PathResolver,
) -> Result<PathBuf, ValidationError> {
    resolver.canonicalize(path).map_err(|err| {
        ValidationError::new(
            ValidationErrorKind::CanonicalizationFailed {
                path: err.path.clone(),
            },
            format!(
                "Could not resolve local path `{}`: {}.",
                err.path.display(),
                err.message
            ),
            "Confirm that the volume is mounted and the directory exists and is accessible.",
        )
    })
}

/// Validates a single local binding and returns its canonical data root.
///
/// Rejects volume roots, empty or absolute `relative_dir`, and parent-directory
/// traversal (`..`).
pub fn validate_local_binding(
    binding: &LocalBinding,
    resolver: &dyn PathResolver,
) -> Result<PathBuf, ValidationError> {
    if binding.volume.stable_id.trim().is_empty() {
        return Err(ValidationError::new(
            ValidationErrorKind::EmptyField {
                field: "local_binding.volume.stable_id".to_owned(),
            },
            "The local volume binding is missing a stable volume identifier.",
            "Re-select the target folder on a supported mounted volume.",
        ));
    }

    let mut has_normal_component = false;
    for component in binding.relative_dir.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(ValidationError::new(
                    ValidationErrorKind::VolumeRootDisallowed {
                        path: binding.resolved_path(),
                    },
                    format!(
                        "Relative folder `{}` must stay strictly inside volume mount `{}`.",
                        binding.relative_dir.display(),
                        binding.mount_path.display()
                    ),
                    "Choose a dedicated subdirectory inside the selected volume without `..` or absolute paths.",
                ));
            }
        }
    }

    if !has_normal_component {
        return Err(ValidationError::new(
            ValidationErrorKind::VolumeRootDisallowed {
                path: binding.resolved_path(),
            },
            format!(
                "Cannot use volume root `{}` as a synchronized folder.",
                binding.mount_path.display()
            ),
            "Create or select a dedicated subdirectory on the volume so sibling recovery folders remain separate.",
        ));
    }

    let canonical_mount = canonicalize_checked(&binding.mount_path, resolver)?;
    let canonical_data = canonicalize_checked(&binding.resolved_path(), resolver)?;

    if canonical_data == canonical_mount
        || canonical_data.parent().is_none()
        || !canonical_data.starts_with(&canonical_mount)
    {
        return Err(ValidationError::new(
            ValidationErrorKind::VolumeRootDisallowed {
                path: canonical_data,
            },
            format!(
                "Resolved folder `{}` is either a volume root or escapes mounted volume `{}`.",
                binding.resolved_path().display(),
                canonical_mount.display()
            ),
            "Select an ordinary subdirectory directly inside the bound volume.",
        ));
    }

    Ok(canonical_data)
}

/// Validates that two local roots do not overlap after canonicalization.
///
/// Equal paths, ancestor paths, and descendant paths are all rejected.
pub fn validate_local_roots_disjoint(
    first: &Path,
    second: &Path,
    resolver: &dyn PathResolver,
) -> Result<(), ValidationError> {
    let canonical_first = canonicalize_checked(first, resolver)?;
    let canonical_second = canonicalize_checked(second, resolver)?;

    if paths_overlap(&canonical_first, &canonical_second) {
        return Err(ValidationError::new(
            ValidationErrorKind::LocalRootOverlap {
                first: canonical_first.clone(),
                second: canonical_second.clone(),
            },
            format!(
                "Local directories `{}` and `{}` overlap after resolving links.",
                canonical_first.display(),
                canonical_second.display()
            ),
            "Choose separate, non-nested local folders for each mapping.",
        ));
    }

    Ok(())
}

/// Validates a complete set of local mappings against each other, their
/// recovery directories, and the application state directory.
pub fn validate_local_layout(
    entries: &[LocalLayoutEntry],
    app_state_dir: &Path,
    resolver: &dyn PathResolver,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    let canonical_app_state = match canonicalize_checked(app_state_dir, resolver) {
        Ok(path) => Some(path),
        Err(err) => {
            errors.push(err);
            None
        }
    };

    let mut sync_roots: Vec<PathBuf> = Vec::with_capacity(entries.len());
    let mut recovery_roots: Vec<PathBuf> = Vec::with_capacity(entries.len());

    for entry in entries {
        match validate_local_binding(&entry.local_binding, resolver) {
            Ok(root) => sync_roots.push(root),
            Err(err) => errors.push(err),
        }
        match canonicalize_checked(&entry.local_recovery_root, resolver) {
            Ok(recovery) => recovery_roots.push(recovery),
            Err(err) => errors.push(err),
        }
    }

    for i in 0..sync_roots.len() {
        for j in (i + 1)..sync_roots.len() {
            if paths_overlap(&sync_roots[i], &sync_roots[j]) {
                errors.push(ValidationError::new(
                    ValidationErrorKind::LocalRootOverlap {
                        first: sync_roots[i].clone(),
                        second: sync_roots[j].clone(),
                    },
                    format!(
                        "Mapped local folders `{}` and `{}` overlap.",
                        sync_roots[i].display(),
                        sync_roots[j].display()
                    ),
                    "Move one mapping to a separate folder outside the other mapping's tree.",
                ));
            }
        }
    }

    for sync_root in &sync_roots {
        for recovery_dir in &recovery_roots {
            if paths_overlap(sync_root, recovery_dir) {
                errors.push(ValidationError::new(
                    ValidationErrorKind::RecoveryDirOverlap {
                        sync_root: sync_root.clone(),
                        recovery_dir: recovery_dir.clone(),
                    },
                    format!(
                        "Recovery directory `{}` overlaps synchronized folder `{}`.",
                        recovery_dir.display(),
                        sync_root.display()
                    ),
                    "Place local recovery directories outside every synchronized folder (for example, in a sibling FolderSyncRecovery folder).",
                ));
            }
        }

        if let Some(app_state) = &canonical_app_state
            && paths_overlap(sync_root, app_state)
        {
            errors.push(ValidationError::new(
                ValidationErrorKind::AppStateDirOverlap {
                    sync_root: sync_root.clone(),
                    app_state_dir: app_state.clone(),
                },
                format!(
                    "Application state directory `{}` overlaps synchronized folder `{}`.",
                    app_state.display(),
                    sync_root.display()
                ),
                "Choose a local folder that does not include or reside inside FolderSync's application state directory.",
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Cloud collection assignment scoped to a specific device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCloudCollection {
    /// Device owning this mapping assignment.
    pub device_id: DeviceId,
    /// Cloud collection mapped on that device.
    pub collection: CloudCollection,
}

fn validate_single_cloud_assignment(
    assignment: &DeviceCloudCollection,
) -> Result<(), ValidationError> {
    if assignment.device_id.is_empty() {
        return Err(ValidationError::new(
            ValidationErrorKind::EmptyField {
                field: "device_id".to_owned(),
            },
            "Cloud collection assignment is missing a device identifier.",
            "Assign a valid device identifier before validating collections.",
        ));
    }
    if assignment.collection.folder_id.trim().is_empty() {
        return Err(ValidationError::new(
            ValidationErrorKind::EmptyField {
                field: "collection.folder_id".to_owned(),
            },
            "Cloud collection is missing its Google Drive folder ID.",
            "Resolve the collection's actual Google Drive folder ID before saving the mapping.",
        ));
    }
    if assignment.collection.normalized_segments().is_empty() {
        return Err(ValidationError::new(
            ValidationErrorKind::EmptyField {
                field: "collection.display_path".to_owned(),
            },
            "Cloud collection is missing a display path under the shared remote root.",
            "Select a specific collection folder under the shared parent instead of an empty path.",
        ));
    }
    Ok(())
}

fn is_segment_prefix(shorter: &[&str], longer: &[&str]) -> bool {
    shorter.len() < longer.len() && longer.starts_with(shorter)
}

/// Validates two cloud collection assignments against the household non-nesting
/// and same-device overlap rules.
///
/// Identical collections (`folder_id` equal) are rejected on the same device
/// and allowed across different devices. Distinct collections must never nest
/// by folder ID ancestry or display path.
pub fn validate_cloud_collection_pair(
    first: &DeviceCloudCollection,
    second: &DeviceCloudCollection,
) -> Result<(), ValidationError> {
    validate_single_cloud_assignment(first)?;
    validate_single_cloud_assignment(second)?;

    if first.collection.folder_id == second.collection.folder_id {
        if first.device_id == second.device_id {
            return Err(ValidationError::new(
                ValidationErrorKind::CloudDuplicateCollectionOnSameDevice {
                    device_id: first.device_id.clone(),
                    folder_id: first.collection.folder_id.clone(),
                },
                format!(
                    "Cloud collection `{}` (ID `{}`) is mapped more than once on device `{}`.",
                    first.collection.display_path, first.collection.folder_id, first.device_id
                ),
                "Keep only one mapping for each cloud collection on this computer.",
            ));
        }
        return Ok(());
    }

    let id_nested = first
        .collection
        .ancestor_ids
        .iter()
        .any(|ancestor| ancestor == &second.collection.folder_id)
        || second
            .collection
            .ancestor_ids
            .iter()
            .any(|ancestor| ancestor == &first.collection.folder_id);

    if id_nested {
        return Err(ValidationError::new(
            ValidationErrorKind::CloudCollectionNesting {
                first_folder_id: first.collection.folder_id.clone(),
                second_folder_id: second.collection.folder_id.clone(),
            },
            format!(
                "Cloud collections `{}` (`{}`) and `{}` (`{}`) are nested by Google Drive folder ancestry.",
                first.collection.display_path,
                first.collection.folder_id,
                second.collection.display_path,
                second.collection.folder_id,
            ),
            "Define sibling collections under the shared Data parent instead of mapping both a parent folder and one of its subfolders.",
        ));
    }

    let first_segments = first.collection.normalized_segments();
    let second_segments = second.collection.normalized_segments();

    if first_segments == second_segments {
        let normalized = first_segments.join("/");
        return Err(ValidationError::new(
            ValidationErrorKind::CloudDuplicateDisplayPath {
                display_path: normalized.clone(),
                first_folder_id: first.collection.folder_id.clone(),
                second_folder_id: second.collection.folder_id.clone(),
            },
            format!(
                "Two distinct Google Drive folders (`{}` and `{}`) share the same path `{}`.",
                first.collection.folder_id, second.collection.folder_id, normalized
            ),
            "Rename or consolidate the duplicate folder in Google Drive before syncing.",
        ));
    }

    if is_segment_prefix(&first_segments, &second_segments)
        || is_segment_prefix(&second_segments, &first_segments)
    {
        return Err(ValidationError::new(
            ValidationErrorKind::CloudCollectionNesting {
                first_folder_id: first.collection.folder_id.clone(),
                second_folder_id: second.collection.folder_id.clone(),
            },
            format!(
                "Cloud collections `{}` and `{}` have nested paths.",
                first.collection.display_path, second.collection.display_path
            ),
            "Use sibling collections under Data rather than nested cloud folders.",
        ));
    }

    Ok(())
}

/// Validates a full collection catalog across one or more devices.
pub fn validate_cloud_collections(
    assignments: &[DeviceCloudCollection],
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    for assignment in assignments {
        if let Err(err) = validate_single_cloud_assignment(assignment) {
            errors.push(err);
        }
    }

    for i in 0..assignments.len() {
        for j in (i + 1)..assignments.len() {
            if let Err(err) = validate_cloud_collection_pair(&assignments[i], &assignments[j]) {
                errors.push(err);
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Machine-readable classification of a filename or path compatibility finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NameFindingKind {
    /// Two or more entries in the inventory share the exact same relative path.
    ExactDuplicateName {
        /// Normalized relative path that appeared multiple times.
        normalized_path: String,
        /// Number of occurrences observed in the input list.
        occurrences: usize,
    },
    /// Two or more distinct paths differ only by letter case.
    CaseOnlyCollision {
        /// Case-folded key shared by the colliding paths.
        normalized_key: String,
    },
    /// Two or more distinct paths differ only by Unicode NFC/NFD normalization.
    UnicodeNormalizationCollision {
        /// Canonical NFC form shared by the colliding paths.
        nfc_form: String,
    },
    /// A path component exceeds [`MAX_COMPONENT_BYTES`].
    OverLongComponent {
        /// The over-long path component.
        component: String,
        /// Actual UTF-8 byte length of the component.
        byte_len: usize,
        /// Maximum supported UTF-8 byte length.
        max_bytes: usize,
    },
    /// A path component contains a reserved or unsupported character.
    ReservedOrUnsupportedCharacter {
        /// The offending path component.
        component: String,
        /// The reserved or unsupported character.
        character: char,
    },
    /// A path contains a reserved component such as `.` or `..`.
    ReservedPathComponent {
        /// The reserved component.
        component: String,
    },
}

/// Single filename or path finding reported by [`validate_names`].
///
/// Reports all affected paths without choosing a winner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameFinding {
    /// Machine-readable finding category.
    pub kind: NameFindingKind,
    /// All relative paths involved in this finding.
    pub paths: Vec<String>,
    /// Plain-language cause of the finding.
    pub cause: String,
    /// Actionable next step for the user.
    pub next_step: String,
}

/// Validates a list of `/`-separated relative paths for case-only collisions,
/// Unicode NFC/NFD collisions, over-long components, and reserved or
/// unsupported characters.
///
/// Reports every finding without picking a winning path.
pub fn validate_names<S: AsRef<str>>(relative_paths: &[S]) -> Vec<NameFinding> {
    let mut findings = Vec::new();
    let mut exact_entries: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unique_prefixes: BTreeSet<String> = BTreeSet::new();

    for raw in relative_paths {
        let path = raw.as_ref();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if !segments.is_empty() {
            exact_entries
                .entry(segments.join("/"))
                .or_default()
                .push(path.to_owned());
        }

        for component in &segments {
            if *component == "." || *component == ".." {
                findings.push(NameFinding {
                    kind: NameFindingKind::ReservedPathComponent {
                        component: (*component).to_owned(),
                    },
                    paths: vec![path.to_owned()],
                    cause: format!(
                        "Path `{path}` contains reserved directory component `{component}`."
                    ),
                    next_step: "Remove or rename the reserved path segment before syncing."
                        .to_owned(),
                });
            }

            if component.len() > MAX_COMPONENT_BYTES {
                findings.push(NameFinding {
                    kind: NameFindingKind::OverLongComponent {
                        component: (*component).to_owned(),
                        byte_len: component.len(),
                        max_bytes: MAX_COMPONENT_BYTES,
                    },
                    paths: vec![path.to_owned()],
                    cause: format!(
                        "Component `{component}` in `{path}` is {} UTF-8 bytes, exceeding the {MAX_COMPONENT_BYTES}-byte limit.",
                        component.len()
                    ),
                    next_step: format!(
                        "Shorten the file or folder name to {MAX_COMPONENT_BYTES} UTF-8 bytes or fewer."
                    ),
                });
            }

            let mut seen_bad_chars = BTreeSet::new();
            for ch in component.chars() {
                if (ch == '\0' || ch.is_control() || RESERVED_FILENAME_CHARS.contains(&ch))
                    && seen_bad_chars.insert(ch)
                {
                    findings.push(NameFinding {
                        kind: NameFindingKind::ReservedOrUnsupportedCharacter {
                            component: (*component).to_owned(),
                            character: ch,
                        },
                        paths: vec![path.to_owned()],
                        cause: format!(
                            "Component `{component}` in `{path}` contains reserved or unsupported character `{}`.",
                            ch.escape_debug()
                        ),
                        next_step: "Rename the item to remove control characters and reserved punctuation (: \\ < > \" | ? *).".to_owned(),
                    });
                }
            }
        }

        for depth in 1..=segments.len() {
            unique_prefixes.insert(segments[..depth].join("/"));
        }
    }

    // Report exact duplicate full paths (distinct from shared directory prefixes).
    for (normalized_path, raw_entries) in exact_entries {
        if raw_entries.len() >= 2 {
            let occurrences = raw_entries.len();
            findings.push(NameFinding {
                kind: NameFindingKind::ExactDuplicateName {
                    normalized_path: normalized_path.clone(),
                    occurrences,
                },
                paths: raw_entries,
                cause: format!(
                    "Multiple items ({occurrences}) share the exact same path `{normalized_path}`."
                ),
                next_step:
                    "Inspect the duplicate items and rename or consolidate them before syncing."
                        .to_owned(),
            });
        }
    }

    // Group prefixes by canonical Unicode NFC form to find NFC/NFD collisions.
    let mut by_nfc: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for prefix in &unique_prefixes {
        let nfc_form: String = prefix.nfc().collect();
        by_nfc.entry(nfc_form).or_default().insert(prefix.clone());
    }

    for (nfc_form, raw_variants) in &by_nfc {
        if raw_variants.len() >= 2 {
            let paths: Vec<String> = raw_variants.iter().cloned().collect();
            findings.push(NameFinding {
                kind: NameFindingKind::UnicodeNormalizationCollision {
                    nfc_form: nfc_form.clone(),
                },
                paths,
                cause: format!(
                    "Multiple items normalize to the same Unicode NFC name `{nfc_form}`."
                ),
                next_step: "Inspect both items and rename or consolidate them so only one normalized name remains.".to_owned(),
            });
        }
    }

    // Group distinct NFC forms by Unicode caseless matching (NFD + Unicode full case folding).
    let mut by_case: BTreeMap<UniCase<String>, BTreeMap<String, BTreeSet<String>>> =
        BTreeMap::new();
    for (nfc_form, raw_variants) in by_nfc {
        let caseless_key = UniCase::unicode(nfc_form.nfd().collect::<String>());
        by_case
            .entry(caseless_key)
            .or_default()
            .entry(nfc_form)
            .or_default()
            .extend(raw_variants);
    }

    for (caseless_key, nfc_groups) in by_case {
        if nfc_groups.len() >= 2 {
            let normalized_key: String = caseless_key
                .into_inner()
                .nfc()
                .collect::<String>()
                .to_lowercase();
            let paths: Vec<String> = nfc_groups
                .into_values()
                .flatten()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            findings.push(NameFinding {
                kind: NameFindingKind::CaseOnlyCollision {
                    normalized_key: normalized_key.clone(),
                },
                paths,
                cause: format!(
                    "Multiple items differ only by letter case (`{normalized_key}`), which collides under caseless matching."
                ),
                next_step: "Review the conflicting items and rename them so each name is unique regardless of case.".to_owned(),
            });
        }
    }

    findings
}
