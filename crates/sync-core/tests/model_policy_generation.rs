//! Unit and property tests for `sync-core` model, policies, and generation
//! fingerprinting.

use std::collections::BTreeSet;
use std::path::PathBuf;

use proptest::prelude::*;
use sync_core::{
    ACCESS_MARKER_FILENAME, ACCESS_MARKER_HEADER, AccessMarker, AccessMarkerError, AccountId,
    CloudCollection, ComparePolicy, ConfigGeneration, ConflictLoserNaming, ConflictPolicy,
    ConflictResolution, DEFAULT_ACTIVE_SCHEDULE_INTERVAL, DEFAULT_EXCLUSIONS,
    DEFAULT_MAX_DELETE_PERCENT, DeviceId, EngineCompatibility, GenerationId, Intent, LocalBinding,
    Mapping, MappingId, MaxDeletePercent, RecoveryPolicy, RunId, SUPPORTED_FILE_POLICY_TABLE,
    SupportedItemKind, SupportedTypesPolicy, ValidationErrorKind, VolumeIdentity, VolumeKind,
    default_exclusion_patterns,
};

fn sample_generation() -> ConfigGeneration {
    ConfigGeneration {
        path1: PathBuf::from("/Volumes/WorkSSD/Research"),
        path2: "household:Data/Research".to_owned(),
        remote_root_folder_id: "root-folder-id-001".to_owned(),
        selected_folder_id: "research-folder-id-002".to_owned(),
        local_binding: LocalBinding {
            volume: VolumeIdentity::new(VolumeKind::External, "APFS-UUID-WORK-001"),
            mount_path: PathBuf::from("/Volumes/WorkSSD"),
            relative_dir: PathBuf::from("Research"),
        },
        effective_filters: default_exclusion_patterns(),
        compare_policy: ComparePolicy::SizeModtime,
        supported_types: SupportedTypesPolicy::default(),
        conflict_policy: ConflictPolicy::default(),
        recovery_policy: RecoveryPolicy::default(),
        engine_compatibility: EngineCompatibility {
            engine_name: "rclone".to_owned(),
            engine_version: "1.76.0".to_owned(),
            profile_version: "v1".to_owned(),
        },
    }
}

#[test]
fn identifiers_and_mapping_roundtrip_serde() {
    let mapping = Mapping {
        id: MappingId::new("map-research"),
        account_id: AccountId::new("acct-1"),
        collection: CloudCollection {
            display_path: "Data/Research".to_owned(),
            folder_id: "drv-research".to_owned(),
            ancestor_ids: vec!["drv-root".to_owned(), "drv-data".to_owned()],
        },
        local_binding: LocalBinding {
            volume: VolumeIdentity::new(VolumeKind::External, "vol-uuid-1"),
            mount_path: PathBuf::from("/Volumes/WorkSSD"),
            relative_dir: PathBuf::from("Research"),
        },
        local_recovery_root: PathBuf::from("/Volumes/WorkSSD/FolderSyncRecovery/research"),
        cloud_recovery_path: "household:Recovery/research".to_owned(),
        schedule_interval: DEFAULT_ACTIVE_SCHEDULE_INTERVAL,
        intent: Intent::Enabled,
    };

    assert_eq!(mapping.display_name(), "Research");
    assert_eq!(
        mapping.local_binding.resolved_path(),
        PathBuf::from("/Volumes/WorkSSD/Research")
    );
    assert_eq!(mapping.local_binding.volume_display_name(), "WorkSSD");

    let dev = DeviceId::new("dev-laptop");
    let gen_id = GenerationId::new("gen-1");
    let run_id = RunId::new("run-42");
    assert_eq!(dev.as_str(), "dev-laptop");
    assert_eq!(gen_id.as_str(), "gen-1");
    assert_eq!(run_id.as_str(), "run-42");

    let json = serde_json::to_string(&mapping).expect("serialize mapping");
    let restored: Mapping = serde_json::from_str(&json).expect("deserialize mapping");
    assert_eq!(restored, mapping);
}

#[test]
fn policy_defaults_and_labels_match_plan() {
    let pct = MaxDeletePercent::default();
    assert_eq!(pct.value(), DEFAULT_MAX_DELETE_PERCENT);
    assert_eq!(pct.value(), 10);
    assert_eq!(pct.label(), "Deletion limit: 10% of tracked files");

    assert!(MaxDeletePercent::new(0).is_ok());
    assert!(MaxDeletePercent::new(100).is_ok());
    let err = MaxDeletePercent::new(101).expect_err("101% must be rejected");
    assert_eq!(
        err.kind,
        ValidationErrorKind::InvalidMaxDeletePercent { value: 101 }
    );
    assert!(!err.cause.is_empty());
    assert!(!err.next_step.is_empty());

    // Deserialization must enforce the same 0..=100 validation as MaxDeletePercent::new.
    assert_eq!(
        serde_json::from_str::<MaxDeletePercent>("0")
            .unwrap()
            .value(),
        0
    );
    assert_eq!(
        serde_json::from_str::<MaxDeletePercent>("100")
            .unwrap()
            .value(),
        100
    );
    assert!(serde_json::from_str::<MaxDeletePercent>("101").is_err());
    assert!(serde_json::from_str::<MaxDeletePercent>("255").is_err());

    assert_eq!(ComparePolicy::default(), ComparePolicy::SizeModtime);
    let conflict = ConflictPolicy::default();
    assert_eq!(conflict.resolution, ConflictResolution::PreserveBoth);
    assert_eq!(conflict.loser, ConflictLoserNaming::Numbered);

    let supported = SupportedTypesPolicy::default();
    assert!(supported.skip_google_native_docs);
    assert!(supported.skip_drive_shortcuts);
    assert!(!supported.follow_symlinks);
    assert!(!supported.sync_empty_dirs);
}

#[test]
fn supported_file_policy_table_covers_all_ten_plan_categories() {
    assert_eq!(SUPPORTED_FILE_POLICY_TABLE.len(), 10);
    let kinds: BTreeSet<SupportedItemKind> =
        SUPPORTED_FILE_POLICY_TABLE.iter().map(|e| e.kind).collect();
    for expected in [
        SupportedItemKind::OrdinaryFiles,
        SupportedItemKind::Symlinks,
        SupportedItemKind::HardLinks,
        SupportedItemKind::DriveShortcuts,
        SupportedItemKind::GoogleNativeDocuments,
        SupportedItemKind::EmptyDirectories,
        SupportedItemKind::PlatformMetadata,
        SupportedItemKind::ActiveDatabasesAndLibraries,
        SupportedItemKind::OtherSyncProviderTrees,
        SupportedItemKind::AmbiguousOrIncompatibleNames,
    ] {
        assert!(kinds.contains(&expected), "missing policy row {expected:?}");
    }
    for entry in SUPPORTED_FILE_POLICY_TABLE {
        assert!(!entry.item.is_empty());
        assert!(!entry.summary.is_empty());
    }
}

#[test]
fn default_exclusions_are_small_and_documented() {
    assert!(!DEFAULT_EXCLUSIONS.is_empty());
    assert!(DEFAULT_EXCLUSIONS.len() <= 15);
    for entry in DEFAULT_EXCLUSIONS {
        assert!(!entry.pattern.is_empty());
        assert!(!entry.reason.is_empty());
    }
    let set = default_exclusion_patterns();
    assert!(set.contains(".DS_Store"));
    assert_eq!(set.len(), DEFAULT_EXCLUSIONS.len());
}

#[test]
fn access_marker_roundtrip_and_unrecognized_data_rejection() {
    assert_eq!(ACCESS_MARKER_FILENAME, "RCLONE_TEST");
    let collection = CloudCollection {
        display_path: "Data/Research".to_owned(),
        folder_id: "folder-abc-123".to_owned(),
        ancestor_ids: vec!["root-1".to_owned()],
    };
    let other_collection = CloudCollection {
        display_path: "Data/Family".to_owned(),
        folder_id: "folder-xyz-999".to_owned(),
        ancestor_ids: vec!["root-1".to_owned()],
    };

    let marker = AccessMarker::from_collection(&collection);
    let formatted = marker.format();
    assert!(formatted.starts_with(ACCESS_MARKER_HEADER));
    assert!(formatted.contains("folder-abc-123"));
    assert!(formatted.contains("Data/Research"));

    let parsed = AccessMarker::parse(&formatted).expect("parse valid marker");
    assert_eq!(parsed, marker);
    assert!(parsed.matches_collection(&collection));
    assert!(!parsed.matches_collection(&other_collection));

    // Unrecognized user content must never be accepted or overwritten.
    assert_eq!(
        AccessMarker::parse("my personal notes"),
        Err(AccessMarkerError::UnrecognizedFormat)
    );
    assert_eq!(
        AccessMarker::parse(""),
        Err(AccessMarkerError::UnrecognizedFormat)
    );
    assert!(matches!(
        AccessMarker::parse("FOLDERSYNC-ACCESS-MARKER-V99\ncollection_id=a\ncollection_path=b\n"),
        Err(AccessMarkerError::UnsupportedVersion(_))
    ));
    assert_eq!(
        AccessMarker::parse("FOLDERSYNC-ACCESS-MARKER-V1\ncollection_id=\ncollection_path=b\n"),
        Err(AccessMarkerError::EmptyField("collection_id"))
    );
    assert_eq!(
        AccessMarker::parse("FOLDERSYNC-ACCESS-MARKER-V1\ncollection_id=a\n"),
        Err(AccessMarkerError::MissingField("collection_path"))
    );
    assert!(matches!(
        AccessMarker::parse("FOLDERSYNC-ACCESS-MARKER-V1\nnot-a-kv-line\n"),
        Err(AccessMarkerError::MalformedLine(_))
    ));
}

#[test]
fn generation_fingerprint_is_sensitive_to_every_frozen_field() {
    let base = sample_generation();
    let base_fp = base.fingerprint();
    assert_eq!(base_fp.to_hex().len(), 64);

    // Identical clone gives identical fingerprint.
    assert_eq!(base.clone().fingerprint(), base_fp);

    // JSON roundtrip of fingerprint works cleanly, and non-ASCII 64-byte strings are rejected cleanly.
    let fp_json = serde_json::to_string(&base_fp).expect("serialize fp");
    let fp_back: sync_core::GenerationFingerprint =
        serde_json::from_str(&fp_json).expect("deserialize fp");
    assert_eq!(fp_back, base_fp);

    let non_ascii_64_bytes = format!("\"{}\"", "é".repeat(32));
    assert!(serde_json::from_str::<sync_core::GenerationFingerprint>(&non_ascii_64_bytes).is_err());

    // Mutate each frozen field one by one and confirm the fingerprint changes.
    let mut g = base.clone();
    g.path1 = PathBuf::from("/Volumes/WorkSSD/Research2");
    assert_ne!(g.fingerprint(), base_fp, "path1 change");

    let mut g = base.clone();
    g.path2 = "household-renamed:Data/Research".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "path2/remote alias change");

    let mut g = base.clone();
    g.remote_root_folder_id = "root-folder-id-999".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "remote_root_folder_id change");

    let mut g = base.clone();
    g.selected_folder_id = "research-folder-id-999".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "selected_folder_id change");

    let mut g = base.clone();
    g.local_binding.volume.kind = VolumeKind::Internal;
    assert_ne!(g.fingerprint(), base_fp, "volume kind change");

    let mut g = base.clone();
    g.local_binding.volume.stable_id = "APFS-UUID-OTHER".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "volume stable_id change");

    let mut g = base.clone();
    g.local_binding.mount_path = PathBuf::from("/Volumes/OtherSSD");
    assert_ne!(g.fingerprint(), base_fp, "mount_path change");

    let mut g = base.clone();
    g.local_binding.relative_dir = PathBuf::from("OtherDir");
    assert_ne!(g.fingerprint(), base_fp, "relative_dir change");

    let mut g = base.clone();
    g.effective_filters.insert("*.iso".to_owned());
    assert_ne!(g.fingerprint(), base_fp, "effective_filters change");

    let mut g = base.clone();
    g.supported_types.skip_google_native_docs = false;
    assert_ne!(g.fingerprint(), base_fp, "skip_google_native_docs change");

    let mut g = base.clone();
    g.supported_types.skip_drive_shortcuts = false;
    assert_ne!(g.fingerprint(), base_fp, "skip_drive_shortcuts change");

    let mut g = base.clone();
    g.supported_types.follow_symlinks = true;
    assert_ne!(g.fingerprint(), base_fp, "follow_symlinks change");

    let mut g = base.clone();
    g.supported_types.sync_empty_dirs = true;
    assert_ne!(g.fingerprint(), base_fp, "sync_empty_dirs change");

    let mut g = base.clone();
    g.recovery_policy.keep_recovery_copies = false;
    assert_ne!(g.fingerprint(), base_fp, "keep_recovery_copies change");

    let mut g = base.clone();
    g.recovery_policy.max_delete_percent = MaxDeletePercent::new(25).unwrap();
    assert_ne!(g.fingerprint(), base_fp, "max_delete_percent change");

    let mut g = base.clone();
    g.engine_compatibility.engine_name = "other-engine".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "engine_name change");

    let mut g = base.clone();
    g.engine_compatibility.engine_version = "1.77.0".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "engine_version change");

    let mut g = base.clone();
    g.engine_compatibility.profile_version = "v2".to_owned();
    assert_ne!(g.fingerprint(), base_fp, "profile_version change");
}

proptest! {
    #[test]
    fn fingerprint_is_invariant_under_filter_set_reordering(
        filters in prop::collection::vec("[a-zA-Z0-9._/*-]{1,20}", 0..15)
    ) {
        let mut g1 = sample_generation();
        g1.effective_filters = filters.iter().cloned().collect();

        let mut reversed = filters;
        reversed.reverse();
        let mut g2 = sample_generation();
        g2.effective_filters = reversed.into_iter().collect();

        prop_assert_eq!(g1.fingerprint(), g2.fingerprint());
    }

    #[test]
    fn fingerprint_distinguishes_adjacent_field_boundaries(
        a in "[a-z0-9]{1,10}",
        b in "[a-z0-9]{1,10}",
        split in 1usize..10
    ) {
        let combined = format!("{a}{b}");
        if split < combined.len() && split != a.len() {
            let (a2, b2) = combined.split_at(split);
            let mut g1 = sample_generation();
            g1.remote_root_folder_id = a;
            g1.selected_folder_id = b;

            let mut g2 = sample_generation();
            g2.remote_root_folder_id = a2.to_owned();
            g2.selected_folder_id = b2.to_owned();

            prop_assert_ne!(g1.fingerprint(), g2.fingerprint());
        }
    }
}
