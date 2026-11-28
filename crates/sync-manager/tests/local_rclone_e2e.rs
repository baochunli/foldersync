use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime},
};

use rclone_adapter::{
    InitPreference, ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, RunHandle,
    SpawnOptions, pinned_binary_path,
};
use sha2::{Digest, Sha256};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccessMarker, Execution, MappingId, Readiness, RunOutcome,
};
use sync_manager::{
    AddMappingCommand, BisyncLauncher, CommandOutcome, FakeProcessInspector, InitializeCommand,
    LaunchPlan, LiveProcessInfo, ManagerConfig, ManagerError, ManagerEvent, ProcessProbeResult,
    RealBisyncLauncher, RunRecord, SqliteStateStore, SyncManager,
};
use tempfile::TempDir;

struct SlowFirstSync;

impl ProcessRunner for SlowFirstSync {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        let mut spec = spec.clone();
        if spec.args.first().is_some_and(|arg| arg == "bisync") {
            spec.args.extend([
                "--disable=Copy".into(),
                "--transfers=1".into(),
                "--bwlimit=512Ki".into(),
                "--stats=200ms".into(),
            ]);
        }
        RealProcessRunner.spawn(&spec, options)
    }
}

struct RealFixture {
    temp: TempDir,
    state_dir: PathBuf,
    local_dir: PathBuf,
    remote_dir: PathBuf,
    local_recovery: PathBuf,
    remote_recovery: PathBuf,
}

impl RealFixture {
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
            temp: tmp,
            state_dir,
            local_dir,
            remote_dir,
            local_recovery,
            remote_recovery,
        }
    }

    fn open_manager(&self) -> SyncManager {
        SyncManager::open(ManagerConfig::with_state_dir(&self.state_dir)).unwrap()
    }

    fn add_mapping_cmd(&self) -> AddMappingCommand {
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

fn require_pinned_rclone(test_name: &str) -> bool {
    if pinned_binary_path().is_none() {
        eprintln!(
            "Skipping {test_name}: pinned rclone binary is absent; run scripts/fetch-rclone.sh"
        );
        return false;
    }
    true
}

fn write_rel(root: &Path, rel: &str, bytes: &[u8]) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, bytes).unwrap();
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn tree_hashes(root: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if rel == ACCESS_MARKER_FILENAME {
                    continue;
                }
                out.insert(rel, sha256_bytes(&fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn stop_real_first_sync(
    fx: &RealFixture,
    direction: InitPreference,
    keep_partial_fixture: bool,
) -> RunRecord {
    let (source, destination) = match direction {
        InitPreference::Path1 => (&fx.local_dir, &fx.remote_dir),
        InitPreference::Path2 => (&fx.remote_dir, &fx.local_dir),
    };
    for i in 0..8 {
        write_rel(source, &format!("file-{i}"), &vec![i; 512 * 1024]);
    }
    write_rel(source, "omit/protected.txt", b"source exclusion");
    let mut config = ManagerConfig::with_state_dir(&fx.state_dir);
    config.launcher = Arc::new(RealBisyncLauncher::with_process_runner(
        None,
        Arc::new(SlowFirstSync),
    ));
    let manager = SyncManager::open(config).unwrap();
    manager
        .register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    let mut mapping = fx.add_mapping_cmd();
    mapping.extra_exclusions.insert("/omit/".into());
    manager.add_mapping(mapping).unwrap();
    let id = MappingId::new("research");
    manager
        .initialize(InitializeCommand {
            mapping_id: id.clone(),
            expected_direction: Some(direction),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let partial_fixture = loop {
        let names: Vec<_> = fs::read_dir(destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        if names.iter().any(|name| {
            name.to_str()
                .is_some_and(|name| name.len() == 6 && name.starts_with("file-"))
        }) {
            if !keep_partial_fixture {
                break None;
            }
            if let Some((name, bytes)) = names
                .iter()
                .filter_map(|name| {
                    let name = name.to_str()?;
                    if !name.ends_with(".partial") {
                        return None;
                    }
                    let bytes = fs::read(destination.join(name)).ok()?;
                    (!bytes.is_empty() && bytes.len() < 256 * 1024)
                        .then(|| (name.to_owned(), bytes))
                })
                .next()
            {
                break Some((name, bytes));
            }
        }
        assert!(
            Instant::now() < deadline,
            "no completed file or requested staging partial observed"
        );
        thread::sleep(Duration::from_millis(15));
    };
    manager.stop_run(id.clone()).unwrap();
    manager.wait_until_idle(Duration::from_secs(90)).unwrap();
    let run = manager.list_runs(&id).unwrap().remove(0);
    assert_eq!(run.outcome_kind.as_deref(), Some("first_sync_stopped"));
    manager.shutdown().unwrap();
    if let Some((name, bytes)) = partial_fixture {
        let final_name = name.split('.').next().unwrap();
        assert!(
            !destination.join(final_name).exists(),
            "the staged file completed before Stop"
        );
        // A graceful engine Stop removes its scratch file. Restore the observed
        // staging bytes to exercise the residue an abrupt exit would leave.
        write_rel(destination, &name, &bytes);
    }
    run
}

fn file_manifest(root: &Path) -> BTreeMap<String, (String, SystemTime)> {
    tree_hashes(root)
        .into_iter()
        .map(|(path, hash)| {
            let mtime = fs::metadata(root.join(&path)).unwrap().modified().unwrap();
            (path, (hash, mtime))
        })
        .collect()
}

fn assert_resume_refused(manager: &SyncManager, id: &MappingId, path: &str) {
    let stale = manager.list_runs(id).unwrap().len();
    manager.resume_first_sync(id.clone()).unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(90)).unwrap();
    let runs = manager.list_runs(id).unwrap();
    assert_eq!(runs.len(), stale + 1);
    assert_eq!(
        runs[0].child_pid, None,
        "unsafe subset launched: {:?}",
        runs[0]
    );
    assert!(
        runs[0]
            .outcome_detail
            .as_deref()
            .is_some_and(|detail| detail.contains(path)),
        "{:?}",
        runs[0]
    );
    assert!(!snapshot.mappings[0].scheduling_enabled);
}

fn verify_stopped_and_resume(direction: InitPreference) {
    let fx = RealFixture::new();
    let first = stop_real_first_sync(&fx, direction, false);
    let (source, destination) = match direction {
        InitPreference::Path1 => (&fx.local_dir, &fx.remote_dir),
        InitPreference::Path2 => (&fx.remote_dir, &fx.local_dir),
    };
    let original = file_manifest(source);
    let completed_before_resume = tree_hashes(destination);
    let copied = completed_before_resume.len();
    assert!((1..8).contains(&copied), "{completed_before_resume:?}");

    let manager = fx.open_manager();
    let snapshot = manager.snapshot();
    assert_eq!(
        snapshot.mappings[0].status_text,
        format!("First sync stopped · {copied} of 8 files copied")
    );
    assert!(!snapshot.mappings[0].scheduling_enabled);
    assert_eq!(snapshot.mappings[0].counters.open_recovery_cases, 0);
    let id = MappingId::new("research");

    // A suffix alone doesn't make a destination-only file an engine partial.
    write_rel(destination, "extra.partial", b"keep this");
    assert_resume_refused(&manager, &id, "extra.partial");
    assert_eq!(
        fs::read(destination.join("extra.partial")).unwrap(),
        b"keep this"
    );
    fs::remove_file(destination.join("extra.partial")).unwrap();

    let sample = completed_before_resume.keys().next().unwrap();
    let path = destination.join(sample);
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, b"unexpected replacement").unwrap();
    assert_resume_refused(&manager, &id, sample);
    assert_eq!(fs::read(&path).unwrap(), b"unexpected replacement");
    fs::write(&path, &bytes).unwrap();
    let mtime = original[sample].1;
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime + Duration::from_secs(2)))
        .unwrap();
    assert_resume_refused(&manager, &id, sample);
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();

    write_rel(destination, "omit/protected.txt", b"destination exclusion");
    let events = manager.subscribe_events(1024);
    manager.resume_first_sync(id.clone()).unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(90)).unwrap();
    assert_eq!(
        snapshot.mappings[0].readiness,
        Readiness::Ready,
        "{:?}",
        snapshot.mappings[0]
    );
    assert!(snapshot.mappings[0].scheduling_enabled);
    assert_eq!(
        snapshot.mappings[0].active_generation_id.as_ref(),
        Some(&first.generation_id)
    );
    assert_eq!(file_manifest(source), original);
    let expected: BTreeMap<_, _> = original
        .iter()
        .filter(|(path, _)| !path.starts_with("omit/"))
        .map(|(path, value)| (path.clone(), value.clone()))
        .chain([(
            "omit/protected.txt".to_owned(),
            (
                sha256_bytes(b"destination exclusion"),
                fs::metadata(destination.join("omit/protected.txt"))
                    .unwrap()
                    .modified()
                    .unwrap(),
            ),
        )])
        .collect();
    assert_eq!(file_manifest(destination), expected);
    assert!(tree_hashes(&fx.local_recovery).is_empty());
    assert!(tree_hashes(&fx.remote_recovery).is_empty());
    let last = manager.list_runs(&id).unwrap().remove(0);
    assert_eq!(last.mode, first.mode);
    assert_eq!(last.backup_dir1, first.backup_dir1);
    assert_eq!(last.backup_dir2, first.backup_dir2);
    assert_eq!(last.workdir_path, first.workdir_path);
    assert_eq!(last.files_already_present, copied as u64);
    assert_eq!(last.first_sync_total_files, Some(8));
    assert!(
        events
            .try_iter()
            .any(|event| matches!(event, ManagerEvent::RunProgress {
        files_already_present, first_sync_total_files: Some(8), ..
    } if files_already_present == copied as u64))
    );
    manager.shutdown().unwrap();
}

#[test]
fn stopped_first_sync_is_neutral_and_resumes_upload_after_restart() {
    if require_pinned_rclone("resume interrupted upload") {
        verify_stopped_and_resume(InitPreference::Path1);
    }
}

#[test]
fn stopped_first_sync_is_neutral_and_resumes_download_after_restart() {
    if require_pinned_rclone("resume interrupted download") {
        verify_stopped_and_resume(InitPreference::Path2);
    }
}

struct DestinationChangedDuringPrepare {
    destination: PathBuf,
    launcher: RealBisyncLauncher,
}

impl BisyncLauncher for DestinationChangedDuringPrepare {
    fn engine_metadata(&self) -> Result<sync_manager::launcher::EngineMetadata, ManagerError> {
        self.launcher.engine_metadata()
    }

    fn prepare(
        &self,
        plan: &LaunchPlan,
    ) -> Result<sync_manager::launcher::PreparedLaunch, ManagerError> {
        write_rel(
            &self.destination,
            "added-before-launch",
            b"destination author data",
        );
        self.launcher.prepare(plan)
    }
}

#[test]
fn resume_pre_spawn_recheck_refuses_a_new_destination_file_and_changed_filters_require_review() {
    if !require_pinned_rclone("resume second subset check and filter identity") {
        return;
    }
    let fx = RealFixture::new();
    let first = stop_real_first_sync(&fx, InitPreference::Path1, false);
    let id = MappingId::new("research");
    let mut config = ManagerConfig::with_state_dir(&fx.state_dir);
    config.launcher = Arc::new(DestinationChangedDuringPrepare {
        destination: fx.remote_dir.clone(),
        launcher: RealBisyncLauncher::default(),
    });
    let manager = SyncManager::open(config).unwrap();
    assert_resume_refused(&manager, &id, "added-before-launch");
    assert_eq!(
        fs::read(fx.remote_dir.join("added-before-launch")).unwrap(),
        b"destination author data"
    );
    assert!(!fx.local_dir.join("added-before-launch").exists());
    manager.shutdown().unwrap();

    let store = SqliteStateStore::open(&fx.state_dir.join("state.sqlite")).unwrap();
    let mut mapping = store.get_mapping(&id).unwrap().unwrap();
    mapping.exclusions.push("*.changed".into());
    store.update_mapping_runtime(&mapping).unwrap();
    drop(store);
    let manager = fx.open_manager();
    let snapshot = manager.snapshot();
    assert!(snapshot.mappings[0].stopped_first_sync.is_none());
    assert_eq!(
        snapshot.mappings[0].readiness,
        Readiness::NeedsInitialization
    );
    let error = manager.resume_first_sync(id.clone()).unwrap_err();
    assert!(error.cause().contains("exclusions have changed"), "{error}");
    assert_eq!(
        manager.list_runs(&id).unwrap()[0].generation_id,
        first.generation_id
    );
    manager.shutdown().unwrap();
}

#[test]
fn orphaned_first_sync_resumes_only_after_hold_resolution_and_archives_stale_lock() {
    if !require_pinned_rclone("resume a reconciled first sync") {
        return;
    }
    let fx = RealFixture::new();
    let first = stop_real_first_sync(&fx, InitPreference::Path1, true);
    let id = MappingId::new("research");
    let (partial, partial_hash) = tree_hashes(&fx.remote_dir)
        .into_iter()
        .find(|(name, _)| name.ends_with(".partial"))
        .unwrap();
    let pid = first.child_pid.unwrap();
    let lock = fs::read_dir(&first.workdir_path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "lck"))
        .unwrap_or_else(|| {
            let session = first.workdir_path.join("fixture");
            let lock = session.with_extension("lck");
            fs::write(
                &lock,
                serde_json::json!({ "PID": pid.to_string(), "Session": session }).to_string(),
            )
            .unwrap();
            lock
        });
    let db = rusqlite::Connection::open(fx.state_dir.join("state.sqlite")).unwrap();
    db.execute(
        "UPDATE runs SET status = 'running', finished_at = NULL WHERE id = ?1",
        [first.id.as_str()],
    )
    .unwrap();
    drop(db);

    let inspector = Arc::new(FakeProcessInspector::new());
    inspector.set_process(
        pid,
        ProcessProbeResult::Live(LiveProcessInfo {
            pid,
            start_time: first.child_start_time,
            executable: first.child_executable.clone(),
        }),
    );
    let mut config = ManagerConfig::with_state_dir(&fx.state_dir);
    config.process_inspector = inspector.clone();
    let manager = SyncManager::open(config).unwrap();
    assert!(manager.resume_first_sync(id.clone()).is_err());
    assert!(lock.is_file());
    inspector.set_process(pid, ProcessProbeResult::NotFound);
    assert!(manager.resume_first_sync(id.clone()).is_err());
    manager.complete_recovery(id.clone(), false).unwrap();
    manager.resume_first_sync(id.clone()).unwrap();
    let snapshot = manager.wait_until_idle(Duration::from_secs(90)).unwrap();
    assert_eq!(
        snapshot.mappings[0].readiness,
        Readiness::Ready,
        "{:?}",
        snapshot.mappings[0]
    );
    assert_eq!(snapshot.mappings[0].counters.open_recovery_cases, 0);
    let resumed = manager.list_runs(&id).unwrap().remove(0);
    assert!(resumed.run_dir.join("interrupted-bisync.lck").is_file());
    assert!(!lock.exists());
    assert!(!fx.remote_dir.join(&partial).exists());
    assert!(!fx.local_dir.join(&partial).exists());
    let archived = Path::new(&resumed.backup_dir2)
        .join(format!("partials-{}", resumed.id))
        .join(partial);
    assert_eq!(sha256_bytes(&fs::read(archived).unwrap()), partial_hash);
    manager.shutdown().unwrap();
}

#[test]
#[ignore = "Documents the pinned rclone concurrent-writer window on disposable local fixtures"]
fn rclone_can_replace_a_file_created_during_first_sync_publish() {
    let Some(binary) = pinned_binary_path() else {
        return;
    };
    rclone_adapter::qualify_pinned_binary().unwrap().unwrap();
    let fx = RealFixture::new();
    let workdir = fx.temp.path().join("work");
    let config = fx.temp.path().join("rclone.conf");
    fs::create_dir(&workdir).unwrap();
    fs::write(&config, b"").unwrap();
    write_rel(&fx.local_dir, "pending.bin", &vec![b'S'; 4 * 1024 * 1024]);
    let mut child = Command::new(binary)
        .arg("bisync")
        .arg(&fx.local_dir)
        .arg(&fx.remote_dir)
        .arg("--config")
        .arg(config)
        .arg("--workdir")
        .arg(&workdir)
        .arg("--backup-dir1")
        .arg(&fx.local_recovery)
        .arg("--backup-dir2")
        .arg(&fx.remote_recovery)
        .args([
            "--resync-mode",
            "path1",
            "--disable=Copy",
            "--transfers=1",
            "--bwlimit=2Mi",
            "--immutable",
            "--no-update-modtime",
            "--check-sync=true",
        ])
        .env_clear()
        .env("HOME", fx.temp.path())
        .env("FOLDERSYNC_STATE_DIR", &fx.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut observed_partial = false;
    while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        if fs::read_dir(&fx.remote_dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial")
        }) {
            observed_partial = true;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let live = fx.remote_dir.join("pending.bin");
    let injected = if observed_partial {
        use std::io::Write;
        fs::File::create_new(&live).and_then(|mut file| file.write_all(b"independent data"))
    } else {
        child.kill().ok();
        Err(std::io::Error::other("no engine partial observed"))
    };
    let status = child.wait().unwrap();
    injected.unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read(&live).unwrap(),
        fs::read(fx.local_dir.join("pending.bin")).unwrap()
    );
    assert!(tree_hashes(&fx.local_recovery).is_empty());
    assert!(tree_hashes(&fx.remote_recovery).is_empty());
}

#[test]
fn upload_initialization_propagates_files_and_persists_generation_and_run_artifacts() {
    if !require_pinned_rclone(
        "upload_initialization_propagates_files_and_persists_generation_and_run_artifacts",
    ) {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "notes.txt", b"upload-init-notes-v1");
    write_rel(&fx.local_dir, "sub/report.md", b"upload-init-report-v1");

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let ack = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
    assert_eq!(
        ack.outcome,
        CommandOutcome::InitializationQueued {
            mapping_id: MappingId::new("research"),
            direction: InitPreference::Path1,
        }
    );

    let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(snap.mappings[0].execution, Execution::Idle);
    assert!(snap.mappings[0].scheduling_enabled);
    assert!(snap.mappings[0].active_generation_fingerprint.is_some());

    // Verify files propagated with exact hashes and recognizable RCLONE_TEST marker created on both sides.
    assert_eq!(tree_hashes(&fx.local_dir), tree_hashes(&fx.remote_dir));
    assert_eq!(tree_hashes(&fx.remote_dir).len(), 2);

    let local_marker = AccessMarker::parse(
        &fs::read_to_string(fx.local_dir.join(ACCESS_MARKER_FILENAME)).unwrap(),
    )
    .unwrap();
    let remote_marker = AccessMarker::parse(
        &fs::read_to_string(fx.remote_dir.join(ACCESS_MARKER_FILENAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(local_marker, remote_marker);

    // Run artifacts exist under runs/<run-id>/.
    let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
    assert_eq!(runs.len(), 1);
    let expected = rclone_adapter::qualify_pinned_binary().unwrap().unwrap();
    assert_eq!(runs[0].engine_version.as_deref(), Some(expected.version()));
    assert_eq!(runs[0].engine_sha256.as_deref(), Some(expected.sha256()));
    assert_eq!(
        runs[0].engine_platform.as_deref(),
        Some(expected.platform())
    );
    let argv: serde_json::Value =
        serde_json::from_slice(&fs::read(runs[0].run_dir.join("argv.json")).unwrap()).unwrap();
    assert_eq!(
        argv["engine"]["executable"],
        expected.path().to_string_lossy().as_ref()
    );
    assert!(runs[0].run_dir.join("output.ndjson").is_file());
    assert!(runs[0].run_dir.join("outcome.json").is_file());

    mgr.shutdown().unwrap();
}

#[test]
fn changing_filters_initializes_with_a_fresh_workdir_and_keeps_excluded_files() {
    if !require_pinned_rclone(
        "changing_filters_initializes_with_a_fresh_workdir_and_keeps_excluded_files",
    ) {
        return;
    }
    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "notes.txt", b"included");
    write_rel(&fx.local_dir, "archive/old.txt", b"preserved");
    let manager = fx.open_manager();
    manager
        .register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    manager.add_mapping(fx.add_mapping_cmd()).unwrap();
    let id = MappingId::new("research");
    manager
        .initialize(InitializeCommand {
            mapping_id: id.clone(),
            expected_direction: None,
        })
        .unwrap();
    let original = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let generation = manager
        .get_generation(original.mappings[0].active_generation_id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let original_listing_hashes = tree_hashes(&generation.workdir_path);
    assert!(!original_listing_hashes.is_empty());

    let selection = std::collections::BTreeSet::from(["/archive/".to_owned()]);
    assert!(
        manager
            .propose_filter_change(&id, selection.clone())
            .unwrap()
            .preflight()
            .direction
            .is_err()
    );
    let preserved = fx.temp.path().join("preserved-remote");
    fs::create_dir(&preserved).unwrap();
    for path in ["notes.txt", "archive"] {
        fs::rename(fx.remote_dir.join(path), preserved.join(path)).unwrap();
    }

    let preview = manager.propose_filter_change(&id, selection).unwrap();
    assert_eq!(preview.preflight().direction, Ok(InitPreference::Path1));
    manager.apply_filter_change(preview).unwrap();
    let migrated = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let active = migrated.mappings[0].active_generation_id.as_ref().unwrap();
    assert_ne!(active, &generation.id);
    assert!(migrated.mappings[0].scheduling_enabled);
    assert_eq!(
        tree_hashes(&generation.workdir_path),
        original_listing_hashes
    );
    assert_eq!(
        fs::read(fx.local_dir.join("archive/old.txt")).unwrap(),
        b"preserved"
    );
    assert_eq!(
        fs::read(preserved.join("archive/old.txt")).unwrap(),
        b"preserved"
    );
    assert_eq!(
        fs::read(fx.remote_dir.join("notes.txt")).unwrap(),
        b"included"
    );
    assert!(!fx.remote_dir.join("archive/old.txt").exists());
    let runs = manager.list_runs(&id).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter()
            .any(|run| &run.generation_id == active && run.mode == "init_path1")
    );
}

#[test]
fn removing_custom_exclusion_uploads_local_only_directory_without_deleting_files() {
    if !require_pinned_rclone(
        "removing_custom_exclusion_uploads_local_only_directory_without_deleting_files",
    ) {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "proj/.git/HEAD", b"ref: refs/heads/main\n");
    write_rel(
        &fx.local_dir,
        "proj/.git/objects/data",
        b"local-only object",
    );
    let expected_files = tree_hashes(&fx.local_dir);
    let manager = fx.open_manager();
    manager
        .register_local_fixture_account("local-1", "Local fixture")
        .unwrap();
    let mut cmd = fx.add_mapping_cmd();
    cmd.extra_exclusions.insert(".git/".to_owned());
    manager.add_mapping(cmd).unwrap();
    let id = MappingId::new("research");
    manager
        .initialize(InitializeCommand {
            mapping_id: id.clone(),
            expected_direction: Some(InitPreference::Path1),
        })
        .unwrap();
    let original = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let original = &original.mappings[0];
    assert_eq!(original.readiness, Readiness::Ready);
    assert_eq!(tree_hashes(&fx.local_dir), expected_files);
    assert!(tree_hashes(&fx.remote_dir).is_empty());
    let marker_path = fx.remote_dir.join(ACCESS_MARKER_FILENAME);
    let original_marker = fs::read(&marker_path).unwrap();

    let preview = manager
        .propose_filter_change(&id, Default::default())
        .unwrap();
    assert_eq!(preview.removed_exclusions(), [".git/"]);
    assert!(preview.added_exclusions().is_empty());
    assert_eq!(preview.preflight().direction, Ok(InitPreference::Path1));
    assert!(tree_hashes(&fx.remote_dir).is_empty());
    manager.apply_filter_change(preview).unwrap();

    let migrated = manager.wait_until_idle(Duration::from_secs(60)).unwrap();
    let migrated = &migrated.mappings[0];
    assert_eq!(migrated.readiness, Readiness::Ready);
    assert_eq!(migrated.last_run_outcome, Some(RunOutcome::Success));
    assert_ne!(migrated.active_generation_id, original.active_generation_id);
    assert_eq!(tree_hashes(&fx.local_dir), expected_files);
    assert_eq!(tree_hashes(&fx.remote_dir), expected_files);
    assert_eq!(fs::read(marker_path).unwrap(), original_marker);
    assert!(tree_hashes(&fx.local_recovery).is_empty());
    assert!(tree_hashes(&fx.remote_recovery).is_empty());
    manager.shutdown().unwrap();
}

#[test]
fn download_initialization_propagates_remote_files_to_empty_local() {
    if !require_pinned_rclone("download_initialization_propagates_remote_files_to_empty_local") {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.remote_dir, "cloud.txt", b"download-init-cloud-v1");
    write_rel(&fx.remote_dir, "deep/data.bin", b"download-init-binary-v1");

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let ack = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
    assert_eq!(
        ack.outcome,
        CommandOutcome::InitializationQueued {
            mapping_id: MappingId::new("research"),
            direction: InitPreference::Path2,
        }
    );

    let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(tree_hashes(&fx.local_dir), tree_hashes(&fx.remote_dir));
    assert_eq!(tree_hashes(&fx.local_dir).len(), 2);

    mgr.shutdown().unwrap();
}

#[test]
fn refuses_both_populated_trees_with_section9_message() {
    if !require_pinned_rclone("refuses_both_populated_trees_with_section9_message") {
        return;
    }

    let fx = RealFixture::new();
    write_rel(&fx.local_dir, "local_only.txt", b"local-data");
    write_rel(&fx.remote_dir, "remote_only.txt", b"remote-data");

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let err = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, ManagerError::BothSidesPopulated { .. }),
        "expected BothSidesPopulated, got {err:?}"
    );
    assert!(
        err.next_step().to_lowercase().contains("empty destination")
            || err.next_step().to_lowercase().contains("consolidat"),
        "next_step should reflect §9 guidance: {}",
        err.next_step()
    );
    assert_eq!(
        mgr.snapshot().mappings[0].readiness,
        Readiness::NeedsInitialization
    );
    // Neither tree was modified or given a marker.
    assert!(!fx.local_dir.join(ACCESS_MARKER_FILENAME).exists());
    assert!(!fx.remote_dir.join(ACCESS_MARKER_FILENAME).exists());
    mgr.shutdown().unwrap();
}

#[test]
fn refuses_unrecognized_access_marker_without_overwriting() {
    let fx = RealFixture::new();
    write_rel(
        &fx.local_dir,
        ACCESS_MARKER_FILENAME,
        b"important user file",
    );

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();

    let err = mgr
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, ManagerError::AccessMarkerConflict { .. }),
        "expected AccessMarkerConflict, got {err:?}"
    );
    assert_eq!(
        fs::read(fx.local_dir.join(ACCESS_MARKER_FILENAME)).unwrap(),
        b"important user file"
    );
    mgr.shutdown().unwrap();
}

#[test]
fn run_once_propagates_edit_and_captures_recovery_backup() {
    if !require_pinned_rclone("run_once_propagates_edit_and_captures_recovery_backup") {
        return;
    }

    let fx = RealFixture::new();
    let v1 = b"original-doc-v1";
    write_rel(&fx.local_dir, "doc.txt", v1);

    let mgr = fx.open_manager();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
    mgr.initialize(InitializeCommand {
        mapping_id: MappingId::new("research"),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(30)).unwrap();

    // Advance mtime past filesystem granularity and edit locally.
    thread::sleep(Duration::from_millis(1100));
    let v2 = b"updated-doc-v2-with-more-content";
    write_rel(&fx.local_dir, "doc.txt", v2);

    mgr.run_now("research").unwrap();
    let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
    assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
    assert_eq!(fs::read(fx.remote_dir.join("doc.txt")).unwrap(), v2);

    // Verify backup_dir2 captured the overwritten v1 file and was recorded on the run row.
    let runs = mgr.list_runs(&MappingId::new("research")).unwrap();
    let latest = &runs[0];
    let backup2_doc = PathBuf::from(&latest.backup_dir2).join("doc.txt");
    assert!(
        backup2_doc.is_file(),
        "backup_dir2 must preserve overwritten copy"
    );
    assert_eq!(fs::read(&backup2_doc).unwrap(), v1);

    mgr.shutdown().unwrap();
}

#[test]
fn deletion_guard_abort_is_recorded_as_recovery_hold_surviving_restart() {
    if !require_pinned_rclone("deletion_guard_abort_is_recorded_as_recovery_hold_surviving_restart")
    {
        return;
    }

    let fx = RealFixture::new();
    for i in 0..10 {
        write_rel(
            &fx.local_dir,
            &format!("item_{i:02}.txt"),
            format!("protected-item-{i:02}").as_bytes(),
        );
    }

    {
        let mgr = fx.open_manager();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(30)).unwrap();

        // Delete 4 of 10 files (40% > 10% deletion guard).
        for i in 0..4 {
            fs::remove_file(fx.local_dir.join(format!("item_{i:02}.txt"))).unwrap();
        }

        mgr.run_now("research").unwrap();
        let snap = mgr.wait_until_idle(Duration::from_secs(30)).unwrap();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].counters.open_recovery_cases, 1);

        // Remote still has all 10 files untouched.
        assert_eq!(tree_hashes(&fx.remote_dir).len(), 10);
        mgr.shutdown().unwrap();
    }

    // Restart and confirm the recovery hold persists.
    {
        let mgr = fx.open_manager();
        assert_eq!(
            mgr.snapshot().mappings[0].readiness,
            Readiness::RecoveryRequired
        );
        let report = mgr.inspect_recovery("research").unwrap();
        assert_eq!(report.open_cases.len(), 1);
        assert_eq!(
            report.open_cases[0].case.reason_kind,
            "deletion_guard_aborted"
        );
        mgr.shutdown().unwrap();
    }
}

#[test]
fn interrupted_run_transitions_to_recovery_required_surviving_restart() {
    if !require_pinned_rclone("interrupted_run_transitions_to_recovery_required_surviving_restart")
    {
        return;
    }

    const TIMEOUT: Duration = Duration::from_secs(120);
    let mapping_id = MappingId::new("research");
    for file_count in [800, 3200] {
        let fx = RealFixture::new();
        let mgr = fx.open_manager();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        mgr.add_mapping(fx.add_mapping_cmd()).unwrap();
        // Initialize empty pair first.
        mgr.initialize(InitializeCommand {
            mapping_id: mapping_id.clone(),
            expected_direction: None,
        })
        .unwrap();
        let initialized = mgr.wait_until_idle(TIMEOUT).unwrap();
        assert_eq!(initialized.mappings[0].readiness, Readiness::Ready);

        let mut bytes = vec![0xABu8; 32 * 1024];
        for i in 0..file_count {
            let hdr = format!("INT-BLOB-{i:04}-");
            bytes[..hdr.len()].copy_from_slice(hdr.as_bytes());
            write_rel(&fx.local_dir, &format!("blob_{i:04}.bin"), &bytes);
        }

        let snapshots = mgr.subscribe_snapshots(1);
        mgr.run_now("research").unwrap();

        // Only the running child can create final .bin files on the destination.
        // The snapshot subscription reports completion even if the batch finishes between polls.
        let started = Instant::now();
        let stop = loop {
            assert!(
                started.elapsed() < TIMEOUT,
                "timed out waiting for partial destination progress in a {file_count}-file run"
            );
            if snapshots.latest().is_some_and(|snap| {
                snap.mappings[0].last_run_id != initialized.mappings[0].last_run_id
            }) {
                break None;
            }
            let copied = fs::read_dir(&fx.remote_dir)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "bin")
                })
                .count();
            if (1..file_count).contains(&copied) {
                eprintln!(
                    "StopRun at {copied}/{file_count} copied files after {:?}",
                    started.elapsed()
                );
                break Some(mgr.stop_run("research"));
            }
            thread::sleep(Duration::from_millis(5));
        };

        let snap = mgr.wait_until_idle(TIMEOUT).unwrap();
        let mapping = &snap.mappings[0];
        assert_ne!(mapping.last_run_id, initialized.mappings[0].last_run_id);
        let stopped = match stop {
            Some(Ok(ack)) => {
                assert_eq!(
                    ack.outcome,
                    CommandOutcome::StopRequested {
                        mapping_id: mapping_id.clone()
                    }
                );
                true
            }
            Some(Err(ManagerError::InvalidState { .. }))
                if mapping.last_run_outcome == Some(RunOutcome::Success) =>
            {
                false
            }
            Some(Err(err)) => panic!("StopRun failed: {err}"),
            None => false,
        };

        // A clean exit can win the race even when StopRun was acknowledged.
        if mapping.last_run_outcome == Some(RunOutcome::Success) {
            assert_eq!(mapping.readiness, Readiness::Ready);
            assert_eq!(mapping.execution, Execution::Idle);
            eprintln!("The {file_count}-file run completed before it could be interrupted");
            mgr.shutdown().unwrap();
            continue;
        }
        assert_eq!(mapping.last_run_outcome, Some(RunOutcome::Interrupted));
        assert!(stopped, "expected partial progress and a StopRun request");
        assert_eq!(mapping.readiness, Readiness::RecoveryRequired);
        assert_eq!(mapping.execution, Execution::Interrupted);
        mgr.shutdown().unwrap();

        // Reopen and confirm the interrupted run and its recovery hold survive restart.
        let mgr = fx.open_manager();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].execution, Execution::Interrupted);
        assert_eq!(snap.mappings[0].last_run_id, mapping.last_run_id);
        assert_eq!(
            snap.mappings[0].last_run_outcome,
            Some(RunOutcome::Interrupted)
        );
        let report = mgr.inspect_recovery("research").unwrap();
        assert_eq!(report.open_cases.len(), 1);
        assert_eq!(report.open_cases[0].case.reason_kind, "interrupted");
        assert_eq!(report.open_cases[0].case.run_id, mapping.last_run_id);
        mgr.shutdown().unwrap();
        return;
    }
    panic!("both batches completed before StopRun could interrupt a transfer");
}
