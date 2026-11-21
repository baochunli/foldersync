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
    contract::{ReviewScenario, RunPhase, ScreenRoute, UiCommand, UiSnapshot, WizardStep},
    manager_source::{ManagerSource, ManagerSourceConfig, UiControlEvent},
    system_actions::{
        LaunchAtLoginOutcome, RecordedSystemAction, RecordingSystemActions, SystemActionOutcome,
        SystemActionRequest,
    },
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
    loop {
        let current = mgr.snapshot();
        if pred(&current) {
            return current;
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            panic!(
                "timed out waiting for expected UiSnapshot; last snapshot: mappings={:?}, action_error={:?}, active_run={:?}, account pending={:?}, accounts={:?}",
                current
                    .mappings
                    .iter()
                    .map(|m| (&m.id, m.state))
                    .collect::<Vec<_>>(),
                current.action_error,
                current.active_run,
                current.account_operation_pending,
                current
                    .accounts
                    .iter()
                    .map(|account| (&account.id, &account.status))
                    .collect::<Vec<_>>(),
            );
        }
        std::thread::sleep(rem.min(Duration::from_millis(15)));
    }
}

fn drain_control_until(
    mgr: &ManagerSource,
    timeout: Duration,
    pred: impl Fn(&UiControlEvent) -> bool,
) -> Option<UiControlEvent> {
    mgr.ui_slot().wait_for_control_timeout(timeout, pred)
}

fn test_mgr_config(
    state_dir: PathBuf,
    finder_dir: PathBuf,
    launcher: Arc<dyn sync_manager::BisyncLauncher>,
    notifier: Arc<dyn sync_manager::FinderNotifier>,
) -> ManagerSourceConfig {
    ManagerSourceConfig {
        state_dir: Some(state_dir),
        finder_status_dir: Some(finder_dir),
        launcher: Some(launcher),
        account_service: None,
        manager_accounts: None,
        finder_notifier: Some(notifier),
        finder_heartbeat_interval: None,
        shutdown_timeout: Duration::from_secs(5),
        finder_poll_interval: None,
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
        test_mgr_config(
            state_dir,
            finder_dir.clone(),
            Arc::new(RealBisyncLauncher::new(Some(rclone_bin))),
            notifier.clone(),
        ),
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
            test_mgr_config(
                state_dir.clone(),
                finder_dir.clone(),
                launcher.clone(),
                Arc::new(RecordingFinderNotifier::new()),
            ),
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
        test_mgr_config(
            state_dir,
            finder_dir,
            launcher,
            Arc::new(RecordingFinderNotifier::new()),
        ),
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
        test_mgr_config(
            state_dir,
            finder_dir,
            launcher,
            Arc::new(RecordingFinderNotifier::new()),
        ),
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
        test_mgr_config(
            state_dir,
            finder_dir.clone(),
            launcher,
            Arc::new(RecordingFinderNotifier::new()),
        ),
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

    let nav = drain_control_until(&mgr, Duration::from_secs(5), |u| {
        matches!(
            u,
            UiControlEvent::ShowWindowAndNavigate(ScreenRoute::Overview)
        )
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
    let quit_upd = drain_control_until(&mgr, Duration::from_secs(5), |u| {
        matches!(
            u,
            UiControlEvent::QuitReady(ControlledShutdownOutcome::Clean)
        )
    });
    assert!(
        quit_upd.is_some(),
        "controlled shutdown must stop active run cleanly and emit QuitReady"
    );
    mgr.shutdown();
}

#[test]
fn wizard_saves_edits_exclusions_persists_accounts_and_invalidates_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    let remote_parent_2 = tmp.path().join("remote-parent-2");
    let remote_folder_2 = remote_parent_2.join("Projects");
    let local_folder_2 = tmp.path().join("local/Projects2");
    let local_recovery_2 = tmp.path().join("recovery/Projects2");
    fs::create_dir_all(&remote_folder_2).unwrap();
    fs::create_dir_all(&local_folder_2).unwrap();
    fs::create_dir_all(&local_recovery_2).unwrap();
    fs::write(remote_folder.join("doc.txt"), "hello\n").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    // 1. Register TWO local fixture accounts -> restart -> parent paths persist and wizard opens on Account (HIGH #3).
    {
        let mgr = ManagerSource::spawn(
            test_mgr_config(
                state_dir.clone(),
                finder_dir.clone(),
                launcher.clone(),
                Arc::new(RecordingFinderNotifier::new()),
            ),
            Arc::new(RecordingSystemActions::new()),
        );
        wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.is_demo_mode);
        mgr.dispatch(UiCommand::AddLocalFixtureAccount {
            id: "acct-1".to_owned(),
            display_name: "Account One".to_owned(),
            parent_dir: remote_parent.to_string_lossy().into_owned(),
        });
        mgr.dispatch(UiCommand::AddLocalFixtureAccount {
            id: "acct-2".to_owned(),
            display_name: "Account Two".to_owned(),
            parent_dir: remote_parent_2.to_string_lossy().into_owned(),
        });
        wait_for_snapshot(&mgr, Duration::from_secs(5), |s| s.accounts.len() == 2);
        mgr.shutdown();
    }

    let mgr = ManagerSource::spawn(
        test_mgr_config(
            state_dir,
            finder_dir,
            launcher.clone(),
            Arc::new(RecordingFinderNotifier::new()),
        ),
        Arc::new(RecordingSystemActions::new()),
    );
    let reopened = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| s.accounts.len() == 2);
    assert!(
        !reopened.cloud_catalog.is_empty(),
        "persisted account parent path must populate catalog after restart (HIGH #3)"
    );
    mgr.dispatch(UiCommand::WizardReset);
    let reset_snap = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.wizard.step == WizardStep::Account
    });
    assert_eq!(reset_snap.wizard.step, WizardStep::Account);

    // 2. Save draft 1 with custom pattern `*.bak`.
    mgr.dispatch(UiCommand::WizardSelectAccount(sync_core::AccountId::new(
        "acct-1",
    )));
    mgr.dispatch(UiCommand::WizardSelectFolder("Projects".to_owned()));
    mgr.dispatch(UiCommand::WizardSetLocalPath(
        local_folder.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardAddPattern("*.bak".to_owned()));
    mgr.dispatch(UiCommand::WizardSetLocalRecoveryPath(
        local_recovery.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardSaveDraft);
    let after_draft1 = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.mappings.len() == 1 && s.mappings[0].state.readiness() == Readiness::Unconfigured
    });
    let draft1_id = after_draft1.mappings[0].id.clone();

    // 3. New draft for same folder name on acct-2 gets a UNIQUE ID (never overwrites draft 1) and clean exclusions (HIGH #1, #3).
    mgr.dispatch(UiCommand::WizardReset);
    mgr.dispatch(UiCommand::WizardSelectAccount(sync_core::AccountId::new(
        "acct-2",
    )));
    mgr.dispatch(UiCommand::WizardSelectFolder("Projects".to_owned()));
    mgr.dispatch(UiCommand::WizardSetLocalPath(
        local_folder_2.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardSetLocalRecoveryPath(
        local_recovery_2.to_string_lossy().into_owned(),
    ));
    mgr.dispatch(UiCommand::WizardSaveDraft);
    let after_draft2 = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| s.mappings.len() == 2);
    assert!(
        after_draft2.wizard.custom_patterns.is_empty(),
        "new draft must not inherit previous wizard exclusions (HIGH #1)"
    );
    let draft2_id = after_draft2
        .mappings
        .iter()
        .find(|m| m.id != draft1_id)
        .unwrap()
        .id
        .clone();
    assert_ne!(draft1_id, draft2_id);

    // 4. Resume draft 1 -> confirms `*.bak` restored -> add `*.log` -> confirm -> mutate disk -> Start invalidates confirmation (HIGH #1)!
    mgr.dispatch(UiCommand::ResumeDraft(draft1_id.clone()));
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.wizard.editing_mapping_id.as_ref() == Some(&draft1_id)
            && s.wizard.custom_patterns.contains(&"*.bak".to_owned())
    });
    mgr.dispatch(UiCommand::WizardAddPattern("*.log".to_owned()));
    mgr.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.wizard.no_other_sync_confirmed
    });

    // Mutate remote inventory AFTER user confirmed -> Start MUST invalidate confirmation!
    fs::write(remote_folder.join("extra.txt"), "new file after review\n").unwrap();
    mgr.dispatch(UiCommand::WizardStartFirstSync);
    let invalidated = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        !s.wizard.no_other_sync_confirmed && s.action_error.is_some()
    });
    assert_eq!(
        invalidated.wizard.review_scenario,
        ReviewScenario::CloudToLocal
    );

    // Re-confirm and start -> edited `*.log` exclusion MUST be persisted in the initialized mapping!
    mgr.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
    mgr.dispatch(UiCommand::WizardStartFirstSync);
    let ready = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
        s.mapping(&draft1_id)
            .is_some_and(|m| m.state.readiness() == Readiness::Ready)
    });
    let m1 = ready.mapping(&draft1_id).unwrap();
    assert!(
        m1.custom_exclusions.contains(&"*.bak".to_owned())
            && m1.custom_exclusions.contains(&"*.log".to_owned()),
        "Start must save reviewed exclusions before initialization, got {:?}",
        m1.custom_exclusions
    );
    mgr.shutdown();
}

#[test]
fn channel_system_actions_live_transfer_progress_and_recovery_identity_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    fs::write(remote_folder.join("big.bin"), "payload\n").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    // Init succeeds
    launcher.push_script(FakeLaunchScript::default());
    // Run 2 emits live rclone JSON stats while waiting for stop -> Activity must show TransferringKnown (HIGH #14)!
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"info","msg":"Transferred: 512 / 1024","stats":{"bytes":512,"totalBytes":1024,"transfers":1,"checks":2}}"#.to_owned(),
            truncated: false,
        }],
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: Some(130),
            signal: None,
        }),
        ..Default::default()
    });

    // Use `spawn_for_app` (channel-based main-thread system actions, NEVER panics off-thread, HIGH #8)!
    let mgr = ManagerSource::spawn_for_app(test_mgr_config(
        state_dir,
        finder_dir,
        launcher,
        Arc::new(RecordingFinderNotifier::new()),
    ));
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.is_demo_mode);

    // Dispatch SetOpenAtLogin -> verify SystemActionRequest arrives over channel and real outcome applies!
    mgr.dispatch(UiCommand::SetOpenAtLogin(true));
    let ev = drain_control_until(&mgr, Duration::from_secs(5), |u| {
        matches!(
            u,
            UiControlEvent::SystemAction {
                request: SystemActionRequest::SetLaunchAtLogin(true),
                ..
            }
        )
    })
    .expect("must dispatch SetLaunchAtLogin over channel to main thread");
    if let UiControlEvent::SystemAction { request_id, .. } = ev {
        mgr.complete_system_action(
            request_id,
            SystemActionOutcome::SetLaunchAtLogin(Ok(LaunchAtLoginOutcome::Unavailable)),
        );
    }
    let after_login = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.status_notice
            .as_deref()
            .is_some_and(|n| n.contains("packaged"))
    });
    assert!(!after_login.preferences.open_at_login);

    // Initialize mapping via wizard.
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
    assert!(ready.wizard.first_sync_preflight_done);
    assert!(ready.wizard.first_sync_markers_ready);
    let mid = ready.mappings[0].id.clone();

    // Start Run 2 -> observe live RunPhase::TransferringKnown (50%) on Activity (HIGH #14)!
    mgr.dispatch(UiCommand::SyncNow(mid.clone()));
    let transferring = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        matches!(
            s.active_run.as_ref().map(|r| &r.phase),
            Some(RunPhase::TransferringKnown { percent: 50, .. })
        )
    });
    assert!(transferring.active_run.is_some());

    // Stop Run 2 -> Interrupted -> RecoveryRequired -> InspectRecovery verifies real identities (HIGH #9)!
    mgr.dispatch(UiCommand::StopRun(mid.clone()));
    wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.mapping(&mid)
            .is_some_and(|m| m.state.readiness() == Readiness::RecoveryRequired)
    });
    mgr.dispatch(UiCommand::InspectRecovery(mid.clone()));
    let rec_ok = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.recovery_checklist.as_ref().is_some_and(|rc| rc.inspected)
    });
    assert!(
        rec_ok
            .recovery_checklist
            .as_ref()
            .unwrap()
            .identities_verified,
        "InspectRecovery must report verified identities when markers match"
    );

    // Remove local RCLONE_TEST marker -> InspectRecovery MUST report identities_verified == false!
    fs::remove_file(local_folder.join("RCLONE_TEST")).unwrap();
    mgr.dispatch(UiCommand::InspectRecovery(mid.clone()));
    let rec_bad = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.recovery_checklist
            .as_ref()
            .is_some_and(|rc| rc.inspected && !rc.identities_verified)
    });
    assert!(
        !rec_bad
            .recovery_checklist
            .as_ref()
            .unwrap()
            .identities_verified
    );
    mgr.shutdown();
}

#[test]
fn manager_source_confirms_new_subfolders_before_reinitializing() {
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, remote_parent, remote_folder, local_folder, local_recovery) =
        setup_dirs(tmp.path());
    let omitted = remote_folder.join("Photos");
    fs::create_dir(&omitted).unwrap();
    fs::write(omitted.join("photo.jpg"), b"fixture").unwrap();

    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());
    launcher.push_script(FakeLaunchScript::default());
    let mgr = ManagerSource::spawn(
        test_mgr_config(
            state_dir,
            finder_dir,
            launcher,
            Arc::new(RecordingFinderNotifier::new()),
        ),
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
        s.mappings
            .first()
            .is_some_and(|m| m.state.readiness() == Readiness::Ready)
    });
    let id = ready.mappings[0].id.clone();
    let generation = ready.mappings[0].generation;
    assert!(ready.mappings[0].scheduling_enabled);
    assert_eq!(ready.mappings[0].schedule_minutes, 5);
    assert!(ready.volumes[0].total_gb > 0);

    mgr.dispatch(UiCommand::SetSchedulingPaused(true));
    mgr.dispatch(UiCommand::SetScheduleInterval {
        mapping_id: id.clone(),
        minutes: 15,
    });
    mgr.dispatch(UiCommand::ChangeSubfolders(id.clone()));
    let select = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.wizard.changing_subfolders
    });
    assert!(select.scheduling_paused);
    assert_eq!(select.mappings[0].schedule_minutes, 15);
    let photos = select
        .wizard
        .subfolders
        .iter()
        .find(|n| n.name == "Photos")
        .unwrap();
    assert_eq!(photos.size_label, "7 B");

    mgr.dispatch(UiCommand::WizardToggleSubfolder(photos.id.clone()));
    mgr.dispatch(UiCommand::PreviewFilterChange);
    let review = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.wizard.filter_review.is_some()
    });
    let diff = review.wizard.filter_review.as_ref().unwrap();
    assert_eq!(diff.added, ["/Photos/**"]);
    assert!(diff.can_apply(), "{:?}", diff.checks);
    assert_eq!(review.mappings[0].generation, generation);
    mgr.dispatch(UiCommand::ApplyFilterChange);
    mgr.dispatch(UiCommand::SetScheduleInterval {
        mapping_id: id,
        minutes: 30,
    });
    let gated = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.mappings[0].schedule_minutes == 30
    });
    assert_eq!(gated.mappings[0].generation, generation);
    assert!(gated.mappings[0].excluded_subfolders.is_empty());

    mgr.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
    mgr.dispatch(UiCommand::ApplyFilterChange);
    let applied = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| {
        s.wizard.step == WizardStep::FirstSync
            && s.mappings[0].generation > generation
            && s.mappings[0].state.readiness() == Readiness::Ready
    });
    assert!(applied.scheduling_paused);
    assert!(applied.mappings[0].scheduling_enabled);
    assert_eq!(applied.mappings[0].excluded_subfolders[0].0, "Photos");
    assert_eq!(fs::read(omitted.join("photo.jpg")).unwrap(), b"fixture");
    mgr.shutdown();
}

#[test]
fn manager_source_reports_pending_verified_and_held_drive_access() {
    use nsh_app_sync_ui::contract::{AccountStatus, FolderSize};
    use rclone_adapter::{FakeProcessRunner, FakeRunScript};
    use sync_core::{AccountId, DeviceId};
    use sync_manager::{
        AccountRecord, AccountService, AccountServiceConfig, InMemorySecretStore, SecretStore,
        SecretString, scoped_credential_key,
        store::{DeviceRecord, SqliteStateStore},
    };

    let Some(binary) = pinned_rclone_path() else {
        eprintln!(
            "skipping fake account-operation integration test: pinned rclone binary not found"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (state_dir, finder_dir, ..) = setup_dirs(&tmp.path().canonicalize().unwrap());
    let account_dir = state_dir.join("accounts/drive-test");
    fs::create_dir_all(&account_dir).unwrap();
    let config_path = account_dir.join("rclone.conf");
    fs::write(&config_path, "RCLONE_ENCRYPT_V0:\nFakeCiphertext==\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let store = SqliteStateStore::open(&state_dir.join("state.sqlite")).unwrap();
    let when = "2026-01-01T00:00:00Z".to_owned();
    let device = store
        .ensure_device(&DeviceRecord {
            id: DeviceId::new("test-device"),
            name: "Test device".to_owned(),
            created_at: when.clone(),
        })
        .unwrap();
    let id = AccountId::new("drive-test");
    store
        .upsert_account(&AccountRecord {
            id: id.clone(),
            display_name: "Test Drive".to_owned(),
            kind: "drive".to_owned(),
            remote_name: "test-drive".to_owned(),
            remote_root_folder_id: "test-parent".to_owned(),
            config_path,
            auth_status: "pending".to_owned(),
            created_at: when.clone(),
            updated_at: when,
        })
        .unwrap();
    drop(store);

    let secrets = Arc::new(InMemorySecretStore::new());
    let key = scoped_credential_key(device.id.as_str(), id.as_str()).unwrap();
    secrets
        .create_secret(&key, &SecretString::new("fixture"))
        .unwrap();
    let runner = Arc::new(FakeProcessRunner::new());
    let script = |stream, text: &str| FakeRunScript {
        lines: vec![OutputLine {
            stream,
            text: text.to_owned(),
            truncated: false,
        }],
        ..Default::default()
    };
    runner.push_run(script(
        StreamKind::Stderr,
        "NOTICE: Please go to the following link: http://127.0.0.1:53682/auth?state=fixture",
    ));
    runner.push_run(FakeRunScript::default());
    runner.push_run(script(
        StreamKind::Stdout,
        "[test-drive]\ntype = drive\ntoken = XXX",
    ));
    runner.push_run(script(StreamKind::Stdout, "[]"));
    let account_config = || {
        let mut config = AccountServiceConfig::with_state_dir(&state_dir);
        config.rclone_binary_override = Some(binary.clone());
        config.secret_store = secrets.clone();
        config.process_runner = runner.clone();
        config
    };
    let manager_accounts = account_config();
    let service = AccountService::for_locked_manager(account_config()).unwrap();

    let mut config = test_mgr_config(
        state_dir,
        finder_dir,
        Arc::new(FakeBisyncLauncher::new()),
        Arc::new(RecordingFinderNotifier::new()),
    );
    config.account_service = Some(Arc::new(service));
    config.manager_accounts = Some(manager_accounts);
    let sys = Arc::new(RecordingSystemActions::new());
    let mgr = ManagerSource::spawn(config, sys.clone());
    let pending = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| !s.accounts.is_empty());
    assert_eq!(pending.accounts[0].status, AccountStatus::Unverified);
    mgr.dispatch(UiCommand::ReconnectAccount(id.clone()));
    let connected = wait_for_snapshot(&mgr, Duration::from_secs(40), |s| {
        s.accounts[0].status == AccountStatus::Connected || s.action_error.is_some()
    });
    assert_eq!(connected.action_error, None);
    assert!(
        connected.accounts[0]
            .access_summary
            .contains("Shared folder listing verified")
    );
    assert_eq!(
        sys.recorded(),
        vec![RecordedSystemAction::OpenDriveAuthorization]
    );

    let data = r#"[{"Name":"Data","Path":"Data","IsDir":true,"ID":"data-id"}]"#;
    let research = r#"[{"Name":"Research","Path":"Research","IsDir":true,"ID":"research-id"}]"#;
    let with_skipped = r#"[
        {"Name":"Research","Path":"Research","IsDir":true,"ID":"research-id"},
        {"Name":"Old","Path":"Old","IsDir":true,"ID":"old-one"},
        {"Name":"Old","Path":"Old","IsDir":true,"ID":"old-two"}
    ]"#;
    let listing = |children| {
        runner.push_run(script(StreamKind::Stdout, data));
        runner.push_run(script(StreamKind::Stdout, children));
    };
    listing(with_skipped);
    mgr.dispatch(UiCommand::WizardReset);
    let catalog = wait_for_snapshot(&mgr, Duration::from_secs(20), |s| {
        !s.cloud_catalog.is_empty() || s.cloud_catalog_error.is_some()
    });
    assert_eq!(catalog.cloud_catalog_error, None);
    assert_eq!(catalog.cloud_catalog.len(), 1);
    assert_eq!(catalog.cloud_catalog[0].size, FolderSize::Unmeasured);
    assert_eq!(catalog.skipped_cloud_folders[0].name, "Old");
    assert_eq!(
        catalog.skipped_cloud_folders[0].reason,
        sync_manager::CollectionSkipReason::Duplicate
    );

    listing(research);
    runner.push_run(FakeRunScript {
        wait_for_stop: true,
        ..Default::default()
    });
    let expected_spawn_count = runner.spawned_specs().len() + 3;
    mgr.dispatch(UiCommand::WizardSelectFolder("Research".to_owned()));
    let measuring = wait_for_snapshot(&mgr, Duration::from_secs(20), |s| s.folder_size_pending);
    assert_eq!(measuring.cloud_catalog[0].size, FolderSize::Calculating);
    assert_eq!(
        measuring
            .wizard
            .drive_collection
            .as_ref()
            .unwrap()
            .ancestor_ids,
        ["test-parent", "data-id"]
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while runner.spawned_specs().len() < expected_spawn_count {
        assert!(Instant::now() < deadline, "fake size process did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    mgr.dispatch(UiCommand::WizardCancelFolderSize);
    let canceled = wait_for_snapshot(&mgr, Duration::from_secs(10), |s| !s.folder_size_pending);
    assert_eq!(canceled.cloud_catalog[0].size, FolderSize::Unmeasured);

    listing(research);
    runner.push_run(script(
        StreamKind::Stdout,
        r#"{"count":2,"bytes":25,"sizeless":0}"#,
    ));
    listing(research);
    mgr.dispatch(UiCommand::WizardSelectFolder("Research".to_owned()));
    let measured = wait_for_snapshot(&mgr, Duration::from_secs(30), |s| {
        matches!(
            s.cloud_catalog[0].size,
            FolderSize::Known { .. } | FolderSize::Failed { .. }
        )
    });
    assert!(
        matches!(
            measured.cloud_catalog[0].size,
            FolderSize::Known {
                bytes: 25,
                files: Some(2),
                ..
            }
        ),
        "{:?}",
        measured.cloud_catalog[0].size
    );

    listing(research);
    mgr.dispatch(UiCommand::WizardCreateNewFolder("Research".to_owned()));
    let duplicate = wait_for_snapshot(&mgr, Duration::from_secs(20), |s| s.action_error.is_some());
    assert!(
        duplicate
            .action_error
            .unwrap()
            .contains("already used in Data")
    );

    secrets.set_locked(true);
    mgr.dispatch(UiCommand::CheckAccountAccess(id.clone()));
    let held = wait_for_snapshot(&mgr, Duration::from_secs(5), |s| {
        s.accounts[0].status == AccountStatus::SignInRequired
    });
    assert!(held.action_error.unwrap().contains("choose Check access"));
    secrets.set_locked(false);

    let mut browser_wait = script(
        StreamKind::Stderr,
        "NOTICE: Please go to the following link: http://127.0.0.1:53682/auth?state=fixture",
    );
    browser_wait.wait_for_stop = true;
    runner.push_run(browser_wait);
    mgr.dispatch(UiCommand::ReconnectAccount(id));
    wait_for_snapshot(&mgr, Duration::from_secs(20), |s| {
        s.can_reopen_authorization
    });
    mgr.request_controlled_shutdown();
    let quit = drain_control_until(&mgr, Duration::from_secs(4), |event| {
        matches!(event, UiControlEvent::QuitReady(_))
    });
    assert!(matches!(
        quit,
        Some(UiControlEvent::QuitReady(ControlledShutdownOutcome::Clean))
    ));
    mgr.shutdown();
}
