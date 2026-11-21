//! Initialization preflight inspection, side classification, filter file
//! rendering, and access marker handling (§9, §11, §18).

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use rclone_adapter::InitPreference;
use sync_core::{ACCESS_MARKER_FILENAME, AccessMarker, CloudCollection, MappingId, validate_names};

use crate::error::ManagerError;

/// Classification of one endpoint during §9 initialization preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideClassification {
    /// Directory contains no user items (a recognized matching `RCLONE_TEST` marker does not count).
    Empty,
    /// Directory contains one or more user items.
    HasData,
}

/// Preflight summary of a local or local-fixture endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInventory {
    pub root: PathBuf,
    pub classification: SideClassification,
    pub item_count: usize,
    pub has_valid_marker: bool,
}

/// Simple rclone-compatible exclusion matcher for onboarding symlink checks.
///
/// Supports exact paths/basenames (`link.txt`, `sub/link.txt`), subtree globs
/// (`dir/**`), and `*` wildcards (`._*`, `*.tmp`, `sub/*.link`).
pub fn matches_exclusion_pattern(rel_path: &str, pattern: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    if let Some(prefix) = pat.strip_suffix("/**") {
        let prefix = prefix.trim_end_matches('/');
        if rel_path == prefix || rel_path.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    if pat.contains('/') {
        wildcard_match(pat, rel_path)
    } else {
        let basename = rel_path.rsplit('/').next().unwrap_or(rel_path);
        wildcard_match(pat, rel_path) || wildcard_match(pat, basename)
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(s) = star_idx {
            pi = s + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn is_excluded(rel_path: &str, exclusions: &BTreeSet<String>) -> bool {
    exclusions
        .iter()
        .any(|pat| matches_exclusion_pattern(rel_path, pat))
}

/// Inspects a local directory tree without following symlinks, validates
/// filenames via `sync_core::validate_names` (preserving literal backslashes),
/// rejects special files (FIFOs, sockets, devices), honors explicit exclusions
/// for symlinks, detects root-level `RCLONE_TEST` case-insensitively without
/// overwriting unrecognized data, and classifies the directory as empty or populated.
pub fn inspect_local_endpoint(
    root: &Path,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
) -> Result<EndpointInventory, ManagerError> {
    let meta = fs::symlink_metadata(root).map_err(|err| ManagerError::Io {
        cause: format!(
            "Endpoint directory `{}` does not exist or cannot be inspected: {err}.",
            root.display()
        ),
        next_step:
            "Confirm the directory exists and is accessible (FolderSync never auto-creates missing roots)."
                .to_string(),
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Endpoint `{}` must be an ordinary directory, not a file or symbolic link.",
                root.display()
            ),
            next_step: "Choose an ordinary directory for this mapping.".to_string(),
        });
    }

    let mut relative_paths = Vec::new();
    let mut symlinks_found = Vec::new();
    let mut special_files_found = Vec::new();
    let mut has_valid_marker = false;

    walk_endpoint(
        root,
        "",
        collection,
        exclusions,
        &mut relative_paths,
        &mut symlinks_found,
        &mut special_files_found,
        &mut has_valid_marker,
    )?;

    if !symlinks_found.is_empty() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Endpoint `{}` contains unsupported symbolic link(s): {}.",
                root.display(),
                symlinks_found.join(", ")
            ),
            next_step:
                "Remove the symbolic links or add them to the mapping's exclusions; FolderSync does not traverse symlinks."
                    .to_string(),
        });
    }

    if !special_files_found.is_empty() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Endpoint `{}` contains unsupported special file(s) (FIFO, socket, or device): {}.",
                root.display(),
                special_files_found.join(", ")
            ),
            next_step: "Remove or relocate the special files outside the synchronized folder."
                .to_string(),
        });
    }

    let name_findings = validate_names(&relative_paths);
    if let Some(first) = name_findings.first() {
        return Err(ManagerError::PreflightRejected {
            cause: first.cause.clone(),
            next_step: first.next_step.clone(),
        });
    }

    let item_count = relative_paths.len();
    let classification = if item_count == 0 {
        SideClassification::Empty
    } else {
        SideClassification::HasData
    };

    Ok(EndpointInventory {
        root: root.to_path_buf(),
        classification,
        item_count,
        has_valid_marker,
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_endpoint(
    current: &Path,
    rel_prefix: &str,
    collection: &CloudCollection,
    exclusions: &BTreeSet<String>,
    relative_paths: &mut Vec<String>,
    symlinks_found: &mut Vec<String>,
    special_files_found: &mut Vec<String>,
    has_valid_marker: &mut bool,
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

        // Build `/`-separated relative paths from raw component names WITHOUT replacing
        // literal backslashes so `sync_core::validate_names` catches `\` as reserved (§18).
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        let ft = meta.file_type();

        // Check root-level RCLONE_TEST access marker case-insensitively (§9, §11).
        if rel_prefix.is_empty() && name.eq_ignore_ascii_case(ACCESS_MARKER_FILENAME) {
            if name != ACCESS_MARKER_FILENAME {
                return Err(ManagerError::AccessMarkerConflict {
                    path: path.clone(),
                    cause: format!(
                        "Existing `{name}` at `{}` collides case-insensitively with FolderSync's `{ACCESS_MARKER_FILENAME}` access marker.",
                        path.display()
                    ),
                    next_step:
                        "Rename or move the existing item before initializing so FolderSync never overwrites or truncates user data."
                            .to_string(),
                });
            }
            validate_marker_file(&path, &meta, collection)?;
            *has_valid_marker = true;
            continue;
        }

        if ft.is_symlink() {
            if !is_excluded(&rel, exclusions) {
                symlinks_found.push(rel);
            }
            continue;
        }

        if !ft.is_file() && !ft.is_dir() {
            special_files_found.push(rel);
            continue;
        }

        relative_paths.push(rel.clone());
        if ft.is_dir() {
            walk_endpoint(
                &path,
                &rel,
                collection,
                exclusions,
                relative_paths,
                symlinks_found,
                special_files_found,
                has_valid_marker,
            )?;
        }
    }

    Ok(())
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
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Requested initial direction `{expected:?}` conflicts with observed folder contents (detected `{detected:?}`)."
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
