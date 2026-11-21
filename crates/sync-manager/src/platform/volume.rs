//! Read-only mounted filesystem identification and capacity.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sync_core::{LocalBinding, VolumeIdentity, VolumeKind};

/// Capacity and bytes available to the current user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSpaceInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl DiskSpaceInfo {
    fn from_blocks(total: impl Into<u64>, available: impl Into<u64>, size: impl Into<u64>) -> Self {
        let size = size.into();
        Self {
            total_bytes: total.into().saturating_mul(size),
            free_bytes: available.into().saturating_mul(size),
        }
    }
}

/// The filesystem actually backing an existing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedVolume {
    pub identity: VolumeIdentity,
    pub mount_path: PathBuf,
    pub display_name: String,
    pub filesystem: String,
    pub space: DiskSpaceInfo,
}

impl MountedVolume {
    pub(crate) fn binding_mount(&self, path: &Path) -> &Path {
        // The APFS Data volume backs firmlink paths such as /Users and /private/var.
        // Keep the user's logical path in the generation while checking the Data UUID.
        #[cfg(target_os = "macos")]
        if self.mount_path == Path::new("/System/Volumes/Data")
            && !path.starts_with(&self.mount_path)
        {
            return Path::new("/");
        }
        let _ = path;
        &self.mount_path
    }
}

/// Looks up a path's current volume and locates a previously bound volume by identity.
pub trait VolumeProbe: Send + Sync {
    fn probe(&self, path: &Path) -> io::Result<MountedVolume>;
    fn find(&self, binding: &LocalBinding) -> io::Result<Option<MountedVolume>>;
}

/// Read-only operating system volume probe.
#[derive(Debug, Default)]
pub struct OsVolumeProbe;

impl VolumeProbe for OsVolumeProbe {
    fn probe(&self, path: &Path) -> io::Result<MountedVolume> {
        os::probe(path)
    }

    fn find(&self, binding: &LocalBinding) -> io::Result<Option<MountedVolume>> {
        os::find(binding)
    }
}

fn volume_name(mount: &Path) -> String {
    mount
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Internal SSD".to_owned())
}

#[cfg(unix)]
fn path_c_string(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(path.as_os_str().as_bytes())?)
}

/// Reads space on the existing path or its nearest ancestor for setup previews.
/// Launch checks use the verified mounted volume instead of this fallback.
pub fn disk_space_for_path(path: &Path) -> Option<DiskSpaceInfo> {
    path.ancestors()
        .find(|ancestor| fs::metadata(ancestor).is_ok())
        .and_then(|ancestor| space_and_writability(ancestor).ok().map(|(space, _)| space))
}

#[cfg(unix)]
pub(crate) fn space_and_writability(path: &Path) -> io::Result<(DiskSpaceInfo, bool)> {
    let path = path_c_string(path)?;
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    let space = DiskSpaceInfo::from_blocks(stat.f_blocks, stat.f_bavail, block_size);
    // faccessat with AT_EACCESS checks the same effective credentials used by rclone.
    let writable = stat.f_flag & libc::ST_RDONLY == 0
        && unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                path.as_ptr(),
                libc::W_OK | libc::X_OK,
                libc::AT_EACCESS,
            )
        } == 0;
    Ok((space, writable))
}

#[cfg(not(unix))]
pub(crate) fn space_and_writability(_: &Path) -> io::Result<(DiskSpaceInfo, bool)> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;
    use std::{
        ffi::{CStr, OsStr, c_void},
        os::unix::ffi::OsStrExt,
        ptr,
    };

    use core_foundation_sys::{
        base::{CFRelease, CFTypeRef},
        dictionary::{CFDictionaryGetValue, CFDictionaryRef},
        number::CFBooleanGetValue,
        string::{
            CFStringGetCString, CFStringGetLength, CFStringGetMaximumSizeForEncoding, CFStringRef,
            kCFStringEncodingUTF8,
        },
        uuid::{CFUUIDCreateString, CFUUIDRef},
    };

    #[link(name = "DiskArbitration", kind = "framework")]
    unsafe extern "C" {
        fn DASessionCreate(allocator: CFTypeRef) -> CFTypeRef;
        fn DADiskCreateFromBSDName(
            allocator: CFTypeRef,
            session: CFTypeRef,
            name: *const libc::c_char,
        ) -> CFTypeRef;
        fn DADiskCopyDescription(disk: CFTypeRef) -> CFDictionaryRef;
        static kDADiskDescriptionVolumeUUIDKey: CFStringRef;
        static kDADiskDescriptionVolumeNameKey: CFStringRef;
        static kDADiskDescriptionDeviceInternalKey: CFStringRef;
    }

    struct OwnedCf(CFTypeRef);

    impl OwnedCf {
        fn new(value: CFTypeRef) -> Option<Self> {
            if value.is_null() {
                None
            } else {
                Some(Self(value))
            }
        }
    }

    impl Drop for OwnedCf {
        fn drop(&mut self) {
            // SAFETY: every non-null value is owned from a Create or Copy API.
            unsafe { CFRelease(self.0) };
        }
    }

    fn as_string(value: CFStringRef) -> Option<String> {
        if value.is_null() {
            return None;
        }
        // SAFETY: callers pass a CFString from a documented string key/Create API.
        unsafe {
            let bytes =
                CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), kCFStringEncodingUTF8);
            let mut buf = vec![0; usize::try_from(bytes).ok()?.checked_add(1)?];
            (CFStringGetCString(
                value,
                buf.as_mut_ptr(),
                buf.len() as _,
                kCFStringEncodingUTF8,
            ) != 0)
                .then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }

    fn describe(stat: &libc::statfs, session: &OwnedCf) -> io::Result<MountedVolume> {
        // SAFETY: statfs provides terminated strings. The description keys declare the
        // corresponding CF types, borrowed for the lifetime of the owned dictionary.
        unsafe {
            let mount = CStr::from_ptr(stat.f_mntonname.as_ptr());
            let mount_path = PathBuf::from(OsStr::from_bytes(mount.to_bytes()));
            let source = CStr::from_ptr(stat.f_mntfromname.as_ptr());
            if !source.to_bytes().starts_with(b"/dev/disk") {
                return Err(io::Error::other(
                    "Volume has no supported local disk identity",
                ));
            }
            let disk = OwnedCf::new(DADiskCreateFromBSDName(
                ptr::null(),
                session.0,
                source.as_ptr(),
            ))
            .ok_or_else(|| io::Error::other("Disk Arbitration could not resolve the disk"))?;
            let description = DADiskCopyDescription(disk.0);
            let _owned_description = OwnedCf::new(description.cast()).ok_or_else(|| {
                io::Error::other("Disk Arbitration returned no volume information")
            })?;
            let value = |key: CFStringRef| CFDictionaryGetValue(description, key.cast::<c_void>());
            let uuid: CFUUIDRef = value(kDADiskDescriptionVolumeUUIDKey).cast();
            let uuid_string = (!uuid.is_null())
                .then(|| OwnedCf::new(CFUUIDCreateString(ptr::null(), uuid).cast()))
                .flatten()
                .and_then(|uuid| as_string(uuid.0.cast()))
                .and_then(|uuid| uuid::Uuid::parse_str(&uuid).ok())
                .filter(|uuid| !uuid.is_nil())
                .ok_or_else(|| io::Error::other("Volume has no persistent filesystem UUID"))?;
            let internal: core_foundation_sys::number::CFBooleanRef =
                value(kDADiskDescriptionDeviceInternalKey).cast();
            if internal.is_null() {
                return Err(io::Error::other("Disk connection type is unavailable"));
            }
            let kind = if CFBooleanGetValue(internal) {
                VolumeKind::Internal
            } else {
                VolumeKind::External
            };
            Ok(MountedVolume {
                identity: VolumeIdentity::new(kind, uuid_string.to_string()),
                display_name: as_string(value(kDADiskDescriptionVolumeNameKey).cast())
                    .unwrap_or_else(|| volume_name(&mount_path)),
                mount_path,
                filesystem: CStr::from_ptr(stat.f_fstypename.as_ptr())
                    .to_string_lossy()
                    .to_uppercase(),
                space: DiskSpaceInfo::from_blocks(stat.f_blocks, stat.f_bavail, stat.f_bsize),
            })
        }
    }

    fn session() -> io::Result<OwnedCf> {
        // SAFETY: null requests the default allocator.
        OwnedCf::new(unsafe { DASessionCreate(ptr::null()) })
            .ok_or_else(|| io::Error::other("Disk Arbitration session is unavailable"))
    }

    pub(super) fn probe(path: &Path) -> io::Result<MountedVolume> {
        let path = path_c_string(path)?;
        // SAFETY: path is terminated and stat is writable.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        describe(&stat, &session()?)
    }

    pub(super) fn find(binding: &LocalBinding) -> io::Result<Option<MountedVolume>> {
        // The startup volume's logical root cannot move. Skip enumeration for it.
        if binding.mount_path == Path::new("/") {
            return Ok(None);
        }
        let session = session()?;
        // SAFETY: a null buffer asks for the mount count. The second call gets
        // kernel metadata only (MNT_NOWAIT) and the buffer is sized in bytes.
        let count = unsafe { libc::getfsstat(ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut mounts = vec![unsafe { std::mem::zeroed::<libc::statfs>() }; count as usize + 8];
        let bytes = libc::c_int::try_from(std::mem::size_of_val(mounts.as_slice()))
            .map_err(io::Error::other)?;
        let count = unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mounts[..count as usize]
            .iter()
            .filter_map(|stat| describe(stat, &session).ok())
            .find(|volume| volume.identity.stable_id == binding.volume.stable_id))
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::*;
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::MetadataExt},
    };

    struct Mount {
        id: u64,
        device: (u32, u32),
        path: PathBuf,
        filesystem: String,
        source: PathBuf,
    }

    fn mount_path(field: &[u8]) -> PathBuf {
        let mut bytes = Vec::with_capacity(field.len());
        let mut remaining = field;
        while let Some((&byte, rest)) = remaining.split_first() {
            remaining = rest;
            if byte == b'\\'
                && let Some(octal) = rest.get(..3)
                && octal.iter().all(|digit| matches!(digit, b'0'..=b'7'))
            {
                let value = (octal[0] - b'0') as u16 * 64
                    + (octal[1] - b'0') as u16 * 8
                    + (octal[2] - b'0') as u16;
                bytes.push(value as u8);
                remaining = &rest[3..];
            } else {
                bytes.push(byte);
            }
        }
        PathBuf::from(OsString::from_vec(bytes))
    }

    fn read_mounts() -> io::Result<Vec<Mount>> {
        fn parse(line: &[u8]) -> Option<Mount> {
            let mut fields = line.split(|&byte| byte == b' ');
            let id = str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            fields.next()?; // parent
            let (major, minor) = str::from_utf8(fields.next()?).ok()?.split_once(':')?;
            fields.next()?; // root of this mount inside the filesystem
            let path = mount_path(fields.next()?);
            fields.find(|field| *field == b"-")?;
            Some(Mount {
                id,
                device: (major.parse().ok()?, minor.parse().ok()?),
                path,
                filesystem: str::from_utf8(fields.next()?).ok()?.to_uppercase(),
                source: mount_path(fields.next()?),
            })
        }
        Ok(fs::read("/proc/self/mountinfo")?
            .split(|&byte| byte == b'\n')
            .filter_map(parse)
            .collect())
    }

    fn uuid_device(path: &Path) -> Option<(u32, u32)> {
        use std::os::unix::fs::FileTypeExt;
        let metadata = fs::metadata(path).ok()?;
        metadata.file_type().is_block_device().then(|| {
            let dev = metadata.rdev();
            (libc::major(dev), libc::minor(dev))
        })
    }

    fn filesystem_uuid(mount: &Mount) -> Option<String> {
        let device = uuid_device(&mount.source).unwrap_or(mount.device);
        fs::read_dir("/dev/disk/by-uuid")
            .ok()?
            .filter_map(Result::ok)
            .find(|entry| uuid_device(&entry.path()) == Some(device))
            .map(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase())
    }

    fn stat_mount_id(path: &std::ffi::CStr, mask: u32) -> io::Result<u64> {
        // SAFETY: path is terminated and stat is writable.
        let mut stat: libc::statx = unsafe { std::mem::zeroed() };
        if unsafe { libc::statx(libc::AT_FDCWD, path.as_ptr(), 0, mask, &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if stat.stx_mask & mask != mask {
            return Err(io::ErrorKind::Unsupported.into());
        }
        Ok(stat.stx_mnt_id)
    }

    fn describe(mount: Mount, stable_id: String, path: &Path) -> io::Result<MountedVolume> {
        let path = path_c_string(path)?;
        // SAFETY: path is terminated and stat is writable.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let block_size = u64::try_from(if stat.f_frsize > 0 {
            stat.f_frsize
        } else {
            stat.f_bsize
        })
        .map_err(io::Error::other)?;
        let external = ["/media", "/run/media", "/mnt"]
            .iter()
            .any(|prefix| mount.path.starts_with(prefix));
        let kind = if external {
            VolumeKind::External
        } else {
            VolumeKind::Internal
        };
        Ok(MountedVolume {
            identity: VolumeIdentity::new(kind, stable_id),
            display_name: volume_name(&mount.path),
            mount_path: mount.path,
            filesystem: mount.filesystem,
            space: DiskSpaceInfo::from_blocks(stat.f_blocks, stat.f_bavail, block_size),
        })
    }

    pub(super) fn probe(path: &Path) -> io::Result<MountedVolume> {
        let c_path = path_c_string(path)?;
        let id = stat_mount_id(&c_path, libc::STATX_MNT_ID)?;
        let mount = read_mounts()?
            .into_iter()
            .find(|mount| mount.id == id)
            .ok_or_else(|| io::Error::other("Mount changed while being inspected"))?;
        let stable_id = match filesystem_uuid(&mount) {
            Some(uuid) => format!("linux-uuid:{uuid}"),
            None => {
                // An overlay/tmpfs has no persistent UUID. It is identifiable only
                // for this particular mount and boot, never across a remount/reboot.
                let unique_id = stat_mount_id(&c_path, libc::STATX_MNT_ID_UNIQUE)?;
                let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
                format!("linux-mount:{}:{unique_id}", boot.trim())
            }
        };
        let volume = describe(mount, stable_id, path)?;
        if stat_mount_id(&c_path, libc::STATX_MNT_ID)? != id {
            return Err(io::Error::other("Mount changed while being inspected"));
        }
        Ok(volume)
    }

    pub(super) fn find(binding: &LocalBinding) -> io::Result<Option<MountedVolume>> {
        let Some(uuid) = binding.volume.stable_id.strip_prefix("linux-uuid:") else {
            return Ok(None);
        };
        for mount in read_mounts()? {
            if filesystem_uuid(&mount).as_deref() != Some(uuid) {
                continue;
            }
            if let Ok(volume) = probe(&mount.path)
                && volume.identity.stable_id == binding.volume.stable_id
            {
                return Ok(Some(volume));
            }
        }
        Ok(None)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod os {
    use super::*;

    pub(super) fn probe(_: &Path) -> io::Result<MountedVolume> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn find(_: &LocalBinding) -> io::Result<Option<MountedVolume>> {
        Ok(None)
    }
}
