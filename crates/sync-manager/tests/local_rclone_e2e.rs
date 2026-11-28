use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use rclone_adapter::{InitPreference, pinned_binary_path};
use sha2::{Digest, Sha256};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccessMarker, Execution, MappingId, Readiness, RunOutcome,
    policy::SUGGESTED_EXCLUSIONS,
};
use sync_manager::{
    AddMappingCommand, CommandOutcome, InitializeCommand, ManagerConfig, ManagerError, SyncManager,
};
use tempfile::TempDir;

struct RealFixture {
    temp: TempDir,
    state_dir: PathBuf,
    local_dir: PathBuf,
    remote_dir: PathBuf,
    local_recovery: PathBuf,
    remote_recovery: PathBuf,
}

impl RealFixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let local_dir = tmp.path().join("local_research");
        let remote_dir = tmp.path().join("remote_research");
        let local_recovery = tmp.path().join("local_recovery");
        let remote_recovery = tmp.path().join("remote_recovery");
        fs::create_dir_all(&local_dir).unwrap();
        fs::create_dir_all(&remote_dir).unwrap();
        fs::create_dir_all(&local_recovery).unwrap();
        fs::create_dir_all(&remote_recovery).unwrap();
        Self {
            temp: tmp,
            state_dir,
            local_dir,
            remote_dir,
            local_recovery,
            remote_recovery,
        }
    }

    fn open_manager(&self) -> SyncManager {
        SyncManager::open(ManagerConfig::with_state_dir(&self.state_dir)).unwrap()
    }

    fn add_mapping_cmd(&self) -> AddMappingCommand {
        AddMappingCommand::local_fixture(
            "research",
            "local-1",
            "Data/Research",
            &self.local_dir,
            &self.remote_dir,
            &self.local_recovery,
            &self.remote_recovery,
        )
        .unwrap()
    }
}

fn require_pinned_rclone(test_name: &str) -> bool {
    if pinned_binary_path().is_none() {
        eprintln!(
            "Skipping {test_name}: pinned rclone binary is absent; run scripts/fetch-rclone.sh"
        );
        return false;
    }
    true
}

fn write_rel(root: &Path, rel: &str, bytes: &[u8]) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, bytes).unwrap();
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn tree_hashes(root: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if rel == ACCESS_MARKER_FILENAME {
                    continue;
                }
                out.insert(rel, sha256_bytes(&fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn suggested_directory_rules_exclude_root_and_nested_directories_only() {
    if !require_pinned_rclone("suggested_directory_rules_exclude_root_and_nested_directories_only")
    {
        return;
    }
    let fx = RealFixture::new();
    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    let mut command = fx.add_mapping_cmd();
    for suggestion in SUGGESTED_EXCLUSIONS {
        let name = suggestion.pattern.strip_suffix("/**").unwrap();
        command
            .extra_exclusions
            .insert(suggestion.pattern.to_owned());
        for dir in [name.to_owned(), format!("a/b/{name}")] {
            write_rel(&fx.local_dir, &format!("{dir}/x.o"), b"excluded");
        }
        write_rel(&fx.local_dir, &format!("keep/{name}"), b"ordinary file");
        write_rel(
            &fx.local_dir,
            &format!("{name}-notes/x"),
            b"ordinary directory",
        );
    }
    write_rel(&fx.local_dir, "buildings/x", b"ordinary directory");
    mgr.add_mapping(command).unwrap();
    let id = MappingId::new("research");
    mgr.initialize(InitializeCommand {
        mapping_id: id,
        expected_direction: None,
    })
    .unwrap();
    let snapshot = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    let generation = mgr
        .get_generation(snapshot.mappings[0].active_generation_id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let rendered = fs::read_to_string(generation.filters_file_path).unwrap();
    for suggestion in SUGGESTED_EXCLUSIONS {
        let name = suggestion.pattern.strip_suffix("/**").unwrap();
        assert!(rendered.lines().any(|line| line == format!("- {name}/**")));
        assert!(!fx.remote_dir.join(name).exists());
        assert!(!fx.remote_dir.join(format!("a/b/{name}")).exists());
        assert_eq!(
            fs::read(fx.remote_dir.join(format!("keep/{name}"))).unwrap(),
            b"ordinary file"
        );
        assert_eq!(
            fs::read(fx.remote_dir.join(format!("{name}-notes/x"))).unwrap(),
            b"ordinary directory"
        );
    }
    assert_eq!(
        fs::read(fx.remote_dir.join("buildings/x")).unwrap(),
        b"ordinary directory"
    );
    mgr.shutdown().unwrap();
}

#[test]
fn upload_initialization_propagates_files_and_persists_generation_and_run_artifacts() {
    if !require_pinned_rclone(
        "upload_initialization_propagates_files_and_persists_generation_and_run_artifacts",
    ) {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "notes.txt", b"upload-init-notes-v1");
    write_rel(&fx.local_dir, "sub/report.md", b"upload-init-report-v1");

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let ack = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
    assert_eq!(
        ack.outcome,
        CommandOutcome::InitializationQueued {
            mapping_id: MappingId::new("research"),
            direction: InitPreference::Path1,
        }
    );

    let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(snap.mappings[0].execution, Execution::Idle);
    assert!(snap.mappings[0].scheduling_enabled);
    assert!(snap.mappings[0].active_generation_fingerprint.is_some());

    let generation = mgr
        .get_generation(snap.mappings[0].active_generation_id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    // Baseline bytes from before wizard suggestions were introduced.
    assert_eq!(
        fs::read(generation.filters_file_path).unwrap(),
        b"# Frozen FolderSync generation filters\n- .DS_Store\n- .Spotlight-V100/**\n- .TemporaryItems/**\n- .Trashes/**\n- ._*\n- .fseventsd/**\n- .~lock.*#\n- Thumbs.db\n- ~$*\n"
    );

    // Verify files propagated with exact hashes and recognizable RCLONE_TEST marker created on both sides.
    assert_eq!(tree_hashes(&fx.local_dir), tree_hashes(&fx.remote_dir));
    assert_eq!(tree_hashes(&fx.remote_dir).len(), 2);

    let local_marker = AccessMarker::parse(
        &fs::read_to_string(fx.local_dir.join(ACCESS_MARKER_FILENAME)).unwrap(),
    )
    .unwrap();
    let remote_marker = AccessMarker::parse(
        &fs::read_to_string(fx.remote_dir.join(ACCESS_MARKER_FILENAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(local_marker, remote_marker);

    // Run artifacts exist under runs/<run-id>/.
    let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
    assert_eq!(runs.len(), 1);
    let expected = rclone_adapter::qualify_pinned_binary().unwrap().unwrap();
    assert_eq!(runs[0].engine_version.as_deref(), Some(expected.version()));
    assert_eq!(runs[0].engine_sha256.as_deref(), Some(expected.sha256()));
    assert_eq!(
        runs[0].engine_platform.as_deref(),
        Some(expected.platform())
    );
    let argv: serde_json::Value =
        serde_json::from_slice(&fs::read(runs[0].run_dir.join("argv.json")).unwrap()).unwrap();
    assert_eq!(
        argv["engine"]["executable"],
        expected.path().to_string_lossy().as_ref()
    );
    assert!(runs[0].run_dir.join("output.ndjson").is_file());
    assert!(runs[0].run_dir.join("outcome.json").is_file());

    mgr.shutdown().unwrap();
}

#[test]
fn changing_filters_initializes_with_a_fresh_workdir_and_keeps_excluded_files() {
    if !require_pinned_rclone(
        "changing_filters_initializes_with_a_fresh_workdir_and_keeps_excluded_files",
    ) {
        return;
    }
    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "notes.txt", b"included");
    write_rel(&fx.local_dir, "archive/old.txt", b"preserved");
    let manager = fx.open_manager();
    manager
        .register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    manager.add_mapping(fx.add_mapping_cmd()).unwrap();
    let id = MappingId::new("research");
    manager
        .initialize(InitializeCommand {
            mapping_id: id.clone(),
            expected_direction: None,
        })
        .unwrap();
    let original = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let generation = manager
        .get_generation(original.mappings[0].active_generation_id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let original_listing_hashes = tree_hashes(&generation.workdir_path);
    assert!(!original_listing_hashes.is_empty());

    let selection = std::collections::BTreeSet::from(["/archive/".to_owned()]);
    assert!(
        manager
            .propose_filter_change(&id, selection.clone())
            .unwrap()
            .preflight()
            .direction
            .is_err()
    );
    let preserved = fx.temp.path().join("preserved-remote");
    fs::create_dir(&preserved).unwrap();
    for path in ["notes.txt", "archive"] {
        fs::rename(fx.remote_dir.join(path), preserved.join(path)).unwrap();
    }

    let preview = manager.propose_filter_change(&id, selection).unwrap();
    assert_eq!(preview.preflight().direction, Ok(InitPreference::Path1));
    manager.apply_filter_change(preview).unwrap();
    let migrated = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let active = migrated.mappings[0].active_generation_id.as_ref().unwrap();
    assert_ne!(active, &generation.id);
    assert!(migrated.mappings[0].scheduling_enabled);
    assert_eq!(
        tree_hashes(&generation.workdir_path),
        original_listing_hashes
    );
    assert_eq!(
        fs::read(fx.local_dir.join("archive/old.txt")).unwrap(),
        b"preserved"
    );
    assert_eq!(
        fs::read(preserved.join("archive/old.txt")).unwrap(),
        b"preserved"
    );
    assert_eq!(
        fs::read(fx.remote_dir.join("notes.txt")).unwrap(),
        b"included"
    );
    assert!(!fx.remote_dir.join("archive/old.txt").exists());
    let runs = manager.list_runs(&id).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter()
            .any(|run| &run.generation_id == active && run.mode == "init_path1")
    );
}

#[test]
fn removing_custom_exclusion_uploads_local_only_directory_without_deleting_files() {
    if !require_pinned_rclone(
        "removing_custom_exclusion_uploads_local_only_directory_without_deleting_files",
    ) {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "proj/.git/HEAD", b"ref: refs/heads/main\n");
    write_rel(
        &fx.local_dir,
        "proj/.git/objects/data",
        b"local-only object",
    );
    let expected_files = tree_hashes(&fx.local_dir);
    let manager = fx.open_manager();
    manager
        .register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    let mut cmd = fx.add_mapping_cmd();
    cmd.extra_exclusions.insert(".git/".to_owned());
    manager.add_mapping(cmd).unwrap();
    let id = MappingId::new("research");
    manager
        .initialize(InitializeCommand {
            mapping_id: id.clone(),
            expected_direction: Some(InitPreference::Path1),
        })
        .unwrap();
    let original = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let original = &original.mappings[0];
    assert_eq!(original.readiness, Readiness::Ready);
    assert_eq!(tree_hashes(&fx.local_dir), expected_files);
    assert!(tree_hashes(&fx.remote_dir).is_empty());
    let marker_path = fx.remote_dir.join(ACCESS_MARKER_FILENAME);
    let original_marker = fs::read(&marker_path).unwrap();

    let preview = manager
        .propose_filter_change(&id, Default::default())
        .unwrap();
    assert_eq!(preview.removed_exclusions(), [".git/"]);
    assert!(preview.added_exclusions().is_empty());
    assert_eq!(preview.preflight().direction, Ok(InitPreference::Path1));
    assert!(tree_hashes(&fx.remote_dir).is_empty());
    manager.apply_filter_change(preview).unwrap();

    let migrated = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let migrated = &migrated.mappings[0];
    assert_eq!(migrated.readiness, Readiness::Ready);
    assert_eq!(migrated.last_run_outcome, Some(RunOutcome::Success));
    assert_ne!(migrated.active_generation_id, original.active_generation_id);
    assert_eq!(tree_hashes(&fx.local_dir), expected_files);
    assert_eq!(tree_hashes(&fx.remote_dir), expected_files);
    assert_eq!(fs::read(marker_path).unwrap(), original_marker);
    assert!(tree_hashes(&fx.local_recovery).is_empty());
    assert!(tree_hashes(&fx.remote_recovery).is_empty());
    manager.shutdown().unwrap();
}

#[test]
fn download_initialization_propagates_remote_files_to_empty_local() {
    if !require_pinned_rclone("download_initialization_propagates_remote_files_to_empty_local") {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.remote_dir, "cloud.txt", b"download-init-cloud-v1");
    write_rel(&fx.remote_dir, "deep/data.bin", b"download-init-binary-v1");

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let ack = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
    assert_eq!(
        ack.outcome,
        CommandOutcome::InitializationQueued {
            mapping_id: MappingId::new("research"),
            direction: InitPreference::Path2,
        }
    );

    let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(tree_hashes(&fx.local_dir), tree_hashes(&fx.remote_dir));
    assert_eq!(tree_hashes(&fx.local_dir).len(), 2);

    mgr.shutdown().unwrap();
}

#[test]
fn refuses_both_populated_trees_with_section9_message() {
    if !require_pinned_rclone("refuses_both_populated_trees_with_section9_message") {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "local_only.txt", b"local-data");
    write_rel(&fx.remote_dir, "remote_only.txt", b"remote-data");

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let err = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, ManagerError::BothSidesPopulated { .. }),
        "expected BothSidesPopulated, got {err:?}"
    );
    assert!(
        err.next_step().to_lowercase().contains("empty destination")
            || err.next_step().to_lowercase().contains("consolidat"),
        "next_step should reflect §9 guidance: {}",
        err.next_step()
    );
    assert_eq!(
        mgr.snapshot().mappings[0].readiness,
        Readiness::NeedsInitialization
    );
    // Neither tree was modified or given a marker.
    assert!(!fx.local_dir.join(ACCESS_MARKER_FILENAME).exists());
    assert!(!fx.remote_dir.join(ACCESS_MARKER_FILENAME).exists());
    mgr.shutdown().unwrap();
}

#[test]
fn refuses_unrecognized_access_marker_without_overwriting() {
    let fx = RealFixture::new();
    write_rel(
        &fx.local_dir,
        ACCESS_MARKER_FILENAME,
        b"important user file",
    );

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let err = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, ManagerError::AccessMarkerConflict { .. }),
        "expected AccessMarkerConflict, got {err:?}"
    );
    assert_eq!(
        fs::read(fx.local_dir.join(ACCESS_MARKER_FILENAME)).unwrap(),
        b"important user file"
    );
    mgr.shutdown().unwrap();
}

#[test]
fn run_once_propagates_edit_and_captures_recovery_backup() {
    if !require_pinned_rclone("run_once_propagates_edit_and_captures_recovery_backup") {
        return;
    }

    let fx = RealFixture::new();
    let v1 = b"original-doc-v1";
    write_rel(&fx.local_dir, "doc.txt", v1);

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(30)).unwrap();

    // Advance mtime past filesystem granularity and edit locally.
    thread::sleep(Duration::from_millis(1100));
    let v2 = b"updated-doc-v2-with-more-content";
    write_rel(&fx.local_dir, "doc.txt", v2);

    mgr.run_now("research").unwrap();
    let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(fs::read(fx.remote_dir.join("doc.txt")).unwrap(), v2);

    // Verify backup_dir2 captured the overwritten v1 file and was recorded on the run row.
    let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
    let latest = &runs[0];
    let backup2_doc = PathBuf::from(&latest.backup_dir2).join("doc.txt");
    assert!(
        backup2_doc.is_file(),
        "backup_dir2 must preserve overwritten copy"
    );
    assert_eq!(fs::read(&backup2_doc).unwrap(), v1);

    mgr.shutdown().unwrap();
}

#[test]
fn deletion_guard_abort_is_recorded_as_recovery_hold_surviving_restart() {
    if !require_pinned_rclone("deletion_guard_abort_is_recorded_as_recovery_hold_surviving_restart")
    {
        return;
    }

    let fx = RealFixture::new();
    for i in 0..10 {
        write_rel(
            &fx.local_dir,
            &format!("item_{i:02}.txt"),
            format!("protected-item-{i:02}").as_bytes(),
        );
    }

    {
        let mgr = fx.open_manager();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(30)).unwrap();

        // Delete 4 of 10 files (40% > 10% deletion guard).
        for i in 0..4 {
            fs::remove_file(fx.local_dir.join(format!("item_{i:02}.txt"))).unwrap();
        }

        mgr.run_now("research").unwrap();
        let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].counters.open_recovery_cases, 1);

        // Remote still has all 10 files untouched.
        assert_eq!(tree_hashes(&fx.remote_dir).len(), 10);
        mgr.shutdown().unwrap();
    }

    // Restart and confirm the recovery hold persists.
    {
        let mgr = fx.open_manager();
        assert_eq!(
            mgr.snapshot().mappings[0].readiness,
            Readiness::RecoveryRequired
        );
        let report = mgr.inspect_recovery("research").unwrap();
        assert_eq!(report.open_cases.len(), 1);
        assert_eq!(
            report.open_cases[0].case.reason_kind,
            "deletion_guard_aborted"
        );
        mgr.shutdown().unwrap();
    }
}

#[test]
fn interrupted_run_transitions_to_recovery_required_surviving_restart() {
    if !require_pinned_rclone("interrupted_run_transitions_to_recovery_required_surviving_restart")
    {
        return;
    }

    const TIMEOUT: Duration = Duration::from_secs(120);
    let mapping_id = MappingId::new("research");
    for file_count in [800, 3200] {
        let fx = RealFixture::new();
        let mgr = fx.open_manager();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
        // Initialize empty pair first.
        mgr.initialize(InitializeCommand {
            mapping_id: mapping_id.clone(),
            expected_direction: None,
        })
        .unwrap();
        let initialized = mgr.wait_until_idle(TIMEOUT).unwrap();
        assert_eq!(initialized.mappings[0].readiness, Readiness::Ready);

        let mut bytes = vec![0xABu8; 32 * 1024];
        for i in 0..file_count {
            let hdr = format!("INT-BLOB-{i:04}-");
            bytes[..hdr.len()].copy_from_slice(hdr.as_bytes());
            write_rel(&fx.local_dir, &format!("blob_{i:04}.bin"), &bytes);
        }

        let snapshots = mgr.subscribe_snapshots(1);
        mgr.run_now("research").unwrap();

        // Only the running child can create final .bin files on the destination.
        // The snapshot subscription reports completion even if the batch finishes between polls.
        let started = Instant::now();
        let stop = loop {
            assert!(
                started.elapsed() < TIMEOUT,
                "timed out waiting for partial destination progress in a {file_count}-file run"
            );
            if snapshots.latest().is_some_and(|snap| {
                snap.mappings[0].last_run_id != initialized.mappings[0].last_run_id
            }) {
                break None;
            }
            let copied = fs::read_dir(&fx.remote_dir)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "bin")
                })
                .count();
            if (1..file_count).contains(&copied) {
                eprintln!(
                    "StopRun at {copied}/{file_count} copied files after {:?}",
                    started.elapsed()
                );
                break Some(mgr.stop_run("research"));
            }
            thread::sleep(Duration::from_millis(5));
        };

        let snap = mgr.wait_until_idle(TIMEOUT).unwrap();
        let mapping = &snap.mappings[0];
        assert_ne!(mapping.last_run_id, initialized.mappings[0].last_run_id);
        let stopped = match stop {
            Some(Ok(ack)) => {
                assert_eq!(
                    ack.outcome,
                    CommandOutcome::StopRequested {
                        mapping_id: mapping_id.clone()
                    }
                );
                true
            }
            Some(Err(ManagerError::InvalidState { .. }))
                if mapping.last_run_outcome == Some(RunOutcome::Success) =>
            {
                false
            }
            Some(Err(err)) => panic!("StopRun failed: {err}"),
            None => false,
        };

        // A clean exit can win the race even when StopRun was acknowledged.
        if mapping.last_run_outcome == Some(RunOutcome::Success) {
            assert_eq!(mapping.readiness, Readiness::Ready);
            assert_eq!(mapping.execution, Execution::Idle);
            eprintln!("The {file_count}-file run completed before it could be interrupted");
            mgr.shutdown().unwrap();
            continue;
        }
        assert_eq!(mapping.last_run_outcome, Some(RunOutcome::Interrupted));
        assert!(stopped, "expected partial progress and a StopRun request");
        assert_eq!(mapping.readiness, Readiness::RecoveryRequired);
        assert_eq!(mapping.execution, Execution::Interrupted);
        mgr.shutdown().unwrap();

        // Reopen and confirm the interrupted run and its recovery hold survive restart.
        let mgr = fx.open_manager();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        assert_eq!(snap.mappings[0].last_run_id, mapping.last_run_id);
        assert_eq!(
            snap.mappings[0].last_run_outcome,
            Some(RunOutcome::Interrupted)
        );
        let report = mgr.inspect_recovery("research").unwrap();
        assert_eq!(report.open_cases.len(), 1);
        assert_eq!(report.open_cases[0].case.reason_kind, "interrupted");
        assert_eq!(report.open_cases[0].case.run_id, mapping.last_run_id);
        mgr.shutdown().unwrap();
        return;
    }
    panic!("both batches completed before StopRun could interrupt a transfer");
}

#[test]
fn narrowing_populated_mapping_catches_up_and_keeps_excluded_local_fixture_files() {
    if !require_pinned_rclone("narrowing_populated_mapping") {
        return;
    }
    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "proj/build/x.o", b"original object");
    write_rel(&fx.local_dir, "proj/node_modules/m.js", b"original module");
    write_rel(&fx.local_dir, "proj/src/a.txt", b"original source");
    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    let mut cmd = fx.add_mapping_cmd();
    cmd.extra_exclusions.insert("archive/**".to_owned());
    mgr.add_mapping(cmd).unwrap();
    let id = MappingId::new("research");
    mgr.initialize(InitializeCommand {
        mapping_id: id.clone(),
        expected_direction: None,
    })
    .unwrap();
    let before = mgr.wait_until_idle(Duration::from_secs(60)).unwrap();
    let old_id = before.mappings[0].active_generation_id.as_ref().unwrap();
    let old_generation = mgr.get_generation(old_id).unwrap().unwrap();
    let old_filters = fs::read(&old_generation.filters_file_path).unwrap();

    let mixed = mgr
        .propose_filter_change(&id, ["build/**".to_owned()].into())
        .unwrap();
    assert!(
        mgr.apply_filter_change(mixed)
            .unwrap_err()
            .cause()
            .contains("already contain data")
    );
    let preview = mgr
        .propose_filter_change(
            &id,
            ["archive/**", "build/**", "node_modules/**"]
                .map(str::to_owned)
                .into(),
        )
        .unwrap();
    write_rel(&fx.local_dir, "proj/src/a.txt", b"pending source edit");
    mgr.apply_filter_change(preview).unwrap();
    let after = mgr.wait_until_idle(Duration::from_secs(60)).unwrap();
    let mapping = &after.mappings[0];
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_ne!(mapping.active_generation_id.as_ref(), Some(old_id));
    assert!(mapping.scheduling_enabled);
    assert_eq!(tree_hashes(&fx.local_dir), tree_hashes(&fx.remote_dir));
    assert_eq!(
        fs::read(fx.remote_dir.join("proj/src/a.txt")).unwrap(),
        b"pending source edit"
    );
    assert_eq!(
        fs::read(&old_generation.filters_file_path).unwrap(),
        old_filters
    );
    assert!(
        tree_hashes(&fx.remote_recovery)
            .keys()
            .all(|path| path.ends_with("/proj/src/a.txt"))
    );

    write_rel(&fx.local_dir, "proj/build/x.o", b"local-only build change");
    write_rel(&fx.local_dir, "proj/src/a.txt", b"included source change");
    mgr.run_now(id.clone()).unwrap();
    mgr.wait_until_idle(Duration::from_secs(60)).unwrap();
    assert_eq!(
        fs::read(fx.remote_dir.join("proj/build/x.o")).unwrap(),
        b"original object"
    );
    assert_eq!(
        fs::read(fx.remote_dir.join("proj/src/a.txt")).unwrap(),
        b"included source change"
    );
    fs::remove_file(fx.remote_dir.join("proj/build/x.o")).unwrap();
    mgr.run_now(id).unwrap();
    mgr.wait_until_idle(Duration::from_secs(60)).unwrap();
    assert_eq!(
        fs::read(fx.local_dir.join("proj/build/x.o")).unwrap(),
        b"local-only build change"
    );
    mgr.shutdown().unwrap();
}

fn fake_narrowing_fixture() -> (
    RealFixture,
    SyncManager,
    std::sync::Arc<sync_manager::FakeBisyncLauncher>,
) {
    let fixture = RealFixture::new();
    write_rel(&fixture.local_dir, "proj/build/x.o", b"built");
    write_rel(&fixture.local_dir, "proj/src/a.txt", b"source");
    let launcher = std::sync::Arc::new(sync_manager::FakeBisyncLauncher::new());
    let mut config = ManagerConfig::with_state_dir(&fixture.state_dir);
    config.launcher = launcher.clone();
    let manager = SyncManager::open(config).unwrap();
    manager
        .register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    manager.add_mapping(fixture.add_mapping_cmd()).unwrap();
    launcher.push_script(Default::default());
    manager
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
    manager.wait_until_idle(Duration::from_secs(15)).unwrap();
    write_rel(&fixture.remote_dir, "proj/build/x.o", b"built");
    write_rel(&fixture.remote_dir, "proj/src/a.txt", b"source");
    (fixture, manager, launcher)
}

#[test]
fn catch_up_retryable_failure_keeps_the_active_generation_and_scheduling() {
    use rclone_adapter::{ExitStatusInfo, OutputLine, StreamKind};
    use sync_manager::FakeLaunchScript;

    let (_fixture, manager, launcher) = fake_narrowing_fixture();
    let id = MappingId::new("research");
    let original = manager.snapshot();
    launcher.push_script(FakeLaunchScript {
        exit_status: ExitStatusInfo {
            code: Some(5),
            signal: None,
        },
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: "temporary network error".to_owned(),
            truncated: false,
        }],
        ..Default::default()
    });
    manager
        .apply_filter_change(
            manager
                .propose_filter_change(&id, ["build/**".to_owned()].into())
                .unwrap(),
        )
        .unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(15)).unwrap();
    let mapping = &snapshot.mappings[0];
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_eq!(
        mapping.active_generation_id,
        original.mappings[0].active_generation_id
    );
    assert_eq!(mapping.exclusions, original.mappings[0].exclusions);
    assert_eq!(
        mapping.scheduling_enabled,
        original.mappings[0].scheduling_enabled
    );
    assert_eq!(mapping.last_run_outcome, Some(RunOutcome::RetryableFailure));
    assert!(
        mapping
            .last_run_summary
            .as_deref()
            .unwrap()
            .contains("Catch-up sync stopped")
    );
    assert_eq!(launcher.launched_plans().len(), 2);
}

#[test]
fn stop_during_catch_up_or_rebaseline_keeps_the_old_generation() {
    use std::sync::{Arc, mpsc};
    use sync_manager::{FakeLaunchScript, FilterMigrationPhase};

    if !require_pinned_rclone("stop_during_catch_up_or_rebaseline") {
        return;
    }
    for phase in [
        FilterMigrationPhase::CatchUp,
        FilterMigrationPhase::Rebaseline,
    ] {
        let (fixture, manager, launcher) = fake_narrowing_fixture();
        let id = MappingId::new("research");
        let original = manager.snapshot();
        let generation = manager
            .get_generation(original.mappings[0].active_generation_id.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let filters = fs::read(&generation.filters_file_path).unwrap();
        if phase == FilterMigrationPhase::Rebaseline {
            launcher.push_script(Default::default());
        }
        let (entered, receiver) = mpsc::channel();
        launcher.push_script(FakeLaunchScript {
            wait_for_stop: true,
            on_wait: Some(Arc::new(move || {
                entered.send(()).unwrap();
            })),
            ..Default::default()
        });
        let preview = manager
            .propose_filter_change(&id, ["build/**".to_owned()].into())
            .unwrap();
        manager.apply_filter_change(preview).unwrap();
        receiver.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(
            manager.snapshot().mappings[0].filter_migration_phase,
            Some(phase)
        );
        manager.stop_run(id).unwrap();
        let after = manager.wait_until_idle(Duration::from_secs(30)).unwrap();
        let mapping = &after.mappings[0];
        assert_eq!(
            mapping.active_generation_id,
            original.mappings[0].active_generation_id
        );
        assert_eq!(mapping.exclusions, original.mappings[0].exclusions);
        assert_eq!(
            mapping.scheduling_enabled,
            original.mappings[0].scheduling_enabled
        );
        assert_eq!(mapping.last_run_outcome, Some(RunOutcome::Interrupted));
        assert_eq!(fs::read(&generation.filters_file_path).unwrap(), filters);
        assert_eq!(
            tree_hashes(&fixture.local_dir),
            tree_hashes(&fixture.remote_dir)
        );
        assert_eq!(launcher.stop_calls(), 1);
    }
}
