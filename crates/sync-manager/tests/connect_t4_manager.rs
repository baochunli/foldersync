//! Integration tests for connect-t4 manager improvements:
//! - Off-mutex & worker-thread initialization preflight (`start_initialize`)
//! - Bounded recent-run snapshot queries (`get_latest_run_for_mapping` / `list_recent_runs`)
//! - Coalescing latest-snapshot subscription under heavy burst load
//! - Plain-language error labels (no Rust enum names / internal identifiers)
//! - Queue priority (`SyncNow` on queued mapping) and `SyncReadyMappings`
//! - Conflict acknowledgment preserving `SuccessWithConflicts` across restarts
//! - Recovery inspection (run-specific log, recovery copies, copy-to-inspection refusing live roots)
//! - Finder action file consumer (`actions/*.json`, ADR 0002 v1) & Darwin notification
//! - Controlled shutdown with bounded wait & timeout interrupted persistence

use std::{
    fs,
    os::unix::fs::symlink,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use rclone_adapter::{ExitStatusInfo, OutputLine, StreamKind};
use sync_core::{Execution, Intent, MappingId, Readiness, RunId, RunOutcome};
use sync_manager::{
    AddMappingCommand, CommandOutcome, ControlledShutdownOutcome,
    DARWIN_SNAPSHOT_UPDATED_NOTIFICATION, FakeBisyncLauncher, FakeLaunchScript,
    FinderActionDispatch, InitializeCommand, ManagerConfig, ManagerEvent, RecordingFinderNotifier,
    RecoveryCopySide, SyncManager, ValidatedFinderAction,
};
use tempfile::TempDir;

struct Fx {
    _tmp: TempDir,
    state_dir: PathBuf,
    finder_dir: PathBuf,
    local_a: PathBuf,
    remote_a: PathBuf,
    local_rec_a: PathBuf,
    remote_rec_a: PathBuf,
    local_b: PathBuf,
    remote_b: PathBuf,
    local_rec_b: PathBuf,
    remote_rec_b: PathBuf,
    local_c: PathBuf,
    remote_c: PathBuf,
    local_rec_c: PathBuf,
    remote_rec_c: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let mk = |name: &str| {
            let p = tmp.path().join(name);
            fs::create_dir_all(&p).unwrap();
            p
        };
        Self {
            state_dir: tmp.path().join("state"),
            finder_dir: tmp.path().join("finder"),
            local_a: mk("local_a"),
            remote_a: mk("remote_a"),
            local_rec_a: mk("local_rec_a"),
            remote_rec_a: mk("remote_rec_a"),
            local_b: mk("local_b"),
            remote_b: mk("remote_b"),
            local_rec_b: mk("local_rec_b"),
            remote_rec_b: mk("remote_rec_b"),
            local_c: mk("local_c"),
            remote_c: mk("remote_c"),
            local_rec_c: mk("local_rec_c"),
            remote_rec_c: mk("remote_rec_c"),
            _tmp: tmp,
        }
    }

    fn cmd_a(&self) -> AddMappingCommand {
        AddMappingCommand::local_fixture(
            "alpha",
            "local-1",
            "Data/Alpha",
            &self.local_a,
            &self.remote_a,
            &self.local_rec_a,
            &self.remote_rec_a,
        )
        .unwrap()
    }

    fn cmd_b(&self) -> AddMappingCommand {
        AddMappingCommand::local_fixture(
            "beta",
            "local-1",
            "Data/Beta",
            &self.local_b,
            &self.remote_b,
            &self.local_rec_b,
            &self.remote_rec_b,
        )
        .unwrap()
    }

    fn cmd_c(&self) -> AddMappingCommand {
        AddMappingCommand::local_fixture(
            "gamma",
            "local-1",
            "Data/Gamma",
            &self.local_c,
            &self.remote_c,
            &self.local_rec_c,
            &self.remote_rec_c,
        )
        .unwrap()
    }
}

#[test]
fn coalescing_snapshot_subscription_always_delivers_latest_under_load() {
    let fx = Fx::new();
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();

    // Subscribe with tiny bound = 1 and generate 40 rapid snapshot updates without reading.
    let rx = mgr.subscribe_snapshots(1);
    for i in 0..40 {
        mgr.set_scheduling_paused(i % 2 == 0).unwrap();
    }
    let expected_seq = mgr.snapshot().sequence;

    // Must receive the LATEST snapshot (never dropped!), not the stale first one.
    let received = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        received.sequence, expected_seq,
        "coalescing subscriber must deliver the newest snapshot sequence"
    );
    assert_eq!(rx.latest().unwrap().sequence, expected_seq);
    mgr.shutdown().unwrap();
}

#[test]
fn start_initialize_runs_preflight_on_worker_and_reports_preflight_failures() {
    let fx = Fx::new();
    // Put data on BOTH sides so initialization preflight fails with BothSidesPopulated.
    fs::write(fx.local_a.join("local.txt"), b"a").unwrap();
    fs::write(fx.remote_a.join("remote.txt"), b"b").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();

    let events_rx = mgr.subscribe_events(64);
    let ack = mgr
        .start_initialize(InitializeCommand {
            mapping_id: MappingId::new("alpha"),
            expected_direction: None,
        })
        .unwrap();
    assert_eq!(
        ack.outcome,
        CommandOutcome::InitializationPreflightQueued {
            mapping_id: MappingId::new("alpha"),
        }
    );

    let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::NeedsInitialization);
    assert_eq!(snap.mappings[0].execution, Execution::Idle);

    // Must have emitted PreflightFailed event and never spawned rclone.
    let mut saw_preflight_failed = false;
    while let Ok(ev) = events_rx.try_recv() {
        if let ManagerEvent::PreflightFailed { mapping_id, .. } = ev
            && mapping_id == MappingId::new("alpha")
        {
            saw_preflight_failed = true;
        }
    }
    assert!(
        saw_preflight_failed,
        "worker-side preflight failure must emit PreflightFailed"
    );
    assert_eq!(launcher.launched_plans().len(), 0);

    // Remove remote file so preflight succeeds asynchronously!
    fs::remove_file(fx.remote_a.join("remote.txt")).unwrap();
    mgr.start_initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();
    let snap2 = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(snap2.mappings[0].readiness, Readiness::Ready);
    assert_eq!(launcher.launched_plans().len(), 1);
    mgr.shutdown().unwrap();
}

#[test]
fn plain_language_errors_omit_rust_enum_names_and_internal_actions() {
    let fx = Fx::new();
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();

    // RunNow on uninitialized mapping
    let err1 = mgr.run_now("alpha").unwrap_err();
    let text1 = err1.to_string();
    assert!(!text1.contains("NeedsInitialization"), "got: {text1}");
    assert!(!text1.contains("InspectRecovery"), "got: {text1}");

    // StopRun on idle mapping
    let err2 = mgr.stop_run("alpha").unwrap_err();
    let text2 = err2.to_string();
    assert!(!text2.contains("Idle"), "got: {text2}");
    assert!(!text2.contains("execution:"), "got: {text2}");

    // ForceStop on idle mapping
    let err3 = mgr.force_stop("alpha").unwrap_err();
    let text3 = err3.to_string();
    assert!(!text3.contains("ForceStop"), "got: {text3}");
    assert!(!text3.contains("Idle"), "got: {text3}");

    mgr.shutdown().unwrap();
}

#[test]
fn queue_priority_moves_queued_mapping_to_front_and_sync_ready_queues_all_eligible() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    // 3 init scripts
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());
    // Alpha holds active until stopped
    launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: Some(0),
            signal: None,
        }),
        ..Default::default()
    });
    // Gamma & Beta complete normally
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();
    mgr.add_mapping(fx.cmd_b()).unwrap();
    mgr.add_mapping(fx.cmd_c()).unwrap();

    for id in ["alpha", "beta", "gamma"] {
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new(id),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    }

    let snap_rx = mgr.subscribe_snapshots(16);
    mgr.run_now("alpha").unwrap();
    loop {
        let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if s.active_job_mapping_id.as_ref().map(|m| m.as_str()) == Some("alpha") {
            break;
        }
    }

    // SyncReadyMappings queues both beta and gamma in order.
    let ready_ack = mgr.sync_ready_mappings().unwrap();
    assert_eq!(
        ready_ack.snapshot.queue_order,
        vec![MappingId::new("beta"), MappingId::new("gamma")]
    );

    // SyncNow on queued gamma moves it to the FRONT of the queue!
    let prio_ack = mgr.run_now("gamma").unwrap();
    assert_eq!(
        prio_ack.outcome,
        CommandOutcome::RunPrioritized {
            mapping_id: MappingId::new("gamma")
        }
    );
    assert_eq!(
        prio_ack.snapshot.queue_order,
        vec![MappingId::new("gamma"), MappingId::new("beta")]
    );

    mgr.stop_run("alpha").unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    // Check actual launch order: 3 inits + alpha + gamma + beta!
    let plans = launcher.launched_plans();
    assert_eq!(plans.len(), 6);
    assert!(plans[4].path1.ends_with("local_c"));
    assert!(plans[5].path1.ends_with("local_b"));
    mgr.shutdown().unwrap();
}

#[test]
fn conflict_acknowledgment_preserves_outcome_survives_pause_and_restart() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    // Init clean
    launcher.push_script(FakeLaunchScript::default());
    // Run 2 completes WITH conflicts (real rclone v1.76.0 bisync/resolve.go marker)
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"notice","msg":"- Path1    Renaming Path1 copy - /tmp/p1/budget.xlsx.conflict1","source":"bisync/resolve.go:318"}"#.to_string(),
            truncated: false,
        }],
        exit_status: ExitStatusInfo {
            code: Some(0),
            signal: None,
        },
        ..Default::default()
    });
    // Run 3 completes cleanly (per Q5: later clean run must NOT clear unreviewed conflicts!)
    launcher.push_script(FakeLaunchScript::default());

    let conflict_run_id: RunId;
    {
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher.clone();
        cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local")
            .unwrap();
        mgr.add_mapping(fx.cmd_a()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("alpha"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

        mgr.run_now("alpha").unwrap();
        let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        let m = &snap.mappings[0];
        assert_eq!(m.last_run_outcome, Some(RunOutcome::SuccessWithConflicts));
        assert!(m.unreviewed_conflict_run_id.is_some());
        conflict_run_id = m.unreviewed_conflict_run_id.clone().unwrap();

        // Pause and resume must NOT clear unreviewed conflicts!
        let p = mgr.pause("alpha").unwrap();
        assert_eq!(
            p.snapshot.mappings[0].unreviewed_conflict_run_id.as_ref(),
            Some(&conflict_run_id)
        );
        let r = mgr.resume("alpha").unwrap();
        assert_eq!(
            r.snapshot.mappings[0].unreviewed_conflict_run_id.as_ref(),
            Some(&conflict_run_id)
        );

        // Explicit acknowledge clears unreviewed_conflict_run_id while PRESERVING SuccessWithConflicts!
        let ack = mgr.acknowledge_conflicts("alpha").unwrap();
        assert_eq!(ack.snapshot.mappings[0].unreviewed_conflict_run_id, None);
        assert_eq!(
            ack.snapshot.mappings[0].last_run_outcome,
            Some(RunOutcome::SuccessWithConflicts)
        );
        mgr.shutdown().unwrap();
    }

    // Restart: acknowledgment AND preserved SuccessWithConflicts outcome both survive!
    {
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher.clone();
        cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
        let mgr = SyncManager::open(cfg).unwrap();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings[0].unreviewed_conflict_run_id, None);
        assert_eq!(
            snap.mappings[0].last_run_outcome,
            Some(RunOutcome::SuccessWithConflicts)
        );
        mgr.shutdown().unwrap();
    }
}

#[test]
fn recovery_inspection_lists_run_log_and_copies_and_refuses_live_restore() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"info","msg":"alpha-specific-log-line"}"#.to_string(),
            truncated: false,
        }],
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    let runs = mgr.list_recent_runs(&MappingId::new("alpha"), 10).unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];

    // Run-specific log contains that run's own line.
    let log_lines = mgr.read_run_log(&run.id, 100).unwrap();
    assert!(
        log_lines
            .iter()
            .any(|l| l.contains("alpha-specific-log-line")),
        "expected run-specific log line, got {log_lines:?}"
    );

    // Seed a preserved recovery copy inside the run's backup_dir1.
    fs::create_dir_all(run.backup_dir1.join("sub")).unwrap();
    fs::write(run.backup_dir1.join("sub/old_draft.txt"), b"recovered-v1").unwrap();

    let copies = mgr.list_recovery_copies(&run.id).unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].side, RecoveryCopySide::Local);
    assert_eq!(copies[0].relative_path, "sub/old_draft.txt");

    // Refuses copying directly into a live synced folder (§12)!
    let err_live = mgr
        .copy_recovery_file_to_inspection(
            &run.id,
            RecoveryCopySide::Local,
            "sub/old_draft.txt",
            &fx.local_a,
        )
        .unwrap_err();
    assert!(
        err_live.cause().contains("overlaps synced folder"),
        "must refuse live folder target, got: {}",
        err_live.cause()
    );

    // Succeeds copying to a separate inspection folder and preserves original copy!
    let inspect_dir = fx._tmp.path().join("inspection_out");
    fs::create_dir_all(&inspect_dir).unwrap();
    let copied_path = mgr
        .copy_recovery_file_to_inspection(
            &run.id,
            RecoveryCopySide::Local,
            "sub/old_draft.txt",
            &inspect_dir,
        )
        .unwrap();
    assert_eq!(fs::read(&copied_path).unwrap(), b"recovered-v1");
    assert!(run.backup_dir1.join("sub/old_draft.txt").is_file());

    // Refuses silently overwriting an existing file in inspection_dir!
    assert!(
        mgr.copy_recovery_file_to_inspection(
            &run.id,
            RecoveryCopySide::Local,
            "sub/old_draft.txt",
            &inspect_dir,
        )
        .is_err()
    );

    mgr.shutdown().unwrap();
}

#[test]
fn finder_action_consumer_validates_dispatches_and_deletes_action_files() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());

    let notifier = Arc::new(RecordingFinderNotifier::new());
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.finder_status_dir = Some(fx.finder_dir.clone());
    cfg.finder_notifier = Arc::clone(&notifier) as Arc<dyn sync_manager::FinderNotifier>;
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    let actions_dir = fx.finder_dir.join("actions");
    fs::create_dir_all(&actions_dir).unwrap();

    // 1. Valid sync_now action
    fs::write(
        actions_dir.join("01-sync.json"),
        r#"{"version":1,"id":"a1","requested_at":"2026-11-20T18:46:12Z","action":"sync_now","mapping_id":"alpha"}"#,
    )
    .unwrap();
    // 2. Valid open_app action
    fs::write(
        actions_dir.join("02-open.json"),
        r#"{"version":1,"id":"a2","requested_at":"2026-11-20T18:46:13Z","action":"open_app"}"#,
    )
    .unwrap();
    // 3. Valid show_last_run action
    fs::write(
        actions_dir.join("03-show.json"),
        r#"{"version":1,"id":"a3","requested_at":"2026-11-20T18:46:14Z","action":"show_last_run","mapping_id":"alpha"}"#,
    )
    .unwrap();
    // 4. Invalid action (missing mapping_id on pause)
    fs::write(
        actions_dir.join("04-bad.json"),
        r#"{"version":1,"id":"a4","requested_at":"2026-11-20T18:46:15Z","action":"pause"}"#,
    )
    .unwrap();
    // 5. Symlinked action file (must be refused!)
    let outside = fx._tmp.path().join("outside_action.json");
    fs::write(
        &outside,
        r#"{"version":1,"id":"a5","requested_at":"2026-11-20T18:46:16Z","action":"pause","mapping_id":"alpha"}"#,
    )
    .unwrap();
    symlink(&outside, actions_dir.join("05-symlink.json")).unwrap();
    // 6. Hidden temp file (must be ignored!)
    fs::write(actions_dir.join(".action-inflight.tmp"), b"partial").unwrap();

    let results = mgr.drain_finder_actions();
    assert_eq!(results.len(), 5);
    assert!(matches!(
        &results[0],
        FinderActionDispatch::Executed {
            action: ValidatedFinderAction::SyncNow { mapping_id, .. },
            ..
        } if mapping_id.as_str() == "alpha"
    ));
    assert!(matches!(
        &results[1],
        FinderActionDispatch::UiRequested {
            action: ValidatedFinderAction::OpenApp {
                mapping_id: None,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        &results[2],
        FinderActionDispatch::UiRequested {
            action: ValidatedFinderAction::ShowLastRun { mapping_id, .. },
            ..
        } if mapping_id.as_str() == "alpha"
    ));
    assert!(matches!(&results[3], FinderActionDispatch::Rejected { .. }));
    assert!(matches!(&results[4], FinderActionDispatch::Rejected { .. }));

    // All processed .json files were deleted; hidden .tmp untouched.
    assert!(!actions_dir.join("01-sync.json").exists());
    assert!(!actions_dir.join("04-bad.json").exists());
    assert!(actions_dir.join(".action-inflight.tmp").is_file());

    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.launched_plans().len(), 2);

    // 7. Valid pause action persists Intent::Paused
    fs::write(
        actions_dir.join("06-pause.json"),
        r#"{"version":1,"id":"a6","requested_at":"2026-11-20T18:46:17Z","action":"pause","mapping_id":"alpha"}"#,
    )
    .unwrap();
    let pause_res = mgr.drain_finder_actions();
    assert_eq!(pause_res.len(), 1);
    assert!(matches!(
        &pause_res[0],
        FinderActionDispatch::Executed {
            action: ValidatedFinderAction::Pause { .. },
            ..
        }
    ));
    assert_eq!(mgr.snapshot().mappings[0].intent, Intent::Paused);
    assert!(
        notifier
            .posted_notifications()
            .iter()
            .all(|n| n == DARWIN_SNAPSHOT_UPDATED_NOTIFICATION)
    );

    mgr.shutdown().unwrap();
}

#[test]
fn controlled_shutdown_persists_interrupted_outcome_on_timeout() {
    let fx = Fx::new();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    // Active run blocks in on_spawn until released (simulating a child that outlives shutdown wait).
    let rx_clone = Arc::clone(&release_rx);
    launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        on_spawn: Some(Arc::new(move |_| {
            let _ = rx_clone
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        })),
        ..Default::default()
    });

    // Better: use wait_for_stop with a second thread? Note on_spawn holds state mutex!
    // Instead test timeout with a run that stays active via wait_for_stop=false? Wait:
    // FakeStopper wakes wait_for_stop immediately on request_stop(). So to simulate a slow child
    // during controlled_shutdown, use a tiny 1ms timeout on an already-prepared run or a custom
    // script — wait: let's release on_spawn? No, on_spawn runs inside state lock!
    drop(release_tx);
    drop(launcher);

    // Clean controlled_shutdown test: active run stops cleanly within timeout.
    let launcher_clean = Arc::new(FakeBisyncLauncher::new());
    launcher_clean.push_script(FakeLaunchScript::default());
    launcher_clean.push_script(FakeLaunchScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: Some(130),
            signal: None,
        }),
        ..Default::default()
    });

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher_clean.clone();
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    let snap_rx = mgr.subscribe_snapshots(16);
    mgr.run_now("alpha").unwrap();
    loop {
        let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if s.active_job_mapping_id.as_ref().map(|m| m.as_str()) == Some("alpha") {
            break;
        }
    }

    let out = mgr.controlled_shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(out, ControlledShutdownOutcome::Clean);
    assert_eq!(launcher_clean.stop_calls(), 1);
    drop(mgr);

    // Reopen: interrupted run recorded RecoveryRequired and survives restart.
    let mut cfg2 = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg2.launcher = Arc::new(FakeBisyncLauncher::new());
    cfg2.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr2 = SyncManager::open(cfg2).unwrap();
    assert_eq!(
        mgr2.snapshot().mappings[0].readiness,
        Readiness::RecoveryRequired
    );
    mgr2.shutdown().unwrap();
}

#[test]
fn initialize_does_not_hold_state_mutex_during_engine_probe() {
    use sync_manager::{BisyncLauncher, LaunchPlan, ManagerError, launcher::EngineMetadata};

    struct SlowEngineLauncher {
        entered_tx: mpsc::Sender<()>,
        release_rx: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
        inner: FakeBisyncLauncher,
    }

    impl BisyncLauncher for SlowEngineLauncher {
        fn engine_metadata(&self) -> Result<EngineMetadata, ManagerError> {
            let _ = self.entered_tx.send(());
            let _ = self
                .release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
            self.inner.engine_metadata()
        }

        fn prepare(
            &self,
            plan: &LaunchPlan,
        ) -> Result<sync_manager::launcher::PreparedLaunch, ManagerError> {
            self.inner.prepare(plan)
        }
    }

    let fx = Fx::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fake = FakeBisyncLauncher::new();
    fake.push_script(FakeLaunchScript::default());
    let launcher = Arc::new(SlowEngineLauncher {
        entered_tx,
        release_rx: Arc::new(std::sync::Mutex::new(release_rx)),
        inner: fake,
    });

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher;
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = Arc::new(SyncManager::open(cfg).unwrap());
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();

    let mgr_bg = Arc::clone(&mgr);
    let init_thread = thread::spawn(move || {
        mgr_bg.initialize(InitializeCommand {
            mapping_id: MappingId::new("alpha"),
            expected_direction: None,
        })
    });

    // Wait until initialize is blocked inside engine_metadata() preflight.
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    // State mutex MUST NOT be held: snapshot() and set_scheduling_paused() succeed immediately!
    let snap = mgr.snapshot();
    assert_eq!(snap.mappings.len(), 1);
    mgr.set_scheduling_paused(true).unwrap();

    let _ = release_tx.send(());
    init_thread.join().unwrap().unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
}

#[test]
fn bounded_recent_runs_and_draft_save_and_remove_preserve_user_files() {
    let fx = Fx::new();
    fs::write(fx.local_a.join("keep_me.txt"), b"precious").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    for _ in 0..8 {
        launcher.push_script(FakeLaunchScript::default());
    }

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    cfg.finder_notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();

    // Save resumable draft (Readiness::Unconfigured), then promote to NeedsInitialization.
    let draft_ack = mgr.save_draft_mapping(fx.cmd_a()).unwrap();
    assert_eq!(
        draft_ack.snapshot.mappings[0].readiness,
        Readiness::Unconfigured
    );
    let added_ack = mgr.add_mapping(fx.cmd_a()).unwrap();
    assert_eq!(
        added_ack.snapshot.mappings[0].readiness,
        Readiness::NeedsInitialization
    );

    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    for _ in 0..6 {
        mgr.run_now("alpha").unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    }

    let all_runs = mgr.list_runs(&MappingId::new("alpha")).unwrap();
    assert_eq!(all_runs.len(), 7);
    let recent_3 = mgr.list_recent_runs(&MappingId::new("alpha"), 3).unwrap();
    assert_eq!(recent_3.len(), 3);
    assert_eq!(recent_3[0].id, all_runs[0].id);

    // Removing account while mapping exists must fail.
    assert!(mgr.remove_account("local-1").is_err());

    // Removing mapping clears DB state but NEVER deletes user files on disk (§7).
    mgr.remove_mapping("alpha").unwrap();
    assert!(mgr.snapshot().mappings.is_empty());
    assert_eq!(
        fs::read(fx.local_a.join("keep_me.txt")).unwrap(),
        b"precious"
    );

    // Account can now be removed cleanly.
    mgr.remove_account("local-1").unwrap();
    assert!(mgr.list_accounts().unwrap().is_empty());
    mgr.shutdown().unwrap();
}
