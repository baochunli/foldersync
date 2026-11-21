use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use rclone_adapter::{InitPreference, pinned_binary_path};
use sha2::{Digest, Sha256};
use sync_core::{ACCESS_MARKER_FILENAME, AccessMarker, Execution, MappingId, Readiness};
use sync_manager::{
    AddMappingCommand, CommandOutcome, InitializeCommand, ManagerConfig, ManagerError, SyncManager,
};
use tempfile::TempDir;

struct RealFixture {
    _tmp: TempDir,
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
            _tmp: tmp,
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
    assert!(!snap.mappings[0].scheduling_enabled);
    assert!(snap.mappings[0].active_generation_fingerprint.is_some());

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
    assert!(runs[0].run_dir.join("argv.json").is_file());
    assert!(runs[0].run_dir.join("output.ndjson").is_file());
    assert!(runs[0].run_dir.join("outcome.json").is_file());

    mgr.shutdown().unwrap();
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

    let fx = RealFixture::new();

    {
        let mgr = fx.open_manager();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
        // Initialize empty pair first.
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(30)).unwrap();

        // Write a batch large enough to catch mid-transfer with StopRun (SIGINT).
        let template = vec![0xABu8; 32 * 1024];
        for i in 0..400u32 {
            let mut bytes = template.clone();
            let hdr = format!("INT-BLOB-{i:04}-");
            bytes[..hdr.len()].copy_from_slice(hdr.as_bytes());
            write_rel(
                &fx.local_dir,
                &format!("batch_{:02}/blob_{:03}.bin", i / 50, i),
                &bytes,
            );
        }

        let events = mgr.subscribe_events(1024);
        mgr.run_now("research").unwrap();

        let mut sent_stop = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(sync_manager::ManagerEvent::RunOutputLine { line, .. }) => {
                    if line.contains("Queue copy to")
                        || line.contains("Applying changes")
                        || line.contains("Copied")
                    {
                        let _ = mgr.stop_run("research");
                        sent_stop = true;
                        break;
                    }
                }
                Ok(sync_manager::ManagerEvent::RunFinished { .. }) => break,
                _ => {}
            }
        }
        assert!(
            sent_stop,
            "expected to observe transfer lines and send StopRun"
        );

        let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        mgr.shutdown().unwrap();
    }

    // Reopen and confirm RecoveryRequired + Interrupted survive restart.
    {
        let mgr = fx.open_manager();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        let report = mgr.inspect_recovery("research").unwrap();
        assert!(!report.open_cases.is_empty());
        mgr.shutdown().unwrap();
    }
}
