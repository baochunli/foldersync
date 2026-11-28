//! Initialization preflight inspection, side classification, filter file
//! rendering, and access marker handling (§9, §11, §18).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rclone_adapter::{InitPreference, LsJsonEntry, ProcessRunner};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccessMarker, CloudCollection, MappingId, VolumeIdentity,
    archive::{ArchiveSuggestion, archive_suggestion},
    validate_names,
};

use crate::{
    accounts::{AccountError, LockedDrive},
    archive_transaction::ARCHIVE_PARTIAL_SUFFIX,
    error::ManagerError,
    manager::drive_error,
    platform::reject_symlinks_under_root,
};

/// Classification of one endpoint during §9 initialization preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideClassification {
    /// Directory contains no user items (a recognized matching `RCLONE_TEST` marker does not count).
    Empty,
    /// Directory contains one or more user items.
    HasData,
}

/// Summary of one immediate subfolder discovered during detailed endpoint inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubfolderInventory {
    pub name: String,
    pub relative_path: String,
    pub total_bytes: u64,
    pub file_count: usize,
    pub has_children: bool,
}

pub use crate::platform::{DiskSpaceInfo, disk_space_for_path};
use crate::platform::{OsVolumeProbe, VolumeProbe};

/// Volume information and mount-relative location of an existing mapping folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedVolume {
    pub identity: VolumeIdentity,
    pub display_name: String,
    pub mount_path: PathBuf,
    pub relative_dir: PathBuf,
    pub filesystem: String,
    pub space: DiskSpaceInfo,
}

/// Identifies the backing volume of an ordinary local directory.
pub fn detect_volume_identity(path: &Path) -> Result<DetectedVolume, ManagerError> {
    detect_volume_with(&OsVolumeProbe, path)
}

pub(crate) fn detect_volume_with(
    probe: &dyn VolumeProbe,
    path: &Path,
) -> Result<DetectedVolume, ManagerError> {
    let choose_subfolder = || ManagerError::PreflightRejected {
        cause: format!(
            "Cannot use volume root `{}` as a mapping folder.",
            path.display()
        ),
        next_step: "Choose a dedicated subfolder inside the volume.".to_owned(),
    };
    if path == Path::new("/") || path == Path::new("/Volumes") {
        return Err(choose_subfolder());
    }
    let inspect_error = |error| ManagerError::Io {
        cause: format!(
            "Could not inspect the volume for `{}`: {error}.",
            path.display()
        ),
        next_step: "Select an existing folder on a supported mounted disk.".to_owned(),
    };
    let meta = fs::symlink_metadata(path).map_err(inspect_error)?;
    if !meta.is_dir() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Local folder `{}` must be an ordinary directory.",
                path.display()
            ),
            next_step: "Choose a directory without symbolic links.".to_owned(),
        });
    }
    let canonical = fs::canonicalize(path).map_err(inspect_error)?;
    let volume = probe.probe(&canonical).map_err(inspect_error)?;
    let mount_path = volume.binding_mount(&canonical).to_path_buf();
    let relative_dir = canonical
        .strip_prefix(&mount_path)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(choose_subfolder)?
        .to_path_buf();
    Ok(DetectedVolume {
        identity: volume.identity,
        display_name: volume.display_name,
        mount_path,
        relative_dir,
        filesystem: volume.filesystem,
        space: volume.space,
    })
}

/// Detailed preflight inventory for UI wizard review and selective sync (§9, §18).
///
/// This is the single source of truth shared by both wizard preview and actual
/// manager preflight (`inspect_local_endpoint` delegates directly here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailedEndpointInventory {
    pub root: PathBuf,
    pub classification: SideClassification,
    /// Total ordinary items (including excluded items) used for empty-vs-populated classification.
    pub all_items_count: usize,
    /// Included (non-excluded) file count for transfer estimates.
    pub file_count: usize,
    /// Included (non-excluded) byte total for disk-space fit checks.
    pub total_bytes: u64,
    pub has_valid_marker: bool,
    pub marker_conflict: Option<String>,
    pub marker_conflict_path: Option<PathBuf>,
    pub subfolders: Vec<SubfolderInventory>,
    /// Contents of matching build and cache folders observed during this scan.
    pub suggested_folders: BTreeMap<&'static str, SuggestedFolderInventory>,
    pub archive_suggestions: Vec<ArchiveSuggestion>,
    pub symlinks: Vec<String>,
    pub special_files: Vec<String>,
    pub name_findings: Vec<String>,
    pub first_name_finding_next_step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedFolderInventory {
    pub folder_count: usize,
    pub file_count: usize,
    pub total_bytes: Option<u64>,
}

fn record_suggested_folder(
    inventory: &mut BTreeMap<&'static str, SuggestedFolderInventory>,
    path: &str,
    is_dir: bool,
    size: Option<u64>,
) {
    let (parents, name) = path.rsplit_once('/').unwrap_or(("", path));
    for suggestion in sync_core::policy::SUGGESTED_EXCLUSIONS {
        let pattern = suggestion.pattern;
        let directory = pattern.trim_end_matches("/**");
        let matching_directory = is_dir && name == directory;
        let matching_file = !is_dir && parents.split('/').any(|parent| parent == directory);
        if !matching_directory && !matching_file {
            continue;
        }
        let count = inventory
            .entry(pattern)
            .or_insert(SuggestedFolderInventory {
                folder_count: 0,
                file_count: 0,
                total_bytes: Some(0),
            });
        if matching_directory {
            count.folder_count += 1;
        } else {
            count.file_count += 1;
            count.total_bytes = count
                .total_bytes
                .zip(size)
                .map(|(total, size)| total.saturating_add(size));
        }
    }
}

/// Inspects a local directory tree without following symlinks and returns a
/// rich non-mutating inventory (classification, byte counts, immediate subfolders,
/// symlinks, special files, naming findings, and access-marker status).
pub fn inspect_local_endpoint_detailed(
    root: &Path,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
) -> Result<DetailedEndpointInventory, ManagerError> {
    inspect_local_endpoint_with_progress(root, collection, exclusions, &mut |_| Ok(()))
}

pub fn inspect_local_endpoint_with_progress(
    root: &Path,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
    progress: &mut impl FnMut(usize) -> Result<(), ManagerError>,
) -> Result<DetailedEndpointInventory, ManagerError> {
    list_local_endpoint(root, progress)?.inventory(collection, exclusions)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
    Symlink,
    Special,
    Marker,
}

struct ListedEntry {
    path: String,
    kind: EntryKind,
    size: Option<u64>,
    modified: i64,
}

/// Raw endpoint entries and summaries that do not depend on the selection.
pub(crate) struct EndpointListing {
    entries: Vec<ListedEntry>,
    base: DetailedEndpointInventory,
    pub(crate) root_names: Vec<String>,
    drive_marker: Option<Vec<u8>>,
    archives: bool,
    masks: BTreeMap<String, Vec<u8>>,
}

impl EndpointListing {
    fn new(
        root: PathBuf,
        entries: Vec<ListedEntry>,
        drive_marker: Option<Vec<u8>>,
        archives: bool,
    ) -> Self {
        let mut subfolders = BTreeMap::<&str, SubfolderInventory>::new();
        let mut suggested_folders = BTreeMap::new();
        let mut special_files = Vec::new();
        let mut paths = Vec::new();
        let mut root_names = Vec::new();
        for entry in &entries {
            if !entry.path.contains('/') {
                root_names.push(entry.path.clone());
                if entry.kind == EntryKind::Directory {
                    subfolders
                        .entry(&entry.path)
                        .or_insert_with(|| SubfolderInventory {
                            name: entry.path.clone(),
                            relative_path: entry.path.clone(),
                            total_bytes: 0,
                            file_count: 0,
                            has_children: false,
                        });
                }
            }
            if entry.kind == EntryKind::Special {
                special_files.push(entry.path.clone());
            }
            if matches!(entry.kind, EntryKind::Directory | EntryKind::File) {
                paths.push(entry.path.as_str());
                record_suggested_folder(
                    &mut suggested_folders,
                    &entry.path,
                    entry.kind == EntryKind::Directory,
                    entry.size,
                );
            }
        }
        for entry in &entries {
            if let Some((top, _)) = entry.path.split_once('/')
                && let Some(folder) = subfolders.get_mut(top)
            {
                if entry.kind == EntryKind::Directory {
                    folder.has_children = true;
                } else if entry.kind == EntryKind::File {
                    folder.file_count += 1;
                    folder.total_bytes = folder.total_bytes.saturating_add(entry.size.unwrap_or(0));
                }
            }
        }
        let findings = validate_names(&paths);
        let base = DetailedEndpointInventory {
            root,
            classification: if paths.is_empty() {
                SideClassification::Empty
            } else {
                SideClassification::HasData
            },
            all_items_count: paths.len(),
            file_count: 0,
            total_bytes: 0,
            has_valid_marker: drive_marker.is_some(),
            marker_conflict: None,
            marker_conflict_path: None,
            subfolders: subfolders.into_values().collect(),
            suggested_folders,
            archive_suggestions: Vec::new(),
            symlinks: Vec::new(),
            special_files,
            first_name_finding_next_step: findings.first().map(|finding| finding.next_step.clone()),
            name_findings: findings.into_iter().map(|finding| finding.cause).collect(),
        };
        Self {
            entries,
            base,
            root_names,
            drive_marker,
            archives,
            masks: BTreeMap::new(),
        }
    }

    pub(crate) fn inventory(
        &mut self,
        collection: &CloudCollection,
        exclusions: &BTreeSet<String>,
    ) -> Result<DetailedEndpointInventory, ManagerError> {
        // Retain recently selected patterns, including checkbox exclusions that
        // can be flipped repeatedly. Bound storage for obsolete custom patterns.
        if self.masks.len() > 64 {
            self.masks.retain(|pattern, _| exclusions.contains(pattern));
        }
        let mut excluded = vec![0u8; self.entries.len()];
        for pattern in exclusions {
            let mask = self.masks.entry(pattern.clone()).or_insert_with(|| {
                let suffix = pattern.strip_prefix('*').filter(|suffix| {
                    pattern.trim() == pattern && !suffix.contains(['*', '?', '/'])
                });
                self.entries
                    .iter()
                    .map(|entry| {
                        if !matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
                            return 0;
                        }
                        let filtered = u8::from(if let Some(suffix) = suffix {
                            entry.path.split('/').any(|part| part.ends_with(suffix))
                        } else {
                            matches_exclusion_pattern(&entry.path, pattern)
                        });
                        let archive_filtered = self.archives
                            && entry.kind == EntryKind::File
                            && if let Some(suffix) = suffix {
                                entry
                                    .path
                                    .rsplit('/')
                                    .next()
                                    .is_some_and(|name| name.ends_with(suffix))
                                    && !entry.path.split('/').any(|part| part == ".git")
                            } else {
                                archive_pattern_excluded(&entry.path, pattern)
                            };
                        filtered | (u8::from(archive_filtered) << 1)
                    })
                    .collect()
            });
            for (combined, flags) in excluded.iter_mut().zip(mask) {
                *combined |= *flags;
            }
        }
        let mut result = self.base.clone();
        let mut archive_totals = BTreeMap::<&str, (u64, u64, i64)>::new();
        for (entry, flags) in self.entries.iter().zip(excluded) {
            match entry.kind {
                EntryKind::File => {
                    if flags & 1 == 0 {
                        let size = entry.size.ok_or_else(|| ManagerError::PreflightRejected {
                            cause: format!(
                                "The Drive file `{}` did not report a size.",
                                entry.path
                            ),
                            next_step: "Review the file or exclude it before starting first sync."
                                .into(),
                        })?;
                        result.file_count += 1;
                        result.total_bytes = result.total_bytes.saturating_add(size);
                    }
                    if self.archives
                        && flags & 2 == 0
                        && !entry.path.ends_with(ARCHIVE_PARTIAL_SUFFIX)
                        && let Some((top, _)) = entry.path.split_once('/')
                    {
                        let (count, bytes, latest) =
                            archive_totals.entry(top).or_insert((0, 0, i64::MIN));
                        *count += 1;
                        *bytes = bytes.saturating_add(entry.size.unwrap_or(0));
                        *latest = (*latest).max(entry.modified);
                    }
                }
                EntryKind::Symlink if flags & 1 == 0 => result.symlinks.push(entry.path.clone()),
                EntryKind::Marker if self.archives => {
                    let path = result.root.join(&entry.path);
                    let check = if entry.path == ACCESS_MARKER_FILENAME {
                        fs::symlink_metadata(&path)
                            .map_err(|err| err.to_string())
                            .and_then(|meta| {
                                validate_marker_file(&path, &meta, collection)
                                    .map_err(|err| err.cause())
                            })
                    } else {
                        Err(format!(
                            "Existing `{}` at `{}` collides case-insensitively with FolderSync's `{ACCESS_MARKER_FILENAME}` access marker.",
                            entry.path,
                            path.display()
                        ))
                    };
                    match check {
                        Ok(()) => result.has_valid_marker = true,
                        Err(error) => {
                            result.marker_conflict = Some(error);
                            result.marker_conflict_path = Some(path);
                        }
                    }
                }
                _ => {}
            }
        }
        let now = Utc::now().timestamp();
        result.archive_suggestions = archive_totals
            .into_iter()
            .filter_map(|(name, (count, bytes, latest))| {
                archive_suggestion(name.to_owned(), count, bytes, latest, now)
            })
            .collect();
        Ok(result)
    }
}

pub(crate) fn list_local_endpoint(
    root: &Path,
    progress: &mut impl FnMut(usize) -> Result<(), ManagerError>,
) -> Result<EndpointListing, ManagerError> {
    progress(0)?;
    let meta = fs::symlink_metadata(root).map_err(|err| ManagerError::Io {
        cause: format!(
            "Folder `{}` does not exist or cannot be inspected: {err}.",
            root.display()
        ),
        next_step:
            "Confirm the folder exists and is accessible (FolderSync never auto-creates missing roots)."
                .to_string(),
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Folder `{}` must be an ordinary directory, not a file or symbolic link.",
                root.display()
            ),
            next_step: "Choose an ordinary directory for this mapping.".to_string(),
        });
    }

    let mut entries = Vec::new();
    let mut file_count = 0;
    walk_local_entries(root, "", &mut entries, &mut file_count, progress)?;
    progress(file_count)?;
    Ok(EndpointListing::new(
        root.to_path_buf(),
        entries,
        None,
        true,
    ))
}

fn walk_local_entries(
    current: &Path,
    prefix: &str,
    entries: &mut Vec<ListedEntry>,
    file_count: &mut usize,
    progress: &mut impl FnMut(usize) -> Result<(), ManagerError>,
) -> Result<(), ManagerError> {
    let read_error = |path: &Path, error: std::io::Error| ManagerError::Io {
        cause: format!("Failed to inspect `{}`: {error}.", path.display()),
        next_step: "Check filesystem permissions and retry.".to_owned(),
    };
    for entry in fs::read_dir(current).map_err(|error| read_error(current, error))? {
        progress(*file_count)?;
        let entry = entry.map_err(|error| read_error(current, error))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|error| read_error(&path, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = if prefix.is_empty() && relative.eq_ignore_ascii_case(ACCESS_MARKER_FILENAME) {
            EntryKind::Marker
        } else if meta.is_dir() {
            EntryKind::Directory
        } else if meta.is_file() {
            *file_count += 1;
            EntryKind::File
        } else if meta.file_type().is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Special
        };
        let modified = if kind == EntryKind::File {
            meta.modified()
                .map(|time| DateTime::<Utc>::from(time).timestamp())
                .unwrap_or(i64::MAX)
        } else {
            i64::MAX
        };
        entries.push(ListedEntry {
            path: relative.clone(),
            kind,
            size: Some(meta.len()),
            modified,
        });
        if kind == EntryKind::Directory {
            walk_local_entries(&path, &relative, entries, file_count, progress)?;
        }
    }
    Ok(())
}

/// Preflight summary of a local or local-fixture endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInventory {
    pub root: PathBuf,
    pub classification: SideClassification,
    pub item_count: usize,
    pub has_valid_marker: bool,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// Rclone-compatible exclusion matcher for onboarding symlink checks (§18).
///
/// Matches the filter rules rendered into `filters.txt`:
/// - Leading `/` anchors the pattern to the sync root (`"/sub/link"` matches
///   `"sub/link"` only, never `"other/sub/link"`).
/// - Without leading `/`, a pattern matches at root or after any `/` boundary.
/// - `*` matches any characters EXCEPT `/`; `**` matches across `/`.
pub fn matches_exclusion_pattern(rel_path: &str, pattern: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    let rel = rel_path.trim_start_matches('/');
    let (anchored, body) = match pat.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, pat),
    };
    if body.is_empty() {
        return false;
    }

    if let Some(directory) = body.strip_suffix("/**").or_else(|| body.strip_suffix('/')) {
        if !anchored && !directory.contains(['/', '*']) {
            return rel
                .rsplit_once('/')
                .is_some_and(|(parents, _)| parents.split('/').any(|name| name == directory));
        }
        return rel.match_indices('/').any(|(end, _)| {
            let prefix = &rel[..end];
            rclone_glob_match(directory, prefix)
                || (!anchored && rclone_glob_match(&format!("**/{directory}"), prefix))
        });
    }

    if anchored {
        rclone_glob_match(body, rel)
    } else if body.contains('/') {
        rclone_glob_match(body, rel) || rclone_glob_match(&format!("**/{body}"), rel)
    } else {
        // Unanchored single-segment pattern matches the path or any segment (basename or parent dir).
        rclone_glob_match(body, rel)
            || rel
                .split('/')
                .any(|segment| rclone_glob_match(body, segment))
    }
}

/// Glob matcher where `*` matches any char except `/` and `**` matches across `/`.
fn rclone_glob_match(pattern: &str, text: &str) -> bool {
    let Some(first) = pattern.find(['*', '?']) else {
        return pattern == text;
    };
    let last = pattern.rfind(['*', '?']).expect("a wildcard exists");
    if !text.starts_with(&pattern[..first]) || !text.ends_with(&pattern[last + 1..]) {
        return false;
    }
    if first == 0 && last == 1 && pattern.starts_with("**") {
        return true; // The literal suffix was checked above.
    }
    if pattern.ends_with("**") && first == pattern.len() - 2 && last == pattern.len() - 1 {
        return true; // The literal prefix was checked above.
    }
    if first == last && pattern.as_bytes()[first] == b'*' {
        return text.len() >= first + pattern.len() - last - 1 && !text.contains('/');
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut memo = vec![vec![None; t.len() + 1]; p.len() + 1];
    glob_dp(&p, &t, 0, 0, &mut memo)
}

fn glob_dp(p: &[char], t: &[char], pi: usize, ti: usize, memo: &mut [Vec<Option<bool>>]) -> bool {
    if let Some(cached) = memo[pi][ti] {
        return cached;
    }
    let res = if pi == p.len() {
        ti == t.len()
    } else if p[pi] == '*' && pi + 1 < p.len() && p[pi + 1] == '*' {
        // `**` matches zero chars OR consumes any single char (including '/').
        glob_dp(p, t, pi + 2, ti, memo) || (ti < t.len() && glob_dp(p, t, pi, ti + 1, memo))
    } else if p[pi] == '*' {
        // Single `*` matches zero chars OR consumes one non-'/' char.
        glob_dp(p, t, pi + 1, ti, memo)
            || (ti < t.len() && t[ti] != '/' && glob_dp(p, t, pi, ti + 1, memo))
    } else {
        ti < t.len()
            && (p[pi] == t[ti] || (p[pi] == '?' && t[ti] != '/'))
            && glob_dp(p, t, pi + 1, ti + 1, memo)
    };
    memo[pi][ti] = Some(res);
    res
}

// Directory exclusions use a trailing slash or a recursive glob. Ordinary file
// patterns match at the end of a path or a slash-delimited suffix.
pub(crate) fn archive_excluded(path: &str, exclusions: &BTreeSet<String>) -> bool {
    if path.ends_with(ARCHIVE_PARTIAL_SUFFIX) {
        return true;
    }
    exclusions
        .iter()
        .any(|pattern| archive_pattern_excluded(path, pattern))
}

fn archive_pattern_excluded(path: &str, pattern: &str) -> bool {
    let matches = |pattern: &str, path: &str| match pattern.strip_prefix('/') {
        Some(pattern) => rclone_glob_match(pattern, path),
        None => {
            rclone_glob_match(pattern, path)
                || path
                    .match_indices('/')
                    .any(|(i, _)| rclone_glob_match(pattern, &path[i + 1..]))
        }
    };
    // Preserve repository internals inside included project folders.
    let git_root = path
        .contains(".git")
        .then(|| {
            path.split('/')
                .scan(0, |offset, segment| {
                    let start = *offset;
                    *offset += segment.len() + 1;
                    Some((start, segment))
                })
                .find_map(|(start, segment)| (segment == ".git").then_some(start))
        })
        .flatten();
    let directory_pattern = pattern.ends_with('/') || pattern.contains("**");
    let normalized;
    let pattern = if pattern.ends_with('/') {
        normalized = format!("{pattern}**");
        &normalized
    } else {
        pattern
    };
    if git_root.is_none() && matches(pattern, path) {
        return true;
    }
    directory_pattern
        && path.match_indices('/').any(|(i, _)| {
            git_root.is_none_or(|git_root| i < git_root) && matches(pattern, &path[..=i])
        })
}

/// Inspects a local directory tree without following symlinks, delegating directly
/// to [`inspect_local_endpoint_detailed`] so wizard preview and manager preflight
/// use the exact same inspection logic (§9, §18).
pub fn inspect_local_endpoint(
    root: &Path,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
) -> Result<EndpointInventory, ManagerError> {
    let inv = inspect_local_endpoint_detailed(root, collection, exclusions)?;

    if let Some(cause) = inv.marker_conflict {
        return Err(ManagerError::AccessMarkerConflict {
            path: inv
                .marker_conflict_path
                .unwrap_or_else(|| root.join(ACCESS_MARKER_FILENAME)),
            cause,
            next_step:
                "Rename or move the existing item before initializing so FolderSync never overwrites or truncates user data."
                    .to_string(),
        });
    }

    if !inv.symlinks.is_empty() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Endpoint `{}` contains unsupported symbolic link(s): {}.",
                root.display(),
                inv.symlinks.join(", ")
            ),
            next_step:
                "Remove the symbolic links or add them to the mapping's exclusions; FolderSync does not traverse symlinks."
                    .to_string(),
        });
    }

    if !inv.special_files.is_empty() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Endpoint `{}` contains unsupported special file(s) (FIFO, socket, or device): {}.",
                root.display(),
                inv.special_files.join(", ")
            ),
            next_step: "Remove or relocate the special files outside the synchronized folder."
                .to_string(),
        });
    }

    if let Some(first_cause) = inv.name_findings.first() {
        return Err(ManagerError::PreflightRejected {
            cause: first_cause.clone(),
            next_step: inv.first_name_finding_next_step.unwrap_or_else(|| {
                "Rename the conflicting file or folder before initializing.".to_string()
            }),
        });
    }

    Ok(EndpointInventory {
        root: inv.root,
        classification: inv.classification,
        item_count: inv.all_items_count,
        has_valid_marker: inv.has_valid_marker,
        file_count: inv.file_count,
        total_bytes: inv.total_bytes,
    })
}

fn validate_marker_file(
    path: &Path,
    meta: &fs::Metadata,
    collection: &CloudCollection,
) -> Result<(), ManagerError> {
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(ManagerError::AccessMarkerConflict {
            path: path.to_path_buf(),
            cause: format!(
                "`{}` at `{}` is not an ordinary FolderSync access marker file.",
                ACCESS_MARKER_FILENAME,
                path.display()
            ),
            next_step:
                "Rename or move the existing item before initializing so FolderSync never overwrites user data."
                    .to_string(),
        });
    }
    let content = fs::read_to_string(path).map_err(|err| ManagerError::AccessMarkerConflict {
        path: path.to_path_buf(),
        cause: format!(
            "Existing `{}` at `{}` could not be read as a FolderSync marker: {err}.",
            ACCESS_MARKER_FILENAME,
            path.display()
        ),
        next_step: "Inspect and move or rename the unrecognized file before continuing."
            .to_string(),
    })?;
    let parsed =
        AccessMarker::parse(&content).map_err(|err| ManagerError::AccessMarkerConflict {
            path: path.to_path_buf(),
            cause: format!(
                "Existing `{}` at `{}` is not a recognized FolderSync access marker: {err}.",
                ACCESS_MARKER_FILENAME,
                path.display()
            ),
            next_step:
                "Move or rename the existing file so FolderSync does not overwrite user data."
                    .to_string(),
        })?;
    if !parsed.matches_collection(collection) {
        return Err(ManagerError::AccessMarkerConflict {
            path: path.to_path_buf(),
            cause: format!(
                "Access marker at `{}` belongs to collection `{}` (`{}`), not `{}` (`{}`).",
                path.display(),
                parsed.collection_display_path,
                parsed.collection_folder_id,
                collection.display_path,
                collection.folder_id
            ),
            next_step:
                "Confirm the expected folder and volume are mounted before syncing this collection."
                    .to_string(),
        });
    }
    Ok(())
}

/// Verifies immediately before launch that `root` contains an exact, regular
/// `RCLONE_TEST` file whose parsed `AccessMarker` matches `collection`, with
/// no case-variant collision.
pub fn verify_endpoint_access_marker(
    root: &Path,
    collection: &CloudCollection,
) -> Result<(), ManagerError> {
    let entries = fs::read_dir(root).map_err(|err| ManagerError::Io {
        cause: format!(
            "Failed to inspect endpoint `{}` before launch: {err}.",
            root.display()
        ),
        next_step: "Confirm the endpoint directory is mounted and readable.".to_string(),
    })?;

    let mut found_exact = false;
    for entry in entries {
        let entry = entry.map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to read directory entry in `{}` before launch: {err}.",
                root.display()
            ),
            next_step: "Confirm the endpoint directory is readable.".to_string(),
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case(ACCESS_MARKER_FILENAME) {
            let path = entry.path();
            if name != ACCESS_MARKER_FILENAME {
                return Err(ManagerError::AccessMarkerConflict {
                    path,
                    cause: format!(
                        "Found unexpected case-variant `{name}` instead of `{ACCESS_MARKER_FILENAME}` in `{}`.",
                        root.display()
                    ),
                    next_step: "Review the folder contents; it may be a replacement tree."
                        .to_string(),
                });
            }
            let meta = fs::symlink_metadata(&path).map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to inspect access marker `{}`: {err}.",
                    path.display()
                ),
                next_step: "Verify endpoint permissions and marker integrity.".to_string(),
            })?;
            validate_marker_file(&path, &meta, collection)?;
            found_exact = true;
        }
    }

    if !found_exact {
        return Err(ManagerError::AccessMarkerConflict {
            path: root.join(ACCESS_MARKER_FILENAME),
            cause: format!(
                "Expected FolderSync access marker `{ACCESS_MARKER_FILENAME}` is missing from `{}`.",
                root.display()
            ),
            next_step:
                "Confirm the right folder or volume is mounted; FolderSync never auto-creates missing markers outside initialization."
                    .to_string(),
        });
    }

    Ok(())
}

/// Decides the explicit `--resync-mode` initialization direction from both side
/// inventories, refusing two populated trees per §9.
pub fn decide_init_direction(
    mapping_id: &MappingId,
    local: &EndpointInventory,
    remote: &EndpointInventory,
    expected_direction: Option<InitPreference>,
) -> Result<InitPreference, ManagerError> {
    let detected = match (local.classification, remote.classification) {
        (SideClassification::HasData, SideClassification::HasData) => {
            return Err(ManagerError::BothSidesPopulated {
                mapping_id: mapping_id.clone(),
                cause: format!(
                    "Both the local folder (`{}`) and the cloud collection (`{}`) already contain data. Automatic initialization will not merge two populated trees.",
                    local.root.display(),
                    remote.root.display()
                ),
                next_step:
                    "Use a new empty destination, or consolidate the two trees outside FolderSync while preserving the existing copy, then initialize again."
                        .to_string(),
            });
        }
        (SideClassification::HasData, SideClassification::Empty) => InitPreference::Path1,
        (SideClassification::Empty, SideClassification::HasData) => InitPreference::Path2,
        (SideClassification::Empty, SideClassification::Empty) => {
            expected_direction.unwrap_or(InitPreference::Path1)
        }
    };

    if let Some(expected) = expected_direction
        && expected != detected
    {
        let label = |dir: InitPreference| match dir {
            InitPreference::Path1 => "upload to the cloud",
            InitPreference::Path2 => "download from the cloud",
        };
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Requested first-sync direction ({}) conflicts with the observed folder contents ({}).",
                label(expected),
                label(detected),
            ),
            next_step:
                "Review which side contains your files and choose the matching initial direction."
                    .to_string(),
        });
    }

    Ok(detected)
}

fn create_marker_exclusively(path: &Path, bytes: &[u8]) -> Result<(), ManagerError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                ManagerError::AccessMarkerConflict {
                    path: path.to_path_buf(),
                    cause: format!(
                        "Refused to create `{}` because an entry already exists at `{}`.",
                        ACCESS_MARKER_FILENAME,
                        path.display()
                    ),
                    next_step:
                        "Inspect and move or rename the existing item before retrying initialization."
                            .to_string(),
                }
            } else {
                ManagerError::Io {
                    cause: format!(
                        "Failed to create access marker `{}`: {err}.",
                        path.display()
                    ),
                    next_step: "Verify write permissions on the folder and retry.".to_string(),
                }
            }
        })?;
    file.write_all(bytes).map_err(|err| ManagerError::Io {
        cause: format!("Failed to write access marker `{}`: {err}.", path.display()),
        next_step: "Check available disk space and retry.".to_string(),
    })?;
    file.sync_all().map_err(|err| ManagerError::Io {
        cause: format!("Failed to sync access marker `{}`: {err}.", path.display()),
        next_step: "Check disk health and retry.".to_string(),
    })?;
    Ok(())
}

/// Establishes or verifies matching FolderSync `RCLONE_TEST` access markers on
/// both endpoints during explicit initialization (§9 step 5, §11), creating new
/// marker files strictly with `create_new` (`O_EXCL`) so existing files are
/// never truncated.
pub fn establish_local_pair_markers(
    local_root: &Path,
    remote_root: &Path,
    local_inv: &EndpointInventory,
    remote_inv: &EndpointInventory,
    collection: &CloudCollection,
) -> Result<(), ManagerError> {
    let canonical_marker = AccessMarker::from_collection(collection).format();
    let local_marker_path = local_root.join(ACCESS_MARKER_FILENAME);
    let remote_marker_path = remote_root.join(ACCESS_MARKER_FILENAME);

    // Joining client case: remote already has a verified matching marker -> reuse its exact bytes locally.
    if remote_inv.has_valid_marker {
        let remote_bytes = fs::read(&remote_marker_path).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to read verified remote access marker `{}`: {err}.",
                remote_marker_path.display()
            ),
            next_step: "Verify endpoint accessibility and retry initialization.".to_string(),
        })?;
        if !local_inv.has_valid_marker {
            create_marker_exclusively(&local_marker_path, &remote_bytes)?;
        }
        return Ok(());
    }

    // New collection or upload-init case: establish canonical marker exclusively on whichever side lacks it.
    if !local_inv.has_valid_marker {
        create_marker_exclusively(&local_marker_path, canonical_marker.as_bytes())?;
    }
    if !remote_inv.has_valid_marker {
        create_marker_exclusively(&remote_marker_path, canonical_marker.as_bytes())?;
    }

    Ok(())
}

const MAX_DRIVE_MARKER_BYTES: u64 = 4096;

fn drive_marker_error(path: &str, cause: &str) -> ManagerError {
    ManagerError::AccessMarkerConflict {
        path: PathBuf::from(path),
        cause: cause.to_owned(),
        next_step: "Review the access marker in the Drive collection before continuing.".to_owned(),
    }
}

pub(crate) fn read_marker_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular access marker"));
    }
    file.take(MAX_DRIVE_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_drive_marker(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
    entries: &[LsJsonEntry],
    runner: &dyn ProcessRunner,
) -> Result<Option<Vec<u8>>, AccountError> {
    let marker_path = format!("{}/{ACCESS_MARKER_FILENAME}", collection.display_path);
    let mut matching = entries
        .iter()
        .filter(|entry| entry.path.eq_ignore_ascii_case(ACCESS_MARKER_FILENAME));
    let Some(marker) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some()
        || marker.path != ACCESS_MARKER_FILENAME
        || marker.is_dir
        || !(1..=MAX_DRIVE_MARKER_BYTES as i64).contains(&marker.size)
    {
        return Err(drive_marker_error(
            &marker_path,
            "The Drive access marker is ambiguous or is not an ordinary marker file.",
        )
        .into());
    }

    let scratch = marker_scratch(drive)?;
    let destination = scratch.path().join(ACCESS_MARKER_FILENAME);
    drive
        .request
        .fetch_marker(&marker_path, &destination, runner)?;
    let bytes = read_marker_bytes(&destination).map_err(|_| {
        drive_marker_error(&marker_path, "The Drive access marker could not be read.")
    })?;
    let valid = bytes.len() == marker.size as usize
        && std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| AccessMarker::parse(text).ok())
            .is_some_and(|parsed| parsed.matches_collection(collection));
    if !valid {
        return Err(drive_marker_error(
            &marker_path,
            "The Drive access marker is unrecognized or identifies another collection.",
        )
        .into());
    }
    Ok(Some(bytes))
}

fn marker_scratch(drive: &LockedDrive<'_>) -> Result<tempfile::TempDir, ManagerError> {
    tempfile::Builder::new()
        .prefix(".marker-")
        .tempdir_in(
            drive
                .account
                .config_path
                .parent()
                .expect("validated account path"),
        )
        .map_err(|err| ManagerError::Io {
            cause: format!(
                "Could not create a temporary folder for the Drive access marker: {err}"
            ),
            next_step: "Check the FolderSync state directory and disk space.".into(),
        })
}

pub(crate) fn inspect_drive_endpoint(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
) -> Result<(DetailedEndpointInventory, Option<Vec<u8>>), ManagerError> {
    let mut listing = list_drive_endpoint(drive, collection)?;
    Ok((
        listing.inventory(collection, exclusions)?,
        listing.drive_marker,
    ))
}

pub(crate) fn list_drive_endpoint(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
) -> Result<EndpointListing, ManagerError> {
    let entries = drive
        .request
        .list_entries(&collection.display_path, true, &drive.runner)
        .map_err(|error| drive_error(error.into()))?;
    let marker =
        read_drive_marker(drive, collection, &entries, &drive.runner).map_err(drive_error)?;
    let entries = entries
        .into_iter()
        .map(|entry| {
            let kind = if entry.path == ACCESS_MARKER_FILENAME {
                EntryKind::Marker
            } else if entry.is_dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            ListedEntry {
                path: entry.path,
                kind,
                size: u64::try_from(entry.size).ok(),
                // Archive eligibility is evaluated only on the Mac.
                modified: i64::MAX,
            }
        })
        .collect();
    Ok(EndpointListing::new(
        PathBuf::from(format!(
            "{}:{}",
            drive.account.remote_name, collection.display_path
        )),
        entries,
        marker,
        false,
    ))
}

pub(crate) fn verify_drive_marker(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
) -> Result<Vec<u8>, ManagerError> {
    verify_drive_marker_with_runner(drive, collection, &drive.runner).map_err(drive_error)
}

pub(crate) fn verify_drive_marker_with_runner(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
    runner: &dyn ProcessRunner,
) -> Result<Vec<u8>, AccountError> {
    let entries = drive
        .request
        .list_entries(&collection.display_path, false, runner)?;
    read_drive_marker(drive, collection, &entries, runner)?.ok_or_else(|| {
        drive_marker_error(
            &collection.display_path,
            "The expected Drive access marker is missing.",
        )
        .into()
    })
}

pub(crate) fn establish_drive_marker(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
    local: &EndpointInventory,
    cloud_marker: Option<Vec<u8>>,
    recovery_dir: &str,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<(), ManagerError> {
    if is_cancelled() {
        return Ok(());
    }
    let marker = match cloud_marker {
        Some(marker) => marker,
        None => {
            let canonical = AccessMarker::from_collection(collection).format();
            if canonical.len() as u64 > MAX_DRIVE_MARKER_BYTES {
                return Err(drive_marker_error(
                    &collection.display_path,
                    "The Drive folder path is too long for its access marker.",
                ));
            }
            let scratch = marker_scratch(drive)?;
            let source = scratch.path().join(ACCESS_MARKER_FILENAME);
            create_marker_exclusively(&source, canonical.as_bytes())?;
            let staging = format!("{recovery_dir}/marker-{}", uuid::Uuid::new_v4());
            let destination = format!("{}/{ACCESS_MARKER_FILENAME}", collection.display_path);
            if is_cancelled() {
                return Ok(());
            }
            drive
                .request
                .stage_marker(&source, &staging, &drive.runner)
                .map_err(|err| drive_error(err.into()))?;
            if is_cancelled() {
                return Ok(());
            }
            drive
                .request
                .publish_staged_marker(&staging, &destination, &drive.runner)
                .map_err(|err| drive_error(err.into()))?;
            verify_drive_marker(drive, collection)?
        }
    };
    if is_cancelled() {
        return Ok(());
    }
    let local_path = local.root.join(ACCESS_MARKER_FILENAME);
    if local.has_valid_marker {
        if read_marker_bytes(&local_path).ok().as_deref() != Some(&marker) {
            return Err(drive_marker_error(
                &collection.display_path,
                "The local and Drive access markers have different contents.",
            ));
        }
    } else {
        create_marker_exclusively(&local_path, &marker)?;
    }
    Ok(())
}

/// Renders frozen rclone `--filters-file` rules from the canonical exclusion set.
pub fn render_filters_file(exclusions: &BTreeSet<String>) -> String {
    let mut out = String::from("# Frozen FolderSync generation filters\n");
    for pattern in exclusions {
        let trimmed = pattern.trim();
        if !trimmed.is_empty() {
            out.push_str("- ");
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out
}

/// Comparison evidence for reusing a recorded first-sync generation.
pub(crate) struct ResumeInventory {
    pub total_files: u64,
    pub total_bytes: Option<u64>,
    pub files_already_present: u64,
    pub bytes_already_present: u64,
    pub partials: Vec<(String, u64)>,
}

pub(crate) fn verify_first_sync_subset(
    source: &[LsJsonEntry],
    destination: &[LsJsonEntry],
    source_root: &Path,
    destination_root: &Path,
    destination_is_local: bool,
    drive: bool,
) -> Result<ResumeInventory, ManagerError> {
    let source = index_resume_entries(source)?;
    let destination = index_resume_entries(destination)?;
    let unique_paths: BTreeSet<_> = source.keys().chain(destination.keys()).copied().collect();
    if let Some(finding) = validate_names(&unique_paths.iter().copied().collect::<Vec<_>>()).first()
    {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "{} Paths: {}.",
                finding.cause,
                finding
                    .paths
                    .iter()
                    .take(5)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            next_step: finding.next_step.clone(),
        });
    }

    // Bisync can use a path from either inventory on either endpoint. In
    // particular, lsjson can omit a symlink that occupies an ancestor of a
    // missing file, and checking only the leaf would follow that symlink.
    for root in [
        (!drive || !destination_is_local).then_some(source_root),
        destination_is_local.then_some(destination_root),
    ]
    .into_iter()
    .flatten()
    {
        for path in &unique_paths {
            reject_symlinks_under_root("first-sync path", root, &root.join(path)).map_err(
                |err| ManagerError::PreflightRejected {
                    cause: format!(
                        "Cannot safely use the first-sync path `{path}`. {}",
                        err.cause()
                    ),
                    next_step:
                        "Review the files and folders at both sync locations before resuming."
                            .into(),
                },
            )?;
        }
    }

    let is_data_file = |entry: &&LsJsonEntry| !entry.is_dir && entry.path != ACCESS_MARKER_FILENAME;
    let mut inventory = ResumeInventory {
        total_files: source.values().copied().filter(is_data_file).count() as u64,
        total_bytes: source.values().copied().filter(is_data_file).try_fold(
            0u64,
            |total, entry| {
                u64::try_from(entry.size)
                    .ok()
                    .map(|bytes| total.saturating_add(bytes))
            },
        ),
        files_already_present: 0,
        bytes_already_present: 0,
        partials: Vec::new(),
    };
    // An unmatched local file is only an engine partial when its exact name
    // matches the pinned engine's fingerprint of an included, still-missing
    // source file. A user file merely ending in .partial is ordinary data.
    let mut expected_partials = BTreeMap::new();
    if destination_is_local
        && destination.values().any(|entry| {
            !entry.is_dir
                && entry.path.ends_with(".partial")
                && !source.contains_key(entry.path.as_str())
        })
    {
        for entry in source.values().copied().filter(is_data_file) {
            if !destination.contains_key(entry.path.as_str())
                && let Some(partial) = rclone_partial_path(entry, drive)
            {
                // A CRC collision is ambiguous even if both source sizes match.
                expected_partials
                    .entry(partial)
                    .and_modify(|size| *size = None)
                    .or_insert(Some(entry.size));
            }
        }
    }

    let mut offending = Vec::new();
    for (&path, &entry) in &destination {
        if path == ACCESS_MARKER_FILENAME || (entry.is_dir && !source.contains_key(path)) {
            continue;
        }
        match source.get(path) {
            Some(source_entry) if entry.is_dir && source_entry.is_dir => {}
            Some(source_entry) if files_match(source_entry, entry, drive) => {
                inventory.files_already_present += 1;
                inventory.bytes_already_present = inventory
                    .bytes_already_present
                    .saturating_add(entry.size as u64);
            }
            None if expected_partials
                .get(path)
                .and_then(|size| *size)
                .is_some_and(|size| entry.size >= 0 && entry.size <= size) =>
            {
                inventory
                    .partials
                    .push((path.to_owned(), entry.size as u64));
            }
            _ if offending.len() < 5 => offending.push(path),
            _ => {}
        }
    }
    // A filtered-out case alias, symlink, or special file can still occupy a
    // new destination pathname on a local filesystem. An absent listing entry
    // alone is insufficient proof that its live pathname is free.
    if destination_is_local {
        for entry in source.values().copied().filter(is_data_file) {
            if offending.len() == 5 {
                break;
            }
            if !destination.contains_key(entry.path.as_str())
                && !matches!(fs::symlink_metadata(destination_root.join(&entry.path)), Err(err) if err.kind() == std::io::ErrorKind::NotFound)
            {
                offending.push(entry.path.as_str());
            }
        }
    }
    if !offending.is_empty() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Both the local folder and the cloud collection already contain data. Automatic initialization will not merge two populated trees. Review these paths: {}.",
                offending.join(", ")
            ),
            next_step: format!(
                "Preserve the files in both folders, then review `{}` and `{}` before trying again.",
                source_root.display(),
                destination_root.display()
            ),
        });
    }
    Ok(inventory)
}

fn index_resume_entries(
    entries: &[LsJsonEntry],
) -> Result<BTreeMap<&str, &LsJsonEntry>, ManagerError> {
    let mut index = BTreeMap::new();
    for entry in entries {
        let path = entry.path.as_str();
        if path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || Path::new(path)
                .file_name()
                .is_none_or(|name| name != entry.name.as_str())
            || (!entry.is_dir && entry.size < 0)
            || index.insert(path, entry).is_some()
        {
            return Err(ManagerError::PreflightRejected {
                cause: format!("Could not safely identify the first-sync path `{path}`."),
                next_step: "Review unsupported or duplicate files and folders before resuming."
                    .into(),
            });
        }
    }
    Ok(index)
}

fn files_match(source: &LsJsonEntry, destination: &LsJsonEntry, drive: bool) -> bool {
    if source.is_dir || destination.is_dir || source.size < 0 || source.size != destination.size {
        return false;
    }
    let Ok(source_time) = chrono::DateTime::parse_from_rfc3339(&source.mod_time) else {
        return false;
    };
    let Ok(destination_time) = chrono::DateTime::parse_from_rfc3339(&destination.mod_time) else {
        return false;
    };
    if drive {
        // Drive stores timestamps at millisecond precision. Match rclone's
        // strict comparison window so the recorded file is also skipped.
        (source_time - destination_time).abs() < chrono::TimeDelta::milliseconds(1)
    } else {
        source_time == destination_time
    }
}

/// rclone v1.76.0 uses CRC32(path + fast source fingerprint), then truncates
/// long basenames before appending the partial suffix.
fn rclone_partial_path(entry: &LsJsonEntry, drive_source: bool) -> Option<String> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(&entry.mod_time)
        .ok()?
        .to_utc();
    let seconds = timestamp.format("%Y-%m-%d %H:%M:%S");
    let nanos = format!("{:09}", timestamp.timestamp_subsec_nanos());
    let fractional = nanos.trim_end_matches('0');
    let mut fingerprint = format!("{}{},{seconds}", entry.path, entry.size);
    if !fractional.is_empty() {
        fingerprint.push('.');
        fingerprint.push_str(fractional);
    }
    fingerprint.push_str(" +0000 UTC");
    if drive_source {
        let md5 = entry
            .hashes
            .iter()
            .find(|(name, value)| name.eq_ignore_ascii_case("md5") && !value.is_empty())?
            .1;
        fingerprint.push(',');
        fingerprint.push_str(md5);
    }
    let suffix = format!(".{:08x}.partial", crc32fast::hash(fingerprint.as_bytes()));
    let mut end = entry.path.len();
    if entry.name.len() > 100 {
        end -= suffix.len();
        while !entry.path.is_char_boundary(end) {
            end -= 1;
        }
    }
    Some(format!("{}{suffix}", &entry.path[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_rejects_a_symlinked_destination_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let outside = temp.path().join("outside");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(source.join("sub/note.txt"), b"source bytes").unwrap();
        std::os::unix::fs::symlink(&outside, destination.join("sub")).unwrap();
        let entries = rclone_adapter::parse_lsjson_entries(
            r#"[
            {"Path":"sub","Name":"sub","IsDir":true},
            {"Path":"sub/note.txt","Name":"note.txt","Size":12,"ModTime":"2026-01-01T00:00:00Z"}
        ]"#,
        )
        .unwrap();

        let error = verify_first_sync_subset(&entries, &[], &source, &destination, true, false)
            .err()
            .expect("a missing leaf behind a symlink must never pass subset preflight");
        assert!(error.cause().contains("symbolic link"), "{error}");
        assert!(!outside.join("note.txt").exists());
    }
}

#[cfg(test)]
mod wizard_inventory_tests {
    use super::*;

    #[test]
    fn refilters_a_large_cached_listing() {
        let root = tempfile::tempdir().unwrap();
        let entries = (0..400_000)
            .map(|i| ListedEntry {
                path: format!("Project/{i}.txt"),
                kind: EntryKind::File,
                size: Some(1),
                modified: 1_600_000_000,
            })
            .collect();
        let mut listing = EndpointListing::new(root.path().into(), entries, None, true);
        let collection = CloudCollection {
            display_path: "Data/Project".into(),
            folder_id: "project-id".into(),
            ancestor_ids: vec!["parent-id".into()],
        };
        let exclusions = sync_core::default_exclusion_patterns();
        let initial = listing.inventory(&collection, &exclusions).unwrap();
        assert_eq!(initial.file_count, 400_000);

        let start = std::time::Instant::now();
        let mut selected = exclusions.clone();
        selected.insert("*.txt".into());
        let excluded = listing.inventory(&collection, &selected).unwrap();
        assert_eq!(excluded.file_count, 0);
        eprintln!(
            "New cached-listing pattern over 400,000 synthetic entries: {:?}",
            start.elapsed()
        );

        let start = std::time::Instant::now();
        let restored = listing.inventory(&collection, &exclusions).unwrap();
        assert_eq!(restored.file_count, 400_000);
        eprintln!(
            "Restoring a cached selection over 400,000 synthetic entries: {:?}",
            start.elapsed()
        );
    }
}
