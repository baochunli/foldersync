//! Initialization preflight inspection, side classification, filter file
//! rendering, and access marker handling (§9, §11, §18).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use rclone_adapter::{InitPreference, LsJsonEntry};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccessMarker, CloudCollection, MappingId, VolumeIdentity,
    validate_names,
};

use crate::{accounts::LockedDrive, error::ManagerError, manager::drive_error};

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
    pub symlinks: Vec<String>,
    pub special_files: Vec<String>,
    pub name_findings: Vec<String>,
    pub first_name_finding_next_step: Option<String>,
}

/// Inspects a local directory tree without following symlinks and returns a
/// rich non-mutating inventory (classification, byte counts, immediate subfolders,
/// symlinks, special files, naming findings, and access-marker status).
pub fn inspect_local_endpoint_detailed(
    root: &Path,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
) -> Result<DetailedEndpointInventory, ManagerError> {
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

    let mut all_relative_paths = Vec::new();
    let mut symlinks = Vec::new();
    let mut special_files = Vec::new();
    let mut has_valid_marker = false;
    let mut marker_conflict = None;
    let mut marker_conflict_path = None;
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut top_subfolders: std::collections::BTreeMap<String, SubfolderInventory> =
        std::collections::BTreeMap::new();

    walk_detailed(
        root,
        "",
        None,
        collection,
        exclusions,
        &mut all_relative_paths,
        &mut symlinks,
        &mut special_files,
        &mut has_valid_marker,
        &mut marker_conflict,
        &mut marker_conflict_path,
        &mut file_count,
        &mut total_bytes,
        &mut top_subfolders,
    )?;

    let findings = validate_names(&all_relative_paths);
    let first_name_finding_next_step = findings.first().map(|f| f.next_step.clone());
    let name_findings: Vec<String> = findings.into_iter().map(|f| f.cause).collect();
    let all_items_count = all_relative_paths.len();
    let classification = if all_items_count == 0 {
        SideClassification::Empty
    } else {
        SideClassification::HasData
    };

    Ok(DetailedEndpointInventory {
        root: root.to_path_buf(),
        classification,
        all_items_count,
        file_count,
        total_bytes,
        has_valid_marker,
        marker_conflict,
        marker_conflict_path,
        subfolders: top_subfolders.into_values().collect(),
        symlinks,
        special_files,
        name_findings,
        first_name_finding_next_step,
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_detailed(
    current: &Path,
    rel_prefix: &str,
    top_subdir: Option<&str>,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
    all_relative_paths: &mut Vec<String>,
    symlinks: &mut Vec<String>,
    special_files: &mut Vec<String>,
    has_valid_marker: &mut bool,
    marker_conflict: &mut Option<String>,
    marker_conflict_path: &mut Option<PathBuf>,
    file_count: &mut usize,
    total_bytes: &mut u64,
    top_subfolders: &mut std::collections::BTreeMap<String, SubfolderInventory>,
) -> Result<(), ManagerError> {
    let entries = fs::read_dir(current).map_err(|err| ManagerError::Io {
        cause: format!("Failed to read directory `{}`: {err}.", current.display()),
        next_step: "Check filesystem permissions on the folder and retry.".to_string(),
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to read directory entry in `{}`: {err}.",
                current.display()
            ),
            next_step: "Check filesystem permissions and retry.".to_string(),
        })?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|err| ManagerError::Io {
            cause: format!("Failed to inspect `{}`: {err}.", path.display()),
            next_step: "Check filesystem permissions and retry.".to_string(),
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        let ft = meta.file_type();

        if rel_prefix.is_empty() && name.eq_ignore_ascii_case(ACCESS_MARKER_FILENAME) {
            if name != ACCESS_MARKER_FILENAME {
                *marker_conflict = Some(format!(
                    "Existing `{name}` at `{}` collides case-insensitively with FolderSync's `{ACCESS_MARKER_FILENAME}` access marker.",
                    path.display()
                ));
                *marker_conflict_path = Some(path);
            } else {
                match validate_marker_file(&path, &meta, collection) {
                    Ok(()) => *has_valid_marker = true,
                    Err(err) => {
                        *marker_conflict = Some(err.cause());
                        *marker_conflict_path = Some(path);
                    }
                }
            }
            continue;
        }

        if ft.is_symlink() {
            if !is_excluded(&rel, exclusions) {
                symlinks.push(rel);
            }
            continue;
        }

        if !ft.is_file() && !ft.is_dir() {
            special_files.push(rel);
            continue;
        }

        if ft.is_dir() && rel_prefix.is_empty() {
            top_subfolders
                .entry(name.clone())
                .or_insert_with(|| SubfolderInventory {
                    name: name.clone(),
                    relative_path: rel.clone(),
                    total_bytes: 0,
                    file_count: 0,
                    has_children: false,
                });
        }

        // Count ALL ordinary entries for side classification & filename validation so
        // preview and actual preflight always agree (§9, HIGH #2).
        all_relative_paths.push(rel.clone());

        let excluded = is_excluded(&rel, exclusions);
        if !excluded && ft.is_file() {
            *file_count += 1;
            *total_bytes = total_bytes.saturating_add(meta.len());
        }

        if let Some(top) = top_subdir
            && let Some(sub) = top_subfolders.get_mut(top)
        {
            if ft.is_dir() {
                sub.has_children = true;
            } else if ft.is_file() {
                sub.file_count += 1;
                sub.total_bytes = sub.total_bytes.saturating_add(meta.len());
            }
        }

        if ft.is_dir() {
            let next_top = if rel_prefix.is_empty() {
                Some(name.as_str())
            } else {
                top_subdir
            };
            walk_detailed(
                &path,
                &rel,
                next_top,
                collection,
                exclusions,
                all_relative_paths,
                symlinks,
                special_files,
                has_valid_marker,
                marker_conflict,
                marker_conflict_path,
                file_count,
                total_bytes,
                top_subfolders,
            )?;
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

    if let Some(prefix) = body.strip_suffix("/**") {
        let prefix = prefix.trim_end_matches('/');
        if anchored {
            return rclone_glob_match(prefix, rel)
                || rclone_glob_match(&format!("{prefix}/**"), rel);
        }
        if rclone_glob_match(prefix, rel) || rclone_glob_match(&format!("{prefix}/**"), rel) {
            return true;
        }
        return rclone_glob_match(&format!("**/{prefix}"), rel)
            || rclone_glob_match(&format!("**/{prefix}/**"), rel);
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
        ti < t.len() && p[pi] == t[ti] && glob_dp(p, t, pi + 1, ti + 1, memo)
    };
    memo[pi][ti] = Some(res);
    res
}

fn is_excluded(rel_path: &str, exclusions: &BTreeSet<String>) -> bool {
    exclusions
        .iter()
        .any(|pat| matches_exclusion_pattern(rel_path, pat))
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

fn read_marker_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .take(MAX_DRIVE_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_drive_marker(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
    entries: &[LsJsonEntry],
) -> Result<Option<Vec<u8>>, ManagerError> {
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
        ));
    }

    let scratch = marker_scratch(drive)?;
    let destination = scratch.path().join(ACCESS_MARKER_FILENAME);
    drive
        .request
        .fetch_marker(&marker_path, &destination, &drive.runner)
        .map_err(|err| drive_error(err.into()))?;
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
        ));
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
    let entries = drive
        .request
        .list_entries(&collection.display_path, true, &drive.runner)
        .map_err(|err| drive_error(err.into()))?;
    let marker = read_drive_marker(drive, collection, &entries)?;
    let paths: Vec<_> = entries
        .iter()
        .filter(|entry| entry.path != ACCESS_MARKER_FILENAME)
        .map(|entry| entry.path.as_str())
        .collect();
    let findings = validate_names(&paths);
    let mut subfolders = BTreeMap::<String, SubfolderInventory>::new();
    let mut file_count = 0;
    let mut total_bytes = 0_u64;
    for entry in entries
        .iter()
        .filter(|entry| entry.path != ACCESS_MARKER_FILENAME)
    {
        if entry.is_dir && !entry.path.contains('/') {
            subfolders
                .entry(entry.path.clone())
                .or_insert_with(|| SubfolderInventory {
                    name: entry.name.clone(),
                    relative_path: entry.path.clone(),
                    total_bytes: 0,
                    file_count: 0,
                    has_children: false,
                });
        }
    }
    for entry in entries
        .iter()
        .filter(|entry| entry.path != ACCESS_MARKER_FILENAME)
    {
        let size = u64::try_from(entry.size);
        if !entry.is_dir && !is_excluded(&entry.path, exclusions) {
            let size = size.map_err(|_| ManagerError::PreflightRejected {
                cause: format!("The Drive file `{}` did not report a size.", entry.path),
                next_step: "Review the file or exclude it before starting first sync.".into(),
            })?;
            file_count += 1;
            total_bytes = total_bytes.saturating_add(size);
        }
        if let Some((top, _)) = entry.path.split_once('/')
            && let Some(subfolder) = subfolders.get_mut(top)
        {
            if entry.is_dir {
                subfolder.has_children = true;
            } else {
                subfolder.file_count += 1;
                subfolder.total_bytes = subfolder
                    .total_bytes
                    .saturating_add(size.unwrap_or_default());
            }
        }
    }
    Ok((
        DetailedEndpointInventory {
            root: PathBuf::from(format!(
                "{}:{}",
                drive.account.remote_name, collection.display_path
            )),
            classification: if paths.is_empty() {
                SideClassification::Empty
            } else {
                SideClassification::HasData
            },
            all_items_count: paths.len(),
            file_count,
            total_bytes,
            has_valid_marker: marker.is_some(),
            marker_conflict: None,
            marker_conflict_path: None,
            subfolders: subfolders.into_values().collect(),
            symlinks: Vec::new(),
            special_files: Vec::new(),
            first_name_finding_next_step: findings.first().map(|finding| finding.next_step.clone()),
            name_findings: findings.into_iter().map(|finding| finding.cause).collect(),
        },
        marker,
    ))
}

pub(crate) fn verify_drive_marker(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
) -> Result<Vec<u8>, ManagerError> {
    let entries = drive
        .request
        .list_entries(&collection.display_path, false, &drive.runner)
        .map_err(|err| drive_error(err.into()))?;
    read_drive_marker(drive, collection, &entries)?.ok_or_else(|| {
        drive_marker_error(
            &collection.display_path,
            "The expected Drive access marker is missing.",
        )
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
