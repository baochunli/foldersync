//! Unit and property tests for local root, cloud ancestry, and filename
//! validation rules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use sync_core::{
    CloudCollection, DeviceCloudCollection, DeviceId, LocalBinding, LocalLayoutEntry,
    MAX_COMPONENT_BYTES, MappingId, NameFindingKind, PathResolutionError, PathResolver,
    StdPathResolver, ValidationErrorKind, VolumeIdentity, VolumeKind,
    validate_cloud_collection_pair, validate_cloud_collections, validate_local_binding,
    validate_local_layout, validate_local_roots_disjoint, validate_names,
};

#[derive(Default)]
struct FakePathResolver {
    rewrites: BTreeMap<PathBuf, PathBuf>,
}

impl FakePathResolver {
    fn with_rewrite(mut self, from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Self {
        self.rewrites.insert(from.into(), to.into());
        self
    }
}

impl PathResolver for FakePathResolver {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, PathResolutionError> {
        if let Some(mapped) = self.rewrites.get(path) {
            return Ok(mapped.clone());
        }
        // Apply longest prefix rewrite to simulate directory symlinks.
        let mut best: Option<(&PathBuf, &PathBuf)> = None;
        for (from, to) in &self.rewrites {
            if path.starts_with(from)
                && best
                    .is_none_or(|(prev, _)| from.components().count() > prev.components().count())
            {
                best = Some((from, to));
            }
        }
        if let Some((from, to)) = best {
            let suffix = path.strip_prefix(from).expect("starts_with matched");
            return Ok(to.join(suffix));
        }
        Ok(path.to_path_buf())
    }
}

fn binding(mount: &str, rel: &str, vol_id: &str) -> LocalBinding {
    LocalBinding {
        volume: VolumeIdentity::new(VolumeKind::External, vol_id),
        mount_path: PathBuf::from(mount),
        relative_dir: PathBuf::from(rel),
    }
}

#[test]
fn rejects_volume_root_as_data_root() {
    let resolver = FakePathResolver::default();

    for bad in [
        binding("/Volumes/WorkSSD", "", "vol-1"),
        binding("/Volumes/WorkSSD", ".", "vol-1"),
        binding("/", "", "vol-root"),
        binding("/Volumes/WorkSSD", "../Other", "vol-1"),
        binding("/Volumes/WorkSSD", "/abs/path", "vol-1"),
    ] {
        let err = validate_local_binding(&bad, &resolver).expect_err("must reject volume root");
        assert!(matches!(
            err.kind,
            ValidationErrorKind::VolumeRootDisallowed { .. }
        ));
        assert!(!err.cause.is_empty());
        assert!(!err.next_step.is_empty());
    }

    let good = binding("/Volumes/WorkSSD", "Research", "vol-1");
    let resolved = validate_local_binding(&good, &resolver).expect("valid subdir");
    assert_eq!(resolved, PathBuf::from("/Volumes/WorkSSD/Research"));
}

#[test]
fn detects_local_root_overlaps_including_canonicalized_symlinks() {
    let resolver = FakePathResolver::default()
        .with_rewrite("/Volumes/AliasSSD", "/Volumes/WorkSSD")
        .with_rewrite("/Volumes/LinkResearch", "/Volumes/WorkSSD/Research");

    // Equal via symlink
    let err = validate_local_roots_disjoint(
        Path::new("/Volumes/WorkSSD/Research"),
        Path::new("/Volumes/LinkResearch"),
        &resolver,
    )
    .expect_err("equal via symlink must fail");
    assert!(matches!(
        err.kind,
        ValidationErrorKind::LocalRootOverlap { .. }
    ));

    // Ancestor / descendant via volume alias
    assert!(
        validate_local_roots_disjoint(
            Path::new("/Volumes/WorkSSD/Research"),
            Path::new("/Volumes/AliasSSD/Research/Sub"),
            &resolver,
        )
        .is_err()
    );
    assert!(
        validate_local_roots_disjoint(
            Path::new("/Volumes/AliasSSD/Research/Sub"),
            Path::new("/Volumes/WorkSSD/Research"),
            &resolver,
        )
        .is_err()
    );

    // Prefix-similar sibling names must NOT collide
    assert!(
        validate_local_roots_disjoint(
            Path::new("/Volumes/WorkSSD/Research"),
            Path::new("/Volumes/WorkSSD/Research-Archive"),
            &resolver,
        )
        .is_ok()
    );
}

#[test]
fn rejects_recovery_and_app_state_dirs_inside_sync_trees() {
    let resolver = FakePathResolver::default();
    let app_state = Path::new("/Users/example/Library/Application Support/FolderSync");

    // Recovery inside own sync tree
    let entries = vec![LocalLayoutEntry {
        mapping_id: MappingId::new("m1"),
        local_binding: binding("/Volumes/WorkSSD", "Research", "vol-1"),
        local_recovery_root: PathBuf::from("/Volumes/WorkSSD/Research/Recovery"),
    }];
    let errs = validate_local_layout(&entries, app_state, &resolver).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ValidationErrorKind::RecoveryDirOverlap { .. }))
    );

    // Recovery of mapping 1 inside mapping 2's sync tree
    let entries = vec![
        LocalLayoutEntry {
            mapping_id: MappingId::new("m1"),
            local_binding: binding("/Volumes/WorkSSD", "Research", "vol-1"),
            local_recovery_root: PathBuf::from("/Volumes/WorkSSD/Family/Recovery"),
        },
        LocalLayoutEntry {
            mapping_id: MappingId::new("m2"),
            local_binding: binding("/Volumes/WorkSSD", "Family", "vol-1"),
            local_recovery_root: PathBuf::from("/Volumes/WorkSSD/FolderSyncRecovery/family"),
        },
    ];
    let errs = validate_local_layout(&entries, app_state, &resolver).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ValidationErrorKind::RecoveryDirOverlap { .. }))
    );

    // App state inside sync tree
    let entries = vec![LocalLayoutEntry {
        mapping_id: MappingId::new("m-home"),
        local_binding: binding("/", "Users/example/Library", "vol-internal"),
        local_recovery_root: PathBuf::from("/Users/example/FolderSyncRecovery/lib"),
    }];
    let errs = validate_local_layout(&entries, app_state, &resolver).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ValidationErrorKind::AppStateDirOverlap { .. }))
    );

    // Sync tree inside app state directory
    let entries = vec![LocalLayoutEntry {
        mapping_id: MappingId::new("m-inside-state"),
        local_binding: binding(
            "/",
            "Users/example/Library/Application Support/FolderSync/Data",
            "vol-internal",
        ),
        local_recovery_root: PathBuf::from("/Users/example/FolderSyncRecovery/state"),
    }];
    let errs = validate_local_layout(&entries, app_state, &resolver).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ValidationErrorKind::AppStateDirOverlap { .. }))
    );

    // Sync tree inside recovery directory
    let entries = vec![LocalLayoutEntry {
        mapping_id: MappingId::new("m-inside-recovery"),
        local_binding: binding("/Volumes/WorkSSD", "FolderSyncRecovery/Research", "vol-1"),
        local_recovery_root: PathBuf::from("/Volumes/WorkSSD/FolderSyncRecovery"),
    }];
    let errs = validate_local_layout(&entries, app_state, &resolver).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ValidationErrorKind::RecoveryDirOverlap { .. }))
    );

    // Clean disjoint layout passes
    let clean = vec![
        LocalLayoutEntry {
            mapping_id: MappingId::new("m1"),
            local_binding: binding("/Volumes/WorkSSD", "Research", "vol-1"),
            local_recovery_root: PathBuf::from("/Volumes/WorkSSD/FolderSyncRecovery/research"),
        },
        LocalLayoutEntry {
            mapping_id: MappingId::new("m2"),
            local_binding: binding("/Volumes/WorkSSD", "Family", "vol-1"),
            local_recovery_root: PathBuf::from("/Volumes/WorkSSD/FolderSyncRecovery/family"),
        },
    ];
    assert!(validate_local_layout(&clean, app_state, &resolver).is_ok());
}

#[test]
fn std_path_resolver_canonicalizes_existing_temp_dir() {
    let resolver = StdPathResolver;
    let temp = std::env::temp_dir();
    let canonical = resolver.canonicalize(&temp).expect("canonicalize temp_dir");
    assert!(canonical.is_absolute());

    let missing = temp.join("foldersync-nonexistent-path-check-987654321");
    assert!(resolver.canonicalize(&missing).is_err());
}

#[test]
fn cloud_validation_enforces_non_nesting_by_ids_and_allows_cross_device_same_collection() {
    let research_laptop = DeviceCloudCollection {
        device_id: DeviceId::new("laptop"),
        collection: CloudCollection {
            display_path: "Data/Research".to_owned(),
            folder_id: "id-research".to_owned(),
            ancestor_ids: vec!["id-root".to_owned(), "id-data".to_owned()],
        },
    };
    let research_desktop = DeviceCloudCollection {
        device_id: DeviceId::new("desktop"),
        collection: CloudCollection {
            display_path: "Data/Research".to_owned(),
            folder_id: "id-research".to_owned(),
            ancestor_ids: vec!["id-root".to_owned(), "id-data".to_owned()],
        },
    };
    // Same collection on different devices is allowed.
    assert!(validate_cloud_collection_pair(&research_laptop, &research_desktop).is_ok());

    // Identical collection on the same device is rejected.
    let err = validate_cloud_collection_pair(&research_laptop, &research_laptop)
        .expect_err("duplicate on same device");
    assert!(matches!(
        err.kind,
        ValidationErrorKind::CloudDuplicateCollectionOnSameDevice { .. }
    ));

    // Nested collection detected by folder ID ancestry EVEN WHEN display_path was renamed!
    let renamed_nested = DeviceCloudCollection {
        device_id: DeviceId::new("desktop"),
        collection: CloudCollection {
            display_path: "UnrelatedName/SubAlbum".to_owned(),
            folder_id: "id-sub".to_owned(),
            ancestor_ids: vec![
                "id-root".to_owned(),
                "id-data".to_owned(),
                "id-research".to_owned(),
            ],
        },
    };
    let err = validate_cloud_collection_pair(&research_laptop, &renamed_nested)
        .expect_err("nesting by folder_id must be rejected across devices");
    assert!(matches!(
        err.kind,
        ValidationErrorKind::CloudCollectionNesting { .. }
    ));
    // Symmetric check
    assert!(validate_cloud_collection_pair(&renamed_nested, &research_laptop).is_err());

    // Distinct folder IDs with same display_path (ambiguous Drive duplicate) rejected
    let dup_name = DeviceCloudCollection {
        device_id: DeviceId::new("desktop"),
        collection: CloudCollection {
            display_path: "Data/Research".to_owned(),
            folder_id: "id-research-duplicate".to_owned(),
            ancestor_ids: vec!["id-root".to_owned(), "id-data".to_owned()],
        },
    };
    let err = validate_cloud_collection_pair(&research_laptop, &dup_name)
        .expect_err("ambiguous duplicate display path");
    assert!(matches!(
        err.kind,
        ValidationErrorKind::CloudDuplicateDisplayPath { .. }
    ));

    // Sibling collections are allowed
    let family = DeviceCloudCollection {
        device_id: DeviceId::new("laptop"),
        collection: CloudCollection {
            display_path: "Data/Family".to_owned(),
            folder_id: "id-family".to_owned(),
            ancestor_ids: vec!["id-root".to_owned(), "id-data".to_owned()],
        },
    };
    assert!(validate_cloud_collections(&[research_laptop, research_desktop, family]).is_ok());
}

#[test]
fn name_validation_reports_all_findings_without_picking_a_winner() {
    let nfc = "notes/caf\u{00E9}.txt".to_owned();
    let nfd = "notes/cafe\u{0301}.txt".to_owned();
    let overlong_component = "a".repeat(MAX_COMPONENT_BYTES + 1);
    let overlong_path = format!("docs/{overlong_component}");

    let paths = vec![
        "docs/Report.pdf".to_owned(),
        "docs/report.pdf".to_owned(),
        "Photos/a.jpg".to_owned(),
        "photos/b.jpg".to_owned(),
        nfc.clone(),
        nfd.clone(),
        overlong_path.clone(),
        "bad/file:name.txt".to_owned(),
        "bad/ctrl\x07char.txt".to_owned(),
        "bad/../escape.txt".to_owned(),
    ];

    let findings = validate_names(&paths);

    // Case-only collision on docs/Report.pdf vs docs/report.pdf
    assert!(findings.iter().any(|f| {
        matches!(f.kind, NameFindingKind::CaseOnlyCollision { .. })
            && f.paths.contains(&"docs/Report.pdf".to_owned())
            && f.paths.contains(&"docs/report.pdf".to_owned())
    }));

    // Directory-level case-only collision on Photos vs photos
    assert!(findings.iter().any(|f| {
        matches!(f.kind, NameFindingKind::CaseOnlyCollision { .. })
            && f.paths.contains(&"Photos".to_owned())
            && f.paths.contains(&"photos".to_owned())
    }));

    // Unicode NFC/NFD collision
    assert!(findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::UnicodeNormalizationCollision { .. }
        ) && f.paths.contains(&nfc)
            && f.paths.contains(&nfd)
    }));

    // Over-long component
    assert!(findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::OverLongComponent {
                byte_len,
                max_bytes,
                ..
            } if byte_len == MAX_COMPONENT_BYTES + 1 && max_bytes == MAX_COMPONENT_BYTES
        ) && f.paths == vec![overlong_path.clone()]
    }));

    // Reserved characters (: and control char)
    assert!(findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::ReservedOrUnsupportedCharacter { character: ':', .. }
        )
    }));
    assert!(findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::ReservedOrUnsupportedCharacter {
                character: '\x07',
                ..
            }
        )
    }));

    // Reserved component (..)
    assert!(findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::ReservedPathComponent { ref component } if component == ".."
        )
    }));

    // Every finding has non-empty cause and actionable next_step
    for finding in &findings {
        assert!(!finding.cause.is_empty());
        assert!(!finding.next_step.is_empty());
    }

    // Clean paths yield zero findings
    let clean = [
        "docs/Report 2026.pdf",
        "docs/Notes_v1.md",
        "photos/IMG_0001.JPG",
    ];
    assert!(validate_names(&clean).is_empty());
}

#[test]
fn name_validation_reports_exact_duplicates_without_flagging_shared_prefixes() {
    let paths = [
        "notes/report.txt",
        "notes/report.txt",
        "notes/other.txt",
        "shared/a.txt",
        "shared/b.txt",
    ];
    let findings = validate_names(&paths);

    assert!(findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::ExactDuplicateName {
                ref normalized_path,
                occurrences: 2
            } if normalized_path == "notes/report.txt"
        ) && f.paths == vec!["notes/report.txt".to_owned(), "notes/report.txt".to_owned()]
    }));
    assert!(!findings.iter().any(|f| {
        matches!(
            f.kind,
            NameFindingKind::ExactDuplicateName {
                ref normalized_path,
                ..
            } if normalized_path == "shared" || normalized_path == "notes"
        )
    }));
}

#[test]
fn name_validation_uses_unicode_caseless_matching() {
    let greek = [
        "docs/\u{03A3}.txt",
        "docs/\u{03C2}.txt",
        "docs/\u{03C3}.txt",
    ];
    let findings = validate_names(&greek);
    assert!(
        findings.iter().any(|f| {
            matches!(f.kind, NameFindingKind::CaseOnlyCollision { .. })
                && f.paths.contains(&"docs/\u{03A3}.txt".to_owned())
                && f.paths.contains(&"docs/\u{03C2}.txt".to_owned())
                && f.paths.contains(&"docs/\u{03C3}.txt".to_owned())
        }),
        "expected Σ/ς/σ to collide under Unicode caseless matching, got: {findings:?}"
    );

    let german = ["docs/stra\u{00DF}e.txt", "docs/STRASSE.txt"];
    let findings = validate_names(&german);
    assert!(
        findings.iter().any(|f| {
            matches!(
                f.kind,
                NameFindingKind::CaseOnlyCollision { ref normalized_key }
                    if normalized_key == "docs/strasse.txt"
            ) && f.paths.contains(&"docs/stra\u{00DF}e.txt".to_owned())
                && f.paths.contains(&"docs/STRASSE.txt".to_owned())
        }),
        "expected ß/SS to collide with folded key docs/strasse.txt, got: {findings:?}"
    );

    let greek_final = ["docs/\u{039F}\u{03A3}", "docs/\u{03BF}\u{03C3}"];
    let findings = validate_names(&greek_final);
    assert!(
        findings.iter().any(|f| {
            matches!(
                f.kind,
                NameFindingKind::CaseOnlyCollision { ref normalized_key }
                    if normalized_key == "docs/\u{03BF}\u{03C3}"
            ) && f.paths.contains(&"docs/\u{039F}\u{03A3}".to_owned())
                && f.paths.contains(&"docs/\u{03BF}\u{03C3}".to_owned())
        }),
        "expected ΟΣ/οσ to report case-folded key docs/οσ, got: {findings:?}"
    );
}

fn path_segments_strategy() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec("[a-z0-9_-]{1,8}", 1..5).prop_map(|segs| {
        let mut pb = PathBuf::from("/Volumes/WorkSSD");
        for s in segs {
            pb.push(s);
        }
        pb
    })
}

proptest! {
    #[test]
    fn local_overlap_check_is_symmetric_and_canonicalization_aware(
        p1 in path_segments_strategy(),
        p2 in path_segments_strategy(),
        use_alias_for_first in any::<bool>(),
        use_alias_for_second in any::<bool>(),
    ) {
        let resolver = FakePathResolver::default()
            .with_rewrite("/Volumes/AliasLink", "/Volumes/WorkSSD");

        let input1 = if use_alias_for_first {
            PathBuf::from("/Volumes/AliasLink").join(p1.strip_prefix("/Volumes/WorkSSD").unwrap())
        } else {
            p1.clone()
        };
        let input2 = if use_alias_for_second {
            PathBuf::from("/Volumes/AliasLink").join(p2.strip_prefix("/Volumes/WorkSSD").unwrap())
        } else {
            p2.clone()
        };

        let ab = validate_local_roots_disjoint(&input1, &input2, &resolver);
        let ba = validate_local_roots_disjoint(&input2, &input1, &resolver);
        let canonical = validate_local_roots_disjoint(&p1, &p2, &resolver);

        prop_assert_eq!(ab.is_ok(), ba.is_ok());
        prop_assert_eq!(ab.is_ok(), canonical.is_ok());
    }

    #[test]
    fn cloud_overlap_check_is_symmetric(
        dev_a in prop::sample::select(vec!["dev-1", "dev-2"]),
        dev_b in prop::sample::select(vec!["dev-1", "dev-2"]),
        id_a in prop::sample::select(vec!["id-1", "id-2", "id-3"]),
        id_b in prop::sample::select(vec!["id-1", "id-2", "id-3"]),
        path_a in prop::sample::select(vec!["Data/A", "Data/A/Sub", "Data/B"]),
        path_b in prop::sample::select(vec!["Data/A", "Data/A/Sub", "Data/B"]),
        a_has_b_ancestor in any::<bool>(),
        b_has_a_ancestor in any::<bool>(),
    ) {
        let mut anc_a = vec!["id-root".to_owned()];
        if a_has_b_ancestor && id_a != id_b {
            anc_a.push(id_b.to_owned());
        }
        let mut anc_b = vec!["id-root".to_owned()];
        if b_has_a_ancestor && id_a != id_b {
            anc_b.push(id_a.to_owned());
        }
        let first = DeviceCloudCollection {
            device_id: DeviceId::new(dev_a),
            collection: CloudCollection {
                display_path: path_a.to_owned(),
                folder_id: id_a.to_owned(),
                ancestor_ids: anc_a,
            },
        };
        let second = DeviceCloudCollection {
            device_id: DeviceId::new(dev_b),
            collection: CloudCollection {
                display_path: path_b.to_owned(),
                folder_id: id_b.to_owned(),
                ancestor_ids: anc_b,
            },
        };
        prop_assert_eq!(
            validate_cloud_collection_pair(&first, &second).is_ok(),
            validate_cloud_collection_pair(&second, &first).is_ok()
        );
    }
}
