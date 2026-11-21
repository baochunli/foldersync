use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use sync_core::{
    Execution, Intent, LocalBinding, MappingId, Readiness, RunOutcome, VolumeIdentity, VolumeKind,
};
use sync_manager::{
    AddMappingCommand, DiskSpaceInfo, FakeBisyncLauncher, FakeLaunchScript, InitializeCommand,
    ManagerConfig, MountedVolume, OsVolumeProbe, StatusSnapshot, SyncManager, VolumeProbe,
    detect_volume_identity,
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    launcher: Arc<FakeBisyncLauncher>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        for name in ["local", "remote", "recovery", "remote-recovery"] {
            fs::create_dir(temp.path().join(name)).unwrap();
        }
        fs::write(temp.path().join("local/note.txt"), "disposable fixture").unwrap();
        Self {
            temp,
            launcher: Arc::new(FakeBisyncLauncher::new()),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        fs::canonicalize(self.temp.path()).unwrap().join(name)
    }

    fn config(&self) -> ManagerConfig {
        let mut config = ManagerConfig::with_state_dir(self.path("state"));
        config.launcher = self.launcher.clone();
        config
    }

    fn initialize(&self, manager: &SyncManager, command: AddMappingCommand) {
        manager
            .register_local_fixture_account("local", "Local Fixture")
            .unwrap();
        manager.add_mapping(command).unwrap();
        self.launcher.push_script(FakeLaunchScript::default());
        manager
            .initialize(InitializeCommand {
                mapping_id: MappingId::new("notes"),
                expected_direction: None,
            })
            .unwrap();
        manager.wait_until_idle(Duration::from_secs(5)).unwrap();
        assert_eq!(self.launcher.launched_plans().len(), 1);
    }

    fn command(&self) -> AddMappingCommand {
        AddMappingCommand::local_fixture(
            "notes",
            "local",
            "Data/Notes",
            self.path("local"),
            self.path("remote"),
            self.path("recovery"),
            self.path("remote-recovery"),
        )
        .unwrap()
    }
}

#[test]
fn real_probe_reads_only_the_temporary_directory_volume() {
    let fixture = Fixture::new();
    let detected = detect_volume_identity(&fixture.path("local")).unwrap();
    assert_eq!(
        detected.mount_path.join(detected.relative_dir),
        fixture.path("local")
    );
    assert!(!detected.filesystem.eq_ignore_ascii_case("unknown"));
    assert!(!detected.display_name.is_empty());
    assert!(detected.space.free_bytes > 0);
    assert!(detected.space.total_bytes >= detected.space.free_bytes);
    #[cfg(target_os = "macos")]
    assert_eq!(detected.identity.kind, VolumeKind::Internal);
    #[cfg(target_os = "macos")]
    assert!(uuid::Uuid::parse_str(&detected.identity.stable_id).is_ok());
}

#[test]
fn a_missing_recovery_destination_refuses_a_run() {
    let fixture = Fixture::new();
    let manager = SyncManager::open(fixture.config()).unwrap();
    fixture.initialize(&manager, fixture.command());
    fs::remove_dir(fixture.path("recovery")).unwrap();

    fixture.launcher.push_script(FakeLaunchScript::default());
    let result = manager.run_now("notes");
    manager.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert!(result.is_err());
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
    assert!(!fixture.path("recovery").exists());
}

#[test]
fn verified_initialization_enables_scheduling_and_global_pause_survives_restart() {
    let fixture = Fixture::new();
    let manager = SyncManager::open(fixture.config()).unwrap();
    fixture.initialize(&manager, fixture.command());
    assert!(manager.snapshot().mappings[0].scheduling_enabled);
    manager.set_scheduling_paused(true).unwrap();
    manager.shutdown().unwrap();

    let reopened = SyncManager::open(fixture.config()).unwrap();
    assert!(reopened.snapshot().scheduling_paused);
    fixture.launcher.push_script(FakeLaunchScript::default());
    reopened.run_now("notes").unwrap();
    reopened.wait_until_idle(Duration::from_secs(15)).unwrap();
    assert_eq!(fixture.launcher.launched_plans().len(), 2);
}

#[derive(Debug, Clone, Copy)]
enum Mount {
    Connected,
    Missing,
    WrongDisk,
    Renamed,
    LowSpace,
    Inaccessible,
}

type StalledProbe = (mpsc::Sender<()>, mpsc::Receiver<()>);

struct FakeVolumeProbe {
    mount: Mutex<Mount>,
    original: PathBuf,
    renamed: PathBuf,
    stall: Mutex<Option<StalledProbe>>,
    recovery: PathBuf,
    recovery_low: AtomicBool,
}

impl FakeVolumeProbe {
    fn set(&self, mount: Mount) {
        *self.mount.lock().unwrap() = mount;
    }

    fn volume(&self, mount: Mount) -> MountedVolume {
        let (id, name, path) = match mount {
            Mount::WrongDisk => ("disk-b", "WorkSSD", &self.original),
            Mount::Renamed => ("disk-a", "RenamedSSD", &self.renamed),
            _ => ("disk-a", "WorkSSD", &self.original),
        };
        MountedVolume {
            identity: VolumeIdentity::new(VolumeKind::External, id),
            mount_path: path.clone(),
            display_name: name.to_owned(),
            filesystem: "APFS".to_owned(),
            space: DiskSpaceInfo {
                total_bytes: 4 * 1024 * 1024 * 1024,
                free_bytes: if matches!(mount, Mount::LowSpace) {
                    99 * 1024 * 1024
                } else {
                    2 * 1024 * 1024 * 1024
                },
            },
        }
    }
}

impl VolumeProbe for FakeVolumeProbe {
    fn probe(&self, path: &Path) -> io::Result<MountedVolume> {
        let mount = *self.mount.lock().unwrap();
        if path == self.recovery && self.recovery_low.load(Ordering::Relaxed) {
            return Ok(self.volume(Mount::LowSpace));
        }
        if path.starts_with(&self.original) {
            let stall = self.stall.lock().unwrap().take();
            if let Some((entered, release)) = stall {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(20)).unwrap();
            }
            match mount {
                Mount::Missing => return Err(io::ErrorKind::NotFound.into()),
                Mount::Inaccessible => return Err(io::ErrorKind::PermissionDenied.into()),
                Mount::Renamed => return OsVolumeProbe.probe(path),
                _ => return Ok(self.volume(mount)),
            }
        }
        OsVolumeProbe.probe(path)
    }

    fn find(&self, binding: &LocalBinding) -> io::Result<Option<MountedVolume>> {
        let mount = *self.mount.lock().unwrap();
        let volume = self.volume(mount);
        Ok(
            (matches!(mount, Mount::Renamed) && binding.volume == volume.identity)
                .then_some(volume),
        )
    }
}

fn open_with_fixture_disk(fixture: &Fixture) -> (SyncManager, Arc<FakeVolumeProbe>) {
    open_with_fixture_disk_config(fixture, fixture.config())
}

fn open_with_fixture_disk_config(
    fixture: &Fixture,
    config: ManagerConfig,
) -> (SyncManager, Arc<FakeVolumeProbe>) {
    let probe = Arc::new(FakeVolumeProbe {
        mount: Mutex::new(Mount::Connected),
        original: fixture.path("WorkSSD"),
        renamed: fixture.path("RenamedSSD"),
        stall: Mutex::new(None),
        recovery: fixture.path("recovery"),
        recovery_low: AtomicBool::new(false),
    });
    fs::create_dir(&probe.original).unwrap();
    fs::rename(fixture.path("local"), probe.original.join("notes")).unwrap();
    let mut command = AddMappingCommand::local_fixture(
        "notes",
        "local",
        "Data/Notes",
        probe.original.join("notes"),
        fixture.path("remote"),
        fixture.path("recovery"),
        fixture.path("remote-recovery"),
    )
    .unwrap();
    command.local_binding = LocalBinding {
        volume: probe.volume(Mount::Connected).identity,
        mount_path: probe.original.clone(),
        relative_dir: PathBuf::from("notes"),
    };
    let manager = SyncManager::open_with_volume_probe(config, probe.clone()).unwrap();
    fixture.initialize(&manager, command);
    (manager, probe)
}

fn wait_for_snapshot(
    manager: &SyncManager,
    matches: impl Fn(&StatusSnapshot) -> bool,
) -> Arc<StatusSnapshot> {
    let updates = manager.subscribe_snapshots(1);
    let deadline = Instant::now() + Duration::from_secs(8);
    let initial = manager.snapshot();
    if matches(&initial) {
        return initial;
    }
    loop {
        let snapshot = updates
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the volume watcher did not publish the expected state");
        if matches(&snapshot) {
            return snapshot;
        }
    }
}

#[test]
fn wrong_disk_at_original_mount_never_launches_even_with_matching_marker() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    probe.set(Mount::WrongDisk);

    let error = manager.run_now("notes").unwrap_err();
    assert!(error.cause().contains("different disk named WorkSSD"));
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.mappings[0].readiness, Readiness::VolumeUnavailable);
    assert_eq!(
        snapshot.mappings[0].status_text,
        "A different disk named WorkSSD is connected"
    );
    assert_eq!(snapshot.mappings[0].volume_space, None);
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
}

#[test]
fn missing_disk_is_distinct_from_an_empty_data_folder() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    fs::remove_file(probe.original.join("notes/note.txt")).unwrap();
    fixture.launcher.push_script(FakeLaunchScript::default());
    manager.run_now("notes").unwrap();
    manager.wait_until_idle(Duration::from_secs(5)).unwrap();

    probe.set(Mount::Missing);
    assert!(manager.run_now("notes").is_err());
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.mappings[0].readiness, Readiness::VolumeUnavailable);
    assert_eq!(snapshot.mappings[0].status_text, "Waiting for WorkSSD");
    assert_eq!(snapshot.mappings[0].volume_space, None);
    assert_eq!(fixture.launcher.launched_plans().len(), 2);
    assert!(probe.original.join("notes/RCLONE_TEST").is_file());
}

#[test]
fn renamed_mount_requires_review_and_preserves_the_saved_path() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    probe.set(Mount::Renamed);

    assert!(
        manager
            .run_now("notes")
            .unwrap_err()
            .cause()
            .contains("new location")
    );
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.mappings[0].readiness, Readiness::EndpointChanged);
    assert_eq!(
        snapshot.mappings[0].local_root,
        probe.original.join("notes")
    );
    assert_eq!(
        snapshot.mappings[0].volume_space,
        Some(probe.volume(Mount::Renamed).space)
    );
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
    assert!(manager.check_again("notes").is_err());
}

#[test]
fn watcher_restores_a_verified_volume_without_clearing_manual_pause() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    manager.pause("notes").unwrap();

    probe.set(Mount::Missing);
    wait_for_snapshot(&manager, |s| {
        s.mappings[0].readiness == Readiness::VolumeUnavailable
    });
    probe.set(Mount::Connected);
    let snapshot = wait_for_snapshot(&manager, |s| s.mappings[0].readiness == Readiness::Ready);
    assert_eq!(snapshot.mappings[0].intent, Intent::Paused);
    assert_eq!(
        snapshot.mappings[0].volume_space,
        Some(probe.volume(Mount::Connected).space)
    );
    assert!(manager.run_now("notes").is_err());
    assert_eq!(fixture.launcher.launched_plans().len(), 1);

    manager.shutdown().unwrap();
    let reopened = SyncManager::open_with_volume_probe(fixture.config(), probe).unwrap();
    assert_eq!(reopened.snapshot().mappings[0].intent, Intent::Paused);
}

#[test]
fn watcher_stops_active_child_once_and_preserves_recovery_even_on_exit_zero() {
    use std::sync::mpsc;

    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    let (entered_tx, entered_rx) = mpsc::channel();
    fixture.launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(rclone_adapter::ExitStatusInfo {
            code: Some(0),
            signal: None,
        }),
        on_wait: Some(Arc::new(move || entered_tx.send(()).unwrap())),
        ..Default::default()
    });
    manager.run_now("notes").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mapping_id = MappingId::new("notes");
    let before = manager
        .list_runs(&mapping_id)
        .unwrap()
        .into_iter()
        .find(|run| run.status == "running")
        .unwrap();
    let run_id = before.id.clone();
    assert!(before.workdir_path.is_dir());

    probe.set(Mount::Missing);
    let snapshot = manager.wait_until_idle(Duration::from_secs(8)).unwrap();
    assert_eq!(fixture.launcher.stop_calls(), 1);
    assert_eq!(fixture.launcher.force_stop_calls(), 0);
    assert_eq!(snapshot.mappings[0].execution, Execution::Interrupted);
    assert_eq!(snapshot.mappings[0].readiness, Readiness::RecoveryRequired);
    assert_eq!(
        snapshot.mappings[0].last_run_outcome,
        Some(RunOutcome::Interrupted)
    );
    let run = manager.get_run(&run_id).unwrap().unwrap();
    assert_eq!(run.workdir_path, before.workdir_path);
    assert!(run.workdir_path.is_dir());
    assert!(run.log_path.is_file());

    probe.set(Mount::Connected);
    let snapshot = wait_for_snapshot(&manager, |s| s.mappings[0].volume_space.is_some());
    assert_eq!(snapshot.mappings[0].readiness, Readiness::RecoveryRequired);
    assert!(manager.run_now("notes").is_err());
}

#[test]
fn volume_and_space_are_checked_again_after_launcher_preparation() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    let prepared_probe = probe.clone();
    fixture.launcher.push_script(FakeLaunchScript {
        on_prepare: Some(Arc::new(move |_| prepared_probe.set(Mount::LowSpace))),
        ..Default::default()
    });

    manager.run_now("notes").unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
    assert!(
        snapshot.mappings[0]
            .last_run_summary
            .as_ref()
            .unwrap()
            .contains("100 MiB")
    );
    assert_eq!(snapshot.mappings[0].readiness, Readiness::Ready);

    probe.set(Mount::Connected);
    fixture.launcher.push_script(FakeLaunchScript::default());
    manager.run_now("notes").unwrap();
    manager.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(fixture.launcher.launched_plans().len(), 2);
}

#[test]
fn missing_access_marker_on_reconnect_latches_endpoint_review() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    probe.set(Mount::Missing);
    assert!(manager.run_now("notes").is_err());
    fs::remove_file(probe.original.join("notes/RCLONE_TEST")).unwrap();

    probe.set(Mount::Connected);
    let snapshot = wait_for_snapshot(&manager, |s| {
        s.mappings[0].readiness == Readiness::EndpointChanged
    });
    assert!(!snapshot.mappings[0].local_root.join("RCLONE_TEST").exists());
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
}

#[test]
fn missing_private_binding_marker_refuses_a_run_without_recreating_it() {
    let fixture = Fixture::new();
    let manager = SyncManager::open(fixture.config()).unwrap();
    fixture.initialize(&manager, fixture.command());
    let marker = fixture.path("state/mappings/notes/generation-1/volume-binding");
    assert!(marker.is_file());
    fs::remove_file(&marker).unwrap();

    let error = manager.run_now("notes").unwrap_err();
    assert!(error.cause().contains("disk binding marker"));
    assert_eq!(
        manager.snapshot().mappings[0].readiness,
        Readiness::EndpointChanged
    );
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
    assert!(!marker.exists());
}

#[test]
fn stalled_volume_probe_leaves_snapshot_and_stop_responsive() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    let (running, running_rx) = mpsc::channel();
    fixture.launcher.push_script(FakeLaunchScript {
        wait_for_stop: true,
        on_wait: Some(Arc::new(move || running.send(()).unwrap())),
        ..Default::default()
    });
    manager.run_now("notes").unwrap();
    running_rx.recv_timeout(Duration::from_secs(15)).unwrap();

    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *probe.stall.lock().unwrap() = Some((entered, release_rx));
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    std::thread::scope(|scope| {
        let (done, done_rx) = mpsc::channel();
        let manager = &manager;
        scope.spawn(move || {
            let snapshot = manager.snapshot();
            let result = manager.stop_run("notes");
            let _ = done.send((snapshot, result));
        });
        let result = done_rx.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        let (snapshot, stop) = result.expect("the stalled probe held the manager mutex");
        assert_eq!(snapshot.mappings[0].execution, Execution::Running);
        stop.unwrap();
    });
    manager.wait_until_idle(Duration::from_secs(15)).unwrap();
}

#[test]
fn filter_migration_initializes_a_fresh_generation_and_preserves_the_old_one() {
    let fixture = Fixture::new();
    let manager = SyncManager::open(fixture.config()).unwrap();
    fixture.initialize(&manager, fixture.command());
    let id = MappingId::new("notes");
    let old_id = manager.snapshot().mappings[0]
        .active_generation_id
        .clone()
        .unwrap();
    let old = manager.get_generation(&old_id).unwrap().unwrap();
    let old_filters = fs::read(&old.filters_file_path).unwrap();
    fs::write(old.workdir_path.join("listing"), "preserve this baseline").unwrap();
    fs::write(fixture.path("local/archive.tmp"), "keep excluded file").unwrap();

    let preview = manager
        .propose_filter_change(&id, ["/archive.tmp".to_owned()].into())
        .unwrap();
    assert_eq!(preview.previous_generation_id(), &old_id);
    assert_eq!(preview.added_exclusions(), &["/archive.tmp"]);
    assert!(preview.preflight().direction.is_ok());

    fixture.launcher.push_script(FakeLaunchScript::default());
    manager.apply_filter_change(preview.clone()).unwrap();
    let finished = manager.wait_until_idle(Duration::from_secs(15)).unwrap();
    let new_id = finished.mappings[0].active_generation_id.as_ref().unwrap();
    assert_ne!(new_id, &old_id);
    let new = manager.get_generation(new_id).unwrap().unwrap();
    assert_eq!(new.generation_number, old.generation_number + 1);
    assert_eq!(fs::read(&old.filters_file_path).unwrap(), old_filters);
    assert_eq!(
        fs::read_to_string(old.workdir_path.join("listing")).unwrap(),
        "preserve this baseline"
    );
    assert_eq!(
        fs::read_to_string(fixture.path("local/archive.tmp")).unwrap(),
        "keep excluded file"
    );
    assert!(
        fs::read_to_string(&new.filters_file_path)
            .unwrap()
            .contains("- /archive.tmp")
    );
    let launches = fixture.launcher.launched_plans();
    assert_eq!(launches[1].workdir, new.workdir_path);
    assert!(matches!(
        launches[1].mode,
        rclone_adapter::RunMode::Initialize(_)
    ));
    assert!(manager.apply_filter_change(preview).is_err());
}

#[test]
fn non_missing_probe_errors_explain_why_the_disk_is_inaccessible() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    probe.set(Mount::Inaccessible);
    let err = manager.run_now("notes").unwrap_err();
    assert!(err.cause().contains("Can’t access"));
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.mappings[0].readiness, Readiness::VolumeUnavailable);
    assert!(snapshot.mappings[0].status_text.contains("Can’t access"));
}

#[test]
fn recovery_capacity_comes_from_the_volume_probe() {
    let fixture = Fixture::new();
    let (manager, probe) = open_with_fixture_disk(&fixture);
    probe.recovery_low.store(true, Ordering::Relaxed);
    let err = manager.run_now("notes").unwrap_err();
    assert!(err.cause().contains("100 MiB"));
    assert!(err.cause().contains("recovery destination"));
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
}

#[test]
fn space_only_updates_notify_ui_without_rewriting_finder_status() {
    let fixture = Fixture::new();
    let now: chrono::DateTime<chrono::Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    let clock = Arc::new(sync_manager::FixedClock::new(now));
    let notifier = Arc::new(sync_manager::RecordingFinderNotifier::default());
    let mut config = fixture.config();
    config.clock = clock.clone();
    config.finder_notifier = notifier.clone();
    config.finder_heartbeat_interval = None;
    let (manager, probe) = open_with_fixture_disk_config(&fixture, config);
    notifier.clear();

    let old = manager.snapshot();
    probe.set(Mount::LowSpace);
    clock.set(now + chrono::TimeDelta::seconds(31));
    let new = wait_for_snapshot(&manager, |snap| {
        snap.mappings[0]
            .volume_space
            .is_some_and(|space| space.free_bytes == 99 * 1024 * 1024)
    });
    assert_eq!(new.sequence, old.sequence + 1);
    assert_eq!(new.mappings[0].readiness, Readiness::Ready);
    assert!(notifier.posted_notifications().is_empty());
}

#[test]
fn filter_migration_rechecks_inventory_and_holds_when_both_sides_have_data() {
    let fixture = Fixture::new();
    let manager = SyncManager::open(fixture.config()).unwrap();
    fixture.initialize(&manager, fixture.command());
    let id = MappingId::new("notes");
    let old = manager.snapshot().mappings[0]
        .active_generation_id
        .clone()
        .unwrap();
    let generation = manager.get_generation(&old).unwrap().unwrap();
    let extra = BTreeSet::from(["/archive/".to_owned()]);
    let preview = manager.propose_filter_change(&id, extra.clone()).unwrap();
    assert!(preview.preflight().direction.is_ok());

    fs::write(fixture.path("remote/independent.txt"), "other data").unwrap();
    let blocked = manager.propose_filter_change(&id, extra).unwrap();
    assert!(
        blocked
            .preflight()
            .direction
            .as_ref()
            .unwrap_err()
            .contains("already contain data")
    );
    assert!(manager.apply_filter_change(blocked).is_err());
    assert_eq!(
        manager.snapshot().mappings[0].active_generation_id.as_ref(),
        Some(&old)
    );

    manager.apply_filter_change(preview).unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(15)).unwrap();
    assert!(!snapshot.mappings[0].scheduling_enabled);
    assert_eq!(
        snapshot.mappings[0].readiness,
        Readiness::NeedsInitialization
    );
    assert!(generation.workdir_path.is_dir());
    assert_eq!(fixture.launcher.launched_plans().len(), 1);
}
