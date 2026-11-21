//! Immutable configuration generations and deterministic SHA-256 fingerprinting.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::model::{LocalBinding, VolumeKind};
use crate::policy::{
    ComparePolicy, ConflictLoserNaming, ConflictPolicy, ConflictResolution, RecoveryPolicy,
    SupportedTypesPolicy,
};

/// Canonical serialization version prefix included in every generation fingerprint.
pub const GENERATION_FINGERPRINT_VERSION: &str = "foldersync.generation.v1";

/// Pinned synchronization engine compatibility metadata frozen in a generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EngineCompatibility {
    /// Engine name (for example, `rclone`).
    pub engine_name: String,
    /// Exact pinned engine version (for example, `1.76.0`).
    pub engine_version: String,
    /// Qualified command-profile compatibility tag (for example, `v1`).
    pub profile_version: String,
}

/// SHA-256 digest uniquely identifying a frozen [`ConfigGeneration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationFingerprint([u8; 32]);

impl GenerationFingerprint {
    /// Constructs a fingerprint from raw 32-byte SHA-256 output.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw 32-byte SHA-256 digest.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Formats the digest as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }
}

impl fmt::Display for GenerationFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for GenerationFingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for GenerationFingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        if hex.len() != 64 || !hex.is_ascii() {
            return Err(serde::de::Error::custom(
                "generation fingerprint hex string must be 64 ASCII hex characters",
            ));
        }
        let mut bytes = [0u8; 32];
        for (i, slot) in bytes.iter_mut().enumerate() {
            let chunk = &hex[i * 2..i * 2 + 2];
            *slot = u8::from_str_radix(chunk, 16).map_err(serde::de::Error::custom)?;
        }
        Ok(Self(bytes))
    }
}

/// Frozen configuration generation capturing every stable value that defines a
/// mapping's synchronization baseline.
///
/// Intentionally excludes OAuth tokens, current timestamps, and per-run backup
/// UUIDs so normal token refreshes and routine runs do not change the
/// fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigGeneration {
    /// Exact local Path1 argument passed to rclone bisync.
    pub path1: PathBuf,
    /// Exact remote Path2 argument passed to rclone bisync (including remote alias).
    pub path2: String,
    /// Actual Google Drive folder ID of the shared remote root.
    pub remote_root_folder_id: String,
    /// Actual Google Drive folder ID of the selected collection.
    pub selected_folder_id: String,
    /// Bound local volume identity, mount path, and relative directory.
    pub local_binding: LocalBinding,
    /// Canonical set of effective exclusion filter patterns for this generation.
    pub effective_filters: BTreeSet<String>,
    /// File comparison policy.
    pub compare_policy: ComparePolicy,
    /// Supported file-type switches.
    pub supported_types: SupportedTypesPolicy,
    /// Conflict preservation and naming policy.
    pub conflict_policy: ConflictPolicy,
    /// Recovery copy and deletion-guard policy.
    pub recovery_policy: RecoveryPolicy,
    /// Qualified engine version and profile compatibility.
    pub engine_compatibility: EngineCompatibility,
}

fn hash_bytes(hasher: &mut Sha256, tag: &str, bytes: &[u8]) {
    hasher.update(tag.as_bytes());
    hasher.update(b":");
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    hasher.update(b"\0");
}

fn hash_str(hasher: &mut Sha256, tag: &str, value: &str) {
    hash_bytes(hasher, tag, value.as_bytes());
}

fn hash_bool(hasher: &mut Sha256, tag: &str, value: bool) {
    hash_bytes(hasher, tag, &[u8::from(value)]);
}

impl ConfigGeneration {
    /// Computes the deterministic SHA-256 fingerprint over a canonical,
    /// versioned serialization of all frozen fields.
    pub fn fingerprint(&self) -> GenerationFingerprint {
        let mut hasher = Sha256::new();
        hash_str(&mut hasher, "version", GENERATION_FINGERPRINT_VERSION);

        hash_bytes(
            &mut hasher,
            "path1",
            self.path1.as_os_str().as_encoded_bytes(),
        );
        hash_str(&mut hasher, "path2", &self.path2);
        hash_str(
            &mut hasher,
            "remote_root_folder_id",
            &self.remote_root_folder_id,
        );
        hash_str(&mut hasher, "selected_folder_id", &self.selected_folder_id);

        let volume_kind = match self.local_binding.volume.kind {
            VolumeKind::Internal => "internal",
            VolumeKind::External => "external",
        };
        hash_str(&mut hasher, "local_binding.volume.kind", volume_kind);
        hash_str(
            &mut hasher,
            "local_binding.volume.stable_id",
            &self.local_binding.volume.stable_id,
        );
        hash_bytes(
            &mut hasher,
            "local_binding.mount_path",
            self.local_binding.mount_path.as_os_str().as_encoded_bytes(),
        );
        hash_bytes(
            &mut hasher,
            "local_binding.relative_dir",
            self.local_binding
                .relative_dir
                .as_os_str()
                .as_encoded_bytes(),
        );

        hash_bytes(
            &mut hasher,
            "effective_filters.len",
            &(self.effective_filters.len() as u64).to_be_bytes(),
        );
        for filter in &self.effective_filters {
            hash_str(&mut hasher, "effective_filters.item", filter);
        }

        let compare = match self.compare_policy {
            ComparePolicy::SizeModtime => "size,modtime",
        };
        hash_str(&mut hasher, "compare_policy", compare);

        hash_bool(
            &mut hasher,
            "supported_types.skip_google_native_docs",
            self.supported_types.skip_google_native_docs,
        );
        hash_bool(
            &mut hasher,
            "supported_types.skip_drive_shortcuts",
            self.supported_types.skip_drive_shortcuts,
        );
        hash_bool(
            &mut hasher,
            "supported_types.follow_symlinks",
            self.supported_types.follow_symlinks,
        );
        hash_bool(
            &mut hasher,
            "supported_types.sync_empty_dirs",
            self.supported_types.sync_empty_dirs,
        );

        let resolution = match self.conflict_policy.resolution {
            ConflictResolution::PreserveBoth => "none",
        };
        let loser = match self.conflict_policy.loser {
            ConflictLoserNaming::Numbered => "num",
        };
        hash_str(&mut hasher, "conflict_policy.resolution", resolution);
        hash_str(&mut hasher, "conflict_policy.loser", loser);

        hash_bool(
            &mut hasher,
            "recovery_policy.keep_recovery_copies",
            self.recovery_policy.keep_recovery_copies,
        );
        hash_bytes(
            &mut hasher,
            "recovery_policy.max_delete_percent",
            &[self.recovery_policy.max_delete_percent.value()],
        );

        hash_str(
            &mut hasher,
            "engine_compatibility.engine_name",
            &self.engine_compatibility.engine_name,
        );
        hash_str(
            &mut hasher,
            "engine_compatibility.engine_version",
            &self.engine_compatibility.engine_version,
        );
        hash_str(
            &mut hasher,
            "engine_compatibility.profile_version",
            &self.engine_compatibility.profile_version,
        );

        let digest: [u8; 32] = hasher.finalize().into();
        GenerationFingerprint::from_bytes(digest)
    }
}
