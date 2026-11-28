//! Archive eligibility and the local input file details used to verify ZIP creation.

use std::{fs::Metadata, io, time::UNIX_EPOCH};

use chrono::{DateTime, Months};

use crate::policy::{ARCHIVE_MIN_AGE_MONTHS, ARCHIVE_MIN_FILES};

/// An included ordinary file and its inspected attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveFile {
    /// Slash-separated path under the mapping root, including the archived subfolder.
    pub path: String,
    /// Inspected file size in bytes.
    pub size: u64,
    /// Integral seconds since the Unix epoch at the start of the modification second.
    pub modified_secs: i64,
    /// Nanoseconds within the modification second.
    pub modified_nanos: u32,
}

impl ArchiveFile {
    /// Converts metadata of an ordinary file into an archive input record.
    pub fn from_metadata(path: String, metadata: &Metadata) -> io::Result<Self> {
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} must be an ordinary file"),
            ));
        }
        let (secs, nanos) = match metadata.modified()?.duration_since(UNIX_EPOCH) {
            Ok(duration) => (i128::from(duration.as_secs()), duration.subsec_nanos()),
            Err(before_epoch) => {
                let duration = before_epoch.duration();
                let secs = -i128::from(duration.as_secs());
                let nanos = duration.subsec_nanos();
                if nanos == 0 {
                    (secs, 0)
                } else {
                    (secs - 1, 1_000_000_000 - nanos)
                }
            }
        };
        let modified_secs = i64::try_from(secs).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "File modification time is out of range",
            )
        })?;
        Ok(Self {
            path,
            size: metadata.len(),
            modified_secs,
            modified_nanos: nanos,
        })
    }
}

/// An immediate subfolder eligible for the wizard's archive suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSuggestion {
    /// Name of the immediate subfolder.
    pub name: String,
    /// Number of included files across the subfolder.
    pub file_count: u64,
    /// Total uncompressed bytes of included files.
    pub bytes: u64,
    /// Modification time of the newest included file, in Unix epoch seconds.
    pub last_changed_unix_secs: i64,
    /// Rough per-file upload overhead avoided, at 2.5 files per second.
    pub estimated_seconds_saved: u64,
}

/// Returns a suggestion when the included file count and age meet policy.
pub fn archive_suggestion(
    name: String,
    file_count: u64,
    bytes: u64,
    last_changed_unix_secs: i64,
    now_unix_secs: i64,
) -> Option<ArchiveSuggestion> {
    let cutoff = DateTime::from_timestamp(now_unix_secs, 0)?
        .checked_sub_months(Months::new(ARCHIVE_MIN_AGE_MONTHS))?
        .timestamp();
    if file_count < ARCHIVE_MIN_FILES || last_changed_unix_secs > cutoff {
        return None;
    }
    Some(ArchiveSuggestion {
        name,
        file_count,
        bytes,
        last_changed_unix_secs,
        estimated_seconds_saved: (u128::from(file_count) * 2 / 5) as u64,
    })
}
