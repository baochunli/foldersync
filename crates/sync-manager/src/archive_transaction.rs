//! Local archive publication and restart recovery. The durable intent precedes
//! both exclusive renames, so a restart can identify every object it may move.

use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sync_core::MappingId;

use crate::{
    ManagerError,
    platform::{
        ensure_private_dir_under, reject_symlinks_under_root, write_private_file_atomic_under,
    },
};

#[cfg(test)]
mod tests;

const HOLDING_FOLDER: &str = "FolderSync Archived Originals";
pub(crate) const ARCHIVE_PARTIAL_SUFFIX: &str = ".zip.foldersync-partial";
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;
const NEXT_STEP: &str = "Check the mapping and holding folder, then try again.";

/// A previously archived original and its current availability for Reveal in Finder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedOriginal {
    pub subfolder: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn for_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    id: Identity,
    bytes: u64,
    modified_seconds: i64,
    modified_nanos: i64,
}

impl Stamp {
    fn for_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            id: Identity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            bytes: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        })
    }
}

struct Entry {
    stamp: Stamp,
    is_dir: bool,
    is_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Phase {
    Writing {
        partial: Identity,
    },
    Verified {
        partial: Stamp,
        holding_parent: Identity,
    },
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    original: ArchivedOriginal,
    root: PathBuf,
    root_id: Identity,
    source: Identity,
    phase: Phase,
}

type Records = BTreeMap<String, Record>;

pub(crate) struct ArchiveJournal {
    state_dir: PathBuf,
    mapping_id: MappingId,
}

impl ArchiveJournal {
    pub(crate) fn new(state_dir: &Path, mapping_id: &MappingId) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
            mapping_id: mapping_id.clone(),
        }
    }

    fn path(&self) -> Result<PathBuf, ManagerError> {
        component(self.mapping_id.as_str())?;
        Ok(self
            .state_dir
            .join("mappings")
            .join(self.mapping_id.as_str())
            .join("archive-journal.json"))
    }

    fn load(&self) -> Result<Records, ManagerError> {
        let path = reject_symlinks_under_root("archive journal", &self.state_dir, &self.path()?)?;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Records::new()),
            Err(error) => return Err(io_error("read the archive journal", error)),
        };
        if !file
            .metadata()
            .map_err(|e| io_error("inspect the archive journal", e))?
            .is_file()
        {
            return Err(reject("The archive journal is not an ordinary file"));
        }
        serde_json::from_reader(file)
            .map_err(|error| reject(format!("The archive journal could not be decoded: {error}")))
    }

    fn save(&self, records: &Records) -> Result<(), ManagerError> {
        let path = self.path()?;
        let parent = path.parent().expect("journal has a mapping directory");
        let parent =
            ensure_private_dir_under("archive journal directory", &self.state_dir, parent)?;
        let bytes = serde_json::to_vec(records)
            .map_err(|error| reject(format!("Could not encode the archive journal: {error}")))?;
        write_private_file_atomic_under("archive journal", &self.state_dir, &path, &bytes)?;
        // The mapping directory may have been created by this operation. Persist
        // the journal name and each new ancestor name before moving user data.
        for dir in parent.ancestors().take(3) {
            sync_dir(&open_absolute_dir(dir)?)?;
        }
        Ok(())
    }

    pub(crate) fn has_pending(&self) -> Result<bool, ManagerError> {
        Ok(self
            .load()?
            .values()
            .any(|row| !matches!(row.phase, Phase::Completed)))
    }

    pub(crate) fn forget_completed(&self) -> Result<(), ManagerError> {
        let records = self.load()?;
        if records
            .values()
            .any(|row| !matches!(row.phase, Phase::Completed))
        {
            return Err(reject("An archive transaction is still pending recovery"));
        }
        if !records.is_empty() {
            self.save(&Records::new())?;
        }
        Ok(())
    }

    pub(crate) fn originals(&self) -> Result<Vec<ArchivedOriginal>, ManagerError> {
        let mut originals = Vec::new();
        for mut row in self.load()?.into_values() {
            if !matches!(row.phase, Phase::Completed) {
                continue;
            }
            component(&row.original.subfolder)?;
            row.original.present = false;
            if let Some(parent) = row.original.path.parent()
                && let Ok(dir) = open_absolute_dir(parent)
            {
                row.original.present = entry(&dir, &row.original.subfolder)?
                    .is_some_and(|item| item.is_dir && item.stamp.id == row.source);
            }
            originals.push(row.original);
        }
        Ok(originals)
    }

    pub(crate) fn recover(&self, root: &Path) -> Result<(), ManagerError> {
        let mut records = self.load()?;
        if records
            .values()
            .all(|row| matches!(row.phase, Phase::Completed))
        {
            return Ok(());
        }
        let (root_path, root) = checked_root(root)?;
        let root_id = identity(&root)?;
        let mut remove = Vec::new();
        for (subfolder, row) in &mut records {
            if matches!(row.phase, Phase::Completed) {
                continue;
            }
            if row.root != root_path
                || row.root_id != root_id
                || subfolder != &row.original.subfolder
            {
                return Err(reject(
                    "The archive journal belongs to a different mapping folder or volume",
                ));
            }
            let names = Names::new(subfolder)?;
            let source = entry(&root, subfolder)?;
            if source
                .as_ref()
                .is_some_and(|item| item.is_dir && item.stamp.id == row.source)
            {
                let partial = match row.phase {
                    Phase::Writing { partial } => partial,
                    Phase::Verified { partial, .. } => partial.id,
                    Phase::Completed => unreachable!(),
                };
                // The original was never moved, or has already been rolled back.
                remove_partial(&root, &names.partial, partial)?;
                remove.push(subfolder.clone());
                continue;
            }
            let Phase::Verified {
                partial,
                holding_parent,
            } = row.phase
            else {
                return Err(reject(
                    "The original folder changed while an archive was being written",
                ));
            };
            if source.is_some() {
                return Err(reject(
                    "The original folder's former name is occupied during archive recovery",
                ));
            }
            let holding = checked_holding(&root_path, &root, &row.original.path, subfolder, false)?
                .ok_or_else(|| reject("The archive holding directory is missing"))?;
            if identity(&holding)? != holding_parent
                || !entry(&holding, subfolder)?
                    .is_some_and(|item| item.is_dir && item.stamp.id == row.source)
            {
                return Err(reject(
                    "The original in the archive holding directory could not be identified",
                ));
            }
            let final_entry = entry(&root, &names.final_name)?;
            if final_entry
                .as_ref()
                .is_some_and(|item| item.is_file && item.stamp == partial)
            {
                // A crash occurred after both renames.
                sync_dir(&root)?;
                sync_dir(&holding)?;
                row.phase = Phase::Completed;
            } else if final_entry.is_none()
                && entry(&root, &names.partial)?
                    .is_some_and(|item| item.is_file && item.stamp == partial)
            {
                // A crash occurred after the original was moved but before publication.
                sync_dir(&holding)?;
                rename_exclusive(&root, &names.partial, &root, &names.final_name)?;
                sync_dir(&root)?;
                row.phase = Phase::Completed;
            } else {
                // A foreign ZIP or damaged partial prevents publication. Restore only
                // into the vacant original name.
                rename_exclusive(&holding, subfolder, &root, subfolder)?;
                sync_dir(&holding)?;
                sync_dir(&root)?;
                remove_partial(&root, &names.partial, partial.id)?;
                remove.push(subfolder.clone());
            }
        }
        for subfolder in remove {
            records.remove(&subfolder);
        }
        self.save(&records)
    }

    pub(crate) fn archive(
        &self,
        root: &Path,
        holding_path: &Path,
        subfolder: &str,
        estimated_bytes: u64,
        cancel: &dyn Fn() -> bool,
        build: impl FnOnce(&mut File) -> Result<u64, ManagerError>,
    ) -> Result<(), ManagerError> {
        check_stop(cancel)?;
        self.recover(root)?;
        let (root_path, root) = checked_root(root)?;
        let root_id = identity(&root)?;
        let names = Names::new(subfolder)?;
        let source = check_source(&root, subfolder)?;
        ensure_vacant(&root, &names.final_name)?;
        ensure_vacant(&root, &names.partial)?;
        if let Some(holding) = checked_holding(&root_path, &root, holding_path, subfolder, false)? {
            ensure_vacant(&holding, subfolder)?;
        }
        check_space(&root_path, estimated_bytes)?;
        let mut records = self.load()?;
        if records.contains_key(subfolder) {
            return Err(reject(format!(
                "The archive journal already contains an original for {subfolder}"
            )));
        }
        check_stop(cancel)?;
        let mut out = create_partial(&root, &names.partial)?;
        let partial_id = identity(&out)?;
        let mut moved = false;
        let operation = (|| {
            records.insert(
                subfolder.to_owned(),
                Record {
                    original: ArchivedOriginal {
                        subfolder: subfolder.to_owned(),
                        path: holding_path.to_owned(),
                        bytes: 0,
                        present: false,
                    },
                    root: root_path.clone(),
                    root_id,
                    source,
                    phase: Phase::Writing {
                        partial: partial_id,
                    },
                },
            );
            self.save(&records)?;
            let original_bytes = build(&mut out)?;
            check_stop(cancel)?;
            out.sync_all()
                .map_err(|e| io_error("flush the verified ZIP", e))?;
            let partial =
                Stamp::for_file(&out).map_err(|e| io_error("identify the verified ZIP", e))?;
            sync_dir(&root)?;
            let holding = checked_holding(&root_path, &root, holding_path, subfolder, true)?
                .ok_or_else(|| reject("The archive holding directory could not be created"))?;
            ensure_vacant(&holding, subfolder)?;
            let holding_parent = identity(&holding)?;
            let row = records
                .get_mut(subfolder)
                .expect("archive intent was inserted");
            row.original.bytes = original_bytes;
            row.phase = Phase::Verified {
                partial,
                holding_parent,
            };
            self.save(&records)?;

            ensure_vacant(&root, &names.final_name)?;
            if identity(&open_absolute_dir(&root_path)?)? != root_id
                || check_source(&root, subfolder)? != source
                || !entry(&root, &names.partial)?
                    .is_some_and(|item| item.is_file && item.stamp == partial)
                || identity(&open_absolute_dir(
                    holding_path.parent().expect("checked holding directory"),
                )?)? != holding_parent
            {
                return Err(reject("An archive path changed during preparation"));
            }
            check_stop(cancel)?;
            rename_exclusive(&root, subfolder, &holding, subfolder)?;
            moved = true;
            sync_dir(&root)?;
            sync_dir(&holding)?;
            if !entry(&holding, subfolder)?
                .is_some_and(|item| item.is_dir && item.stamp.id == source)
            {
                return Err(reject(
                    "The original changed while being moved into the holding directory",
                ));
            }
            if !entry(&root, &names.partial)?
                .is_some_and(|item| item.is_file && item.stamp == partial)
            {
                return Err(reject("The verified ZIP changed before publication"));
            }
            // Once the original moves, complete this short commit section. On an
            // error the journal lets the next startup finish or roll it back.
            rename_exclusive(&root, &names.partial, &root, &names.final_name)?;
            sync_dir(&root)?;
            if !entry(&root, &names.final_name)?
                .is_some_and(|item| item.is_file && item.stamp == partial)
            {
                return Err(reject("The verified ZIP changed during publication"));
            }
            records
                .get_mut(subfolder)
                .expect("archive intent was inserted")
                .phase = Phase::Completed;
            self.save(&records)
        })();
        if operation.is_err() && !moved {
            remove_partial(&root, &names.partial, partial_id)?;
            records.remove(subfolder);
            self.save(&records)?;
        }
        operation
    }
}

struct Names {
    final_name: String,
    partial: String,
}

impl Names {
    fn new(subfolder: &str) -> Result<Self, ManagerError> {
        component(subfolder)?;
        Ok(Self {
            final_name: format!("{subfolder}.zip"),
            partial: format!("{subfolder}{ARCHIVE_PARTIAL_SUFFIX}"),
        })
    }
}

pub(crate) fn holding_folder(volume: &Path, collection: &str) -> PathBuf {
    volume.join(HOLDING_FOLDER).join(collection)
}

pub(crate) fn check_archive_paths(
    root: &Path,
    volume_root: &Path,
    collection: &str,
    subfolder: &str,
) -> Result<PathBuf, ManagerError> {
    component(collection)?;
    let names = Names::new(subfolder)?;
    let (root_path, root) = checked_root(root)?;
    check_source(&root, subfolder)?;
    ensure_vacant(&root, &names.final_name)?;
    ensure_vacant(&root, &names.partial)?;
    let volume = volume_root
        .canonicalize()
        .map_err(|e| io_error("resolve the mapping's volume", e))?;
    let holding = holding_folder(&volume, collection).join(subfolder);
    if let Some(parent) = checked_holding(&root_path, &root, &holding, subfolder, false)? {
        ensure_vacant(&parent, subfolder)?;
    }
    Ok(holding)
}

fn checked_root(path: &Path) -> Result<(PathBuf, File), ManagerError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| io_error("inspect the mapping folder", e))?;
    if !metadata.is_dir() {
        return Err(reject("The mapping folder must be an ordinary directory"));
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| io_error("resolve the mapping folder", e))?;
    let root = open_absolute_dir(&canonical)?;
    let initial = Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if identity(&root)? != initial {
        return Err(reject(
            "The mapping folder changed while opening the archive paths",
        ));
    }
    Ok((canonical, root))
}

fn check_source(root: &File, name: &str) -> Result<Identity, ManagerError> {
    let root_id = identity(root)?;
    let source = entry(root, name)?
        .filter(|item| item.is_dir)
        .ok_or_else(|| {
            reject(format!(
                "The archive source {name} is missing or is not an ordinary folder"
            ))
        })?;
    same_volume(root_id, source.stamp.id)?;
    Ok(source.stamp.id)
}

fn checked_holding(
    root_path: &Path,
    root: &File,
    holding: &Path,
    subfolder: &str,
    create: bool,
) -> Result<Option<File>, ManagerError> {
    let parent = holding
        .parent()
        .ok_or_else(|| reject("Invalid archive holding path"))?;
    let base = parent
        .parent()
        .ok_or_else(|| reject("Invalid archive holding path"))?;
    let volume = base
        .parent()
        .ok_or_else(|| reject("Invalid archive holding path"))?;
    let collection = parent
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| reject("Invalid collection name in archive holding path"))?;
    component(collection)?;
    if holding.file_name().and_then(OsStr::to_str) != Some(subfolder)
        || base.file_name() != Some(OsStr::new(HOLDING_FOLDER))
        || holding.starts_with(root_path)
        || root_path.starts_with(base)
    {
        return Err(reject(
            "The archive holding folder must be outside the mapping folder",
        ));
    }
    let root_id = identity(root)?;
    let mut dir = open_absolute_dir(volume)?;
    let volume_id = identity(&dir)?;
    same_volume(root_id, volume_id)?;
    if volume_id == root_id {
        return Err(reject(
            "The archive holding folder cannot be outside a mapping of the whole volume",
        ));
    }
    for name in [HOLDING_FOLDER, collection] {
        if entry(&dir, name)?.is_none() {
            if !writable(&dir)? {
                return Err(reject(format!(
                    "The archive holding folder {} is not writable",
                    parent.display()
                )));
            }
            if !create {
                return Ok(None);
            }
            mkdir(&dir, name)?;
            sync_dir(&dir)?;
        }
        dir = open_child_dir(&dir, OsStr::new(name))?;
        let dir_id = identity(&dir)?;
        same_volume(root_id, dir_id)?;
        if dir_id == root_id {
            return Err(reject(
                "The archive holding folder overlaps the mapping folder",
            ));
        }
    }
    if !writable(&dir)? {
        return Err(reject(format!(
            "The archive holding folder {} is not writable",
            parent.display()
        )));
    }
    Ok(Some(dir))
}

fn same_volume(root: Identity, other: Identity) -> Result<(), ManagerError> {
    if root.device != other.device {
        return Err(reject(
            "The original can only be moved into holding on the same volume",
        ));
    }
    Ok(())
}

fn check_space(root: &Path, estimate: u64) -> Result<(), ManagerError> {
    let (space, _) = crate::platform::space_and_writability(root)
        .map_err(|e| io_error("check space for the archive", e))?;
    let required = estimate.saturating_add(SPACE_MARGIN);
    if space.free_bytes < required {
        return Err(reject(format!(
            "The archive needs {required} bytes free ({estimate} estimated ZIP bytes and {SPACE_MARGIN} bytes of margin). There are {} bytes available",
            space.free_bytes
        )));
    }
    Ok(())
}

fn check_stop(cancel: &dyn Fn() -> bool) -> Result<(), ManagerError> {
    if cancel() {
        Err(reject("Stopped before moving the original folder"))
    } else {
        Ok(())
    }
}

fn component(name: &str) -> Result<(), ManagerError> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0', '\r', '\n'])
    {
        return Err(reject(format!("Unsafe archive name: {name:?}")));
    }
    Ok(())
}

fn cstr(name: &OsStr) -> Result<CString, ManagerError> {
    CString::new(name.as_bytes()).map_err(|_| reject("An archive path contains a NUL byte"))
}

fn open_absolute_dir(path: &Path) -> Result<File, ManagerError> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(reject("Archive paths must be absolute"));
    }
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|e| io_error("open the filesystem root", e))?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(reject(
                "Archive paths must not contain traversal components",
            ));
        };
        dir = open_child_dir(&dir, name)?;
    }
    Ok(dir)
}

fn open_child_dir(dir: &File, name: &OsStr) -> Result<File, ManagerError> {
    let name = cstr(name)?;
    // SAFETY: Valid, open directory descriptor and NUL-terminated single path component.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    file_from_fd(fd, "open an archive directory")
}

fn file_from_fd(fd: i32, what: &str) -> Result<File, ManagerError> {
    if fd < 0 {
        return Err(io_error(what, io::Error::last_os_error()));
    }
    // SAFETY: Successful open returned an owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn identity(file: &File) -> Result<Identity, ManagerError> {
    Identity::for_file(file).map_err(|e| io_error("identify an archive path", e))
}

#[allow(clippy::unnecessary_cast)] // libc field widths differ by target.
fn entry(dir: &File, name: &str) -> Result<Option<Entry>, ManagerError> {
    let name = cstr(OsStr::new(name))?;
    // SAFETY: Valid directory descriptor, NUL-terminated path, and writable stat buffer.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(io_error("inspect an archive path", error));
    }
    let mode = stat.st_mode & libc::S_IFMT;
    Ok(Some(Entry {
        stamp: Stamp {
            // The underlying libc types differ across supported platforms.
            id: Identity {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
            },
            bytes: stat.st_size.max(0) as u64,
            modified_seconds: stat.st_mtime as i64,
            modified_nanos: stat.st_mtime_nsec as i64,
        },
        is_dir: mode == libc::S_IFDIR,
        is_file: mode == libc::S_IFREG,
    }))
}

fn ensure_vacant(dir: &File, name: &str) -> Result<(), ManagerError> {
    if entry(dir, name)?.is_some() {
        return Err(reject(format!(
            "An item named {name} already exists at an archive destination"
        )));
    }
    Ok(())
}

fn create_partial(dir: &File, name: &str) -> Result<File, ManagerError> {
    let name = cstr(OsStr::new(name))?;
    // SAFETY: Valid directory fd and NUL-terminated name. O_EXCL and O_NOFOLLOW protect other objects.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    file_from_fd(fd, "create the archive partial")
}

fn remove_partial(dir: &File, name: &str, expected: Identity) -> Result<(), ManagerError> {
    let Some(object) = entry(dir, name)? else {
        return Ok(());
    };
    if !object.is_file || object.stamp.id != expected {
        return Err(reject(
            "The archive partial changed ownership and was left in place",
        ));
    }
    let name = cstr(OsStr::new(name))?;
    // SAFETY: Valid directory descriptor and NUL-terminated path for the recorded partial.
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(io_error(
            "remove the owned archive partial",
            io::Error::last_os_error(),
        ));
    }
    sync_dir(dir)
}

fn mkdir(dir: &File, name: &str) -> Result<(), ManagerError> {
    let name = cstr(OsStr::new(name))?;
    // SAFETY: Valid directory descriptor and NUL-terminated name.
    let result = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) };
    if result != 0 {
        return Err(io_error(
            "create the archive holding directory",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn writable(dir: &File) -> Result<bool, ManagerError> {
    // SAFETY: Valid directory descriptor and NUL-terminated path.
    let result = unsafe {
        libc::faccessat(
            dir.as_raw_fd(),
            c".".as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EACCESS,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(libc::EROFS)
    {
        return Ok(false);
    }
    Err(io_error("check holding directory permissions", error))
}

fn sync_dir(dir: &File) -> Result<(), ManagerError> {
    dir.sync_all()
        .map_err(|e| io_error("flush an archive directory", e))
}

fn rename_exclusive(
    source: &File,
    source_name: &str,
    target: &File,
    target_name: &str,
) -> Result<(), ManagerError> {
    let source_name = cstr(OsStr::new(source_name))?;
    let target_name = cstr(OsStr::new(target_name))?;
    // SAFETY: Both descriptors remain open and both single-component names are NUL-terminated.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            source.as_raw_fd(),
            source_name.as_ptr(),
            target.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            source.as_raw_fd(),
            source_name.as_ptr(),
            target.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = return Err(reject(
        "This platform does not support exclusive archive moves",
    ));
    if result != 0 {
        return Err(io_error(
            "move an archive path without replacing existing data",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn reject(cause: impl Into<String>) -> ManagerError {
    ManagerError::PreflightRejected {
        cause: cause.into(),
        next_step: NEXT_STEP.to_owned(),
    }
}

fn io_error(action: &str, error: io::Error) -> ManagerError {
    reject(format!("Could not {action}: {error}"))
}
