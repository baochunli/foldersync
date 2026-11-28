//! Red-then-green regression tests for all 10 remaining HIGH items in managert3-review2.md.

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};

use rclone_adapter::{OutputLine, ProcessStartTime, StreamKind};
use rusqlite::Connection;
use sync_core::{ACCESS_MARKER_FILENAME, Execution, MappingId, Readiness};
use sync_manager::{
    AddMappingCommand, FakeBisyncLauncher, FakeLaunchScript, FakeProcessInspector,
    InitializeCommand, LiveProcessInfo, ManagerConfig, ManagerError, ProcessProbeResult,
    SqliteStateStore, SyncManager,
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

// Item (2): Refuse symlinked leaves (filters.txt, bisync, outcome.json) under state root.
#[test]
fn r2_h2_refuses_symlinked_leaves_under_state_root() {
    // Case A: symlinked filters.txt pointing outside state_dir must not be truncated or chmodded.
    {
        let fx = Fx::new();
        let outside = fx._tmp.path().join("outside_secret.txt");
        fs::write(&outside, b"untouched-secret").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = Arc::new(FakeBisyncLauncher::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.cmd()).unwrap();

        let gen1 = fx.state_dir.join("mappings/research/generation-1");
        fs::create_dir_all(&gen1).unwrap();
        symlink(&outside, gen1.join("filters.txt")).unwrap();

        let err = mgr
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("research"),
                expected_direction: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "symlinked filters.txt must be refused, got {err:?}"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"untouched-secret");
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o644
        );
        mgr.shutdown().unwrap();
    }

    // Case B: symlinked bisync directory pointing outside state_dir must be refused.
    {
        let fx = Fx::new();
        let outside_dir = fx._tmp.path().join("outside_dir");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::set_permissions(&outside_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = Arc::new(FakeBisyncLauncher::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.cmd()).unwrap();

        let gen1 = fx.state_dir.join("mappings/research/generation-1");
        fs::create_dir_all(&gen1).unwrap();
        symlink(&outside_dir, gen1.join("bisync")).unwrap();

        let err = mgr
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("research"),
                expected_direction: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "symlinked bisync dir must be refused, got {err:?}"
        );
        assert_eq!(
            fs::metadata(&outside_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        mgr.shutdown().unwrap();
    }

    // Case C: symlinked outcome.json inside a run dir during startup reconciliation must not overwrite target.
    {
        let fx = Fx::new();
        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());
        {
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
            mgr.shutdown().unwrap();
        }

        let outside = fx._tmp.path().join("outside_outcome.txt");
        fs::write(&outside, b"do-not-overwrite").unwrap();
        let canonical_state = fs::canonicalize(&fx.state_dir).unwrap();
        let run_dir = canonical_state.join("runs/symlink-reconcile-run");
        fs::create_dir_all(&run_dir).unwrap();
        symlink(&outside, run_dir.join("outcome.json")).unwrap();

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
                    id: sync_core::RunId::new("symlink-reconcile-run"),
                    mapping_id: MappingId::new("research"),
                    generation_id: gid,
                    mode: "normal".to_string(),
                    status: "starting".to_string(),
                    started_at: "2026-11-20T18:00:00Z".to_string(),
                    finished_at: None,
                    workdir_path: g.workdir_path,
                    backup_dir1: fx.local_recovery.join("research/symlink-reconcile-run"),
                    backup_dir2: fx
                        .remote_recovery
                        .join("symlink-reconcile-run")
                        .to_string_lossy()
                        .into_owned(),
                    run_dir: run_dir.clone(),
                    log_path: run_dir.join("output.ndjson"),
                    child_pid: None,
                    child_start_time: None,
                    child_executable: None,
                    engine_version: None,
                    engine_sha256: None,
                    engine_platform: None,
                    exit_code: None,
                    exit_signal: None,
                    outcome_kind: None,
                    outcome_detail: None,
                    conflict_paths: vec![],
                    last_verified_phase: "pre_spawn".to_string(),
                    files_checked: 0,
                    files_transferred: 0,
                    first_sync_total_files: None,
                    files_already_present: 0,
                    bytes_transferred: 0,
                    conflict_count: 0,
                })
                .unwrap();
        }

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher;
        let _ = SyncManager::open(cfg);
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"do-not-overwrite",
            "startup reconciliation must never write through a symlinked outcome.json"
        );
    }
}

// Item (3): Existing regular file at remote recovery root is rejected BEFORE creating local recovery dir.
#[test]
fn r2_h3_rejects_regular_file_at_remote_recovery_before_creating_local_recovery() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_research");
    let remote_dir = tmp.path().join("remote_research");
    let new_local_recovery = tmp.path().join("not_yet_created_local_rec");
    let file_at_remote_recovery = tmp.path().join("remote_rec_is_a_file");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::write(&file_at_remote_recovery, b"i am a regular file").unwrap();

    let mut cfg = ManagerConfig::with_state_dir(&state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();

    let cmd = AddMappingCommand::local_fixture(
        "research",
        "local-1",
        "Data/Research",
        &local_dir,
        &remote_dir,
        &new_local_recovery,
        &file_at_remote_recovery,
    )
    .unwrap();
    let res = mgr.add_mapping(cmd);
    assert!(
        res.is_err(),
        "regular file at remote recovery root must be rejected during layout validation"
    );
    assert!(
        !new_local_recovery.exists(),
        "local recovery dir must NOT have been created"
    );
    assert!(!local_dir.join(ACCESS_MARKER_FILENAME).exists());
    assert!(!remote_dir.join(ACCESS_MARKER_FILENAME).exists());
    mgr.shutdown().unwrap();
}

// Item (4): Root and marker verification runs LAST immediately before spawn (after claim & engine prep).
#[test]
fn r2_h4_rechecks_marker_immediately_before_spawn_after_claim_and_prepare() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    // 1. Init succeeds cleanly.
    launcher.push_script(FakeLaunchScript::default());

    // 2. Normal run: tamper remote marker DURING prepare (after claim, right before spawn!).
    let remote_marker = fx.remote_dir.join(ACCESS_MARKER_FILENAME);
    launcher.push_script(FakeLaunchScript {
        on_prepare: Some(Arc::new(move |_plan| {
            fs::write(&remote_marker, b"tampered right before spawn").unwrap();
        })),
        ..Default::default()
    });

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

    mgr.run_now("research").unwrap();
    let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(
        launcher.launched_plans().len(),
        1,
        "child must NOT be spawned when marker changes between claim/prepare and spawn"
    );
    assert_eq!(snap.mappings[0].readiness, Readiness::EndpointChanged);
    mgr.shutdown().unwrap();
}

// Item (5) & Item (9): Barrier between claim and spawn — StopRun AND ForceStop prevent spawn!
#[test]
fn r2_h5_h9_stop_run_and_force_stop_in_claim_to_spawn_gap_prevent_spawn() {
    // Subtest A: StopRun in the claim-to-spawn gap aborts without spawning.
    {
        let fx = Fx::new();
        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        launcher.push_script(FakeLaunchScript {
            on_prepare: Some(Arc::new(move |_plan| {
                let _ = entered_tx.send(());
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
            })),
            ..Default::default()
        });

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

        mgr.run_now("research").unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should reach prepare hook between claim and spawn");

        // Request StopRun while claimed but NOT yet spawned!
        mgr.stop_run("research").unwrap();
        let _ = release_tx.send(());

        let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        assert_eq!(
            launcher.launched_plans().len(),
            1,
            "StopRun in claim-to-spawn gap must prevent spawning child"
        );
        assert_eq!(snap.mappings[0].execution, Execution::Idle);
        mgr.shutdown().unwrap();
    }

    // Subtest B: ForceStop in the claim-to-spawn gap aborts without spawning and latches RecoveryRequired!
    {
        let fx = Fx::new();
        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        launcher.push_script(FakeLaunchScript {
            on_prepare: Some(Arc::new(move |_plan| {
                let _ = entered_tx.send(());
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
            })),
            ..Default::default()
        });

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

        mgr.run_now("research").unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should reach prepare hook between claim and spawn");

        // Request ForceStop while claimed but NOT yet spawned!
        let ack = mgr.force_stop("research").unwrap();
        assert_eq!(
            ack.snapshot.mappings[0].readiness,
            Readiness::RecoveryRequired
        );
        let _ = release_tx.send(());

        let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        assert_eq!(
            launcher.launched_plans().len(),
            1,
            "ForceStop in claim-to-spawn gap must prevent spawning child"
        );
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        mgr.shutdown().unwrap();
    }
}

// Item (6): Non-conclusive executable comparison ('(deleted)' or uncanonicalizable) stays
// inconclusive ('None') and blocks globally; missing-PID also blocks globally.
#[test]
fn r2_h6_deleted_exe_and_missing_pid_stay_inconclusive_and_block_globally() {
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

    let orphan_pid = 8123u32;
    let orphan_start = ProcessStartTime::UnixMicros(1_700_000_812_300_000);

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

    // Insert unfinished run with recorded PID + start_time + exe `/fake/rclone`.
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
                id: sync_core::RunId::new("deleted-exe-orphan"),
                mapping_id: MappingId::new("research"),
                generation_id: gid,
                mode: "normal".to_string(),
                status: "running".to_string(),
                started_at: "2026-11-20T18:10:00Z".to_string(),
                finished_at: None,
                workdir_path: g.workdir_path,
                backup_dir1: fx.local_recovery.join("research/deleted-exe-orphan"),
                backup_dir2: fx
                    .remote_recovery
                    .join("deleted-exe-orphan")
                    .to_string_lossy()
                    .into_owned(),
                run_dir: fx.state_dir.join("runs/deleted-exe-orphan"),
                log_path: fx.state_dir.join("runs/deleted-exe-orphan/output.ndjson"),
                child_pid: Some(orphan_pid),
                child_start_time: Some(orphan_start),
                child_executable: Some(PathBuf::from("/fake/rclone")),
                engine_version: Some("1.76.0".to_string()),
                engine_sha256: Some("0".repeat(64)),
                engine_platform: Some("linux-amd64".to_string()),
                exit_code: None,
                exit_signal: None,
                outcome_kind: None,
                outcome_detail: None,
                conflict_paths: vec![],
                last_verified_phase: "spawned".to_string(),
                files_checked: 0,
                files_transferred: 0,
                first_sync_total_files: None,
                files_already_present: 0,
                bytes_transferred: 0,
                conflict_count: 0,
            })
            .unwrap();
    }

    // Simulate Linux `/proc/<pid>/exe` reporting `/fake/rclone (deleted)` with the SAME start time!
    let inspector = Arc::new(FakeProcessInspector::new());
    inspector.set_process(
        orphan_pid,
        ProcessProbeResult::Live(LiveProcessInfo {
            pid: orphan_pid,
            start_time: Some(orphan_start),
            executable: Some(PathBuf::from("/fake/rclone (deleted)")),
        }),
    );

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher;
    cfg.process_inspector = inspector;
    let mgr = SyncManager::open(cfg).unwrap();

    // Must NOT be treated as reused PID! Must remain ambiguous/possibly alive and block `family` globally!
    let report = mgr.inspect_recovery("research").unwrap();
    assert_eq!(report.open_cases.len(), 1);
    assert_eq!(
        report.open_cases[0].case.reason_kind, "orphan_ambiguous",
        "deleted exe with matching start_time must stay ambiguous, got {:?}",
        report.open_cases[0].case.reason_kind
    );
    assert_eq!(report.open_cases[0].orphan_still_alive, None);
    assert!(
        matches!(
            mgr.run_now("family"),
            Err(ManagerError::ActiveJobConflict { .. })
        ),
        "deleted-exe ambiguous orphan must block other mappings globally"
    );
    mgr.shutdown().unwrap();
}

// Item (7): Terminal DB errors surface via wait_until_idle and keep mapping blocked;
// ForceStop commit failure does not set in-memory force_stopped flag.
#[test]
fn r2_h7_terminal_db_error_surfaces_to_caller_and_keeps_mapping_blocked() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
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

    // Inject a SQLite trigger that fails terminal UPDATE on `runs`.
    let conn = Connection::open(fx.state_dir.join("state.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_terminal_run BEFORE UPDATE ON runs
         WHEN NEW.status IN ('completed', 'failed', 'interrupted')
         BEGIN
             SELECT RAISE(ABORT, 'injected terminal DB failure');
         END;",
    )
    .unwrap();

    mgr.run_now("research").unwrap();
    let wait_res = mgr.wait_until_idle(Duration::from_secs(5));
    assert!(
        matches!(wait_res, Err(ManagerError::Storage { .. })),
        "wait_until_idle must surface terminal DB commit failure, got {wait_res:?}"
    );
    // Subsequent RunNow must stay blocked because the terminal commit failed!
    assert!(
        mgr.run_now("research").is_err(),
        "mapping must remain blocked after a failed terminal DB commit"
    );

    conn.execute_batch("DROP TRIGGER fail_terminal_run;")
        .unwrap();
    mgr.shutdown().unwrap();

    // Subtest B: ForceStop transaction failure does NOT set in-memory force_stopped or signal child!
    {
        let fx = Fx::new();
        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());
        let (spawned_tx, spawned_rx) = mpsc::channel();
        launcher.push_script(FakeLaunchScript {
            wait_for_stop: true,
            on_spawn: Some(Arc::new(move |_| {
                let _ = spawned_tx.send(());
            })),
            ..Default::default()
        });

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

        mgr.run_now("research").unwrap();
        spawned_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let conn = Connection::open(fx.state_dir.join("state.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_force_stop_case BEFORE INSERT ON recovery_cases
             BEGIN
                 SELECT RAISE(ABORT, 'injected force_stop commit failure');
             END;",
        )
        .unwrap();

        let err = mgr.force_stop("research").unwrap_err();
        assert!(
            matches!(err, ManagerError::Storage { .. }),
            "ForceStop must surface DB commit error, got {err:?}"
        );
        assert_eq!(
            launcher.force_stop_calls(),
            0,
            "failed ForceStop commit must NOT signal child"
        );

        // Drop the trigger and gracefully stop: if force_stopped had been leaked in memory,
        // finalization would suppress opening the recovery case; with commit-first it still opens one!
        conn.execute_batch("DROP TRIGGER fail_force_stop_case;")
            .unwrap();
        mgr.stop_run("research").unwrap();
        let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        let report = mgr.inspect_recovery("research").unwrap();
        assert_eq!(
            report.open_cases.len(),
            1,
            "in-memory force_stopped must not have been set on failed ForceStop commit"
        );
        mgr.shutdown().unwrap();
    }
}

// Item (8): Child identity failure stops child immediately; startup rewrites missing outcome.json;
// disk log retains all adapter lines even under burst exceeding observer channel capacity.
#[test]
fn r2_h8_child_id_failure_stops_child_and_log_retains_burst_and_startup_rewrites_outcome() {
    // Subtest A: child identity DB failure stops child immediately and writes interrupted recovery.
    {
        let fx = Fx::new();
        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());
        launcher.push_script(FakeLaunchScript {
            wait_for_stop: true,
            ..Default::default()
        });

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

        // Fail ONLY the child_pid update!
        let conn = Connection::open(fx.state_dir.join("state.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_child_id BEFORE UPDATE ON runs
             WHEN NEW.child_pid IS NOT NULL AND OLD.child_pid IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'injected child_id failure');
             END;",
        )
        .unwrap();

        mgr.run_now("research").unwrap();
        let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        assert_eq!(
            launcher.force_stop_calls(),
            1,
            "failed child_identity update must force-stop the child immediately"
        );
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
        assert!(!runs[0].run_dir.join("argv.json").exists());
        assert!(runs[0].run_dir.join("outcome.json").is_file());
        conn.execute_batch("DROP TRIGGER fail_child_id;").unwrap();
        mgr.shutdown().unwrap();
    }

    // Subtest B: Burst of 1,500 lines (> observer capacity 1024) all retained in output.ndjson,
    // AND deleting outcome.json from a terminal row is repaired on restart!
    {
        let fx = Fx::new();
        let burst_lines: Vec<OutputLine> = (0..1500)
            .map(|i| OutputLine {
                stream: StreamKind::Stderr,
                text: format!(r#"{{"level":"info","msg":"burst line {i}"}}"#),
                truncated: false,
            })
            .collect();
        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript {
            lines: burst_lines,
            ..Default::default()
        });

        let outcome_path;
        {
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

            let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
            let ndjson = fs::read_to_string(&runs[0].log_path).unwrap();
            let count = ndjson.lines().count();
            assert_eq!(
                count, 1500,
                "disk output.ndjson must retain all 1500 adapter-retained lines even under burst"
            );
            outcome_path = runs[0].run_dir.join("outcome.json");
            assert!(outcome_path.is_file());
            mgr.shutdown().unwrap();
        }

        // Simulate missing outcome.json on a committed terminal row; startup must rewrite it.
        fs::remove_file(&outcome_path).unwrap();
        assert!(!outcome_path.exists());
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher;
        let mgr = SyncManager::open(cfg).unwrap();
        assert!(
            outcome_path.is_file(),
            "startup reconciliation must rewrite missing outcome.json for terminal run rows"
        );
        mgr.shutdown().unwrap();
    }
}

// Item (12): Leading `/` anchors at sync root, and `*` does NOT cross `/`.
#[test]
fn r2_h12_symlink_exclusion_matches_rclone_root_anchor_and_single_star_semantics() {
    // A: `/sub/link.txt` matches `sub/link.txt` at root!
    {
        let fx = Fx::new();
        let outside = fx._tmp.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        fs::create_dir_all(fx.local_dir.join("sub")).unwrap();
        symlink(&outside, fx.local_dir.join("sub/link.txt")).unwrap();

        let launcher = Arc::new(FakeBisyncLauncher::new());
        launcher.push_script(FakeLaunchScript::default());
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher;
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        let mut cmd = fx.cmd();
        cmd.extra_exclusions = BTreeSet::from(["/sub/link.txt".to_string()]);
        mgr.add_mapping(cmd).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .expect("root-anchored `/sub/link.txt` must exclude `sub/link.txt`");
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.shutdown().unwrap();
    }

    // B: `/sub/link.txt` must NOT exclude `other/sub/link.txt`.
    {
        let fx = Fx::new();
        let outside = fx._tmp.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        fs::create_dir_all(fx.local_dir.join("other/sub")).unwrap();
        symlink(&outside, fx.local_dir.join("other/sub/link.txt")).unwrap();

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = Arc::new(FakeBisyncLauncher::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        let mut cmd = fx.cmd();
        cmd.extra_exclusions = BTreeSet::from(["/sub/link.txt".to_string()]);
        mgr.add_mapping(cmd).unwrap();
        let err = mgr
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("research"),
                expected_direction: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "root-anchored `/sub/link.txt` must not match `other/sub/link.txt`, got {err:?}"
        );
        mgr.shutdown().unwrap();
    }

    // C: `sub/*.link` must NOT match `sub/deep/a.link` (`*` does not cross `/`).
    {
        let fx = Fx::new();
        let outside = fx._tmp.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        fs::create_dir_all(fx.local_dir.join("sub/deep")).unwrap();
        symlink(&outside, fx.local_dir.join("sub/deep/a.link")).unwrap();

        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = Arc::new(FakeBisyncLauncher::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        let mut cmd = fx.cmd();
        cmd.extra_exclusions = BTreeSet::from(["sub/*.link".to_string()]);
        mgr.add_mapping(cmd).unwrap();
        let err = mgr
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("research"),
                expected_direction: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, ManagerError::PreflightRejected { .. }),
            "`*` in `sub/*.link` must not cross `/` to match `sub/deep/a.link`, got {err:?}"
        );
        mgr.shutdown().unwrap();
    }
}
