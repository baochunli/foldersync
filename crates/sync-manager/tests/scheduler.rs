use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use chrono::{TimeDelta, Utc};
use rclone_adapter::{ExitStatusInfo, RunMode};
use sync_core::{Execution, MappingId, Readiness, RunOutcome};
use sync_manager::{
    AddMappingCommand, FakeBisyncLauncher, FakeLaunchScript, FixedClock, InitializeCommand,
    LocalChangeWatcher, ManagerConfig, OsVolumeProbe, StatusSnapshot, SyncManager,
};
use tempfile::TempDir;

const DEADLINE: Duration = Duration::from_secs(15);

struct FakeWatcher {
    changes: mpsc::Receiver<BTreeSet<PathBuf>>,
    roots: mpsc::Sender<BTreeSet<PathBuf>>,
}

impl LocalChangeWatcher for FakeWatcher {
    fn set_roots(&mut self, roots: &BTreeSet<PathBuf>) {
        let _ = self.roots.send(roots.clone());
    }

    fn take_changed_roots(&mut self) -> BTreeSet<PathBuf> {
        self.changes.recv_timeout(DEADLINE).unwrap_or_default()
    }
}

struct Fixture {
    // Disconnect the artificial watcher before dropping the manager that joins it.
    changes: mpsc::Sender<BTreeSet<PathBuf>>,
    manager: SyncManager,
    roots: mpsc::Receiver<BTreeSet<PathBuf>>,
    launcher: Arc<FakeBisyncLauncher>,
    clock: Arc<FixedClock>,
    epoch: chrono::DateTime<Utc>,
    temp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let epoch = "2026-01-01T00:00:00Z".parse().unwrap();
        let clock = Arc::new(FixedClock::new(epoch));
        let launcher = Arc::new(FakeBisyncLauncher::new());
        let mut config = ManagerConfig::with_state_dir(temp.path().join("state"));
        config.clock = clock.clone();
        config.launcher = launcher.clone();
        config.finder_heartbeat_interval = None;
        let (changes, changed_rx) = mpsc::channel();
        let (roots_tx, roots) = mpsc::channel();
        let manager = SyncManager::open_with_watcher(
            config,
            Arc::new(OsVolumeProbe),
            Box::new(FakeWatcher {
                changes: changed_rx,
                roots: roots_tx,
            }),
        )
        .unwrap();
        manager
            .register_local_fixture_account("local", "Local fixture")
            .unwrap();
        Self {
            changes,
            manager,
            roots,
            launcher,
            clock,
            epoch,
            temp,
        }
    }

    fn root(&self, id: &str) -> PathBuf {
        fs::canonicalize(self.temp.path())
            .unwrap()
            .join(id)
            .join("local")
    }

    fn add(&self, id: &str) {
        let base = self.temp.path().join(id);
        for part in ["local", "remote", "recovery", "remote-recovery"] {
            fs::create_dir_all(base.join(part)).unwrap();
        }
        fs::write(base.join("local/note.txt"), b"fixture").unwrap();
        self.manager
            .add_mapping(
                AddMappingCommand::local_fixture(
                    id,
                    "local",
                    format!("Data/{id}"),
                    base.join("local"),
                    base.join("remote"),
                    base.join("recovery"),
                    base.join("remote-recovery"),
                )
                .unwrap(),
            )
            .unwrap();
    }

    fn initialize(&self, id: &str) {
        self.launcher.push_script(FakeLaunchScript::default());
        self.manager
            .initialize(InitializeCommand {
                mapping_id: MappingId::new(id),
                expected_direction: None,
            })
            .unwrap();
        self.idle();
    }

    fn tick(&self, seconds: i64, changed: &[&Path]) -> BTreeSet<PathBuf> {
        self.clock.set(self.epoch + TimeDelta::seconds(seconds));
        self.changes
            .send(changed.iter().map(|path| path.to_path_buf()).collect())
            .unwrap();
        self.roots
            .recv_timeout(DEADLINE)
            .expect("scheduler processed the fake watcher turn")
    }

    fn idle(&self) -> Arc<StatusSnapshot> {
        self.manager.wait_until_idle(DEADLINE).unwrap()
    }

    fn hold_next_run(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (running, running_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        self.launcher.push_script(FakeLaunchScript {
            on_wait: Some(Arc::new(move || {
                running.send(()).unwrap();
                let _ = release_rx.lock().unwrap().recv_timeout(DEADLINE);
            })),
            ..Default::default()
        });
        (running_rx, release)
    }
}

#[test]
fn hints_coalesce_and_one_dirty_follow_up_respects_cooldown() {
    let fx = Fixture::new();
    fx.add("notes");
    let root = fx.root("notes");
    assert!(!fx.tick(0, &[&root]).contains(&root));
    fx.initialize("notes");
    assert!(fx.tick(1, &[]).contains(&root));

    for (second, hints) in [
        (12, &[][..]),
        (24, &[][..]),
        (25, &[root.as_path()][..]),
        (29, &[root.as_path()][..]),
        (30, &[][..]),
    ] {
        fx.tick(second, hints);
        assert_eq!(fx.launcher.launched_plans().len(), 1);
    }
    let (running, release) = fx.hold_next_run();
    fx.tick(31, &[]);
    running.recv_timeout(DEADLINE).unwrap();
    fx.tick(32, &[&root]);
    fx.tick(33, &[&root]);
    assert!(fx.manager.snapshot().queue_order.is_empty());
    release.send(()).unwrap();
    fx.idle();

    for second in [45, 57, 62] {
        fx.tick(second, &[]);
        assert_eq!(fx.launcher.launched_plans().len(), 2);
    }
    fx.launcher.push_script(FakeLaunchScript::default());
    fx.tick(63, &[]);
    assert_eq!(
        fx.idle().mappings[0].last_run_outcome,
        Some(RunOutcome::Success)
    );
    fx.tick(64, &[]);
    assert_eq!(fx.launcher.launched_plans().len(), 3);
}

#[test]
fn stopping_a_claimed_poll_before_spawn_restarts_the_interval() {
    assert_claimed_poll_restarts_interval(false);
}

#[test]
fn force_stopping_a_claimed_poll_before_spawn_restarts_the_interval() {
    assert_claimed_poll_restarts_interval(true);
}

fn assert_claimed_poll_restarts_interval(force_stop: bool) {
    let fx = Fixture::new();
    fx.add("notes");
    fx.initialize("notes");

    let (claimed, claimed_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    fx.launcher.push_script(FakeLaunchScript {
        on_prepare: Some(Arc::new(move |_| {
            claimed.send(()).unwrap();
            let _ = release_rx.lock().unwrap().recv_timeout(DEADLINE);
        })),
        ..Default::default()
    });
    fx.tick(300, &[]);
    claimed_rx.recv_timeout(DEADLINE).unwrap();
    fx.clock.set(fx.epoch + TimeDelta::seconds(301));
    let stopped = if force_stop {
        fx.manager.force_stop("notes")
    } else {
        fx.manager.stop_run("notes")
    };
    release.send(()).unwrap();
    stopped.unwrap();
    let snapshot = fx.idle();
    assert_eq!(
        snapshot.mappings[0].last_run_outcome,
        Some(RunOutcome::Interrupted)
    );
    assert_eq!(fx.launcher.launched_plans().len(), 1);
    if force_stop {
        assert_eq!(snapshot.mappings[0].readiness, Readiness::RecoveryRequired);
        fx.manager.complete_recovery("notes", false).unwrap();
        fx.manager.resume("notes").unwrap();
    }

    fx.launcher.push_script(FakeLaunchScript::default());
    fx.tick(302, &[]);
    assert_eq!(
        fx.idle().mappings[0].last_run_outcome,
        Some(RunOutcome::Interrupted)
    );
    assert_eq!(fx.launcher.launched_plans().len(), 1);

    fx.tick(600, &[]);
    assert_eq!(
        fx.idle().mappings[0].last_run_outcome,
        Some(RunOutcome::Interrupted)
    );
    fx.tick(601, &[]);
    assert_eq!(
        fx.idle().mappings[0].last_run_outcome,
        Some(RunOutcome::Success)
    );
    assert_eq!(fx.launcher.launched_plans().len(), 2);
}

#[test]
fn sync_now_promotes_one_entry_and_global_pause_leaves_manual_jobs_alone() {
    let fx = Fixture::new();
    for id in ["alpha", "bravo", "charlie", "delta"] {
        fx.add(id);
        fx.initialize(id);
    }
    let (running, release) = fx.hold_next_run();
    fx.manager.run_now("alpha").unwrap();
    running.recv_timeout(DEADLINE).unwrap();
    fx.tick(300, &[]);
    fx.tick(301, &[]);
    let ids = |ids: &[&str]| ids.iter().map(|id| MappingId::new(*id)).collect::<Vec<_>>();
    assert_eq!(
        fx.manager.snapshot().queue_order,
        ids(&["bravo", "charlie", "delta"])
    );
    let promoted = fx.manager.run_now("delta").unwrap();
    assert_eq!(
        promoted.snapshot.queue_order,
        ids(&["delta", "bravo", "charlie"])
    );
    fx.manager.set_scheduling_paused(true).unwrap();
    fx.manager.pause("bravo").unwrap();
    fx.launcher.push_script(FakeLaunchScript::default());
    release.send(()).unwrap();
    fx.idle();
    assert_eq!(fx.launcher.launched_plans().len(), 6);
    assert_eq!(fx.launcher.launched_plans()[5].path1, fx.root("delta"));
    fx.tick(310, &[&fx.root("bravo"), &fx.root("charlie")]);
    fx.manager.set_scheduling_paused(false).unwrap();
    fx.launcher.push_script(FakeLaunchScript::default());
    fx.tick(311, &[]);
    let snapshot = fx.idle();
    assert_eq!(fx.launcher.launched_plans()[6].path1, fx.root("charlie"));
    assert_eq!(snapshot.mappings[1].intent, sync_core::Intent::Paused);
}

#[test]
fn retry_backoff_is_bounded_and_wake_revalidates_roots() {
    let fx = Fixture::new();
    fx.add("notes");
    fx.initialize("notes");
    fx.launcher.push_script(FakeLaunchScript {
        exit_status: ExitStatusInfo {
            code: Some(5),
            signal: None,
        },
        ..Default::default()
    });
    fx.manager.run_now("notes").unwrap();
    assert_eq!(
        fx.idle().mappings[0].last_run_outcome,
        Some(RunOutcome::RetryableFailure)
    );
    fx.tick(1, &[&fx.root("notes")]);
    fx.manager.notify_environment_changed();
    fx.tick(29, &[]);
    assert_eq!(fx.launcher.launched_plans().len(), 2);
    fx.launcher.push_script(FakeLaunchScript::default());
    fx.tick(37, &[]);
    assert_eq!(
        fx.idle().mappings[0].last_run_outcome,
        Some(RunOutcome::Success)
    );

    // Establish the cooldown without reaching the normal remote polling deadline.
    fx.tick(50, &[]);
    fx.tick(63, &[]);
    fx.tick(67, &[]);
    fs::remove_file(fx.root("notes").join(sync_core::ACCESS_MARKER_FILENAME)).unwrap();
    fx.manager.notify_environment_changed();
    fx.tick(69, &[]);
    let snapshot = fx.idle();
    assert_eq!(snapshot.mappings[0].readiness, Readiness::EndpointChanged);
    assert_eq!(snapshot.mappings[0].execution, Execution::Idle);
    fx.tick(400, &[]);
    assert_eq!(fx.launcher.launched_plans().len(), 3);
    assert_eq!(fx.launcher.launched_plans()[2].mode, RunMode::Normal);
}
