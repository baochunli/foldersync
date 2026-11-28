//! Read-only ZIP input and independent verification of the staged archive.

use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

use chrono::{DateTime, Datelike, Timelike};
use sync_core::archive::ArchiveFile;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FullFileOptions};

const WINDOWS_EPOCH_OFFSET_SECS: i128 = 11_644_473_600;
const ZIP_END_ALLOWANCE: u64 = 128;
const ZIP_ENTRY_ALLOWANCE: u64 = 256;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

pub(crate) fn estimated_zip_size(files: &[ArchiveFile]) -> u64 {
    files.iter().fold(ZIP_END_ALLOWANCE, |size, file| {
        size.saturating_add(estimated_payload_size(file))
            .saturating_add((file.path.len() as u64).saturating_mul(2))
            .saturating_add(ZIP_ENTRY_ALLOWANCE)
    })
}

pub(crate) fn write_zip(
    root: &Path,
    files: &[ArchiveFile],
    output: impl Write + Seek,
    mut on_progress: impl FnMut(u64) -> io::Result<()>,
) -> io::Result<()> {
    on_progress(0)?;
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    let mut zip = ZipWriter::new(output);
    let mut completed = 0;
    for file in files {
        on_progress(completed)?;
        let mut input = open_beneath(&root, &file.path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("Could not read {}: {error}", file.path),
            )
        })?;
        check_metadata(&input, file)?;
        zip.start_file(&file.path, entry_options(file)?)?;
        copy_exact(
            &mut input,
            &mut zip,
            file.size,
            &mut completed,
            &mut on_progress,
        )?;
        check_metadata(&input, file)?;
    }
    zip.finish()?.flush()?;
    on_progress(completed)
}

pub(crate) fn verify_zip(
    input: impl Read + Seek,
    files: &[ArchiveFile],
    mut on_progress: impl FnMut(u64) -> io::Result<()>,
) -> io::Result<()> {
    on_progress(0)?;
    let mut zip = ZipArchive::new(input)?;
    if zip.len() != files.len() || zip.has_duplicate_names() {
        return Err(invalid_data(
            "The ZIP entry list differs from the source files",
        ));
    }
    let mut completed = 0;
    for (index, file) in files.iter().enumerate() {
        on_progress(completed)?;
        let mut entry = zip.by_index(index)?;
        if !entry.is_file()
            || entry.name_raw() != file.path.as_bytes()
            || entry.name()?.as_ref() != file.path
            || entry.size() != file.size
        {
            return Err(invalid_data(format!(
                "The ZIP entry differs from the source file {}",
                file.path
            )));
        }
        // Reading through EOF checks the entry's CRC, including empty entries.
        copy_exact(
            &mut entry,
            io::sink(),
            file.size,
            &mut completed,
            &mut on_progress,
        )?;
    }
    on_progress(completed)
}

fn open_beneath(root: &File, path: &str) -> io::Result<File> {
    let mut names = path.split('/').peekable();
    if path.contains(['\\', ':']) || !path.contains('/') {
        return Err(invalid_data(format!("Invalid archive path {path}")));
    }
    let mut directory = None;
    while let Some(name) = names.next() {
        if matches!(name, "" | "." | "..") {
            return Err(invalid_data(format!("Invalid archive path {path}")));
        }
        let name = CString::new(name).map_err(invalid_data)?;
        let flags = if names.peek().is_some() {
            libc::O_DIRECTORY
        } else {
            // A special file could appear between inventory and opening.
            libc::O_NONBLOCK
        };
        let parent = directory.as_ref().unwrap_or(root);
        // SAFETY: the parent descriptor and null-terminated single name remain
        // valid for the call. No creation flags are passed.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new, owned descriptor.
        let entry = unsafe { File::from_raw_fd(fd) };
        if names.peek().is_none() {
            return Ok(entry);
        }
        directory = Some(entry);
    }
    Err(invalid_data(format!("Invalid archive path {path}")))
}

fn check_metadata(input: &File, expected: &ArchiveFile) -> io::Result<()> {
    let actual = ArchiveFile::from_metadata(expected.path.clone(), &input.metadata()?)?;
    if actual != *expected {
        return Err(invalid_data(format!(
            "{} changed during archive preparation",
            expected.path
        )));
    }
    Ok(())
}

fn entry_options(file: &ArchiveFile) -> io::Result<FullFileOptions<'static, 'static>> {
    let modified = DateTime::from_timestamp(file.modified_secs, file.modified_nanos)
        .ok_or_else(|| invalid_data("File modification time is out of range"))?;
    let dos_time = zip::DateTime::from_date_and_time(
        modified.year().clamp(1980, 2107) as u16,
        modified.month() as u8,
        modified.day() as u8,
        modified.hour() as u8,
        modified.minute() as u8,
        modified.second() as u8,
    )
    .unwrap_or_default();
    let mut options = FullFileOptions::default()
        .compression_method(compression_method(&file.path))
        .last_modified_time(dos_time)
        .large_file(estimated_payload_size(file) >= u64::from(u32::MAX));
    // Extended Timestamp is widely understood on POSIX. NTFS records fractional
    // seconds in 100-nanosecond units and covers older and later dates.
    if let Ok(seconds) = u32::try_from(file.modified_secs) {
        options = options.with_extended_timestamp(Some(seconds), None, None)?;
    }
    let windows_time = (i128::from(file.modified_secs) + WINDOWS_EPOCH_OFFSET_SECS) * 10_000_000
        + i128::from(file.modified_nanos / 100);
    let windows_time = u64::try_from(windows_time)
        .map_err(|_| invalid_data("File modification time is out of ZIP timestamp range"))?;
    Ok(options.with_ntfs_times(windows_time, 0, 0)?)
}

fn estimated_payload_size(file: &ArchiveFile) -> u64 {
    if compression_method(&file.path) == CompressionMethod::Stored {
        return file.size;
    }
    // Allow for deflate block overhead on incompressible files.
    file.size.saturating_add(file.size / 100).saturating_add(64)
}

fn compression_method(path: &str) -> CompressionMethod {
    const ALREADY_COMPRESSED: &[&str] = &[
        "7z", "avif", "bz2", "docx", "flac", "gif", "gz", "heic", "jpeg", "jpg", "m4a", "mkv",
        "mov", "mp3", "mp4", "ogg", "pdf", "png", "pptx", "rar", "webm", "webp", "xlsx", "xz",
        "zip", "zst",
    ];
    let extension = Path::new(path).extension().and_then(|value| value.to_str());
    if path.contains("/.git/objects/")
        || extension.is_some_and(|value| {
            ALREADY_COMPRESSED
                .iter()
                .any(|ext| value.eq_ignore_ascii_case(ext))
        })
    {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    }
}

fn copy_exact(
    mut input: impl Read,
    mut output: impl Write,
    size: u64,
    completed: &mut u64,
    on_progress: &mut impl FnMut(u64) -> io::Result<()>,
) -> io::Result<()> {
    let mut buffer = [0; COPY_BUFFER_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        let capacity = remaining.min(buffer.len() as u64) as usize;
        let count = input.read(&mut buffer[..capacity])?;
        if count == 0 {
            return Err(invalid_data("File ended before its inspected size"));
        }
        output.write_all(&buffer[..count])?;
        remaining -= count as u64;
        *completed = completed.saturating_add(count as u64);
        on_progress(*completed)?;
    }
    if input.read(&mut buffer[..1])? != 0 {
        return Err(invalid_data("File grew past its inspected size"));
    }
    Ok(())
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::{self, Cursor, Read, Seek, SeekFrom, Write},
        os::unix::fs::symlink,
        path::Path,
        time::{Duration, UNIX_EPOCH},
    };

    use sync_core::archive::ArchiveFile;
    use zip::{CompressionMethod, ZipArchive, extra_fields::ExtraField};

    use super::{COPY_BUFFER_SIZE, estimated_zip_size, verify_zip, write_zip};

    fn metadata(root: &Path, path: &str) -> ArchiveFile {
        ArchiveFile::from_metadata(path.into(), &fs::metadata(root.join(path)).unwrap()).unwrap()
    }

    fn fixture(root: &Path, path: &str, data: &[u8]) -> ArchiveFile {
        let path_on_disk = root.join(path);
        fs::create_dir_all(path_on_disk.parent().unwrap()).unwrap();
        fs::write(&path_on_disk, data).unwrap();
        File::open(&path_on_disk)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::new(1_600_000_001, 48_201_195))
            .unwrap();
        metadata(root, path)
    }

    #[test]
    fn exact_round_trip_includes_nested_git_and_preserves_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let object = b"\x78\x9c\x00git-object\xff\x00";
        let note = b"Conference material\n";
        let files = [
            fixture(root, "Done/proj/.git/objects/aa/bb", object),
            fixture(root, "Done/proj/nested/.git/objects/cc/dd", object),
            fixture(root, "Done/résumé.txt", note),
        ];
        fixture(root, "Done/proj/node_modules/excluded.txt", b"excluded");
        let mut zip = Cursor::new(Vec::new());
        write_zip(root, &files, &mut zip, |_| Ok(())).unwrap();
        assert!(zip.get_ref().len() as u64 <= estimated_zip_size(&files));
        verify_zip(&mut zip, &files, |_| Ok(())).unwrap();
        let mut reader = ZipArchive::new(zip).unwrap();
        assert_eq!(reader.len(), 3);
        for file in &files {
            let mut entry = reader.by_name(&file.path).unwrap();
            let mut actual = Vec::new();
            entry.read_to_end(&mut actual).unwrap();
            let is_git_object = file.path.contains("/.git/objects/");
            let expected_data: &[u8] = if is_git_object { object } else { note };
            assert_eq!(actual, expected_data);
            let method = if is_git_object {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            assert_eq!(entry.compression(), method);
            assert!(entry.extra_data_fields().any(|field| matches!(
                field, ExtraField::ExtendedTimestamp(time)
                    if time.mod_time() == Some(1_600_000_001)
            )));
            let expected_ntfs_time = 132_444_736_010_000_000 + u64::from(file.modified_nanos / 100);
            assert!(entry.extra_data_fields().any(|field| matches!(
                field, ExtraField::Ntfs(time) if time.mtime() == expected_ntfs_time
            )));
            assert_eq!(metadata(root, &file.path), *file);
        }
    }

    #[test]
    fn verification_rejects_wrong_paths_sizes_and_bad_crc() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let data = b"unique file bytes in the archive";
        let files = [fixture(root, "Done/photo.jpg", data)];
        let mut zip = Cursor::new(Vec::new());
        write_zip(root, &files, &mut zip, |_| Ok(())).unwrap();

        let mut wrong = files.clone();
        wrong[0].path = "Done/wrong.jpg".into();
        assert!(verify_zip(&mut zip, &wrong, |_| Ok(())).is_err());
        wrong[0].path.clone_from(&files[0].path);
        wrong[0].size += 1;
        assert!(verify_zip(&mut zip, &wrong, |_| Ok(())).is_err());

        let offset = zip
            .get_ref()
            .windows(data.len())
            .position(|bytes| bytes == data)
            .unwrap();
        zip.get_mut()[offset] ^= 1;
        let error = verify_zip(zip, &files, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    #[test]
    fn nofollow_refuses_an_ancestor_replaced_by_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let file = fixture(root, "Done/inner/photo.jpg", b"must not follow");
        fs::rename(root.join("Done/inner"), root.join("moved")).unwrap();
        symlink(root.join("moved"), root.join("Done/inner")).unwrap();
        let error = write_zip(root, &[file], Cursor::new(Vec::new()), |_| Ok(())).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn cancellation_is_checked_during_writing_and_verification() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let files = [fixture(root, "Done/photo.jpg", &[42; 128 * 1024])];
        let cancel_after_chunk = |written| {
            if written > 0 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "Stopped"))
            } else {
                Ok(())
            }
        };
        let mut zip = Cursor::new(Vec::new());
        let err = write_zip(root, &files, &mut zip, cancel_after_chunk).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        zip.get_mut().clear();
        zip.set_position(0);
        write_zip(root, &files, &mut zip, |_| Ok(())).unwrap();
        let err = verify_zip(zip, &files, cancel_after_chunk).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn zip64_preserves_a_sparse_entry_above_four_gibibytes() {
        struct SparseOutput(File);

        impl Write for SparseOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.len() == COPY_BUFFER_SIZE {
                    // Full-size writes contain the zero-filled STORED fixture.
                    // Advance over them so the output stays sparse as well.
                    self.0.seek(SeekFrom::Current(bytes.len() as i64))?;
                    return Ok(bytes.len());
                }
                self.0.write(bytes)
            }

            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }

        impl Seek for SparseOutput {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.0.seek(position)
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let path = "Done/large.jpg";
        let size = u64::from(u32::MAX) + 8193;
        fs::create_dir(root.join("Done")).unwrap();
        File::create(root.join(path))
            .unwrap()
            .set_len(size)
            .unwrap();
        let files = [metadata(root, path)];
        let output = tempfile::tempfile_in(temp.path()).unwrap();
        let mut output = SparseOutput(output);
        write_zip(root, &files, &mut output, |_| Ok(())).unwrap();
        assert!(output.0.metadata().unwrap().len() > size);
        let mut zip = ZipArchive::new(output.0).unwrap();
        let entry = zip.by_name(path).unwrap();
        assert_eq!(entry.size(), size);
        assert!(
            entry
                .extra_data_fields()
                .any(|field| matches!(field, ExtraField::Zip64ExtendedInformation(_)))
        );
    }
}
