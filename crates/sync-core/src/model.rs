//! Domain identifiers and persistent mapping record types.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::Intent;

/// Default remote polling interval for small active mappings (5 minutes).
pub const DEFAULT_ACTIVE_SCHEDULE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Default remote polling interval for large archival mappings (15 minutes).
pub const DEFAULT_ARCHIVE_SCHEDULE_INTERVAL: Duration = Duration::from_secs(15 * 60);

macro_rules! define_id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a new identifier wrapping the given string value.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Returns the underlying string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Returns `true` if the identifier has no non-whitespace content.
            pub fn is_empty(&self) -> bool {
                self.0.trim().is_empty()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

define_id_newtype!(
    /// Stable identifier for a computer participating in household sync.
    DeviceId
);

define_id_newtype!(
    /// Stable identifier for a configured cloud account.
    AccountId
);

define_id_newtype!(
    /// Stable identifier for a single local-to-cloud folder mapping.
    MappingId
);

define_id_newtype!(
    /// Stable identifier for an immutable configuration generation.
    GenerationId
);

define_id_newtype!(
    /// Stable identifier for a single synchronization execution run.
    RunId
);

/// Shared Google Drive folder selected as a synchronization collection.
///
/// Stores both the human-readable path under the shared parent root and the
/// actual Google Drive folder ID plus ancestor folder IDs so nesting and
/// rename checks rely on real object identity rather than path strings alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CloudCollection {
    /// Human-readable relative path under the remote root (for example, `Data/Research`).
    pub display_path: String,
    /// Actual Google Drive object ID of this collection folder.
    pub folder_id: String,
    /// Actual Google Drive object IDs of ancestor folders above this collection.
    pub ancestor_ids: Vec<String>,
}

impl CloudCollection {
    /// Returns the short leaf folder name derived from `display_path`.
    pub fn display_name(&self) -> &str {
        self.normalized_segments()
            .last()
            .copied()
            .unwrap_or(self.display_path.as_str())
    }

    /// Splits `display_path` into non-empty path segments separated by `/`.
    pub fn normalized_segments(&self) -> Vec<&str> {
        self.display_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect()
    }
}

/// Classification of the storage volume hosting a local mapping directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum VolumeKind {
    /// Internal system or fixed storage volume.
    Internal,
    /// External or removable storage volume (for example, a USB SSD).
    External,
}

/// Opaque persistent identity of a local filesystem volume.
///
/// Platform-specific probing lives outside `sync-core`; this type carries only
/// the volume classification and its stable identifier string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VolumeIdentity {
    /// Whether this volume is internal or external/removable.
    pub kind: VolumeKind,
    /// Opaque stable identifier (for example, an APFS volume UUID).
    pub stable_id: String,
}

impl VolumeIdentity {
    /// Creates a new volume identity from its kind and stable identifier.
    pub fn new(kind: VolumeKind, stable_id: impl Into<String>) -> Self {
        Self {
            kind,
            stable_id: stable_id.into(),
        }
    }
}

/// Binding of a local directory to a specific volume identity, mount point,
/// and relative path within that volume.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalBinding {
    /// Persistent identity of the backing volume.
    pub volume: VolumeIdentity,
    /// Observed mount path of the volume (for example, `/Volumes/WorkSSD` or `/`).
    pub mount_path: PathBuf,
    /// Relative directory inside the volume (never empty or a volume root).
    pub relative_dir: PathBuf,
}

impl LocalBinding {
    /// Returns the combined local path (`mount_path.join(&relative_dir)`).
    pub fn resolved_path(&self) -> PathBuf {
        self.mount_path.join(&self.relative_dir)
    }

    /// Returns a human-readable volume label derived from `mount_path`.
    pub fn volume_display_name(&self) -> String {
        self.mount_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.mount_path.to_string_lossy().into_owned())
    }
}

/// Complete configuration record pairing one cloud collection with one local
/// directory on a computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// Unique identifier for this mapping on this computer.
    pub id: MappingId,
    /// Account used to authenticate against the shared cloud parent.
    pub account_id: AccountId,
    /// Cloud collection paired with the local directory.
    pub collection: CloudCollection,
    /// Bound local volume and relative directory.
    pub local_binding: LocalBinding,
    /// Local directory root where per-run recovery copies are written.
    pub local_recovery_root: PathBuf,
    /// Cloud recovery path prefix under the same remote (for example, `household:Recovery/research`).
    pub cloud_recovery_path: String,
    /// Periodic synchronization interval.
    pub schedule_interval: Duration,
    /// Persisted user intent (enabled or paused).
    pub intent: Intent,
}

impl Mapping {
    /// Returns the short display name for this mapping derived from its cloud collection.
    pub fn display_name(&self) -> &str {
        self.collection.display_name()
    }
}
