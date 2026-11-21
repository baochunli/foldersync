//! Integration tests for the real `ManagerSource` wiring in `nsh-app-sync-ui`.
//!
//! All tests use isolated temp directories, injectable `RecordingSystemActions`
//! (zero real macOS system calls), and `RecordingFinderNotifier`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use nsh_app_sync_ui::{
    contract::{ScreenRoute, UiCommand, UiSnapshot},
    manager_source::{ManagerSource, ManagerSourceConfig, UiUpdate},
    system_actions::{RecordedSystemAction, RecordingSystemActions},
};
use rclone_adapter::{ExitStatusInfo, OutputLine, StreamKind};
use sync_core::{Execution, Intent, MappingId, Readiness, RunOutcome};
use sync_manager::{
    ControlledShutdownOutcome, FakeBisyncLauncher, FakeLaunchScript, RealBisyncLauncher,
    RecordingFinderNotifier,
};

fn pinned_rclone_path() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent()?.parent()?;
    let candidate = repo_root.join(".tools/rclone/1.76.0/rclone");
    candidate.is_file().then_some(candidate)
}

fn wait_for_snapshot(
    mgr: &ManagerSource,
    timeout: Duration,
    mut pred: impl FnMut(&UiSnapshot) -> bool,
) -> UiSnapshot {
    let deadline = Instant::now() + timeout;
    let rx = mgr.updates_receiver();
    loop {
        let current = mgr.snapshot();
        if pred(&current) {
            return current;
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            panic!(
                "timed out waiting for expected UiSnapshot; last snapshot: mappings={:?}, action_error={:?}, active_run={:?}",
                current
                    .mappings
                    .iter()
                    .map(|m| (&m.id, m.state))
                    .collect::<Vec<_>>(),
                current.action_error,
                current.active_run
            );
        }
        let step = rem.min(Duration::from_millis(50));
        let _ = rx.lock().unwrap().recv_timeout(step);
    }
}

fn drain_updates_until(
    mgr: &ManagerSource,
    timeout: Duration,
    mut pred: impl FnMut(&UiUpdate) -> bool,
) -> Option<UiUpdate> {
    let deadline = Instant::now() + timeout;
    let rx = mgr.updates_receiver();
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return None;
        }
        if let Ok(upd) = rx
            .lock()
            .unwrap()
            .recv_timeout(rem.min(Duration::from_millis(50)))
            && pred(&upd)
        {
            return Some(upd);
        }
    }
}

fn setup_dirs(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let state_dir = root.join("state");
    let finder_dir = root.join("finder");
    let remote_parent = root.join("remote-parent");
    let remote_folder = remote_parent.join("Projects");
    let local_folder = root.join("local/Projects");
    let local_recovery = root.join("recovery/Projects");
    fs::create_dir_all(&remote_folder).unwrap();
    fs::create_dir_all(&local_folder).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();
    (
        state_dir,
        finder_dir,
        remote_parent,
        remote_folder,
        local_folder,
        local_recovery,
    )
}

fn drive_wizard_init(
    mgr: &ManagerSource,
    remote_parent: &Path,
    remote_folder: &Path,
    local_folder: &Path,
    local_recovery: &Path,
) {
    mgr.dispatch(UiCommand::AddLocalFixtureAccount {
        id: "fixture".to_owned(),
        display_name: "Local Fixture".to_owned(),
        parent_dir: remote_parent.to_string_lossy().into_owned(),
    });
    mgr.dispatch(UiCommand::WizardSelectAccount(sync_core::AccountId::new(
        "fixture",
    )));
    mgr.dispatch(UiCommand::WizardSetRemotePath(
        remote_folder.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardSetLocalPath(
        local_folder.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardSetLocalRecoveryPath(
        local_recovery.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
    mgr.dispatch(UiCommand::WizardStartFirstSync);
}

#[test]
fn manager_source_add_init_sync_now_real_pinned_rclone() {
    let Some(rclone_bin) = pinned_rclone_path() else {
        eprintln!(
            "skipping real pinned rclone UI integration test: pinned rclone binary not found"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    fs::write(remote_folder.join("hello.txt"), "from cloud fixture\n").unwrap();

    let sys = Arc::new(RecordingSystemActions::new());
    let notifier = Arc::new(RecordingFinderNotifier::new());
    let mgr = ManagerSource::spawn(
        ManagerSourceConfig {
            state_dir: Some(state_dir),
            finder_status_dir: Some(finder_dir.clone()),
            launcher: Some(Arc::new(RealBisyncLauncher::new(Some(rclone_bin)))),
            finder_notifier: Some(notifier.clone()),
            shutdown_timeout: Duration::from_secs(5),
            finder_poll_interval: None,
        },
        sys.clone(),
    );

    let init_snap = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.is_demo_mode);
    assert!(init_snap.mappings.is_empty());

    drive_wizard_init(
        &mgr,
        &remote_parent,
        &remote_folder,
        &local_folder,
        &local_recovery,
    );

    // Wait for real rclone initialization to finish and mapping to become Ready + Idle.
    let ready_snap = wait_for_snapshot(&mgr, Duration::from_secs(30), |s| {
        s.mappings.iter().any(|m| {
            m.state.readiness() == Readiness::Ready && m.state.execution == Execution::Idle
        })
    });
    assert!(ready_snap.wizard.first_sync_completed);
    assert_eq!(
        fs::read_to_string(local_folder.join("hello.txt")).unwrap(),
        "from cloud fixture\n"
    );

    // Create a local file and run Sync now through the UI command.
    let mid = ready_snap.mappings[0].id.clone();
    fs::write(local_folder.join("added-locally.txt"), "local change\n").unwrap();
    let prev_runs = ready_snap.mappings[0].recent_runs.len();
    mgr.dispatch(UiCommand::SyncNow(mid.clone()));

    let synced_snap = wait_for_snapshot(&mgr, Duration::from_secs(30), |s| {
        s.mapping(&mid).is_some_and(|m| {
            m.state.execution == Execution::Idle && m.recent_runs.len() > prev_runs
        })
    });
    assert_eq!(
        fs::read_to_string(remote_folder.join("added-locally.txt")).unwrap(),
        "local change\n"
    );
    assert_eq!(
        synced_snap.mapping(&mid).unwrap().last_run.unwrap().outcome,
        RunOutcome::Success
    );
    assert!(
        !notifier.posted_notifications().is_empty(),
        "Finder Darwin notifier must fire after status.json writes"
    );
    assert!(
        sys.recorded().is_empty(),
        "no real OS system actions should have been invoked"
    );
    mgr.shutdown();
}

#[test]
fn pause_survives_restart_and_conflict_ack_preserves_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    fs::write(remote_folder.join("doc.txt"), "v1\n").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"notice","msg":"- Path1    Renaming Path1 copy - /tmp/p1/doc.txt.conflict1","source":"bisync/resolve.go:318"}"#.to_owned(),
            truncated: false,
        }],
        ..Default::default()
    });

    let sys = Arc::new(RecordingSystemActions::new());
    let mid: MappingId;
    {
        let mgr = ManagerSource::spawn(
            ManagerSourceConfig {
                state_dir: Some(state_dir.clone()),
                finder_status_dir: Some(finder_dir.clone()),
                launcher: Some(launcher.clone()),
                finder_notifier: Some(Arc::new(RecordingFinderNotifier::new())),
                shutdown_timeout: Duration::from_secs(5),
                finder_poll_interval: None,
            },
            sys.clone(),
        );
        wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.is_demo_mode);

        drive_wizard_init(
            &mgr,
            &remote_parent,
            &remote_folder,
            &local_folder,
            &local_recovery,
        );

        let ready = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
            s.mappings.iter().any(|m| {
                m.state.readiness() == Readiness::Ready && m.state.execution == Execution::Idle
            })
        });
        mid = ready.mappings[0].id.clone();

        // Trigger a conflict run.
        mgr.dispatch(UiCommand::SyncNow(mid.clone()));
        let conflicted = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
            s.mapping(&mid)
                .is_some_and(|m| m.unreviewed_conflicts.is_some())
        });
        assert_eq!(conflicted.attention_badge_count(), 1);

        // Pause must preserve unreviewed conflicts and survive restart.
        mgr.dispatch(UiCommand::PauseMapping(mid.clone()));
        let paused = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
            s.mapping(&mid)
                .is_some_and(|m| m.state.intent == Intent::Paused)
        });
        assert!(paused.mapping(&mid).unwrap().unreviewed_conflicts.is_some());
        mgr.shutdown();
    }

    // Reopen fresh ManagerSource on same state dir -> Pause & unreviewed conflicts survive restart.
    let mgr2 = ManagerSource::spawn(
        ManagerSourceConfig {
            state_dir: Some(state_dir),
            finder_status_dir: Some(finder_dir),
            launcher: Some(launcher),
            finder_notifier: Some(Arc::new(RecordingFinderNotifier::new())),
            shutdown_timeout: Duration::from_secs(5),
            finder_poll_interval: None,
        },
        sys,
    );
    let reopened = wait_for_snapshot(&mgr2, Duration::from_secs(5), |s| s.mapping(&mid).is_some());
    let m = reopened.mapping(&mid).unwrap();
    assert_eq!(m.state.intent, Intent::Paused);
    assert!(m.unreviewed_conflicts.is_some());

    // Mark conflicts reviewed -> clears unreviewed notice while keeping SuccessWithConflicts outcome.
    mgr2.dispatch(UiCommand::MarkConflictsReviewed(mid.clone()));
    let reviewed = wait_for_snapshot(&mgr2, Duration::from_secs(5), |s| {
        s.mapping(&mid)
            .is_some_and(|m| m.unreviewed_conflicts.is_none())
    });
    assert_eq!(reviewed.attention_badge_count(), 0);
    assert_eq!(
        reviewed.mapping(&mid).unwrap().last_run.unwrap().outcome,
        RunOutcome::SuccessWithConflicts
    );
    mgr2.shutdown();
}

#[test]
fn recovery_inspection_lists_copies_and_refuses_live_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    fs::write(remote_folder.join("notes.txt"), "v1\n").unwrap();
    let inspection_dest = tmp.path().join("inspection-out");
    fs::create_dir_all(&inspection_dest).unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"info","msg":"init-specific-log-line"}"#.to_owned(),
            truncated: false,
        }],
        ..Default::default()
    });
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"info","msg":"second-run-specific-log-line"}"#.to_owned(),
            truncated: false,
        }],
        ..Default::default()
    });

    let mgr = ManagerSource::spawn(
        ManagerSourceConfig {
            state_dir: Some(state_dir),
            finder_status_dir: Some(finder_dir),
            launcher: Some(launcher),
            finder_notifier: Some(Arc::new(RecordingFinderNotifier::new())),
            shutdown_timeout: Duration::from_secs(5),
            finder_poll_interval: None,
        },
        Arc::new(RecordingSystemActions::new()),
    );
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.is_demo_mode);

    drive_wizard_init(
        &mgr,
        &remote_parent,
        &remote_folder,
        &local_folder,
        &local_recovery,
    );

    let ready = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
        s.mappings.iter().any(|m| {
            m.state.readiness() == Readiness::Ready && m.state.execution == Execution::Idle
        })
    });
    let mid = ready.mappings[0].id.clone();
    let prev_runs = ready.mappings[0].recent_runs.len();

    mgr.dispatch(UiCommand::SyncNow(mid.clone()));
    let after = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
        s.mapping(&mid).is_some_and(|m| {
            m.state.execution == Execution::Idle && m.recent_runs.len() > prev_runs
        })
    });
    let run_id = after.mapping(&mid).unwrap().recent_runs[0].run_id.clone();

    // Seed a preserved recovery copy inside the per-run local recovery directory
    // (`<local_recovery_root>/<mapping_id>/<run_id>/`).
    let run_backup_dir = local_recovery.join(mid.as_str()).join(run_id.as_str());
    fs::create_dir_all(&run_backup_dir).unwrap();
    fs::write(
        run_backup_dir.join("notes.txt"),
        "preserved earlier version\n",
    )
    .unwrap();

    mgr.dispatch(UiCommand::InspectRun {
        mapping_id: mid.clone(),
        run_id: run_id.clone(),
    });
    let inspected = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.inspected_run.as_ref().is_some_and(|r| r.run_id == run_id)
    });
    let insp = inspected.inspected_run.as_ref().unwrap();
    assert!(
        insp.log_lines
            .iter()
            .any(|l| l.contains("second-run-specific-log-line")),
        "expected run-specific log line, got {:?}",
        insp.log_lines
    );
    assert_eq!(insp.recovery_copies.len(), 1);
    assert_eq!(insp.recovery_copies[0].relative_path, "notes.txt");

    // Refuses copying into live synced folder!
    mgr.dispatch(UiCommand::CopyRecoveryCopyToInspection {
        run_id: run_id.clone(),
        is_remote: false,
        relative_path: "notes.txt".to_owned(),
        dest_dir: local_folder.to_string_lossy().into_owned(),
    });
    let refused = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| s.action_error.is_some());
    assert!(refused.action_error.as_deref().unwrap().contains("synced"));
    assert!(!local_folder.join("notes.txt").exists());

    // Succeeds copying into separate inspection folder.
    mgr.dispatch(UiCommand::DismissError);
    mgr.dispatch(UiCommand::CopyRecoveryCopyToInspection {
        run_id,
        is_remote: false,
        relative_path: "notes.txt".to_owned(),
        dest_dir: inspection_dest.to_string_lossy().into_owned(),
    });
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.inspected_run
            .as_ref()
            .and_then(|r| r.copy_notice.as_ref())
            .is_some()
    });
    assert_eq!(
        fs::read_to_string(inspection_dest.join("notes.txt")).unwrap(),
        "preserved earlier version\n"
    );
    mgr.shutdown();
}

#[test]
fn finder_action_files_system_actions_recording_and_controlled_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    fs::write(remote_folder.join("a.txt"), "a\n").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: Some(130),
            signal: None,
        }),
        ..Default::default()
    });

    let sys = Arc::new(RecordingSystemActions::new());
    let mgr = ManagerSource::spawn(
        ManagerSourceConfig {
            state_dir: Some(state_dir),
            finder_status_dir: Some(finder_dir.clone()),
            launcher: Some(launcher),
            finder_notifier: Some(Arc::new(RecordingFinderNotifier::new())),
            shutdown_timeout: Duration::from_secs(5),
            finder_poll_interval: None,
        },
        sys.clone(),
    );
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.is_demo_mode);

    // System actions go through the recording fake and never touch real macOS.
    mgr.dispatch(UiCommand::RevealPath(
        local_folder.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::OpenCloudUrl(
        "https://drive.google.com".to_owned(),
    ));
    mgr.dispatch(UiCommand::SetOpenAtLogin(true));
    // Burst scheduling toggles -> latest snapshot state delivered without drops.
    for i in 0..21 {
        mgr.dispatch(UiCommand::SetSchedulingPaused(i % 2 == 0));
    }
    let burst_snap = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.preferences.open_at_login && s.scheduling_paused
    });
    assert!(burst_snap.scheduling_paused);
    mgr.dispatch(UiCommand::SetSchedulingPaused(false));
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.scheduling_paused);
    let recorded = sys.recorded();
    assert!(
        recorded
            .iter()
            .any(|a| matches!(a, RecordedSystemAction::RevealPath(_)))
    );
    assert!(
        recorded
            .iter()
            .any(|a| matches!(a, RecordedSystemAction::OpenUrl(_)))
    );
    assert!(
        recorded
            .iter()
            .any(|a| matches!(a, RecordedSystemAction::SetLaunchAtLogin(true)))
    );

    // Create & init mapping.
    drive_wizard_init(
        &mgr,
        &remote_parent,
        &remote_folder,
        &local_folder,
        &local_recovery,
    );

    let ready = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
        s.mappings.iter().any(|m| {
            m.state.readiness() == Readiness::Ready && m.state.execution == Execution::Idle
        })
    });
    let mid = ready.mappings[0].id.clone();

    // Write valid ADR 0002 v1 Finder action files (`open_app` and `sync_now`).
    let actions_dir = finder_dir.join("actions");
    fs::create_dir_all(&actions_dir).unwrap();
    fs::write(
        actions_dir.join("01-open.json"),
        r#"{"version":1,"id":"act-1","requested_at":"2026-11-20T14:46:00Z","action":"open_app"}"#,
    )
    .unwrap();
    fs::write(
        actions_dir.join("02-sync.json"),
        format!(
            r#"{{"version":1,"id":"act-2","requested_at":"2026-11-20T14:46:01Z","action":"sync_now","mapping_id":"{}"}}"#,
            mid
        ),
    )
    .unwrap();
    mgr.poll_finder_actions();

    let nav = drain_updates_until(&mgr, Duration::from_secs(5), |u| {
        matches!(u, UiUpdate::ShowWindowAndNavigate(ScreenRoute::Overview))
    });
    assert!(
        nav.is_some(),
        "open_app Finder action must navigate to Overview"
    );
    assert!(!actions_dir.join("01-open.json").exists());
    assert!(!actions_dir.join("02-sync.json").exists());

    // Wait for the Finder-triggered SyncNow run to start, then invoke controlled shutdown!
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.mapping(&mid)
            .is_some_and(|m| m.state.execution == Execution::Running)
    });
    mgr.request_controlled_shutdown();
    let quit_upd = drain_updates_until(&mgr, Duration::from_secs(5), |u| {
        matches!(u, UiUpdate::QuitReady(ControlledShutdownOutcome::Clean))
    });
    assert!(
        quit_upd.is_some(),
        "controlled shutdown must stop active run cleanly and emit QuitReady"
    );
    mgr.shutdown();
}
