//! Read-only archive planning for Review and the worker.

use std::{
    collections::BTreeSet,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use sync_core::{
    CloudCollection,
    archive::{ArchiveFile, archive_suggestion},
    validate_names,
};

use crate::{
    RemoteEndpoint, accounts::LockedDrive, archive_transaction::check_archive_paths,
    error::ManagerError, init::archive_excluded, manager::drive_error,
};

pub(crate) fn collection_name(collection: &CloudCollection) -> &str {
    collection
        .display_path
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

pub(crate) fn archive_rejected(cause: impl Into<String>) -> ManagerError {
    ManagerError::PreflightRejected {
        cause: cause.into(),
        next_step:
            "Resolve the issue or remove this archive selection, then review first sync again."
                .into(),
    }
}

pub(crate) fn remote_names(
    remote: &RemoteEndpoint,
    drive: Option<&LockedDrive<'_>>,
    collection: &CloudCollection,
) -> Result<Vec<String>, ManagerError> {
    match remote {
        RemoteEndpoint::LocalDir(root) => local_names(root)
            .map_err(|err| archive_rejected(format!("Could not inspect the destination: {err}."))),
        RemoteEndpoint::DriveRemote { .. } => {
            let drive =
                drive.ok_or_else(|| archive_rejected("The Drive account is unavailable."))?;
            drive
                .request
                .list_sibling_names(&collection.display_path, &drive.runner)
                .map_err(|error| drive_error(error.into()))
        }
        RemoteEndpoint::OtherRemote { .. } => Err(archive_rejected(
            "This destination does not support first sync.",
        )),
    }
}

fn local_names(root: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect()
}

fn check_zip_name(zip: &str, names: &[String], folder: &str) -> Result<(), ManagerError> {
    for name in names {
        // Ignore standalone compatibility findings about existing names. Check
        // portable collisions even when the current volume distinguishes them.
        if validate_names(&[name.as_str(), zip])
            .iter()
            .any(|finding| finding.paths.len() > 1)
        {
            return Err(archive_rejected(format!(
                "`{zip}` conflicts with `{name}` in {folder}."
            )));
        }
    }
    Ok(())
}

pub(crate) fn check_archive_selection(
    root: &Path,
    volume: &Path,
    collection: &CloudCollection,
    folder: &str,
    exclusions: &BTreeSet<String>,
    remote_names: &[String],
    cached_local_names: Option<&[String]>,
) -> Result<PathBuf, ManagerError> {
    // The ordinary preview models FolderSync's simple path globs. Advanced
    // rclone syntax is rejected here so an approximate match never decides
    // which files to include in an archive.
    if let Some(pattern) = exclusions
        .iter()
        .find(|pattern| pattern.contains(['[', ']', '{', '}', '\\']) || pattern.contains("***"))
    {
        return Err(archive_rejected(format!(
            "Archive creation cannot verify the exclusion pattern `{pattern}`."
        )));
    }
    let holding_path = check_archive_paths(root, volume, collection_name(collection), folder)?;
    let zip = format!("{folder}.zip");
    if archive_excluded(&zip, exclusions) {
        return Err(archive_rejected(format!(
            "`{zip}` matches a mapping exclusion and would not sync."
        )));
    }
    if !validate_names(&[&zip]).is_empty() {
        return Err(archive_rejected(format!(
            "The archive name `{zip}` is not supported."
        )));
    }
    if let Some(names) = cached_local_names {
        check_zip_name(&zip, names, "the Mac folder")?;
    } else {
        let names = local_names(root)
            .map_err(|err| archive_rejected(format!("Could not inspect the Mac folder: {err}.")))?;
        check_zip_name(&zip, &names, "the Mac folder")?;
    }
    check_zip_name(&zip, remote_names, "the destination folder")?;
    Ok(holding_path)
}

#[derive(PartialEq, Eq)]
pub(crate) struct ArchiveInputs {
    pub(crate) files: Vec<ArchiveFile>,
    pub(crate) original_bytes: u64,
}

pub(crate) fn archive_inputs(
    root: &Path,
    folder: &str,
    exclusions: &BTreeSet<String>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ArchiveInputs, ManagerError> {
    fn walk(
        root: &Path,
        device: u64,
        relative: &str,
        exclusions: &BTreeSet<String>,
        cancelled: &dyn Fn() -> bool,
        result: &mut ArchiveInputs,
    ) -> io::Result<()> {
        if cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let path = root.join(relative);
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("`{relative}` is no longer an ordinary folder"),
            ));
        }
        for entry in fs::read_dir(path)? {
            if cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "A filename is not valid UTF-8")
            })?;
            let relative = format!("{relative}/{name}");
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.dev() != device {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    format!("`{relative}` crosses onto another volume"),
                ));
            }
            if metadata.is_dir() {
                walk(root, device, &relative, exclusions, cancelled, result)?;
            } else if metadata.is_file() {
                result.original_bytes = result.original_bytes.saturating_add(metadata.len());
                if !archive_excluded(&relative, exclusions) {
                    result
                        .files
                        .push(ArchiveFile::from_metadata(relative, &metadata)?);
                }
            } else if !metadata.file_type().is_symlink() || !archive_excluded(&relative, exclusions)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("`{relative}` is a symbolic link or special file"),
                ));
            }
        }
        Ok(())
    }
    let mut inputs = ArchiveInputs {
        files: Vec::new(),
        original_bytes: 0,
    };
    let device = fs::symlink_metadata(root)
        .map_err(|err| archive_rejected(format!("Could not inspect the mapping root: {err}.")))?
        .dev();
    walk(root, device, folder, exclusions, cancelled, &mut inputs).map_err(|err| {
        archive_rejected(format!(
            "Could not inspect the original folder `{folder}`: {err}."
        ))
    })?;
    inputs
        .files
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(inputs)
}

pub(crate) fn still_eligible(folder: &str, inputs: &ArchiveInputs, now: i64) -> bool {
    let latest = inputs.files.iter().map(|file| file.modified_secs).max();
    latest.is_some_and(|latest| {
        archive_suggestion(
            folder.to_owned(),
            inputs.files.len() as u64,
            inputs.original_bytes,
            latest,
            now,
        )
        .is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::check_zip_name;

    #[test]
    fn zip_names_reject_portable_case_and_normalization_collisions() {
        for name in ["Café.zip", "CAFÉ.ZIP", "Cafe\u{301}.zip"] {
            let err = check_zip_name("Café.zip", &[name.to_owned()], "the Mac folder").unwrap_err();
            assert!(err.cause().contains(name));
        }
        assert!(
            check_zip_name("Café.zip", &["unrelated?.zip".to_owned()], "the Mac folder").is_ok()
        );
    }
}
