//! Initialization preflight inspection, side classification, filter file
//! rendering, and access marker handling (§9, §11, §18).

use std::{
    collections::BTreeSet,
    fs,
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

/// Inspects a local directory tree without following symlinks, validates
/// filenames via `sync_core::validate_names`, validates any existing
/// `RCLONE_TEST` marker without overwriting unrecognized data, and classifies
/// the directory as empty or populated.
pub fn inspect_local_endpoint(
    root: &Path,
    collection: &CloudCollection,
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
    let mut has_valid_marker = false;

    walk_endpoint(
        root,
        root,
        collection,
        &mut relative_paths,
        &mut symlinks_found,
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
                "Remove the symbolic links or exclude them explicitly; FolderSync does not traverse symlinks."
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

fn walk_endpoint(
    base: &Path,
    current: &Path,
    collection: &CloudCollection,
    relative_paths: &mut Vec<String>,
    symlinks_found: &mut Vec<String>,
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
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if meta.file_type().is_symlink() {
            symlinks_found.push(rel);
            continue;
        }

        // Check root-level RCLONE_TEST access marker (§9, §11).
        if current == base && rel == ACCESS_MARKER_FILENAME {
            if !meta.is_file() {
                return Err(ManagerError::AccessMarkerConflict {
                    path: path.clone(),
                    cause: format!(
                        "`{}` exists at `{}` and is not a FolderSync access marker file.",
                        ACCESS_MARKER_FILENAME,
                        path.display()
                    ),
                    next_step:
                        "Rename or move the existing item before initializing so FolderSync never overwrites user data."
                            .to_string(),
                });
            }
            let content =
                fs::read_to_string(&path).map_err(|err| ManagerError::AccessMarkerConflict {
                    path: path.clone(),
                    cause: format!(
                        "Existing `{}` at `{}` could not be read as a FolderSync marker: {err}.",
                        ACCESS_MARKER_FILENAME,
                        path.display()
                    ),
                    next_step:
                        "Inspect and move or rename the unrecognized file before initializing."
                            .to_string(),
                })?;
            let parsed =
                AccessMarker::parse(&content).map_err(|err| ManagerError::AccessMarkerConflict {
                    path: path.clone(),
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
                    path: path.clone(),
                    cause: format!(
                        "Access marker at `{}` belongs to collection `{}` (`{}`), not `{}` (`{}`).",
                        path.display(),
                        parsed.collection_display_path,
                        parsed.collection_folder_id,
                        collection.display_path,
                        collection.folder_id
                    ),
                    next_step:
                        "Confirm you selected the right local folder and cloud collection before initializing."
                            .to_string(),
                });
            }
            *has_valid_marker = true;
            continue;
        }

        relative_paths.push(rel);
        if meta.is_dir() {
            walk_endpoint(
                base,
                &path,
                collection,
                relative_paths,
                symlinks_found,
                has_valid_marker,
            )?;
        }
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

/// Establishes or verifies matching FolderSync `RCLONE_TEST` access markers on
/// both endpoints during explicit initialization (§9 step 5, §11).
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
            fs::write(&local_marker_path, &remote_bytes).map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to create matching local access marker `{}`: {err}.",
                    local_marker_path.display()
                ),
                next_step: "Verify write permissions on the local folder and retry.".to_string(),
            })?;
        }
        return Ok(());
    }

    // New collection or upload-init case: establish canonical marker on whichever side lacks it.
    if !local_inv.has_valid_marker {
        fs::write(&local_marker_path, canonical_marker.as_bytes()).map_err(|err| {
            ManagerError::Io {
                cause: format!(
                    "Failed to write local access marker `{}`: {err}.",
                    local_marker_path.display()
                ),
                next_step: "Verify write permissions on the local folder and retry.".to_string(),
            }
        })?;
    }
    if !remote_inv.has_valid_marker {
        fs::write(&remote_marker_path, canonical_marker.as_bytes()).map_err(|err| {
            ManagerError::Io {
                cause: format!(
                    "Failed to write remote access marker `{}`: {err}.",
                    remote_marker_path.display()
                ),
                next_step: "Verify write permissions on the remote destination and retry."
                    .to_string(),
            }
        })?;
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
