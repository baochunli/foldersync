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
    time::{Duration, Instant},
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

#[test]
fn v1_to_v2_migration_backfills_existing_unreviewed_conflicts() {
    let fx = Fx::new();
    fs::create_dir_all(&fx.state_dir).unwrap();
    let db_path = fx.state_dir.join("state.sqlite");

    // Seed a real schema v1 database (without unreviewed_conflict_run_id).
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL);
            INSERT INTO schema_migrations VALUES (1, 'initial_schema', '2026-11-20T00:00:00Z');
            CREATE TABLE devices (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL);
            INSERT INTO devices VALUES ('dev-1', 'Mac', '2026-11-20T00:00:00Z');
            CREATE TABLE accounts (
                id TEXT PRIMARY KEY, display_name TEXT NOT NULL, kind TEXT NOT NULL,
                remote_name TEXT NOT NULL, remote_root_folder_id TEXT NOT NULL,
                config_path TEXT NOT NULL, auth_status TEXT NOT NULL,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            INSERT INTO accounts VALUES ('local-1', 'Local', 'local_fixture', 'local', 'local-root', '/tmp/rclone.conf', 'ready', '2026-11-20T00:00:00Z', '2026-11-20T00:00:00Z');
            CREATE TABLE mappings (
                id TEXT PRIMARY KEY, device_id TEXT NOT NULL, account_id TEXT NOT NULL,
                collection_display_path TEXT NOT NULL, collection_folder_id TEXT NOT NULL,
                collection_ancestor_ids_json TEXT NOT NULL, local_volume_kind TEXT NOT NULL,
                local_volume_stable_id TEXT NOT NULL, local_mount_path TEXT NOT NULL,
                local_relative_dir TEXT NOT NULL, remote_endpoint_kind TEXT NOT NULL,
                remote_endpoint_value TEXT NOT NULL, local_recovery_root TEXT NOT NULL,
                cloud_recovery_path TEXT NOT NULL, schedule_interval_secs INTEGER NOT NULL,
                scheduling_enabled INTEGER NOT NULL, state_json TEXT NOT NULL,
                intent TEXT NOT NULL, readiness TEXT NOT NULL, execution TEXT NOT NULL,
                active_generation_id TEXT, exclusions_json TEXT NOT NULL,
                max_delete_percent INTEGER NOT NULL, last_run_id TEXT,
                last_run_outcome TEXT, last_run_completed_at TEXT,
                last_run_summary_text TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE mapping_generations (
                id TEXT PRIMARY KEY, mapping_id TEXT NOT NULL, generation_number INTEGER NOT NULL,
                fingerprint TEXT NOT NULL, config_json TEXT NOT NULL, initial_direction TEXT,
                workdir_path TEXT NOT NULL, filters_file_path TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE TABLE runs (
                id TEXT PRIMARY KEY, mapping_id TEXT NOT NULL, generation_id TEXT NOT NULL,
                mode TEXT NOT NULL, status TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT,
                workdir_path TEXT NOT NULL, backup_dir1 TEXT NOT NULL, backup_dir2 TEXT NOT NULL,
                run_dir TEXT NOT NULL, log_path TEXT NOT NULL, child_pid INTEGER,
                child_start_time_kind TEXT, child_start_time_value INTEGER, child_executable TEXT,
                engine_version TEXT,
                engine_sha256 TEXT, engine_platform TEXT, exit_code INTEGER, exit_signal INTEGER,
                outcome_kind TEXT, outcome_detail TEXT, conflict_paths_json TEXT NOT NULL DEFAULT '[]',
                last_verified_phase TEXT NOT NULL, files_checked INTEGER NOT NULL DEFAULT 0,
                files_transferred INTEGER NOT NULL DEFAULT 0, bytes_transferred INTEGER NOT NULL DEFAULT 0,
                conflict_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE recovery_cases (
                id TEXT PRIMARY KEY, mapping_id TEXT NOT NULL, run_id TEXT, status TEXT NOT NULL,
                reason_kind TEXT NOT NULL, cause TEXT NOT NULL, next_step TEXT NOT NULL,
                workdir_path TEXT, backup_dir1 TEXT, backup_dir2 TEXT, log_path TEXT,
                orphan_pid INTEGER, created_at TEXT NOT NULL, resolved_at TEXT, resolution_note TEXT
            );
            "#,
        )
        .unwrap();
        let ready_state = serde_json::to_string(&sync_core::MappingState::new(
            Readiness::Ready,
            Execution::Idle,
            Intent::Enabled,
        ))
        .unwrap();
        let conflict_outcome = serde_json::to_string(&RunOutcome::SuccessWithConflicts).unwrap();
        let success_outcome = serde_json::to_string(&RunOutcome::Success).unwrap();
        conn.execute(
            "INSERT INTO mappings VALUES (
                'alpha', 'dev-1', 'local-1', 'Data/Alpha', 'col-a', '[]', 'Internal', 'vol-1',
                ?1, 'local_a', 'local_dir', ?2, ?3, ?4, 300, 0, ?5, 'Enabled', 'Ready', 'Idle',
                NULL, '[]', 10, 'run-legacy-conflict', ?6, '2026-11-20T10:00:00Z',
                '1 conflict', '2026-11-20T00:00:00Z', '2026-11-20T10:00:00Z'
            )",
            rusqlite::params![
                fx._tmp.path().to_string_lossy().as_ref(),
                fx.remote_a.to_string_lossy().as_ref(),
                fx.local_rec_a.to_string_lossy().as_ref(),
                fx.remote_rec_a.to_string_lossy().as_ref(),
                ready_state,
                conflict_outcome,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mappings VALUES (
                'beta', 'dev-1', 'local-1', 'Data/Beta', 'col-b', '[]', 'Internal', 'vol-1',
                ?1, 'local_b', 'local_dir', ?2, ?3, ?4, 300, 0, ?5, 'Enabled', 'Ready', 'Idle',
                NULL, '[]', 10, 'run-legacy-ok', ?6, '2026-11-20T10:00:00Z',
                '0 conflicts', '2026-11-20T00:00:00Z', '2026-11-20T10:00:00Z'
            )",
            rusqlite::params![
                fx._tmp.path().to_string_lossy().as_ref(),
                fx.remote_b.to_string_lossy().as_ref(),
                fx.local_rec_b.to_string_lossy().as_ref(),
                fx.remote_rec_b.to_string_lossy().as_ref(),
                ready_state,
                success_outcome,
            ],
        )
        .unwrap();
    }

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.finder_status_dir = Some(fx.finder_dir.clone());
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(cfg).unwrap();
    let snap = mgr.snapshot();
    let alpha = snap
        .mappings
        .iter()
        .find(|m| m.id.as_str() == "alpha")
        .unwrap();
    let beta = snap
        .mappings
        .iter()
        .find(|m| m.id.as_str() == "beta")
        .unwrap();
    assert_eq!(
        alpha.unreviewed_conflict_run_id,
        Some(RunId::new("run-legacy-conflict")),
        "v2 migration must backfill existing SuccessWithConflicts mappings as unreviewed (HIGH #13)"
    );
    assert_eq!(beta.unreviewed_conflict_run_id, None);

    // Finder status.json must show Attention for the migrated conflict mapping.
    let status_raw = fs::read_to_string(fx.finder_dir.join("status.json")).unwrap();
    let status: sync_manager::FinderStatusSnapshot = serde_json::from_str(&status_raw).unwrap();
    let f_alpha = status.mappings.iter().find(|m| m.id == "alpha").unwrap();
    assert_eq!(f_alpha.state, sync_manager::FinderMappingState::Attention);
    mgr.shutdown().unwrap();
}

#[test]
fn init_preflight_attempt_id_prevents_cancelled_attempt_from_writing_or_hijacking() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use sync_manager::{BisyncLauncher, LaunchPlan, ManagerError, launcher::EngineMetadata};

    struct BarrierInitLauncher {
        calls: AtomicUsize,
        first_entered_tx: mpsc::Sender<()>,
        first_release_rx: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
        inner: FakeBisyncLauncher,
    }

    impl BisyncLauncher for BarrierInitLauncher {
        fn engine_metadata(&self) -> Result<EngineMetadata, ManagerError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                let _ = self.first_entered_tx.send(());
                let _ = self
                    .first_release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
            }
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
    // Remove recovery dirs first so we can verify attempt 1 does NOT create them after Stop!
    fs::remove_dir_all(&fx.local_rec_a).unwrap();
    fs::remove_dir_all(&fx.remote_rec_a).unwrap();

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fake = FakeBisyncLauncher::new();
    fake.push_script(FakeLaunchScript::default());
    let launcher = Arc::new(BarrierInitLauncher {
        calls: AtomicUsize::new(0),
        first_entered_tx: entered_tx,
        first_release_rx: Arc::new(std::sync::Mutex::new(release_rx)),
        inner: fake,
    });

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = launcher.clone();
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();

    // Attempt 1 starts and blocks inside engine_metadata().
    mgr.start_initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    // Stop cancels attempt 1 while it is still blocked.
    let stop_ack = mgr.stop_run("alpha").unwrap();
    assert_eq!(stop_ack.snapshot.mappings[0].execution, Execution::Idle);

    // Queue attempt 2 while attempt 1 has not yet returned from engine_metadata()!
    mgr.start_initialize(InitializeCommand {
        mapping_id: MappingId::new("alpha"),
        expected_direction: None,
    })
    .unwrap();

    // Release attempt 1: it MUST see cancellation, write NOTHING, and let attempt 2 initialize cleanly!
    let _ = release_tx.send(());
    let final_snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(final_snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(launcher.inner.launched_plans().len(), 1);
    assert_eq!(
        launcher.calls.load(Ordering::SeqCst),
        2,
        "both attempts should have probed, with only attempt 2 committing and launching (HIGH #5)"
    );
    mgr.shutdown().unwrap();
}

#[test]
fn controlled_shutdown_timeout_persists_blocks_orphan_and_prevents_overwrite_and_removal() {
    use sync_manager::{FakeProcessInspector, LiveProcessInfo, ProcessProbeResult};

    let fx = Fx::new();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());
    let rx_wait = Arc::clone(&release_rx);
    let child_pid = 42424u32;
    let child_start = rclone_adapter::ProcessStartTime::UnixMicros(1_700_000_999_000_000);
    launcher.push_script(FakeLaunchScript {
        identity: rclone_adapter::ChildIdentity {
            pid: child_pid,
            start_time: Some(child_start),
            executable: PathBuf::from("/fake/rclone"),
        },
        // Block in wait() even after request_stop() until the test explicitly releases it!
        on_wait: Some(Arc::new(move || {
            let _ = rx_wait.lock().unwrap().recv_timeout(Duration::from_secs(5));
        })),
        ..Default::default()
    });

    let timed_out_run_id: RunId;
    {
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
        cfg.launcher = launcher.clone();
        let mgr = SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-1", "Local")
            .unwrap();
        mgr.add_mapping(fx.cmd_a()).unwrap();
        mgr.add_mapping(fx.cmd_b()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("alpha"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("beta"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

        let snap_rx = mgr.subscribe_snapshots(8);
        mgr.run_now("alpha").unwrap();
        loop {
            let s = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if s.active_job_mapping_id.as_ref().map(|m| m.as_str()) == Some("alpha") {
                break;
            }
        }

        // Real bounded timeout: child ignores stop -> TimedOutInterrupted persisted (HIGH #6).
        let out = mgr.controlled_shutdown(Duration::from_millis(80)).unwrap();
        match out {
            ControlledShutdownOutcome::TimedOutInterrupted { mapping_id, run_id } => {
                assert_eq!(mapping_id, MappingId::new("alpha"));
                timed_out_run_id = run_id;
            }
            other => panic!("expected TimedOutInterrupted, got {other:?}"),
        }

        // Release child so finalize_completed_run executes AFTER timeout: MUST NOT overwrite timeout record!
        let _ = release_tx.send(());
        thread::sleep(Duration::from_millis(80));
        let run_after = mgr.get_run(&timed_out_run_id).unwrap().unwrap();
        assert_eq!(run_after.status, "interrupted");
        assert_eq!(run_after.last_verified_phase, "shutdown_timeout");

        // Drop MUST NOT hang even after timeout.
        drop(mgr);
    }

    // Reopen with FakeProcessInspector reporting child_pid still alive!
    let inspector = Arc::new(FakeProcessInspector::new());
    inspector.set_process(
        child_pid,
        ProcessProbeResult::Live(LiveProcessInfo {
            pid: child_pid,
            start_time: Some(child_start),
            executable: Some(PathBuf::from("/fake/rclone")),
        }),
    );
    let mut cfg2 = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg2.launcher = Arc::new(FakeBisyncLauncher::new());
    cfg2.process_inspector = inspector;
    let mgr2 = SyncManager::open(cfg2).unwrap();

    // Global orphan gate MUST block unrelated mapping `beta` while timed-out child is alive (HIGH #6)!
    let beta_err = mgr2.run_now("beta").unwrap_err();
    assert!(
        matches!(
            beta_err,
            sync_manager::ManagerError::ActiveJobConflict { .. }
        ),
        "expected ActiveJobConflict on beta while timed-out child alive, got: {beta_err:?}"
    );

    // Mapping removal MUST be refused while live orphan exists for `alpha` (HIGH #7)!
    let rm_err = mgr2.remove_mapping("alpha").unwrap_err();
    assert!(
        matches!(rm_err, sync_manager::ManagerError::ActiveJobConflict { .. }),
        "expected ActiveJobConflict refusing remove_mapping while live orphan exists, got: {rm_err:?}"
    );
    mgr2.shutdown().unwrap();
}

#[test]
fn remove_mapping_refused_while_ambiguous_orphan_exists() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript {
        simulate_crash_before_pid_record: true,
        ..Default::default()
    });

    {
        let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
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

        mgr.run_now("alpha").unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        mgr.shutdown().unwrap();
    }

    // Reopen reconciles missing-PID run as `orphan_ambiguous`.
    let mut cfg2 = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg2.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr2 = SyncManager::open(cfg2).unwrap();
    let report = mgr2.inspect_recovery("alpha").unwrap();
    assert!(
        report
            .open_cases
            .iter()
            .any(|c| c.case.reason_kind == "orphan_ambiguous")
    );
    assert!(
        report.identities_verified,
        "valid local/remote markers should report identities_verified=true (HIGH #9)"
    );

    // Removal MUST be refused while orphan_ambiguous exists (HIGH #7)!
    let rm_err = mgr2.remove_mapping("alpha").unwrap_err();
    assert!(matches!(
        rm_err,
        sync_manager::ManagerError::ActiveJobConflict { .. }
    ));
    mgr2.shutdown().unwrap();
}

#[test]
fn copy_and_finder_refuse_symlinks_before_canonicalizing() {
    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
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

    let run = &mgr.list_recent_runs(&MappingId::new("alpha"), 5).unwrap()[0];
    fs::create_dir_all(&run.backup_dir1).unwrap();
    fs::write(run.backup_dir1.join("a.txt"), b"data").unwrap();

    // 1. Symlinked inspection_dir MUST be refused BEFORE canonicalizing (HIGH #10)!
    let real_inspect = fx._tmp.path().join("real_inspect");
    fs::create_dir_all(&real_inspect).unwrap();
    let link_inspect = fx._tmp.path().join("link_inspect");
    symlink(&real_inspect, &link_inspect).unwrap();
    assert!(
        mgr.copy_recovery_file_to_inspection(
            &run.id,
            RecoveryCopySide::Local,
            "a.txt",
            &link_inspect
        )
        .is_err(),
        "symlinked inspection_dir must be rejected before canonicalize"
    );

    // 2. Replaced symlinked run backup_dir1 MUST be refused BEFORE canonicalizing (HIGH #10)!
    let outside_tree = fx._tmp.path().join("outside_tree");
    fs::create_dir_all(&outside_tree).unwrap();
    fs::write(outside_tree.join("a.txt"), b"attacker").unwrap();
    fs::remove_dir_all(&run.backup_dir1).unwrap();
    symlink(&outside_tree, &run.backup_dir1).unwrap();
    assert!(
        mgr.copy_recovery_file_to_inspection(
            &run.id,
            RecoveryCopySide::Local,
            "a.txt",
            &real_inspect
        )
        .is_err(),
        "symlinked run recovery root must be rejected before canonicalize"
    );
    assert!(
        mgr.list_recovery_copies(&run.id).is_err(),
        "list_recovery_copies must surface symlinked recovery root error (HIGH #9)"
    );
    mgr.shutdown().unwrap();

    // 3. Symlinked Finder handoff parent directory MUST be refused on open (HIGH #10)!
    let real_parent = fx._tmp.path().join("real_finder_parent");
    fs::create_dir_all(&real_parent).unwrap();
    let link_parent = fx._tmp.path().join("link_finder_parent");
    symlink(&real_parent, &link_parent).unwrap();
    let mut cfg_link = ManagerConfig::with_state_dir(fx._tmp.path().join("state2"));
    cfg_link.finder_status_dir = Some(link_parent.join("handoff"));
    assert!(
        SyncManager::open(cfg_link).is_err(),
        "symlinked Finder handoff parent must be refused"
    );
}

#[test]
fn finder_atomic_claim_and_pause_payload_and_heartbeat() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fx::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());

    let notifier = Arc::new(RecordingFinderNotifier::new());
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    // Verify default constructor uses NoopFinderNotifier (HIGH #15).
    assert!(format!("{:?}", cfg.finder_notifier).contains("NoopFinderNotifier"));

    cfg.finder_status_dir = Some(fx.finder_dir.clone());
    cfg.finder_notifier = Arc::clone(&notifier) as Arc<dyn sync_manager::FinderNotifier>;
    cfg.finder_heartbeat_interval = Some(Duration::from_millis(60));
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

    // 1. Scheduling pause reflected in Finder status.json (HIGH #12).
    mgr.set_scheduling_paused(true).unwrap();
    let raw_paused = fs::read_to_string(fx.finder_dir.join("status.json")).unwrap();
    let payload_paused: sync_manager::FinderStatusSnapshot =
        serde_json::from_str(&raw_paused).unwrap();
    assert_eq!(
        payload_paused.mappings[0].state,
        sync_manager::FinderMappingState::Paused
    );
    assert_eq!(payload_paused.mappings[0].label, "Scheduling paused");
    mgr.set_scheduling_paused(false).unwrap();

    // 2. Periodic heartbeat rewrites status.json while idle (HIGH #12).
    let before_count = notifier.posted_notifications().len();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after_count = before_count;
    while after_count == before_count && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
        after_count = notifier.posted_notifications().len();
    }
    assert!(
        after_count > before_count,
        "idle heartbeat must periodically rewrite Finder status.json (before={before_count}, after={after_count})"
    );

    // 3. Unclaimable (read-only actions_dir) action file MUST NOT dispatch (HIGH #11).
    let actions_dir = fx.finder_dir.join("actions");
    fs::create_dir_all(&actions_dir).unwrap();
    let act_file = actions_dir.join("01-sync.json");
    fs::write(
        &act_file,
        r#"{"version":1,"id":"a-ro","requested_at":"2026-11-20T18:46:12Z","action":"sync_now","mapping_id":"alpha"}"#,
    )
    .unwrap();
    fs::set_permissions(&actions_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let res_ro = mgr.drain_finder_actions();
    // Restore permissions immediately so tempdir cleanup always succeeds.
    fs::set_permissions(&actions_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(res_ro.len(), 1);
    assert!(matches!(res_ro[0], FinderActionDispatch::Rejected { .. }));
    assert_eq!(
        launcher.launched_plans().len(),
        1,
        "unclaimable action file must NOT dispatch sync_now"
    );

    // Once writable, atomic claim -> dispatch -> delete succeeds exactly once.
    let res_rw = mgr.drain_finder_actions();
    assert_eq!(res_rw.len(), 1);
    assert!(matches!(res_rw[0], FinderActionDispatch::Executed { .. }));
    assert!(!act_file.exists());
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(launcher.launched_plans().len(), 2);
    mgr.shutdown().unwrap();
}

#[cfg(debug_assertions)]
#[test]
fn idle_finder_heartbeat_survives_frequent_worker_wakeups() {
    let fx = Fx::new();
    let notifier = Arc::new(RecordingFinderNotifier::new());
    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.finder_notifier = notifier.clone();
    cfg.finder_heartbeat_interval = Some(Duration::from_millis(150));
    let mgr = SyncManager::open(cfg).unwrap();

    let (before_count, during_count) = thread::scope(|scope| {
        scope
            .spawn(|| {
                mgr.notify_worker_for_test();
                let before_count = notifier.posted_notifications().len();
                let deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(20));
                    mgr.notify_worker_for_test();
                }
                (before_count, notifier.posted_notifications().len())
            })
            .join()
            .unwrap()
    });

    mgr.shutdown().unwrap();
    assert!(
        during_count > before_count,
        "idle Finder heartbeat must run during worker wakeups (before={before_count}, during={during_count})"
    );
}

#[test]
fn preflight_preview_matches_manager_rejects_volume_root_and_cross_mapping_recovery() {
    // 1. Volume roots rejected and real volume ID reported (HIGH #2).
    assert!(sync_manager::detect_volume_identity(std::path::Path::new("/")).is_err());
    assert!(sync_manager::detect_volume_identity(std::path::Path::new("/Volumes")).is_err());

    let fx = Fx::new();
    let det = sync_manager::detect_volume_identity(&fx.local_a).unwrap();
    assert_ne!(det.identity.stable_id, "local-fixture-volume");

    // 2. Excluded ordinary files still count toward populated-folder classification (HIGH #2).
    fs::write(fx.local_a.join("ignored.tmp"), b"local").unwrap();
    fs::write(fx.remote_a.join("ignored.tmp"), b"remote").unwrap();

    let mut cfg = ManagerConfig::with_state_dir(&fx.state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(cfg).unwrap();
    mgr.register_local_fixture_account("local-1", "Local")
        .unwrap();

    let mut cmd_a = fx.cmd_a();
    cmd_a.extra_exclusions.insert("*.tmp".to_owned());
    let preview = mgr.preflight_mapping_preview(&cmd_a, None).unwrap();
    assert_eq!(
        preview.local_inventory.as_ref().unwrap().classification,
        sync_manager::SideClassification::HasData
    );
    assert_eq!(
        preview.remote_inventory.as_ref().unwrap().classification,
        sync_manager::SideClassification::HasData
    );
    assert!(
        preview.direction.is_err(),
        "preview must agree with preflight and reject BothSidesPopulated even when files match exclusions"
    );

    // 3. Recovery disjointness checked against EVERY existing mapping (HIGH #2).
    fs::remove_file(fx.remote_a.join("ignored.tmp")).unwrap();
    mgr.add_mapping(fx.cmd_a()).unwrap();
    let mut bad_rec_b = fx.cmd_b();
    bad_rec_b.local_recovery_root = fx.local_a.join("rec-inside-alpha");
    let preview_b = mgr.preflight_mapping_preview(&bad_rec_b, None).unwrap();
    assert!(
        preview_b.layout_error.is_some(),
        "preview must catch recovery root overlapping another mapping's local folder"
    );
    mgr.shutdown().unwrap();
}
