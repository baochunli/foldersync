//! Platform state-directory resolution, private permissions (0700/0600),
//! path-ID/containment validation, exclusive OS file locking (`flock`), and
//! live process identity inspection.

mod volume;

pub(crate) use volume::space_and_writability;
pub use volume::{DiskSpaceInfo, MountedVolume, OsVolumeProbe, VolumeProbe, disk_space_for_path};

use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    io::AsRawFd,
};

use directories::BaseDirs;
use rclone_adapter::{ProcessStartTime, read_process_start_time};

use crate::error::ManagerError;

static ATOMIC_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAX_SAFE_ID_LEN: usize = 128;

/// Validates an identifier used as a single path component (mapping ID, account
/// ID, run ID, device ID): non-empty, <= 128 chars, starts with ASCII
/// alphanumeric, and contains only `[A-Za-z0-9_-]`.
pub fn validate_safe_path_id(field: &str, value: &str) -> Result<(), ManagerError> {
    if value.is_empty() || value.len() > MAX_SAFE_ID_LEN {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "{field} `{value}` must be between 1 and {MAX_SAFE_ID_LEN} characters long."
            ),
            next_step: "Use a short identifier containing only ASCII letters, digits, `-`, or `_`."
                .to_string(),
        });
    }
    let mut chars = value.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !first_ok || !rest_ok {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "{field} `{value}` contains unsupported characters or path syntax."
            ),
            next_step:
                "Start the identifier with an ASCII letter or digit and use only ASCII letters, digits, `-`, or `_` (no slashes, spaces, or `..`)."
                    .to_string(),
        });
    }
    Ok(())
}

/// Resolves `path` to an absolute path by joining `current_dir` when relative
/// and canonicalizing its longest existing ancestor (rejecting empty paths,
/// NUL bytes, and unresolvable ancestors).
pub fn resolve_absolute_path(field: &str, path: &Path) -> Result<PathBuf, ManagerError> {
    if path.as_os_str().is_empty() {
        return Err(ManagerError::Io {
            cause: format!("{field} path cannot be empty."),
            next_step: format!("Provide a valid path for {field}."),
        });
    }
    if path.to_string_lossy().contains('\0') {
        return Err(ManagerError::PreflightRejected {
            cause: format!("{field} path contains a NUL character."),
            next_step: format!("Provide a valid filesystem path for {field}."),
        });
    }

    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| ManagerError::Io {
                cause: format!("Failed to resolve current working directory for {field}: {err}."),
                next_step: format!("Pass an absolute path for {field}."),
            })?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    let mut current = normalized.as_path();
    let mut tail = Vec::new();
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => {
                let mut resolved = fs::canonicalize(current).map_err(|err| ManagerError::Io {
                    cause: format!(
                        "Could not canonicalize existing ancestor `{}` for {field}: {err}.",
                        current.display()
                    ),
                    next_step: format!("Check permissions and symlinks along {field}."),
                })?;
                for part in tail.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(ManagerError::Io {
                        cause: format!(
                            "No existing ancestor directory found for {field} (`{}`).",
                            path.display()
                        ),
                        next_step: format!(
                            "Provide a valid path under an existing directory for {field}."
                        ),
                    });
                };
                tail.push(name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(ManagerError::Io {
                        cause: format!(
                            "No existing ancestor directory found for {field} (`{}`).",
                            path.display()
                        ),
                        next_step: format!(
                            "Provide a valid path under an existing directory for {field}."
                        ),
                    });
                };
                current = parent;
            }
            Err(err) => {
                return Err(ManagerError::Io {
                    cause: format!(
                        "Could not inspect `{}` for {field}: {err}.",
                        current.display()
                    ),
                    next_step: format!("Check filesystem permissions for {field}."),
                });
            }
        }
    }
}

/// Verifies that `derived` resolves strictly inside `root` (never equal to or
/// escaping `root`) before any directory creation or file write.
pub fn ensure_contained_path(
    field: &str,
    root: &Path,
    derived: &Path,
) -> Result<PathBuf, ManagerError> {
    let resolved_root = resolve_absolute_path("root", root)?;
    let resolved_derived = resolve_absolute_path(field, derived)?;
    if resolved_derived == resolved_root || !resolved_derived.starts_with(&resolved_root) {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Derived {field} path `{}` escapes its allowed root `{}`.",
                resolved_derived.display(),
                resolved_root.display()
            ),
            next_step: "Use a safe single-component identifier without path traversal.".to_string(),
        });
    }
    Ok(resolved_derived)
}

/// Verifies that `target` stays inside `root` and that neither `root`, `target`,
/// nor any existing path component under `root` is a symbolic link (`symlink_metadata`).
pub fn reject_symlinks_under_root(
    field: &str,
    root: &Path,
    target: &Path,
) -> Result<PathBuf, ManagerError> {
    let root_meta = fs::symlink_metadata(root).map_err(|err| ManagerError::Io {
        cause: format!(
            "Could not inspect root `{}` for {field}: {err}.",
            root.display()
        ),
        next_step: "Verify the FolderSync directory exists and is accessible.".to_string(),
    })?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Refused symbolic link or non-directory root `{}` for {field}.",
                root.display()
            ),
            next_step: "Use an ordinary directory with no symbolic links.".to_string(),
        });
    }
    let canonical_root = fs::canonicalize(root).map_err(|err| ManagerError::Io {
        cause: format!("Could not resolve state root `{}`: {err}.", root.display()),
        next_step: "Verify the FolderSync state directory exists and is accessible.".to_string(),
    })?;

    // Compute the relative tail beneath `root` without following symlinks beneath `root`.
    // If `target` uses an OS-level ancestor alias for `root` (e.g. macOS `/var` -> `/private/var`),
    // locate the shallowest prefix of `target` that canonicalizes to `canonical_root` so every
    // component beneath `root` is still checked strictly via `symlink_metadata`.
    let rel = if let Ok(r) = target.strip_prefix(&canonical_root) {
        r.to_path_buf()
    } else if let Ok(r) = target.strip_prefix(root) {
        r.to_path_buf()
    } else {
        let mut prefix = PathBuf::new();
        let mut comps = target.components();
        let mut matched_tail = None;
        while let Some(comp) = comps.next() {
            prefix.push(comp.as_os_str());
            if fs::canonicalize(&prefix).ok().as_deref() == Some(canonical_root.as_path()) {
                matched_tail = Some(comps.as_path().to_path_buf());
                break;
            }
        }
        matched_tail.ok_or_else(|| ManagerError::PreflightRejected {
            cause: format!(
                "{field} path `{}` is outside allowed root `{}`.",
                target.display(),
                canonical_root.display()
            ),
            next_step: "Keep all state files strictly inside the FolderSync state directory."
                .to_string(),
        })?
    };

    let mut cur = canonical_root.clone();
    let mut has_component = false;
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                has_component = true;
                cur.push(part);
                match fs::symlink_metadata(&cur) {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() {
                            return Err(ManagerError::PreflightRejected {
                                cause: format!(
                                    "Refused to follow symbolic link `{}` inside state root `{}` for {field}.",
                                    cur.display(),
                                    canonical_root.display()
                                ),
                                next_step:
                                    "Remove the unexpected symbolic link inside FolderSync's state directory."
                                        .to_string(),
                            });
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(ManagerError::Io {
                            cause: format!(
                                "Failed to inspect `{}` for {field}: {err}.",
                                cur.display()
                            ),
                            next_step: "Verify permissions inside the FolderSync state directory."
                                .to_string(),
                        });
                    }
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "{field} path `{}` contains invalid traversal components.",
                        target.display()
                    ),
                    next_step: "Use a safe path strictly inside the FolderSync state directory."
                        .to_string(),
                });
            }
        }
    }

    if !has_component {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "{field} path `{}` must be strictly inside `{}`.",
                target.display(),
                canonical_root.display()
            ),
            next_step: "Provide a child path inside the state directory.".to_string(),
        });
    }

    Ok(cur)
}

/// Creates directory `target` strictly inside `root` with `0700` permissions,
/// refusing any symbolic link or non-directory component under `root`.
pub fn ensure_private_dir_under(
    field: &str,
    root: &Path,
    target: &Path,
) -> Result<PathBuf, ManagerError> {
    let checked = reject_symlinks_under_root(field, root, target)?;
    let canonical_root = fs::canonicalize(root).map_err(|err| ManagerError::Io {
        cause: format!("Could not resolve state root `{}`: {err}.", root.display()),
        next_step: "Verify the FolderSync state directory exists.".to_string(),
    })?;
    let rel =
        checked
            .strip_prefix(&canonical_root)
            .map_err(|_| ManagerError::PreflightRejected {
                cause: format!("{field} escaped state root `{}`.", canonical_root.display()),
                next_step: "Keep state directories inside the FolderSync state directory."
                    .to_string(),
            })?;

    let mut cur = canonical_root;
    for comp in rel.components() {
        if let Component::Normal(part) = comp {
            cur.push(part);
            match fs::symlink_metadata(&cur) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        return Err(ManagerError::PreflightRejected {
                            cause: format!(
                                "Path `{}` inside state root is not an ordinary directory.",
                                cur.display()
                            ),
                            next_step:
                                "Remove the unexpected file or symlink inside the state directory."
                                    .to_string(),
                        });
                    }
                    #[cfg(unix)]
                    fs::set_permissions(&cur, fs::Permissions::from_mode(0o700)).map_err(
                        |err| ManagerError::Io {
                            cause: format!(
                                "Failed to set private 0700 permissions on `{}`: {err}.",
                                cur.display()
                            ),
                            next_step: "Verify ownership of the state directory.".to_string(),
                        },
                    )?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let create_res = {
                        #[cfg(unix)]
                        {
                            fs::DirBuilder::new().mode(0o700).create(&cur)
                        }
                        #[cfg(not(unix))]
                        {
                            fs::create_dir(&cur)
                        }
                    };
                    match create_res {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                            let meta =
                                fs::symlink_metadata(&cur).map_err(|merr| ManagerError::Io {
                                    cause: format!(
                                        "Failed to inspect `{}`: {merr}.",
                                        cur.display()
                                    ),
                                    next_step: "Check permissions inside the state directory."
                                        .to_string(),
                                })?;
                            if meta.file_type().is_symlink() || !meta.is_dir() {
                                return Err(ManagerError::PreflightRejected {
                                    cause: format!(
                                        "Path `{}` inside state root is not an ordinary directory.",
                                        cur.display()
                                    ),
                                    next_step:
                                        "Remove the unexpected file or symlink inside the state directory."
                                            .to_string(),
                                });
                            }
                        }
                        Err(err) => {
                            return Err(ManagerError::Io {
                                cause: format!(
                                    "Failed to create private directory `{}`: {err}.",
                                    cur.display()
                                ),
                                next_step: "Confirm the state directory is writable.".to_string(),
                            });
                        }
                    }
                    #[cfg(unix)]
                    fs::set_permissions(&cur, fs::Permissions::from_mode(0o700)).map_err(
                        |err| ManagerError::Io {
                            cause: format!(
                                "Failed to set 0700 permissions on `{}`: {err}.",
                                cur.display()
                            ),
                            next_step: "Verify ownership of the state directory.".to_string(),
                        },
                    )?;
                }
                Err(err) => {
                    return Err(ManagerError::Io {
                        cause: format!("Failed to inspect `{}`: {err}.", cur.display()),
                        next_step: "Check permissions inside the state directory.".to_string(),
                    });
                }
            }
        }
    }

    Ok(checked)
}

/// Opens a private `0600` file strictly inside `root` for writing, rejecting
/// symlinks in any component and using `O_NOFOLLOW` on Unix.
pub fn open_private_write_file_under(
    field: &str,
    root: &Path,
    target: &Path,
) -> Result<File, ManagerError> {
    let checked = reject_symlinks_under_root(field, root, target)?;
    if let Some(parent) = checked.parent()
        && parent != root
        && fs::canonicalize(root).ok().as_deref() != Some(parent)
    {
        ensure_private_dir_under(field, root, parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(&checked).map_err(|err| {
        #[cfg(unix)]
        if err.raw_os_error() == Some(libc::ELOOP) {
            return ManagerError::PreflightRejected {
                cause: format!(
                    "Refused to open symlinked file `{}` for {field}.",
                    checked.display()
                ),
                next_step: "Remove the unexpected symbolic link inside the state directory."
                    .to_string(),
            };
        }
        ManagerError::Io {
            cause: format!("Failed to open `{}` for writing: {err}.", checked.display()),
            next_step: "Check available disk space and permissions in the state directory."
                .to_string(),
        }
    })?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to set private 0600 permissions on `{}`: {err}.",
                checked.display()
            ),
            next_step: "Verify ownership inside the state directory.".to_string(),
        })?;
    Ok(file)
}

/// Writes `bytes` to `target` strictly inside `root`, rejecting symlinks (`O_NOFOLLOW`).
pub fn write_private_file_under(
    field: &str,
    root: &Path,
    target: &Path,
    bytes: &[u8],
) -> Result<(), ManagerError> {
    let mut file = open_private_write_file_under(field, root, target)?;
    file.write_all(bytes).map_err(|err| ManagerError::Io {
        cause: format!("Failed to write `{}`: {err}.", target.display()),
        next_step: "Check available disk space and retry.".to_string(),
    })?;
    file.sync_all().map_err(|err| ManagerError::Io {
        cause: format!("Failed to flush `{}` to disk: {err}.", target.display()),
        next_step: "Check disk health and retry.".to_string(),
    })?;
    Ok(())
}

/// Atomically writes `bytes` to `target` strictly inside `root` (`0600`, `O_NOFOLLOW`,
/// `sync_all`, rename), refusing symlinks on any component under `root`.
pub fn write_private_file_atomic_under(
    field: &str,
    root: &Path,
    target: &Path,
    bytes: &[u8],
) -> Result<(), ManagerError> {
    let checked = reject_symlinks_under_root(field, root, target)?;
    let parent = checked.parent().ok_or_else(|| ManagerError::Io {
        cause: format!(
            "Target path `{}` has no parent directory.",
            checked.display()
        ),
        next_step: "Specify a valid path inside the state directory.".to_string(),
    })?;
    if fs::canonicalize(root).ok().as_deref() != Some(parent) {
        ensure_private_dir_under(field, root, parent)?;
    }

    let file_name = checked
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let seq = ATOMIC_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let write_res = (|| -> Result<(), ManagerError> {
        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = opts.open(&tmp_path).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to create temporary file `{}`: {err}.",
                tmp_path.display()
            ),
            next_step: "Verify directory permissions and free disk space.".to_string(),
        })?;
        file.write_all(bytes).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to write temporary file `{}`: {err}.",
                tmp_path.display()
            ),
            next_step: "Verify free disk space and retry.".to_string(),
        })?;
        file.sync_all().map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to sync temporary file `{}`: {err}.",
                tmp_path.display()
            ),
            next_step: "Check disk health and retry.".to_string(),
        })?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to set 0600 permissions on `{}`: {err}.",
                    tmp_path.display()
                ),
                next_step: "Verify ownership inside the state directory.".to_string(),
            })?;
        drop(file);
        // Re-check target leaf is not a symlink before rename.
        reject_symlinks_under_root(field, root, &checked)?;
        fs::rename(&tmp_path, &checked).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to atomically replace `{}`: {err}.",
                checked.display()
            ),
            next_step: "Confirm no conflicting file lock blocks renaming.".to_string(),
        })?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

/// Resolves the FolderSync state directory:
/// explicit override -> `FOLDERSYNC_STATE_DIR` -> platform application data/state directory.
/// Always resolves overrides to a canonical absolute path before any directory creation.
pub fn resolve_state_dir(explicit: Option<&Path>) -> Result<PathBuf, ManagerError> {
    if let Some(dir) = explicit {
        return resolve_absolute_path("state_dir", dir);
    }

    if let Some(env_dir) = env::var_os("FOLDERSYNC_STATE_DIR")
        && !env_dir.is_empty()
    {
        return resolve_absolute_path("FOLDERSYNC_STATE_DIR", Path::new(&env_dir));
    }

    let base = BaseDirs::new().ok_or_else(|| ManagerError::Io {
        cause: "Could not resolve the platform application support directory.".to_string(),
        next_step: "Set FOLDERSYNC_STATE_DIR to an explicit writable directory.".to_string(),
    })?;

    #[cfg(target_os = "macos")]
    {
        Ok(base.data_dir().join("FolderSync"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(base
            .state_dir()
            .unwrap_or_else(|| base.data_local_dir())
            .join("FolderSync"))
    }
}

/// Ensures `path` exists with private `0700` permissions on Unix.
pub fn ensure_private_dir(path: &Path) -> Result<(), ManagerError> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to create private directory `{}`: {err}.",
                    path.display()
                ),
                next_step: "Confirm the parent directory is writable by your user account."
                    .to_string(),
            })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|err| {
            ManagerError::Io {
                cause: format!(
                    "Failed to set private 0700 permissions on `{}`: {err}.",
                    path.display()
                ),
                next_step: "Verify ownership and filesystem permissions on the state directory."
                    .to_string(),
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).map_err(|err| ManagerError::Io {
            cause: format!("Failed to create directory `{}`: {err}.", path.display()),
            next_step: "Confirm the parent directory is writable.".to_string(),
        })?;
    }
    Ok(())
}

/// Ensures `path` has private `0600` file permissions on Unix.
pub fn set_private_file_permissions(path: &Path) -> Result<(), ManagerError> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            ManagerError::Io {
                cause: format!(
                    "Failed to set private 0600 permissions on `{}`: {err}.",
                    path.display()
                ),
                next_step: "Verify ownership and permissions inside the state directory."
                    .to_string(),
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Creates or truncates a private `0600` file and returns the open handle.
pub fn open_private_write_file(path: &Path) -> Result<File, ManagerError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let file = opts.open(path).map_err(|err| ManagerError::Io {
        cause: format!("Failed to open `{}` for writing: {err}.", path.display()),
        next_step: "Check available disk space and permissions in the state directory.".to_string(),
    })?;
    set_private_file_permissions(path)?;
    Ok(file)
}

/// Writes `bytes` to `path` with private `0600` permissions on Unix.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), ManagerError> {
    let mut file = open_private_write_file(path)?;
    file.write_all(bytes).map_err(|err| ManagerError::Io {
        cause: format!("Failed to write `{}`: {err}.", path.display()),
        next_step: "Check available disk space and retry.".to_string(),
    })?;
    file.sync_all().map_err(|err| ManagerError::Io {
        cause: format!("Failed to flush `{}` to disk: {err}.", path.display()),
        next_step: "Check disk health and retry.".to_string(),
    })?;
    set_private_file_permissions(path)?;
    Ok(())
}

/// Atomically writes `bytes` to `path` via a sibling temp file (`0600`, `sync_all`, rename).
pub fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), ManagerError> {
    let parent = path.parent().ok_or_else(|| ManagerError::Io {
        cause: format!("Target path `{}` has no parent directory.", path.display()),
        next_step: "Specify a valid file path inside an existing directory.".to_string(),
    })?;
    ensure_private_dir(parent)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("status");
    let seq = ATOMIC_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let write_res = (|| -> Result<(), ManagerError> {
        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to create temporary file `{}`: {err}.",
                tmp_path.display()
            ),
            next_step: "Verify directory permissions and free disk space.".to_string(),
        })?;
        file.write_all(bytes).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to write temporary file `{}`: {err}.",
                tmp_path.display()
            ),
            next_step: "Verify free disk space and retry.".to_string(),
        })?;
        file.sync_all().map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to sync temporary file `{}`: {err}.",
                tmp_path.display()
            ),
            next_step: "Check disk health and retry.".to_string(),
        })?;
        drop(file);
        set_private_file_permissions(&tmp_path)?;
        fs::rename(&tmp_path, path).map_err(|err| ManagerError::Io {
            cause: format!("Failed to atomically replace `{}`: {err}.", path.display()),
            next_step: "Confirm no conflicting file lock blocks renaming inside the directory."
                .to_string(),
        })?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

/// Exclusive OS file lock held for the lifetime of a `SyncManager` instance.
#[derive(Debug)]
pub struct StateLock {
    file: File,
    lock_path: PathBuf,
}

impl StateLock {
    /// Acquires a non-blocking exclusive OS lock (`flock`) inside `state_dir`,
    /// refusing symlinks on `state_dir` and `manager.lock` (`O_NOFOLLOW`).
    pub fn acquire(state_dir: &Path) -> Result<Self, ManagerError> {
        if let Ok(meta) = fs::symlink_metadata(state_dir)
            && meta.file_type().is_symlink()
        {
            return Err(ManagerError::PreflightRejected {
                cause: format!(
                    "Refused to follow symbolic link at state directory `{}`.",
                    state_dir.display()
                ),
                next_step: "Point FolderSync at a real local state directory, not a symbolic link."
                    .to_string(),
            });
        }
        ensure_private_dir(state_dir)?;
        let lock_path =
            reject_symlinks_under_root("manager_lock", state_dir, &state_dir.join("manager.lock"))?;

        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = opts.open(&lock_path).map_err(|err| {
            #[cfg(unix)]
            if err.raw_os_error() == Some(libc::ELOOP) {
                return ManagerError::PreflightRejected {
                    cause: format!(
                        "Refused to follow symbolic link at state lock `{}`.",
                        lock_path.display()
                    ),
                    next_step: "Remove the unexpected symbolic link inside the state directory."
                        .to_string(),
                };
            }
            ManagerError::Io {
                cause: format!(
                    "Failed to open state lock file `{}`: {err}.",
                    lock_path.display()
                ),
                next_step: "Verify permissions on the FolderSync state directory.".to_string(),
            }
        })?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to set private 0600 permissions on `{}`: {err}.",
                    lock_path.display()
                ),
                next_step: "Verify ownership inside the state directory.".to_string(),
            })?;

        #[cfg(unix)]
        {
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                let raw = err.raw_os_error();
                if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
                    return Err(ManagerError::StateLocked {
                        lock_path: lock_path.clone(),
                        cause: format!(
                            "Another FolderSync instance already holds the state lock at `{}`.",
                            lock_path.display()
                        ),
                        next_step:
                            "Quit the running FolderSync app or CLI command before starting another instance."
                                .to_string(),
                    });
                }
                return Err(ManagerError::Io {
                    cause: format!(
                        "Failed to acquire exclusive OS lock on `{}`: {err}.",
                        lock_path.display()
                    ),
                    next_step: "Verify the state directory resides on a local filesystem supporting file locks."
                        .to_string(),
                });
            }
        }

        // Write diagnostic PID after acquiring the OS flock (ownership is enforced by flock, never PID existence).
        let _ = file.set_len(0);
        let _ = writeln!(file, "pid={}", std::process::id());
        let _ = file.sync_all();

        Ok(Self { file, lock_path })
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Live OS process identity observed during startup reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveProcessInfo {
    pub pid: u32,
    pub start_time: Option<ProcessStartTime>,
    pub executable: Option<PathBuf>,
}

/// Result of inspecting a recorded PID against the OS process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessProbeResult {
    NotFound,
    Live(LiveProcessInfo),
    Unavailable(String),
}

/// Inspects live OS processes by PID and workdir reference for orphan/PID-reuse
/// reconciliation (§14).
pub trait ProcessInspector: Send + Sync {
    fn inspect(&self, pid: u32) -> ProcessProbeResult;

    /// Read-only best-effort investigation of surviving processes whose command
    /// line references `workdir` (used for missing-PID crash diagnostics, §14).
    /// Returns `None` when process-table scanning is unavailable.
    fn find_processes_referencing_workdir(&self, _workdir: &Path) -> Option<Vec<u32>> {
        None
    }
}

/// Real OS process inspector using `proc_pidinfo`/`proc_pidpath` on macOS and
/// `/proc/<pid>` on Linux.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsProcessInspector;

#[cfg(target_os = "macos")]
fn read_process_executable(pid: u32) -> Option<PathBuf> {
    let mut buf = vec![0u8; 4096];
    let ret = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len() as u32,
        )
    };
    if ret > 0 {
        let len = (ret as usize).min(buf.len());
        let s = String::from_utf8_lossy(&buf[..len]);
        let trimmed = s.trim_end_matches('\0');
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_process_executable(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

impl ProcessInspector for OsProcessInspector {
    fn inspect(&self, pid: u32) -> ProcessProbeResult {
        #[cfg(unix)]
        {
            let kill_res = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if kill_res != 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::ESRCH) => return ProcessProbeResult::NotFound,
                    Some(libc::EPERM) => {
                        return ProcessProbeResult::Unavailable(format!(
                            "Permission denied inspecting pid {pid}"
                        ));
                    }
                    _ => return ProcessProbeResult::Unavailable(err.to_string()),
                }
            }
            let start_time = read_process_start_time(pid);
            let executable = read_process_executable(pid);
            ProcessProbeResult::Live(LiveProcessInfo {
                pid,
                start_time,
                executable,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            ProcessProbeResult::Unavailable(
                "Process inspection is not implemented on this platform".to_string(),
            )
        }
    }

    fn find_processes_referencing_workdir(&self, workdir: &Path) -> Option<Vec<u32>> {
        let needle = workdir.to_string_lossy();
        if needle.is_empty() {
            return None;
        }
        let self_pid = std::process::id();

        #[cfg(target_os = "linux")]
        {
            let mut matches = Vec::new();
            let entries = fs::read_dir("/proc").ok()?;
            for entry in entries.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                if pid == self_pid {
                    continue;
                }
                if let Ok(raw) = fs::read(entry.path().join("cmdline")) {
                    let cmdline = String::from_utf8_lossy(&raw);
                    if cmdline.contains(needle.as_ref()) {
                        matches.push(pid);
                    }
                }
            }
            Some(matches)
        }

        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("/bin/ps")
                .args(["-axo", "pid=,args="])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let mut matches = Vec::new();
            for line in text.lines() {
                let trimmed = line.trim_start();
                let Some((pid_str, args)) = trimmed.split_once(char::is_whitespace) else {
                    continue;
                };
                let Ok(pid) = pid_str.parse::<u32>() else {
                    continue;
                };
                if pid != self_pid && args.contains(needle.as_ref()) {
                    matches.push(pid);
                }
            }
            Some(matches)
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = self_pid;
            None
        }
    }
}

/// Deterministic test double for [`ProcessInspector`].
#[derive(Debug, Default)]
pub struct FakeProcessInspector {
    processes: Mutex<HashMap<u32, ProcessProbeResult>>,
    workdir_matches: Mutex<HashMap<PathBuf, Vec<u32>>>,
}

impl FakeProcessInspector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_process(&self, pid: u32, result: ProcessProbeResult) {
        self.processes.lock().unwrap().insert(pid, result);
    }

    pub fn set_workdir_processes(&self, workdir: impl Into<PathBuf>, pids: Vec<u32>) {
        self.workdir_matches
            .lock()
            .unwrap()
            .insert(workdir.into(), pids);
    }
}

impl ProcessInspector for FakeProcessInspector {
    fn inspect(&self, pid: u32) -> ProcessProbeResult {
        self.processes
            .lock()
            .unwrap()
            .get(&pid)
            .cloned()
            .unwrap_or(ProcessProbeResult::NotFound)
    }

    fn find_processes_referencing_workdir(&self, workdir: &Path) -> Option<Vec<u32>> {
        self.workdir_matches.lock().unwrap().get(workdir).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_process_inspector_reads_current_process_identity() {
        let inspector = OsProcessInspector;
        let pid = std::process::id();
        let ProcessProbeResult::Live(info) = inspector.inspect(pid) else {
            panic!("current process should be reported Live");
        };
        assert_eq!(info.pid, pid);
        assert!(
            info.start_time.is_some(),
            "OS inspector should report start time for current process"
        );
        assert!(
            info.executable.is_some(),
            "OS inspector should report executable path for current process"
        );
    }

    #[test]
    fn validates_safe_path_ids() {
        assert!(validate_safe_path_id("mapping_id", "research-1_a").is_ok());
        assert!(validate_safe_path_id("mapping_id", "../escape").is_err());
        assert!(validate_safe_path_id("mapping_id", "/abs").is_err());
        assert!(validate_safe_path_id("mapping_id", "a/b").is_err());
        assert!(validate_safe_path_id("mapping_id", "-dash").is_err());
        assert!(validate_safe_path_id("mapping_id", "").is_err());
    }
}
