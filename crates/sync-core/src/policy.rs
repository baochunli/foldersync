//! Synchronization policies, supported-file policy data, default exclusion
//! patterns, and the shared collection access marker format.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::model::CloudCollection;
use crate::validation::{ValidationError, ValidationErrorKind};

/// Default percentage deletion guard (`10%` of tracked files).
pub const DEFAULT_MAX_DELETE_PERCENT: u8 = 10;

/// Minimum number of included files in a suggested subfolder archive.
pub const ARCHIVE_MIN_FILES: u64 = 2_000;

/// Minimum age in calendar months of the newest included file in an archive suggestion.
pub const ARCHIVE_MIN_AGE_MONTHS: u32 = 12;

/// Filename used as the common rclone bisync access marker on both endpoints.
pub const ACCESS_MARKER_FILENAME: &str = "RCLONE_TEST";

/// Header identifying a FolderSync-managed access marker file.
pub const ACCESS_MARKER_HEADER: &str = "FOLDERSYNC-ACCESS-MARKER-V1";

const ACCESS_MARKER_PREFIX: &str = "FOLDERSYNC-ACCESS-MARKER-";

/// Percentage deletion limit passed to rclone bisync (`--max-delete`).
///
/// This is a **percentage of tracked files**, not an absolute file count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MaxDeletePercent(u8);

impl<'de> Deserialize<'de> for MaxDeletePercent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let percent = u8::deserialize(deserializer)?;
        Self::new(percent).map_err(serde::de::Error::custom)
    }
}

impl MaxDeletePercent {
    /// Creates a validated percentage in `0..=100`.
    pub fn new(percent: u8) -> Result<Self, ValidationError> {
        if percent > 100 {
            return Err(ValidationError::new(
                ValidationErrorKind::InvalidMaxDeletePercent { value: percent },
                format!(
                    "Deletion limit {percent}% is out of range because it is a percentage of tracked files."
                ),
                "Choose a deletion limit between 0% and 100% (default: 10%).",
            ));
        }
        Ok(Self(percent))
    }

    /// Returns the numeric percentage (`0..=100`).
    pub fn value(self) -> u8 {
        self.0
    }

    /// Returns the plain household label describing this deletion guard.
    pub fn label(self) -> String {
        format!("Deletion limit: {}% of tracked files", self.0)
    }
}

impl Default for MaxDeletePercent {
    fn default() -> Self {
        Self(DEFAULT_MAX_DELETE_PERCENT)
    }
}

/// File comparison strategy used during synchronization.
///
/// Version 1 compares size and modification time only. Edits preserving both
/// size and modification time can be missed until a future checksum policy or
/// content audit is qualified.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum ComparePolicy {
    /// Compare file size and modification time (`--compare size,modtime`).
    #[default]
    SizeModtime,
}

/// Strategy for resolving conflicting edits on both endpoints.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum ConflictResolution {
    /// Preserve both versions without silently choosing a winner (`--conflict-resolve none`).
    #[default]
    PreserveBoth,
}

/// Naming scheme applied to conflict copies when conflicts are preserved.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum ConflictLoserNaming {
    /// Append a numbered suffix to conflict copies (`--conflict-loser num`).
    #[default]
    Numbered,
}

/// Combined ordinary conflict policy for a mapping generation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct ConflictPolicy {
    /// How conflicting edits are resolved.
    pub resolution: ConflictResolution,
    /// How preserved conflict copies are named.
    pub loser: ConflictLoserNaming,
}

/// Recovery copy and deletion-guard policy frozen in a configuration generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecoveryPolicy {
    /// Whether per-run recovery copies are retained on local and cloud backup directories.
    pub keep_recovery_copies: bool,
    /// Percentage deletion limit guarding against accidental bulk removals.
    pub max_delete_percent: MaxDeletePercent,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            keep_recovery_copies: true,
            max_delete_percent: MaxDeletePercent::default(),
        }
    }
}

/// Categories of files, links, and metadata covered by the version 1 support policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SupportedItemKind {
    /// Ordinary documents, source files, images, videos, and archives.
    OrdinaryFiles,
    /// Symbolic links.
    Symlinks,
    /// Hard-link relationships between files.
    HardLinks,
    /// Google Drive shortcuts.
    DriveShortcuts,
    /// Google-native Docs, Sheets, and Slides.
    GoogleNativeDocuments,
    /// Empty directories.
    EmptyDirectories,
    /// macOS tags, resource forks, ACLs, and extended attributes.
    PlatformMetadata,
    /// Running databases, VM images, and live application libraries.
    ActiveDatabasesAndLibraries,
    /// Files owned by another sync provider's managed tree.
    OtherSyncProviderTrees,
    /// Duplicate names or incompatible case/Unicode names.
    AmbiguousOrIncompatibleNames,
}

/// Disposition assigned to each item category in the version 1 support policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SupportDisposition {
    /// Supported within qualified filesystem and naming constraints.
    Supported,
    /// Do not traverse; report presence and require explicit exclusion or layout change.
    DoNotTraverseAndReport,
    /// Behavior is not preserved in version 1.
    NotPreserved,
    /// Skip during sync and disclose the policy clearly to the user.
    SkipAndDisclose,
    /// Left outside managed byte-file scope.
    OutsideScope,
    /// Deferred until the corresponding engine option is qualified.
    Deferred,
    /// Exclude active instances; qualify closed or exported forms separately.
    ExcludeActive,
    /// Reject as a local mapping root.
    RejectAsRoot,
    /// Stop for user resolution rather than choosing a winner arbitrarily.
    StopForResolution,
}

/// Single row of the version 1 supported-file policy table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedFilePolicyEntry {
    /// Machine-readable item category.
    pub kind: SupportedItemKind,
    /// Plain-language item description.
    pub item: &'static str,
    /// Enforced or disclosed policy disposition.
    pub disposition: SupportDisposition,
    /// Plain-language summary of the version 1 policy.
    pub summary: &'static str,
}

/// Complete version 1 supported-file policy table as static data.
pub const SUPPORTED_FILE_POLICY_TABLE: &[SupportedFilePolicyEntry] = &[
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::OrdinaryFiles,
        item: "Ordinary documents, source files, images, videos, archives",
        disposition: SupportDisposition::Supported,
        summary: "Supported within qualified filesystem and naming constraints.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::Symlinks,
        item: "Symlinks",
        disposition: SupportDisposition::DoNotTraverseAndReport,
        summary: "Do not traverse; report their presence and require an explicit exclusion or supported data layout.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::HardLinks,
        item: "Hard-link relationships",
        disposition: SupportDisposition::NotPreserved,
        summary: "Do not promise link-identity preservation.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::DriveShortcuts,
        item: "Drive shortcuts",
        disposition: SupportDisposition::SkipAndDisclose,
        summary: "Skip; disclose this policy and do not count target contents as synchronized.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::GoogleNativeDocuments,
        item: "Google-native Docs/Sheets/Slides",
        disposition: SupportDisposition::OutsideScope,
        summary: "Leave outside the managed byte-file scope.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::EmptyDirectories,
        item: "Empty directories",
        disposition: SupportDisposition::Deferred,
        summary: "Defer exact empty-directory replication until its engine option is qualified.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::PlatformMetadata,
        item: "macOS tags, resource forks, ACLs, extended attributes",
        disposition: SupportDisposition::NotPreserved,
        summary: "Do not claim faithful preservation in version 1.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::ActiveDatabasesAndLibraries,
        item: "Running databases, VM images, application libraries",
        disposition: SupportDisposition::ExcludeActive,
        summary: "Exclude active instances; qualify closed/exported forms separately.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::OtherSyncProviderTrees,
        item: "Files owned by another sync provider's managed tree",
        disposition: SupportDisposition::RejectAsRoot,
        summary: "Reject as local mapping roots.",
    },
    SupportedFilePolicyEntry {
        kind: SupportedItemKind::AmbiguousOrIncompatibleNames,
        item: "Duplicate names or incompatible case/Unicode names",
        disposition: SupportDisposition::StopForResolution,
        summary: "Stop for resolution rather than choosing arbitrarily.",
    },
];

/// Serializable supported-type switches frozen into a configuration generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SupportedTypesPolicy {
    /// Skip Google-native Docs/Sheets/Slides (`--drive-skip-gdocs`).
    pub skip_google_native_docs: bool,
    /// Skip Google Drive shortcuts (`--drive-skip-shortcuts`).
    pub skip_drive_shortcuts: bool,
    /// Never follow symbolic links (`false` in v1; never enable `--copy-links`).
    pub follow_symlinks: bool,
    /// Exact empty-directory replication (`false` until qualified).
    pub sync_empty_dirs: bool,
}

impl Default for SupportedTypesPolicy {
    fn default() -> Self {
        Self {
            skip_google_native_docs: true,
            skip_drive_shortcuts: true,
            follow_symlinks: false,
            sync_empty_dirs: false,
        }
    }
}

/// One default exclusion pattern together with the reason it is excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultExclusion {
    /// Filter exclusion pattern.
    pub pattern: &'static str,
    /// Brief plain-language explanation of why this pattern is excluded by default.
    pub reason: &'static str,
}

/// Small, documented default exclusion patterns for OS metadata and temporary files.
pub const DEFAULT_EXCLUSIONS: &[DefaultExclusion] = &[
    DefaultExclusion {
        pattern: ".DS_Store",
        reason: "macOS Finder view metadata regenerated locally by Finder.",
    },
    DefaultExclusion {
        pattern: "._*",
        reason: "macOS AppleDouble resource-fork sidecar files.",
    },
    DefaultExclusion {
        pattern: ".Spotlight-V100/**",
        reason: "macOS Spotlight index metadata owned by the local volume.",
    },
    DefaultExclusion {
        pattern: ".Trashes/**",
        reason: "macOS volume Trash directory.",
    },
    DefaultExclusion {
        pattern: ".fseventsd/**",
        reason: "macOS filesystem event log metadata owned by the local volume.",
    },
    DefaultExclusion {
        pattern: ".TemporaryItems/**",
        reason: "macOS temporary staging files.",
    },
    DefaultExclusion {
        pattern: "Thumbs.db",
        reason: "Windows Explorer thumbnail cache file.",
    },
    DefaultExclusion {
        pattern: "~$*",
        reason: "Temporary office document lock files.",
    },
    DefaultExclusion {
        pattern: ".~lock.*#",
        reason: "Temporary LibreOffice document lock files.",
    },
    DefaultExclusion {
        pattern: "*.zip.foldersync-partial",
        reason: "Unfinished ZIP files created during archive preparation.",
    },
];

/// Returns the default exclusion patterns as an ordered set.
pub fn default_exclusion_patterns() -> BTreeSet<String> {
    DEFAULT_EXCLUSIONS
        .iter()
        .map(|entry| entry.pattern.to_owned())
        .collect()
}

/// Errors returned when parsing an existing `RCLONE_TEST` access marker file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessMarkerError {
    /// The file does not begin with the FolderSync marker header and may be user data.
    UnrecognizedFormat,
    /// The marker header specifies an unsupported format version.
    UnsupportedVersion(String),
    /// A required key-value field was missing.
    MissingField(&'static str),
    /// A required field had an empty value.
    EmptyField(&'static str),
    /// A line could not be parsed as a valid key-value pair.
    MalformedLine(String),
}

impl fmt::Display for AccessMarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrecognizedFormat => write!(
                f,
                "access marker does not match the FolderSync format and may contain user data"
            ),
            Self::UnsupportedVersion(header) => {
                write!(f, "unsupported access marker version: {header}")
            }
            Self::MissingField(field) => write!(f, "access marker is missing field `{field}`"),
            Self::EmptyField(field) => write!(f, "access marker field `{field}` is empty"),
            Self::MalformedLine(line) => write!(f, "malformed access marker line: `{line}`"),
        }
    }
}

impl std::error::Error for AccessMarkerError {}

/// Recognizable contents stored in the shared `RCLONE_TEST` access marker file.
///
/// The marker identifies the collection without device-specific fields so every
/// computer syncing the same collection shares identical marker contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessMarker {
    /// Actual Google Drive folder ID of the collection.
    pub collection_folder_id: String,
    /// Human-readable collection path under the remote root.
    pub collection_display_path: String,
}

impl AccessMarker {
    /// Builds an access marker for the given cloud collection.
    pub fn from_collection(collection: &CloudCollection) -> Self {
        Self {
            collection_folder_id: collection.folder_id.clone(),
            collection_display_path: collection.display_path.clone(),
        }
    }

    /// Formats the marker into its canonical plain-text representation.
    pub fn format(&self) -> String {
        format!(
            "{ACCESS_MARKER_HEADER}\ncollection_id={}\ncollection_path={}\n",
            self.collection_folder_id, self.collection_display_path
        )
    }

    /// Parses and validates an existing access marker file.
    ///
    /// Rejects unrecognized files so onboarding never overwrites user data.
    pub fn parse(contents: &str) -> Result<Self, AccessMarkerError> {
        let mut lines = contents.lines();
        let Some(first_line) = lines.next().map(str::trim) else {
            return Err(AccessMarkerError::UnrecognizedFormat);
        };
        if !first_line.starts_with(ACCESS_MARKER_PREFIX) {
            return Err(AccessMarkerError::UnrecognizedFormat);
        }
        if first_line != ACCESS_MARKER_HEADER {
            return Err(AccessMarkerError::UnsupportedVersion(first_line.to_owned()));
        }

        let mut collection_folder_id: Option<String> = None;
        let mut collection_display_path: Option<String> = None;

        for raw_line in lines {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(AccessMarkerError::MalformedLine(line.to_owned()));
            };
            match key {
                "collection_id" => {
                    if value.trim().is_empty() {
                        return Err(AccessMarkerError::EmptyField("collection_id"));
                    }
                    collection_folder_id = Some(value.to_owned());
                }
                "collection_path" => {
                    if value.trim().is_empty() {
                        return Err(AccessMarkerError::EmptyField("collection_path"));
                    }
                    collection_display_path = Some(value.to_owned());
                }
                _ => return Err(AccessMarkerError::MalformedLine(line.to_owned())),
            }
        }

        Ok(Self {
            collection_folder_id: collection_folder_id
                .ok_or(AccessMarkerError::MissingField("collection_id"))?,
            collection_display_path: collection_display_path
                .ok_or(AccessMarkerError::MissingField("collection_path"))?,
        })
    }

    /// Returns `true` when this marker identifies the given collection by folder ID.
    pub fn matches_collection(&self, collection: &CloudCollection) -> bool {
        !self.collection_folder_id.is_empty() && self.collection_folder_id == collection.folder_id
    }
}
