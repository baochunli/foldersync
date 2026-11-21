use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rclone_adapter::{ChildIdentity, ExitStatusInfo, ProcessStartTime};
use sync_core::{Execution, MappingId, Readiness};
use sync_manager::{
    AddMappingCommand, FakeBisyncLauncher, FakeLaunchScript, FakeProcessInspector,
    InitializeCommand, LiveProcessInfo, ManagerConfig, ProcessProbeResult, SqliteStateStore,
    SyncManager,
};
use tempfile::TempDir;

struct TestEnv {
    _tmp: TempDir,
    state_dir: PathBuf,
    local_dir: PathBuf,
    remote_dir: PathBuf,
    local_recovery: PathBuf,
    remote_recovery: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let local_dir = tmp.path().join("local");
        let remote_dir = tmp.path().join("remote");
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

    fn add_cmd(&self) -> AddMappingCommand {
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

#[test]
fn run_start_row_is_persisted_before_spawn_with_recovery_paths() {
    let env = TestEnv::new();
    let db_path = env.state_dir.join("state.sqlite");
    let saw_pre_spawn_row = Arc::new(AtomicBool::new(false));
    let saw_flag = Arc::clone(&saw_pre_spawn_row);
    let db_path_for_hook = db_path.clone();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript {
        on_spawn: Some(Arc::new(move |plan| {
            // Query SQLite directly inside launch() BEFORE the child process exists.
            let store = SqliteStateStore::open(&db_path_for_hook).unwrap();
            let runs = store
                .list_runs_for_mapping(&MappingId::new("research"))
                .unwrap();
            let row = runs
                .iter()
                .find(|r| r.id.as_str() == plan.run_id)
                .expect("run-start row must exist in SQLite before spawn");
            assert_eq!(row.status, "starting");
            assert_eq!(row.last_verified_phase, "pre_spawn");
            assert!(row.child_pid.is_none());
            assert_eq!(row.workdir_path, plan.workdir);
            assert_eq!(row.backup_dir1, plan.backup_dir1);
            assert!(!row.backup_dir2.is_empty());
            saw_flag.store(true, Ordering::SeqCst);
        })),
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
    cfg.launcher = launcher;
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(env.add_cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    assert!(
        saw_pre_spawn_row.load(Ordering::SeqCst),
        "on_spawn hook must have verified pre-spawn row"
    );

    // After completion, child identity and completed status are recorded.
    let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "completed");
    assert_eq!(runs[0].child_pid, Some(4242));
    assert!(runs[0].run_dir.join("argv.json").is_file());
    assert!(runs[0].run_dir.join("output.ndjson").is_file());
    assert!(runs[0].run_dir.join("outcome.json").is_file());

    mgr.shutdown().unwrap();
}

#[test]
fn crash_between_spawn_and_pid_record_leaves_mapping_blocked_on_restart() {
    let env = TestEnv::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    // Init succeeds.
    launcher.push_script(FakeLaunchScript::default());
    // Normal run simulates crash immediately after spawn BEFORE PID is written to SQLite.
    launcher.push_script(FakeLaunchScript {
        simulate_crash_before_pid_record: true,
        ..Default::default()
    });

    {
        let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
        cfg.launcher = launcher.clone();
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(env.add_cmd()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

        mgr.run_now("research").unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        // Drop manager without cleaning up the simulated crashed run row.
        mgr.shutdown().unwrap();
    }

    // Reopen fresh manager: must detect unfinished row with missing PID and block mapping.
    {
        let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
        cfg.launcher = launcher.clone();
        let mgr = SyncManager::open(cfg).unwrap();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        assert!(mgr.run_now("research").is_err());

        let report = mgr.inspect_recovery("research").unwrap();
        assert_eq!(report.open_cases.len(), 1);
        assert_eq!(report.open_cases[0].case.reason_kind, "orphan_ambiguous");
        assert!(report.open_cases[0].case.orphan_pid.is_none());
        mgr.shutdown().unwrap();
    }
}

#[test]
fn reused_pid_with_different_start_time_is_not_trusted_on_restart() {
    let env = TestEnv::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    let run_id_to_corrupt;
    let recorded_pid = 9999u32;
    let recorded_start = ProcessStartTime::UnixMicros(1_700_000_000_000_000);
    let reused_start = ProcessStartTime::UnixMicros(1_799_999_999_000_000);

    {
        let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
        cfg.launcher = launcher.clone();
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(env.add_cmd()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
        run_id_to_corrupt = runs[0].id.clone();
        mgr.shutdown().unwrap();
    }

    // Insert an unfinished 'running' row with a recorded PID and start time, simulating an abrupt crash.
    {
        let store = SqliteStateStore::open(&env.state_dir.join("state.sqlite")).unwrap();
        let mapping = store
            .get_mapping(&MappingId::new("research"))
            .unwrap()
            .unwrap();
        let gen_id = mapping.active_generation_id.clone().unwrap();
        let generation = store.get_generation(&gen_id).unwrap().unwrap();
        let crashed_run = sync_manager::RunRecord {
            id: sync_core::RunId::new("crashed-run-reused-pid"),
            mapping_id: MappingId::new("research"),
            generation_id: gen_id,
            mode: "normal".to_string(),
            status: "running".to_string(),
            started_at: "2026-11-20T18:00:00Z".to_string(),
            finished_at: None,
            workdir_path: generation.workdir_path,
            backup_dir1: env.local_recovery.join("research/crashed-run-reused-pid"),
            backup_dir2: env
                .remote_recovery
                .join("crashed-run-reused-pid")
                .to_string_lossy()
                .into_owned(),
            run_dir: env.state_dir.join("runs/crashed-run-reused-pid"),
            log_path: env
                .state_dir
                .join("runs/crashed-run-reused-pid/output.ndjson"),
            child_pid: Some(recorded_pid),
            child_start_time: Some(recorded_start),
            child_executable: Some(PathBuf::from("/fake/rclone")),
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
        };
        store.insert_run_start(&crashed_run).unwrap();
        let _ = run_id_to_corrupt;
    }

    // FakeProcessInspector reports the SAME PID AND SAME executable, changing ONLY start_time
    // so this test isolates the start-time check (managert3-review1 Medium follow-up).
    let inspector = Arc::new(FakeProcessInspector::new());
    inspector.set_process(
        recorded_pid,
        ProcessProbeResult::Live(LiveProcessInfo {
            pid: recorded_pid,
            start_time: Some(reused_start),
            executable: Some(PathBuf::from("/fake/rclone")),
        }),
    );

    let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
    cfg.launcher = launcher.clone();
    cfg.process_inspector = inspector;
    let mgr = SyncManager::open(cfg).unwrap();

    let snap = mgr.snapshot();
    assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
    assert_eq!(snap.mappings[0].execution, Execution::Interrupted);

    let report = mgr.inspect_recovery("research").unwrap();
    assert_eq!(report.open_cases.len(), 1);
    assert_eq!(report.open_cases[0].case.reason_kind, "interrupted");
    // Reused PID must NEVER be recorded as an active orphan PID!
    assert_eq!(report.open_cases[0].case.orphan_pid, None);

    let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
    let reconciled = runs
        .iter()
        .find(|r| r.id.as_str() == "crashed-run-reused-pid")
        .unwrap();
    assert_eq!(reconciled.last_verified_phase, "reconciled_pid_reused");
    mgr.shutdown().unwrap();
}

#[test]
fn live_matching_orphan_is_detected_and_blocks_second_job() {
    let env = TestEnv::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    let orphan_pid = 7777u32;
    let orphan_start = ProcessStartTime::UnixMicros(1_700_000_123_456_789);
    let orphan_exe = PathBuf::from("/fake/rclone");

    {
        let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
        cfg.launcher = launcher.clone();
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(env.add_cmd()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.shutdown().unwrap();
    }

    {
        let store = SqliteStateStore::open(&env.state_dir.join("state.sqlite")).unwrap();
        let mapping = store
            .get_mapping(&MappingId::new("research"))
            .unwrap()
            .unwrap();
        let gen_id = mapping.active_generation_id.clone().unwrap();
        let generation = store.get_generation(&gen_id).unwrap().unwrap();
        let orphan_run = sync_manager::RunRecord {
            id: sync_core::RunId::new("orphan-live-run"),
            mapping_id: MappingId::new("research"),
            generation_id: gen_id,
            mode: "normal".to_string(),
            status: "running".to_string(),
            started_at: "2026-11-20T18:05:00Z".to_string(),
            finished_at: None,
            workdir_path: generation.workdir_path,
            backup_dir1: env.local_recovery.join("research/orphan-live-run"),
            backup_dir2: env
                .remote_recovery
                .join("orphan-live-run")
                .to_string_lossy()
                .into_owned(),
            run_dir: env.state_dir.join("runs/orphan-live-run"),
            log_path: env.state_dir.join("runs/orphan-live-run/output.ndjson"),
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
        };
        store.insert_run_start(&orphan_run).unwrap();
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

    let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
    cfg.launcher = launcher.clone();
    cfg.process_inspector = inspector;
    let mgr = SyncManager::open(cfg).unwrap();

    assert_eq!(
        mgr.snapshot().mappings[0].readiness,
        Readiness::RecoveryRequired
    );
    assert!(mgr.run_now("research").is_err());

    let report = mgr.inspect_recovery("research").unwrap();
    assert_eq!(report.open_cases.len(), 1);
    assert_eq!(report.open_cases[0].case.reason_kind, "orphan_running");
    assert_eq!(report.open_cases[0].case.orphan_pid, Some(orphan_pid));
    assert_eq!(report.open_cases[0].orphan_still_alive, Some(true));
    mgr.shutdown().unwrap();
}

#[test]
fn force_stop_terminates_active_child_and_latches_recovery_hold_before_ack() {
    let env = TestEnv::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript {
        identity: ChildIdentity {
            pid: 5555,
            start_time: Some(ProcessStartTime::UnixMicros(1_700_000_555_000_000)),
            executable: PathBuf::from("/fake/rclone"),
        },
        wait_for_stop: true,
        exit_status_on_force_stop: Some(ExitStatusInfo {
            code: None,
            signal: Some(9),
        }),
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(env.add_cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    let snap_rx = mgr.subscribe_snapshots(16);
    mgr.run_now("research").unwrap();
    // Wait until snapshot reports Running.
    loop {
        let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if s.mappings[0].execution == Execution::Running {
            break;
        }
    }

    let ack = mgr.force_stop("research").unwrap();
    assert_eq!(
        ack.snapshot.mappings[0].readiness,
        Readiness::RecoveryRequired
    );
    assert_eq!(ack.snapshot.mappings[0].execution, Execution::Interrupted);

    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.force_stop_calls(), 1);
    mgr.shutdown().unwrap();
}

#[test]
fn global_single_mutating_job_blocks_concurrent_init_and_serializes_runs() {
    let env = TestEnv::new();
    let local2 = env._tmp.path().join("local2");
    let remote2 = env._tmp.path().join("remote2");
    let local_rec2 = env._tmp.path().join("local_rec2");
    let remote_rec2 = env._tmp.path().join("remote_rec2");
    fs::create_dir_all(&local2).unwrap();
    fs::create_dir_all(&remote2).unwrap();
    fs::create_dir_all(&local_rec2).unwrap();
    fs::create_dir_all(&remote_rec2).unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    // 1. Init mapping 1 succeeds.
    launcher.push_script(FakeLaunchScript::default());
    // 2. RunNow on mapping 1 stays running until stopped.
    launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: Some(0),
            signal: None,
        }),
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&env.state_dir);
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(env.add_cmd()).unwrap();
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

    let snap_rx = mgr.subscribe_snapshots(16);
    mgr.run_now("research").unwrap();
    loop {
        let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if s.active_job_mapping_id.as_ref().map(|id| id.as_str()) == Some("research") {
            break;
        }
    }

    // Initializing a second mapping while the first is running must be rejected (§3).
    let err = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("family"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, sync_manager::ManagerError::ActiveJobConflict { .. }),
        "expected ActiveJobConflict while another job is active, got {err:?}"
    );

    mgr.stop_run("research").unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.launched_plans().len(), 2);
    mgr.shutdown().unwrap();
}
