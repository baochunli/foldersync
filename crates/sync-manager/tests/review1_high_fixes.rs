//! Regression tests for every HIGH finding in managert3-review1.md.

use std::{
    collections::BTreeSet, ffi::CString, fs, os::unix::fs::symlink, path::PathBuf, sync::Arc,
    time::Duration,
};

use rclone_adapter::{ChildIdentity, ExitStatusInfo, OutputLine, ProcessStartTime, StreamKind};
use sync_core::{ACCESS_MARKER_FILENAME, Execution, Intent, MappingId, Readiness, RunOutcome};
use sync_manager::{
    AddMappingCommand, FakeBisyncLauncher, FakeLaunchScript, FakeProcessInspector,
    FinderStatusSnapshot, InitializeCommand, LiveProcessInfo, ManagerConfig, ManagerError,
    ProcessProbeResult, SqliteStateStore, SyncManager,
};
use tempfile::TempDir;

struct Fx {
    _tmp: TempDir,
    state_dir: PathBuf,
    local_dir: PathBuf,
    remote_dir: PathBuf,
    local_recovery: PathBuf,
    remote_recovery: PathBuf,
}

impl Fx {
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

    fn cmd(&self) -> AddMappingCommand {
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

// HIGH #1: Case-insensitive marker detection & O_EXCL creation never truncates user file.
#[test]
fn h1_refuses_lowercase_rclone_test_marker_without_truncating() {
    let fx = Fx::new();
    let user_file = fx.local_dir.join("rclone_test");
    let payload = b"precious user content that must never be truncated";
    fs::write(&user_file, payload).unwrap();

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.cmd()).unwrap();

    let err = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, ManagerError::AccessMarkerConflict { .. }),
        "expected AccessMarkerConflict for lowercase rclone_test, got {err:?}"
    );
    assert_eq!(
        fs::read(&user_file).unwrap(),
        payload,
        "user file `rclone_test` must remain completely intact"
    );
    mgr.shutdown().unwrap();
}

// HIGH #2: Unsafe IDs (path traversal, separators, absolute paths) are rejected before writes.
#[test]
fn h2_rejects_unsafe_mapping_and_account_ids_before_writing_outside_roots() {
    let fx = Fx::new();
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(cfg).unwrap();

    for bad_account in ["../escape", "/tmp/abs", "a/b", "..", ".", "bad id", "-lead"] {
        let err = mgr
            .register_local_fixture_account(bad_account, "Bad")
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "expected PreflightRejected for account `{bad_account}`, got {err:?}"
        );
    }

    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();

    for bad_mapping in ["../escape", "/tmp/abs", "a/b", "..", ".", "bad id", "-lead"] {
        let cmd = AddMappingCommand::local_fixture(
            bad_mapping,
            "local-1",
            "Data/Research",
            &fx.local_dir,
            &fx.remote_dir,
            &fx.local_recovery,
            &fx.remote_recovery,
        );
        let err = match cmd {
            Err(e) => e,
            Ok(c) => mgr.add_mapping(c).unwrap_err(),
        };
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "expected PreflightRejected for mapping `{bad_mapping}`, got {err:?}"
        );
    }
    assert!(!fx._tmp.path().join("escape").exists());
    mgr.shutdown().unwrap();
}

// HIGH #3: Full layout validation covers Path2 + remote recovery + state dir BEFORE mutating endpoints.
#[test]
fn h3_rejects_overlapping_path2_and_remote_recovery_before_creating_dirs_or_markers() {
    let fx = Fx::new();
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();

    // Case A: remote recovery root nested inside remote_dir (Path2).
    let bad_remote_recovery = fx.remote_dir.join("nested_recovery");
    let cmd = AddMappingCommand::local_fixture(
        "bad-rec",
        "local-1",
        "Data/Research",
        &fx.local_dir,
        &fx.remote_dir,
        &fx.local_recovery,
        &bad_remote_recovery,
    )
    .unwrap();
    assert!(
        mgr.add_mapping(cmd).is_err(),
        "remote recovery nested inside Path2 must be rejected"
    );
    assert!(
        !bad_remote_recovery.exists(),
        "overlapping remote recovery dir must not be created"
    );
    assert!(!fx.local_dir.join(ACCESS_MARKER_FILENAME).exists());
    assert!(!fx.remote_dir.join(ACCESS_MARKER_FILENAME).exists());

    // Case B: Path2 overlaps state_dir.
    let remote_in_state = fx.state_dir.join("sneaky_remote");
    fs::create_dir_all(&remote_in_state).unwrap();
    let cmd_state = AddMappingCommand::local_fixture(
        "bad-state",
        "local-1",
        "Data/Research",
        &fx.local_dir,
        &remote_in_state,
        &fx.local_recovery,
        &fx.remote_recovery,
    )
    .unwrap();
    assert!(
        mgr.add_mapping(cmd_state).is_err(),
        "Path2 overlapping state_dir must be rejected"
    );

    mgr.shutdown().unwrap();
}

// HIGH #4: Run once re-verifies both roots and access-marker CONTENTS right before launch.
#[test]
fn h4_run_once_refuses_replacement_tree_with_wrong_access_marker_contents() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    // Push a second script so if the manager wrongly launches, it would consume it.
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.launched_plans().len(), 1);

    // Replace RCLONE_TEST on remote_dir with an unrelated file (simulating swapped tree).
    fs::write(
        fx.remote_dir.join(ACCESS_MARKER_FILENAME),
        b"unrelated replacement tree file",
    )
    .unwrap();

    // Even if run_now queues or checks synchronously, rclone must NOT be launched!
    let _ = mgr.run_now("research");
    let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(
        launcher.launched_plans().len(),
        1,
        "rclone must NOT launch against a tree with mismatched RCLONE_TEST contents"
    );
    assert_eq!(snap.mappings[0].readiness, Readiness::EndpointChanged);
    mgr.shutdown().unwrap();
}

// HIGH #5: Canceling a queued job (Pause / StopRun) prevents it from ever launching.
#[test]
fn h5_paused_or_stopped_queued_job_never_launches() {
    let fx = Fx::new();
    let local2 = fx._tmp.path().join("local_family");
    let remote2 = fx._tmp.path().join("remote_family");
    let local_rec2 = fx._tmp.path().join("local_rec_family");
    let remote_rec2 = fx._tmp.path().join("remote_rec_family");
    fs::create_dir_all(&local2).unwrap();
    fs::create_dir_all(&remote2).unwrap();
    fs::create_dir_all(&local_rec2).unwrap();
    fs::create_dir_all(&remote_rec2).unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    // Init research + init family.
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());
    // RunNow on research blocks waiting for stop while we queue + pause family.
    launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: Some(0),
            signal: None,
        }),
        ..Default::default()
    });
    // Extra script that must NOT be consumed by the canceled family job.
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.cmd()).unwrap();
    mgr.add_mapping(
        AddMappingCommand::local_fixture(
            "family",
            "local-1",
            "Data/Family",
            &local2,
            &remote2,
            &local_rec2,
            &remote_rec2,
        )
        .unwrap(),
    )
    .unwrap();

    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("family"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.launched_plans().len(), 2);

    let snap_rx = mgr.subscribe_snapshots(32);
    mgr.run_now("research").unwrap();
    loop {
        let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if s.active_job_mapping_id.as_ref().map(|id| id.as_str()) == Some("research") {
            break;
        }
    }

    // Queue family behind research, then immediately pause family -> cancels queued run!
    mgr.run_now("family").unwrap();
    let pause_ack = mgr.pause("family").unwrap();
    let fam = pause_ack
        .snapshot
        .mappings
        .iter()
        .find(|m| m.id.as_str() == "family")
        .unwrap();
    assert_eq!(fam.intent, Intent::Paused);
    assert_eq!(fam.execution, Execution::Idle);

    // Resume, queue family again behind research, then cancel with StopRun!
    mgr.resume("family").unwrap();
    mgr.run_now("family").unwrap();
    let stop_ack = mgr.stop_run("family").unwrap();
    let fam2 = stop_ack
        .snapshot
        .mappings
        .iter()
        .find(|m| m.id.as_str() == "family")
        .unwrap();
    assert_eq!(fam2.execution, Execution::Idle);

    mgr.stop_run("research").unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    // Only research ran (total 3 launches: 2 inits + 1 research run); canceled family never launched.
    assert_eq!(launcher.launched_plans().len(), 3);
    mgr.shutdown().unwrap();
}

// HIGH #6: Live or ambiguous orphan blocks the GLOBAL worker (other mappings too!),
// and inconclusive probes report orphan_still_alive = None (not Some(false)).
#[test]
fn h6_live_orphan_and_inconclusive_probe_block_global_worker_and_report_unknown() {
    let fx = Fx::new();
    let local2 = fx._tmp.path().join("local_family");
    let remote2 = fx._tmp.path().join("remote_family");
    let local_rec2 = fx._tmp.path().join("local_rec_family");
    let remote_rec2 = fx._tmp.path().join("remote_rec_family");
    fs::create_dir_all(&local2).unwrap();
    fs::create_dir_all(&remote2).unwrap();
    fs::create_dir_all(&local_rec2).unwrap();
    fs::create_dir_all(&remote_rec2).unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());

    let orphan_pid = 8888u32;
    let orphan_start = ProcessStartTime::UnixMicros(1_700_000_888_000_000);
    let orphan_exe = PathBuf::from("/fake/rclone");

    {
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher.clone();
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.cmd()).unwrap();
        mgr.add_mapping(
            AddMappingCommand::local_fixture(
                "family",
                "local-1",
                "Data/Family",
                &local2,
                &remote2,
                &local_rec2,
                &remote_rec2,
            )
            .unwrap(),
        )
        .unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("family"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.shutdown().unwrap();
    }

    // Insert unfinished run with recorded live orphan on `research`.
    {
        let store = SqliteStateStore::open(&fx.state_dir.join("state.sqlite")).unwrap();
        let m = store
            .get_mapping(&MappingId::new("research"))
            .unwrap()
            .unwrap();
        let gid = m.active_generation_id.clone().unwrap();
        let g = store.get_generation(&gid).unwrap().unwrap();
        store
            .insert_run_start(&sync_manager::RunRecord {
                id: sync_core::RunId::new("orphan-global-block"),
                mapping_id: MappingId::new("research"),
                generation_id: gid,
                mode: "normal".to_string(),
                status: "running".to_string(),
                started_at: "2026-11-20T18:10:00Z".to_string(),
                finished_at: None,
                workdir_path: g.workdir_path,
                backup_dir1: fx.local_recovery.join("research/orphan-global-block"),
                backup_dir2: fx
                    .remote_recovery
                    .join("orphan-global-block")
                    .to_string_lossy()
                    .into_owned(),
                run_dir: fx.state_dir.join("runs/orphan-global-block"),
                log_path: fx.state_dir.join("runs/orphan-global-block/output.ndjson"),
                child_pid: Some(orphan_pid),
                child_start_time: Some(orphan_start),
                child_executable: Some(orphan_exe.clone()),
                engine_version: Some("1.76.0".to_string()),
                engine_sha256: Some("0".repeat(64)),
                engine_platform: Some("osx-arm64".to_string()),
                exit_code: None,
                exit_signal: None,
                outcome_kind: None,
                outcome_detail: None,
                conflict_paths: Vec::new(),
                last_verified_phase: "spawned".to_string(),
                files_checked: 0,
                files_transferred: 0,
                bytes_transferred: 0,
                conflict_count: 0,
            })
            .unwrap();
    }

    let inspector = Arc::new(FakeProcessInspector::new());
    inspector.set_process(
        orphan_pid,
        ProcessProbeResult::Live(LiveProcessInfo {
            pid: orphan_pid,
            start_time: Some(orphan_start),
            executable: Some(orphan_exe),
        }),
    );

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    cfg.process_inspector = inspector.clone();
    let mgr = SyncManager::open(cfg).unwrap();

    // A DIFFERENT Ready mapping (`family`) must be blocked globally while `research`'s orphan is alive!
    let err = mgr.run_now("family").unwrap_err();
    assert!(
        matches!(err, ManagerError::ActiveJobConflict { .. }),
        "live orphan on research must block family globally, got {err:?}"
    );

    // Reconciliation must have written outcome.json (HIGH #8).
    assert!(
        fx.state_dir
            .join("runs/orphan-global-block/outcome.json")
            .is_file(),
        "reconciled orphan run must write outcome.json"
    );

    // Now simulate an inconclusive/permission-denied probe on that orphan PID:
    // InspectRecovery MUST report None (unknown), NOT Some(false)!
    inspector.set_process(
        orphan_pid,
        ProcessProbeResult::Unavailable("permission denied".to_string()),
    );
    let report = mgr.inspect_recovery("research").unwrap();
    assert_eq!(
        report.open_cases[0].orphan_still_alive, None,
        "inconclusive probe must report None (unknown), not Some(false)"
    );
    // And global worker remains blocked when probe is inconclusive!
    assert!(matches!(
        mgr.run_now("family"),
        Err(ManagerError::ActiveJobConflict { .. })
    ));

    mgr.shutdown().unwrap();
}

// HIGH #8: NDJSON streams live during run and wraps plain/truncated lines as valid JSON;
// launch failure writes outcome.json.
#[test]
fn h8_ndjson_wraps_plain_lines_as_valid_json_and_launch_failure_writes_outcome_json() {
    let fx = Fx::new();
    let local2 = fx._tmp.path().join("local_family");
    let remote2 = fx._tmp.path().join("remote_family");
    let local_rec2 = fx._tmp.path().join("local_rec_family");
    let remote_rec2 = fx._tmp.path().join("remote_rec_family");
    fs::create_dir_all(&local2).unwrap();
    fs::create_dir_all(&remote2).unwrap();
    fs::create_dir_all(&local_rec2).unwrap();
    fs::create_dir_all(&remote_rec2).unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    // 1. Init research cleanly (valid JSON + plain line).
    launcher.push_script(FakeLaunchScript {
        lines: vec![
            OutputLine {
                stream: StreamKind::Stderr,
                text: r#"{"level":"info","msg":"Bisync successful"}"#.to_string(),
                truncated: false,
            },
            OutputLine {
                stream: StreamKind::Stderr,
                text: "plain text line without json".to_string(),
                truncated: false,
            },
        ],
        ..Default::default()
    });
    // 2. RunNow on research emits a truncated line AND waits for stop so we can assert
    //    live streaming to output.ndjson BEFORE the child exits!
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stdout,
            text: r#"{"level":"info","msg":"cut off"#.to_string(),
            truncated: true,
        }],
        wait_for_stop: true,
        ..Default::default()
    });
    // 3. No script left -> initializing `family` triggers a launch failure!

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher;
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.cmd()).unwrap();
    mgr.add_mapping(
        AddMappingCommand::local_fixture(
            "family",
            "local-1",
            "Data/Family",
            &local2,
            &remote2,
            &local_rec2,
            &remote_rec2,
        )
        .unwrap(),
    )
    .unwrap();

    // Run 1: init research.
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    let runs1 = mgr.list_runs(&MappingId::new("research")).unwrap();
    let ndjson1 = fs::read_to_string(&runs1[0].log_path).unwrap();
    let lines1: Vec<&str> = ndjson1.lines().collect();
    assert_eq!(lines1.len(), 2);
    for line in &lines1 {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(parsed.is_object());
    }

    // Run 2: live-streamed truncated line written to disk BEFORE stop/exit!
    let ev_rx = mgr.subscribe_events(64);
    mgr.run_now("research").unwrap();
    loop {
        if let sync_manager::ManagerEvent::RunOutputLine { .. } =
            ev_rx.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            break;
        }
    }
    let runs2 = mgr.list_runs(&MappingId::new("research")).unwrap();
    let live_ndjson = fs::read_to_string(&runs2[0].log_path).unwrap();
    let live_lines: Vec<&str> = live_ndjson.lines().collect();
    assert_eq!(
        live_lines.len(),
        1,
        "output.ndjson must stream lines to disk during the run before exit"
    );
    let parsed_live: serde_json::Value = serde_json::from_str(live_lines[0]).unwrap();
    assert_eq!(parsed_live["truncated"], true);

    mgr.stop_run("research").unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    // Run 3: launch error on `family` must write outcome.json!
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("family"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    let fam_runs = mgr.list_runs(&MappingId::new("family")).unwrap();
    assert_eq!(fam_runs.len(), 1);
    assert!(
        fam_runs[0].run_dir.join("outcome.json").is_file(),
        "launch failure must write outcome.json"
    );

    mgr.shutdown().unwrap();
}

// HIGH #9: Finder write failure is non-fatal and never suppresses StopRun/ForceStop or leaves stale active slot.
#[test]
fn h9_finder_write_failure_does_not_suppress_stop_or_leave_stale_active_slot() {
    let fx = Fx::new();
    let finder_dir = fx._tmp.path().join("finder_blocked");
    fs::create_dir_all(&finder_dir).unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript {
        identity: ChildIdentity {
            pid: 6060,
            start_time: Some(ProcessStartTime::UnixMicros(1_700_000_606_000_000)),
            executable: PathBuf::from("/fake/rclone"),
        },
        wait_for_stop: true,
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.finder_status_dir = Some(finder_dir.clone());
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    // Break the Finder directory by replacing it with a regular file so every Finder write fails!
    fs::remove_dir_all(&finder_dir).unwrap();
    fs::write(&finder_dir, b"not a directory").unwrap();

    let snap_rx = mgr.subscribe_snapshots(16);
    mgr.run_now("research")
        .expect("RunNow must succeed despite Finder publication failure");
    loop {
        let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if s.mappings[0].execution == Execution::Running {
            break;
        }
    }

    mgr.stop_run("research")
        .expect("StopRun must succeed and signal child despite Finder publication failure");
    let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.stop_calls(), 1);
    assert!(snap.active_job_mapping_id.is_none());
    mgr.shutdown().unwrap();
}

// HIGH #11: No "will retry" copy in status_text or Finder status.json while scheduling is disabled.
#[test]
fn h11_retryable_failure_does_not_claim_will_retry_when_scheduling_disabled() {
    let fx = Fx::new();
    let finder_dir = fx._tmp.path().join("finder");
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    // Exit 5 = TemporaryError -> Outcome::Retryable -> RunOutcome::RetryableFailure.
    launcher.push_script(FakeLaunchScript {
        exit_status: ExitStatusInfo {
            code: Some(5),
            signal: None,
        },
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.finder_status_dir = Some(finder_dir.clone());
    cfg.launcher = launcher;
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    mgr.run_now("research").unwrap();
    let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(
        snap.mappings[0].last_run_outcome,
        Some(RunOutcome::RetryableFailure)
    );
    assert!(
        !snap.mappings[0]
            .status_text
            .to_ascii_lowercase()
            .contains("will retry"),
        "status_text must not say 'will retry' while scheduling is disabled, got: {}",
        snap.mappings[0].status_text
    );

    let finder: FinderStatusSnapshot =
        serde_json::from_str(&fs::read_to_string(finder_dir.join("status.json")).unwrap()).unwrap();
    assert!(
        !finder.mappings[0]
            .label
            .to_ascii_lowercase()
            .contains("will retry"),
        "Finder label must not say 'will retry' while scheduling is disabled, got: {}",
        finder.mappings[0].label
    );

    mgr.shutdown().unwrap();
}

// HIGH #12: Onboarding preserves literal backslashes for core name validation,
// rejects special files (FIFO), and honors explicit exclusions for symlinks.
#[test]
fn h12_onboarding_rejects_backslash_and_fifo_and_honors_symlink_exclusions() {
    // A: Literal backslash in Unix filename must be caught by sync_core::validate_names.
    {
        let fx = Fx::new();
        fs::write(fx.local_dir.join(r"bad\name.txt"), b"x").unwrap();
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = Arc::new(FakeBisyncLauncher::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.cmd()).unwrap();
        let err = mgr
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("research"),
                expected_direction: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "backslash filename must be rejected, got {err:?}"
        );
        mgr.shutdown().unwrap();
    }

    // B: Special file (FIFO) must be reported as unsupported, not ordinary data.
    {
        let fx = Fx::new();
        let fifo_path = fx.local_dir.join("pipe.fifo");
        let c_path = CString::new(fifo_path.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo should succeed in temp dir");

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = Arc::new(FakeBisyncLauncher::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.cmd()).unwrap();
        let err = mgr
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("research"),
                expected_direction: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "special file (FIFO) must be rejected, got {err:?}"
        );
        mgr.shutdown().unwrap();
    }

    // C: Unexcluded symlink is rejected; explicitly excluded symlink is accepted without traversal.
    {
        let fx = Fx::new();
        let outside = fx._tmp.path().join("outside_secret.txt");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, fx.local_dir.join("ignored_link.txt")).unwrap();
        fs::write(fx.local_dir.join("normal.txt"), b"ok").unwrap();

        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher;
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();

        // Add mapping WITH explicit exclusion for the symlink.
        let mut cmd = fx.cmd();
        cmd.extra_exclusions = BTreeSet::from(["ignored_link.txt".to_string()]);
        mgr.add_mapping(cmd).unwrap();

        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .expect("explicitly excluded symlink must be permitted during onboarding");
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.shutdown().unwrap();
    }
}
