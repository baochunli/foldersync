use std::{
    collections::BTreeSet,
    fs::{self, FileTimes},
    io::{Cursor, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use rclone_adapter::{
    InitPreference, ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, RunHandle,
    SpawnOptions, pinned_binary_path,
};
use sync_core::{LocalBinding, MappingId, Readiness};
use sync_manager::{
    AddMappingCommand, ArchiveState, InitializeCommand, ManagerConfig, ManagerEvent, MountedVolume,
    OsVolumeProbe, RealBisyncLauncher, SyncManager, VolumeProbe,
};
use tempfile::TempDir;

struct ScratchVolume(PathBuf);

struct SlowUpload;

impl ProcessRunner for SlowUpload {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        let mut spec = spec.clone();
        if spec.args.first().is_some_and(|arg| arg == "bisync") {
            spec.args.extend([
                "--disable=Copy".into(),
                "--transfers=1".into(),
                "--order-by=size,ascending".into(),
                "--bwlimit=512Ki".into(),
            ]);
        }
        RealProcessRunner.spawn(&spec, options)
    }
}

impl VolumeProbe for ScratchVolume {
    fn probe(&self, path: &Path) -> std::io::Result<MountedVolume> {
        let mut volume = OsVolumeProbe.probe(path)?;
        volume.mount_path = self.0.clone();
        Ok(volume)
    }

    fn find(&self, _: &LocalBinding) -> std::io::Result<Option<MountedVolume>> {
        Ok(None)
    }
}

struct Fixture {
    scratch: TempDir,
    local: PathBuf,
    cloud: PathBuf,
    id: MappingId,
}

impl Fixture {
    fn new() -> Self {
        let scratch = TempDir::new().unwrap();
        let local = fs::canonicalize(scratch.path()).unwrap().join("mac-one");
        let cloud = local.with_file_name("cloud");
        fs::create_dir(&local).unwrap();
        fs::create_dir(&cloud).unwrap();
        Self {
            scratch,
            local,
            cloud,
            id: MappingId::new("research"),
        }
    }

    fn manager(&self, state: &str) -> SyncManager {
        self.open(self.config(state))
    }

    fn config(&self, state: &str) -> ManagerConfig {
        let mut config = ManagerConfig::with_state_dir(self.scratch.path().join(state));
        config.finder_heartbeat_interval = None;
        config
    }

    fn open(&self, config: ManagerConfig) -> SyncManager {
        let volume = Arc::new(ScratchVolume(
            fs::canonicalize(self.scratch.path()).unwrap(),
        ));
        let manager = SyncManager::open_with_volume_probe(config, volume).unwrap();
        manager
            .register_local_fixture_account("fixture", "Local fixture")
            .unwrap();
        manager
    }

    fn command(&self, local: &Path, label: &str) -> AddMappingCommand {
        let mut command = AddMappingCommand::local_fixture(
            "research",
            "fixture",
            "Data/Research",
            local,
            &self.cloud,
            self.scratch.path().join(format!("{label}-recovery")),
            self.scratch.path().join(format!("{label}-cloud-recovery")),
        )
        .unwrap();
        command.local_binding.mount_path = fs::canonicalize(self.scratch.path()).unwrap();
        command.local_binding.relative_dir = local.file_name().unwrap().into();
        command
    }

    fn populate(&self) {
        let folder = self.local.join("Finished");
        fs::create_dir_all(folder.join("proj/.git/objects")).unwrap();
        fs::create_dir(folder.join("node_modules")).unwrap();
        for i in 0..99 {
            write_old(&folder.join(format!("{i}.txt")), b"finished");
        }
        write_old(&folder.join("proj/.git/objects/fixture"), b"git object");
        fs::write(folder.join("node_modules/recent"), b"excluded").unwrap();
    }
}

#[test]
fn resume_reuses_the_finished_zip_and_counts_it_as_already_present() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping archive resume: pinned rclone is absent");
        return;
    }
    let fixture = Fixture::new();
    fixture.populate();
    let ordinary = vec![42; 4 * 1024 * 1024];
    fs::write(fixture.local.join("ordinary.bin"), &ordinary).unwrap();
    let partial = "leftover.zip.foldersync-partial";
    fs::write(fixture.local.join(partial), b"local staging").unwrap();

    let mut config = fixture.config("state");
    config.launcher = Arc::new(RealBisyncLauncher::with_process_runner(
        None,
        Arc::new(SlowUpload),
    ));
    let manager = fixture.open(config);
    let mut command = fixture.command(&fixture.local, "one");
    command.extra_exclusions.insert("node_modules/**".into());
    manager
        .add_mapping_with_archives(command, selected())
        .unwrap();
    manager
        .start_initialize(InitializeCommand {
            mapping_id: fixture.id.clone(),
            expected_direction: Some(InitPreference::Path1),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    while !fixture.cloud.join("Finished.zip").exists() {
        assert!(Instant::now() < deadline, "{:?}", manager.snapshot());
        thread::sleep(Duration::from_millis(10));
    }
    manager.stop_run(fixture.id.clone()).unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(30)).unwrap();
    let stopped = snapshot.mappings[0]
        .stopped_first_sync
        .as_ref()
        .expect("first sync can resume");
    assert_eq!(stopped.total_files, Some(2));
    assert_eq!(stopped.files_copied, 1);
    assert!(!fixture.cloud.join("ordinary.bin").exists());
    let interrupted = manager
        .get_run(snapshot.mappings[0].last_run_id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let archive = &snapshot.archives[0];
    assert_eq!(archive.state, ArchiveState::Complete);
    let holding = archive.holding_path.clone().unwrap();
    let zip_path = fixture.local.join("Finished.zip");
    let zip_metadata = fs::metadata(&zip_path).unwrap();
    manager.shutdown().unwrap();

    fs::write(fixture.cloud.join(partial), b"destination staging").unwrap();
    let manager = fixture.manager("state");
    let events = manager.subscribe_events(256);
    manager.resume_first_sync(fixture.id.clone()).unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    assert_eq!(
        snapshot.mappings[0].readiness,
        Readiness::Ready,
        "{snapshot:?}"
    );
    assert_eq!(
        snapshot.mappings[0].active_generation_id.as_ref(),
        Some(&interrupted.generation_id)
    );
    assert_eq!(snapshot.archives[0].holding_path.as_ref(), Some(&holding));
    let zip_after = fs::metadata(&zip_path).unwrap();
    assert_eq!(zip_metadata.ino(), zip_after.ino());
    assert_eq!(
        zip_metadata.modified().unwrap(),
        zip_after.modified().unwrap()
    );
    assert_eq!(
        fs::read(&zip_path).unwrap(),
        fs::read(fixture.cloud.join("Finished.zip")).unwrap()
    );
    assert_eq!(
        fs::read(fixture.cloud.join("ordinary.bin")).unwrap(),
        ordinary
    );
    assert_eq!(
        fs::read(fixture.local.join(partial)).unwrap(),
        b"local staging"
    );
    assert_eq!(
        fs::read(fixture.cloud.join(partial)).unwrap(),
        b"destination staging"
    );
    assert!(events.try_iter().any(|event| matches!(event, ManagerEvent::RunProgress {
        bytes_already_present,
        expected_bytes: Some(expected_bytes),
        files_already_present: 1,
        first_sync_total_files: Some(2),
        ..
    } if bytes_already_present == zip_metadata.len() && expected_bytes == zip_metadata.len() + ordinary.len() as u64)));
    manager.shutdown().unwrap();
}

fn write_old(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    fs::File::open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000)))
        .unwrap();
}

fn selected() -> BTreeSet<String> {
    BTreeSet::from(["Finished".into()])
}

#[test]
fn review_uses_included_inventory_and_refuses_an_existing_zip() {
    let fixture = Fixture::new();
    fixture.populate();
    let manager = fixture.manager("state");
    let mut command = fixture.command(&fixture.local, "one");
    command.extra_exclusions.insert("node_modules/**".into());
    let preview = manager
        .preflight_mapping_preview_with_archives(&command, None, &selected())
        .unwrap();
    let inventory = preview.local_inventory.unwrap();
    assert_eq!(inventory.archive_suggestions[0].file_count, 100);
    assert!(
        preview.archive_errors.is_empty(),
        "{:?}",
        preview.archive_errors
    );
    let holding = preview.archive_holding_folder.unwrap();
    assert!(!holding.exists());

    fs::write(fixture.cloud.join("Finished.zip"), b"existing").unwrap();
    let preview = manager
        .preflight_mapping_preview_with_archives(&command, None, &selected())
        .unwrap();
    assert!(
        preview
            .archive_errors
            .iter()
            .any(|error| error.contains("Finished.zip"))
    );
    assert!(
        manager
            .add_mapping_with_archives(command, selected())
            .is_err()
    );
    assert_eq!(
        fs::read(fixture.cloud.join("Finished.zip")).unwrap(),
        b"existing"
    );
    assert!(fixture.local.join("Finished").is_dir());
    assert!(
        !fixture
            .local
            .join("Finished.zip.foldersync-partial")
            .exists()
    );
    assert!(!holding.exists());
}

#[test]
fn verified_zip_and_retained_originals_integrate_with_regular_two_way_sync() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping archive first sync: pinned rclone is absent");
        return;
    }
    let fixture = Fixture::new();
    fixture.populate();
    let mut command = fixture.command(&fixture.local, "one");
    command.extra_exclusions.insert("node_modules/**".into());
    let original = fixture.local.join("Finished/proj/.git/objects/fixture");
    let original_mtime = fs::metadata(&original).unwrap().modified().unwrap();
    let folder_date = UNIX_EPOCH + Duration::from_secs(1_605_000_000);
    fs::File::open(fixture.local.join("Finished"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(folder_date))
        .unwrap();

    // The reserved name remains outside both sync directions even when a
    // leftover partial is present alongside the successfully prepared ZIP.
    let stale_partial = fixture.local.join("abandoned.zip.foldersync-partial");
    fs::write(&stale_partial, b"local-only partial").unwrap();
    let ordinary = b"ordinary file";
    fs::write(fixture.local.join("ordinary.txt"), ordinary).unwrap();
    let first = fixture.manager("state-one");
    first
        .add_mapping_with_archives(command, selected())
        .unwrap();
    assert!(!fixture.local.join("Finished.zip").exists());
    let events = first.subscribe_events(256);
    first
        .start_initialize(InitializeCommand {
            mapping_id: fixture.id.clone(),
            expected_direction: Some(InitPreference::Path1),
        })
        .unwrap();
    let initialized = first.wait_until_idle(Duration::from_secs(60)).unwrap();
    assert_eq!(
        initialized.mappings[0].readiness,
        Readiness::Ready,
        "{initialized:?}"
    );
    let archive = &initialized.archives[0];
    assert_eq!(archive.state, ArchiveState::Complete);
    let holding = archive.holding_path.as_ref().unwrap();
    assert!(!holding.starts_with(&fixture.local));
    assert!(!fixture.local.join("Finished").exists());
    assert!(!fixture.cloud.join("Finished").exists());
    assert!(
        !fixture
            .cloud
            .join("Finished.zip.foldersync-partial")
            .exists()
    );
    let held_git = holding.join("proj/.git/objects/fixture");
    assert_eq!(fs::read(&held_git).unwrap(), b"git object");
    assert_eq!(
        fs::metadata(held_git).unwrap().modified().unwrap(),
        original_mtime
    );
    assert_eq!(
        fs::read(holding.join("node_modules/recent")).unwrap(),
        b"excluded"
    );
    for root in [&fixture.local, &fixture.cloud] {
        assert_eq!(
            fs::metadata(root.join("Finished.zip"))
                .unwrap()
                .modified()
                .unwrap(),
            folder_date
        );
    }
    let zip = fs::read(fixture.local.join("Finished.zip")).unwrap();
    assert_eq!(fs::read(fixture.cloud.join("Finished.zip")).unwrap(), zip);
    let expected = events.try_iter().find_map(|event| match event {
        ManagerEvent::RunProgress {
            expected_files,
            expected_bytes,
            ..
        } => expected_files.zip(expected_bytes),
        _ => None,
    });
    assert_eq!(expected, Some((2, (zip.len() + ordinary.len()) as u64)));
    assert!(
        !fixture
            .cloud
            .join("abandoned.zip.foldersync-partial")
            .exists()
    );
    assert_eq!(fs::read(&stale_partial).unwrap(), b"local-only partial");
    let mut entries = zip::ZipArchive::new(Cursor::new(&zip)).unwrap();
    assert_eq!(entries.len(), 100);
    assert!(entries.by_name("Finished/node_modules/recent").is_err());
    let mut git_bytes = Vec::new();
    entries
        .by_name("Finished/proj/.git/objects/fixture")
        .unwrap()
        .read_to_end(&mut git_bytes)
        .unwrap();
    assert_eq!(git_bytes, b"git object");

    let staging_rule = "*.zip.foldersync-partial";
    let reviewed = first
        .propose_filter_change(
            &fixture.id,
            BTreeSet::from(["node_modules/**".into(), staging_rule.into()]),
        )
        .unwrap();
    assert!(reviewed.added_exclusions().is_empty());
    assert!(reviewed.removed_exclusions().is_empty());

    let reviewed = first
        .propose_filter_change(
            &fixture.id,
            BTreeSet::from(["node_modules/**".into(), "build/**".into()]),
        )
        .unwrap();
    assert_eq!(reviewed.added_exclusions(), ["build/**"]);
    assert!(reviewed.removed_exclusions().is_empty());
    first.apply_filter_change(reviewed).unwrap();
    let narrowed = first.wait_until_idle(Duration::from_secs(60)).unwrap();
    let mapping = &narrowed.mappings[0];
    assert_eq!(mapping.readiness, Readiness::Ready, "{narrowed:?}");
    assert_ne!(
        mapping.active_generation_id, initialized.mappings[0].active_generation_id,
        "{narrowed:?}"
    );
    assert!(
        !mapping
            .exclusions
            .iter()
            .any(|pattern| pattern == staging_rule)
    );
    let generation = first
        .get_generation(mapping.active_generation_id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert!(generation.config.effective_filters.contains(staging_rule));
    assert!(
        generation
            .config
            .effective_filters
            .contains("node_modules/**")
    );
    assert!(generation.config.effective_filters.contains("build/**"));
    assert_eq!(fs::read(stale_partial).unwrap(), b"local-only partial");
    assert!(
        !fixture
            .cloud
            .join("abandoned.zip.foldersync-partial")
            .exists()
    );
    assert_eq!(fs::read(fixture.cloud.join("Finished.zip")).unwrap(), zip);
    first.shutdown().unwrap();

    let mac_two = fixture.local.with_file_name("mac-two");
    fs::create_dir(&mac_two).unwrap();
    let second = fixture.manager("state-two");
    second
        .add_mapping(fixture.command(&mac_two, "two"))
        .unwrap();
    second
        .initialize(InitializeCommand {
            mapping_id: fixture.id.clone(),
            expected_direction: Some(InitPreference::Path2),
        })
        .unwrap();
    let initialized = second.wait_until_idle(Duration::from_secs(60)).unwrap();
    assert_eq!(initialized.mappings[0].readiness, Readiness::Ready);
    assert_eq!(fs::read(mac_two.join("Finished.zip")).unwrap(), zip);
    second.shutdown().unwrap();
}
