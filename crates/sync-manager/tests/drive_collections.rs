use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use chrono::TimeDelta;
use rclone_adapter::{
    AccountConfigLock, ExitStatusInfo, FakeProcessRunner, FakeRunScript, OutputLine,
    PasswordCommand, ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, RunHandle,
    SpawnOptions, StreamKind, pinned_binary_path,
};
use sync_core::{AccountId, CloudCollection, MappingId, Readiness, RunOutcome};
use sync_manager::{
    AccountRecord, AccountServiceConfig, AddMappingCommand, CollectionSkipReason,
    DriveAuthorization, FilterChangePreview, FilterMigrationPhase, FixedClock, FolderSizeStatus,
    InMemorySecretStore, InitializeCommand, ManagerConfig, ManagerEvent, MappingStatusSnapshot,
    RealBisyncLauncher, SecretStore, SecretString, SqliteStateStore, SyncManager,
    scoped_credential_key,
};

struct FixtureRunner {
    fake: FakeProcessRunner,
    root: PathBuf,
    mkdir_calls: AtomicUsize,
    fail_next_mkdir: AtomicBool,
    synthesize_drive_ids: bool,
    pre_move_check_fault: Mutex<Option<CheckFault>>,
    post_move_check_fault: Mutex<Option<(usize, CheckFault)>>,
    stop_after_first_move: Mutex<Option<mpsc::Sender<()>>>,
    fake_first_bisync: AtomicBool,
}

enum CheckFault {
    Fail,
    WaitForStop(mpsc::Sender<()>),
}

impl FixtureRunner {
    fn take_check_fault(&self) -> Option<CheckFault> {
        if self.root.join("Data/Research/proj/build/x.o").exists() {
            self.pre_move_check_fault.lock().unwrap().take()
        } else {
            let mut fault = self.post_move_check_fault.lock().unwrap();
            match fault.as_mut() {
                Some((remaining, _)) if *remaining > 0 => {
                    *remaining -= 1;
                    None
                }
                _ => fault.take().map(|(_, fault)| fault),
            }
        }
    }
}

fn wait_for_test_stop(
    spec: &ProcessSpec,
    options: SpawnOptions,
    started: mpsc::Sender<()>,
) -> Result<RunHandle, ProcessError> {
    let runner = FakeProcessRunner::new();
    runner.push_run(FakeRunScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(ExitStatusInfo {
            code: None,
            signal: Some(libc::SIGINT),
        }),
        ..Default::default()
    });
    let child = runner.spawn(spec, options)?;
    started.send(()).unwrap();
    Ok(child)
}

impl ProcessRunner for FixtureRunner {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        let args = &spec.args;
        let command = args[0].to_str().unwrap();
        if command == "mkdir" {
            self.mkdir_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_next_mkdir.swap(false, Ordering::SeqCst) {
                return Err(ProcessError::Spawn {
                    program: spec.program.clone(),
                    source: std::io::Error::other("injected recovery mkdir failure"),
                });
            }
        }
        let directory_listing = args.iter().any(|arg| arg == "--dirs-only");
        if directory_listing && !self.synthesize_drive_ids
            || command == "bisync" && self.fake_first_bisync.swap(false, Ordering::Relaxed)
            || command == "config"
            || command == "size" && args.iter().any(|arg| arg == "household:Data/Slow")
        {
            return self.fake.spawn(spec, options);
        }
        let mut spec = spec.clone();
        spec.current_dir = Some(self.root.clone());
        if command == "check"
            && let Some(fault) = self.take_check_fault()
        {
            if let CheckFault::WaitForStop(started) = fault {
                return wait_for_test_stop(&spec, options, started);
            }
            let runner = FakeProcessRunner::new();
            runner.push_run(FakeRunScript {
                lines: vec![OutputLine {
                    stream: StreamKind::Stderr,
                    text: "injected check difference".to_owned(),
                    truncated: false,
                }],
                exit_status: ExitStatusInfo {
                    code: Some(1),
                    signal: None,
                },
                ..Default::default()
            });
            return runner.spawn(&spec, options);
        }
        if command == "move"
            && let Some(started) = self.stop_after_first_move.lock().unwrap().take()
        {
            let path = |arg: &std::ffi::OsStr| {
                self.root
                    .join(arg.to_str().unwrap().strip_prefix("household:").unwrap())
                    .join(EXCLUDED_BUILD_FILES[0].0)
            };
            let destination = path(&args[2]);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::rename(path(&args[1]), destination).unwrap();
            return wait_for_test_stop(&spec, options, started);
        }
        if self.synthesize_drive_ids
            && command == "lsjson"
            && let Some(subpath) = args
                .iter()
                .find_map(|arg| arg.to_str()?.strip_prefix("household:"))
        {
            let output = RealProcessRunner
                .spawn(&spec, SpawnOptions::default())?
                .wait()?;
            let stdout = output.output.stdout_text();
            let mut lines = output.output.lines;
            if output.exit_status.is_success() {
                let mut entries: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
                for entry in &mut entries {
                    let relative = Path::new(subpath).join(entry["Path"].as_str().unwrap());
                    // A local same-filesystem move preserves the inode, as a Drive move
                    // must preserve the object ID. Keep the pinned engine's actual hashes.
                    let id = match relative.to_str().unwrap() {
                        "Data" => "data-id".to_owned(),
                        "Data/Research" => "research-id".to_owned(),
                        _ => format!(
                            "fixture-{}",
                            fs::metadata(self.root.join(relative)).unwrap().ino()
                        ),
                    };
                    entry["ID"] = id.into();
                }
                lines.retain(|line| line.stream != StreamKind::Stdout);
                lines.push(OutputLine {
                    stream: StreamKind::Stdout,
                    text: serde_json::to_string(&entries).unwrap(),
                    truncated: false,
                });
            }
            let runner = FakeProcessRunner::new();
            runner.push_run(FakeRunScript {
                lines,
                exit_status: output.exit_status,
                ..Default::default()
            });
            return runner.spawn(&spec, options);
        }
        RealProcessRunner.spawn(&spec, options)
    }
}

struct Fixture {
    manager: SyncManager,
    clock: Arc<FixedClock>,
    runner: Arc<FixtureRunner>,
    id: AccountId,
    secrets: Arc<InMemorySecretStore>,
    state: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(ready: bool) -> Option<Self> {
        Self::new_with_drive_ids(ready, false)
    }

    fn new_with_drive_ids(ready: bool, synthesize_drive_ids: bool) -> Option<Self> {
        let binary = pinned_binary_path()?;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let runner = Arc::new(FixtureRunner {
            fake: FakeProcessRunner::new(),
            root: root.clone(),
            mkdir_calls: AtomicUsize::new(0),
            fail_next_mkdir: AtomicBool::new(false),
            synthesize_drive_ids,
            pre_move_check_fault: Mutex::new(None),
            post_move_check_fault: Mutex::new(None),
            stop_after_first_move: Mutex::new(None),
            fake_first_bisync: AtomicBool::new(false),
        });
        let state = root.join("state");
        let account_dir = state.join("accounts/drive");
        fs::create_dir_all(&account_dir).unwrap();
        let config_path = account_dir.join("rclone.conf");
        fs::write(&config_path, "[household]\ntype = local\n").unwrap();
        let clock = Arc::new(FixedClock::new("2026-01-01T00:00:00Z".parse().unwrap()));
        let secrets = Arc::new(InMemorySecretStore::new());
        let manager = open_manager(&state, binary, &clock, &runner, &secrets);
        let store = SqliteStateStore::open(&state.join("state.sqlite")).unwrap();
        let id = AccountId::new("drive");
        store
            .upsert_account(&AccountRecord {
                id: id.clone(),
                display_name: "Fixture Drive".into(),
                kind: "drive".into(),
                remote_name: "household".into(),
                remote_root_folder_id: "parent-id".into(),
                config_path,
                auth_status: if ready { "ready" } else { "pending" }.into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
        let device = store.get_default_device().unwrap().unwrap();
        secrets
            .create_secret(
                &scoped_credential_key(device.id.as_str(), id.as_str()).unwrap(),
                &SecretString::new("fixture"),
            )
            .unwrap();
        fs::create_dir_all(root.join("Data/Research")).unwrap();
        Some(Self {
            manager,
            clock,
            runner,
            id,
            secrets,
            state,
            _dir: dir,
        })
    }

    fn root(&self) -> &Path {
        &self.runner.root
    }

    fn listing(&self, children: &[(&str, &str)]) {
        if self.runner.synthesize_drive_ids {
            return;
        }
        self.dirs(&[("Data", "data-id")]);
        self.dirs(children);
    }

    fn dirs(&self, children: &[(&str, &str)]) {
        let records: Vec<_> = children.iter().map(|(name, id)| serde_json::json!({
            "Name": name, "Path": name, "IsDir": true, "ID": id, "ModTime": "2026-01-02T03:04:05Z"
        })).collect();
        self.runner.fake.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: serde_json::to_string(&records).unwrap(),
                truncated: false,
            }],
            ..Default::default()
        });
    }

    fn mapping(&self, id: &str, folder_id: &str, path: &str) -> AddMappingCommand {
        let local = self.root().join(id);
        fs::create_dir_all(&local).unwrap();
        AddMappingCommand::drive(
            id,
            self.id.clone(),
            CloudCollection {
                folder_id: folder_id.into(),
                display_path: path.into(),
                ancestor_ids: Vec::new(),
            },
            &local,
            self.root().join("local-recovery"),
        )
        .unwrap()
    }

    fn reopen(mut self) -> Self {
        drop(self.manager);
        self.manager = open_manager(
            &self.state,
            pinned_binary_path().unwrap(),
            &self.clock,
            &self.runner,
            &self.secrets,
        );
        self
    }
}

fn open_manager(
    state: &Path,
    binary: PathBuf,
    clock: &Arc<FixedClock>,
    runner: &Arc<FixtureRunner>,
    secrets: &Arc<InMemorySecretStore>,
) -> SyncManager {
    let mut config = ManagerConfig::with_state_dir(state);
    config.clock = clock.clone();
    config.launcher = Arc::new(RealBisyncLauncher::with_process_runner(
        Some(binary.clone()),
        runner.clone(),
    ));
    let mut account_config = AccountServiceConfig::with_state_dir(state);
    account_config.secret_store = secrets.clone();
    account_config.process_runner = runner.clone();
    account_config.rclone_binary_override = Some(binary);
    account_config.password_command_factory = Some(Arc::new(|_| {
        Ok(PasswordCommand::new("/usr/bin/false", vec![]).unwrap())
    }));
    SyncManager::open_with_account_config(config, account_config).unwrap()
}

const MIXED_CHILDREN: [(&str, &str); 4] = [
    ("Research", "research-id"),
    ("Archive", "archive-1"),
    ("Archive", "archive-2"),
    ("Draft?", "draft-id"),
];

#[test]
fn browse_keeps_usable_data_children_among_incompatible_siblings() {
    let Some(fx) = Fixture::new(true) else { return };
    fx.listing(&MIXED_CHILDREN);
    let listing = fx.manager.list_collection_candidates(&fx.id).unwrap();
    assert_eq!(
        listing
            .skipped
            .iter()
            .map(|skip| (skip.name.as_str(), skip.reason))
            .collect::<Vec<_>>(),
        [
            ("Archive", CollectionSkipReason::Duplicate),
            ("Draft?", CollectionSkipReason::UnsupportedName),
        ],
    );
    let candidates = listing.candidates;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].collection.folder_id, "research-id");
    assert_eq!(
        candidates[0].collection.ancestor_ids,
        ["parent-id", "data-id"]
    );
    assert_eq!(candidates[0].name, "Research");
    assert_eq!(candidates[0].modified_at, "2026-01-02T03:04:05Z");
}

#[test]
fn create_refuses_bad_names_before_mkdir_and_ignores_unrelated_siblings() {
    let Some(fx) = Fixture::new(true) else { return };
    assert!(
        fx.manager
            .create_collection_folder(&fx.id, "Invalid?")
            .is_err()
    );
    assert!(fx.runner.fake.spawned_specs().is_empty());

    fs::write(fx.root().join("Data/Existing.txt"), b"existing file").unwrap();
    for name in ["Research", "existing.txt"] {
        fx.dirs(&[("Data", "data-id")]);
        assert!(fx.manager.create_collection_folder(&fx.id, name).is_err());
    }
    assert_eq!(fx.runner.mkdir_calls.load(Ordering::Relaxed), 0);

    fx.dirs(&[("Data", "data-id")]);
    let mut children = MIXED_CHILDREN.to_vec();
    children.push(("Notes", "notes-id"));
    fx.listing(&children);
    let created = fx
        .manager
        .create_collection_folder(&fx.id, "Notes")
        .unwrap();
    assert_eq!(fx.runner.mkdir_calls.load(Ordering::Relaxed), 1);
    assert!(fx.root().join("Data/Notes").is_dir());
    assert_eq!(created.collection.folder_id, "notes-id");
}

#[test]
fn size_is_lazy_cancellable_and_cached() {
    let Some(fx) = Fixture::new(true) else { return };
    fs::write(fx.root().join("Data/Research/note"), b"12345").unwrap();
    assert!(fx.manager.snapshot().folder_sizes.is_empty());
    for _ in 0..2 {
        fx.listing(&MIXED_CHILDREN);
    }
    fx.manager
        .request_folder_size(&fx.id, "research-id")
        .unwrap()
        .wait()
        .unwrap();
    assert!(matches!(
        fx.manager.snapshot().folder_sizes[0].status,
        FolderSizeStatus::Known {
            bytes: 5,
            files: 1,
            ..
        }
    ));
    let calls = fx.runner.fake.spawned_specs().len();
    fx.manager
        .request_folder_size(&fx.id, "research-id")
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(calls, fx.runner.fake.spawned_specs().len());

    fx.listing(&[("Slow", "slow-id")]);
    fx.runner.fake.push_run(FakeRunScript {
        wait_for_stop: true,
        exit_status_on_force_stop: Some(rclone_adapter::ExitStatusInfo {
            code: None,
            signal: Some(9),
        }),
        ..Default::default()
    });
    let handle = fx.manager.request_folder_size(&fx.id, "slow-id").unwrap();
    assert!(matches!(
        fx.manager
            .snapshot()
            .folder_sizes
            .iter()
            .find(|size| size.folder_id == "slow-id")
            .unwrap()
            .status,
        FolderSizeStatus::Calculating
    ));
    assert!(
        fx.manager
            .request_folder_size(&fx.id, "another-id")
            .is_err()
    );
    wait_for_spawn(&fx.runner.fake, "size");
    handle.cancel();
    assert!(handle.wait().is_err());
    assert_eq!(fx.runner.fake.stop_calls(), 1);
    assert!(fx.manager.snapshot().folder_sizes.iter().any(|entry| {
        entry.folder_id == "slow-id" && matches!(entry.status, FolderSizeStatus::Failed { .. })
    }));
}

#[test]
fn mapping_checks_ready_account_leaf_and_ancestry_across_accounts() {
    let Some(fx) = Fixture::new(false) else {
        return;
    };
    assert!(
        fx.manager
            .add_mapping(fx.mapping("local", "research-id", "Data/Research"))
            .unwrap_err()
            .to_string()
            .contains("Sign in")
    );
    let store = SqliteStateStore::open(&fx.state.join("state.sqlite")).unwrap();
    let mut record = store.get_account(&fx.id).unwrap().unwrap();
    record.auth_status = "ready".into();
    store.upsert_account(&record).unwrap();
    fx.listing(&[("Research", "research-id")]);
    assert!(
        fx.manager
            .add_mapping(fx.mapping("local", "wrong-id", "Data/Research"))
            .is_err()
    );
    fx.listing(&[("Research", "research-id")]);
    let mut wrong_ancestors = fx.mapping("local", "research-id", "Data/Research");
    wrong_ancestors.collection.ancestor_ids = vec!["wrong-parent".into()];
    assert!(fx.manager.add_mapping(wrong_ancestors).is_err());
    fx.listing(&[("Research", "research-id")]);
    fx.manager
        .add_mapping(fx.mapping("local", "research-id", "Data/Research"))
        .unwrap();
    assert_eq!(
        store
            .get_mapping(&MappingId::new("local"))
            .unwrap()
            .unwrap()
            .collection
            .ancestor_ids,
        ["parent-id", "data-id"]
    );

    record.id = AccountId::new("second");
    let mut child = fx.mapping("child", "nested-id", "Data/Research/Nested");
    child.account_id = record.id.clone();
    // The second account is scoped to another shared root alias with the same collection IDs.
    let second_config_dir = fx.state.join("accounts/second");
    fs::create_dir_all(&second_config_dir).unwrap();
    fs::copy(&record.config_path, second_config_dir.join("rclone.conf")).unwrap();
    record.config_path = second_config_dir.join("rclone.conf");
    store.upsert_account(&record).unwrap();
    let device = store.get_default_device().unwrap().unwrap();
    fx.secrets
        .create_secret(
            &scoped_credential_key(device.id.as_str(), record.id.as_str()).unwrap(),
            &SecretString::new("fixture"),
        )
        .unwrap();
    fx.listing(&[("Research", "research-id")]);
    fx.dirs(&[("Nested", "nested-id")]);
    assert!(
        fx.manager
            .add_mapping(child)
            .unwrap_err()
            .to_string()
            .contains("nested")
    );
}

const RESEARCH: [(&str, &str); 1] = [("Research", "research-id")];

fn initialize_research(fx: &Fixture, expected_direction: rclone_adapter::InitPreference) {
    fx.listing(&RESEARCH);
    fx.manager
        .add_mapping(fx.mapping("local", "research-id", "Data/Research"))
        .unwrap();
    for _ in 0..3 {
        fx.listing(&RESEARCH);
    }
    fx.manager
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("local"),
            expected_direction: Some(expected_direction),
        })
        .unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert_eq!(
        snapshot.mappings[0].readiness,
        Readiness::Ready,
        "{:?}",
        snapshot.mappings[0].last_run_summary
    );
    let run = fx
        .manager
        .list_runs(&MappingId::new("local"))
        .unwrap()
        .remove(0);
    assert!(
        run.backup_dir2
            .starts_with("household:Recovery/research-id/device-")
    );
    let argv = fs::read_to_string(run.run_dir.join("argv.json")).unwrap();
    for expected in [
        "--drive-skip-gdocs",
        "--drive-skip-shortcuts",
        "--password-command",
        "--resync-mode",
    ] {
        assert!(argv.contains(expected), "missing {expected}");
    }
}

#[test]
fn first_resume_drive_check_preserves_endpoint_and_authentication_failures() {
    let Some(fx) = Fixture::new(true) else { return };
    let mapping = fx.mapping("local", "research-id", "Data/Research");
    fs::write(fx.root().join("local/note"), b"from local").unwrap();
    fx.listing(&RESEARCH);
    fx.manager.add_mapping(mapping).unwrap();
    for _ in 0..3 {
        fx.listing(&RESEARCH);
    }
    fx.runner.fake_first_bisync.store(true, Ordering::Relaxed);
    fx.runner.fake.push_run(FakeRunScript {
        wait_for_stop: true,
        exit_status_on_stop: Some(rclone_adapter::ExitStatusInfo {
            code: None,
            signal: Some(libc::SIGINT),
        }),
        ..Default::default()
    });
    let events = fx.manager.subscribe_events(64);
    let id = MappingId::new("local");
    fx.manager
        .initialize(InitializeCommand {
            mapping_id: id.clone(),
            expected_direction: Some(rclone_adapter::InitPreference::Path1),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !matches!(
        events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap(),
        ManagerEvent::RunStarted { .. }
    ) {}
    fx.manager.stop_run(id.clone()).unwrap();
    let stopped = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert_eq!(
        stopped.mappings[0].readiness,
        Readiness::NeedsInitialization
    );

    fx.listing(&[("Research", "replacement-id")]);
    fx.manager.resume_first_sync(id.clone()).unwrap();
    let endpoint = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();

    fx.runner.fake.push_run(FakeRunScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: "oauth2: invalid_grant".into(),
            truncated: false,
        }],
        exit_status: rclone_adapter::ExitStatusInfo {
            code: Some(1),
            signal: None,
        },
        ..Default::default()
    });
    fx.manager.resume_first_sync(id.clone()).unwrap();
    let authentication = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let runs = fx.manager.list_runs(&id).unwrap();
    assert!(runs[..2].iter().all(|run| run.child_pid.is_none()));
    assert_eq!(endpoint.mappings[0].readiness, Readiness::EndpointChanged);
    assert_eq!(
        authentication.mappings[0].readiness,
        Readiness::AuthenticationRequired
    );
}

#[test]
fn scheduled_drive_run_retries_after_account_lock_contention() {
    let Some(fx) = Fixture::new(true) else { return };
    fs::create_dir(fx.root().join("local")).unwrap();
    fs::write(fx.root().join("local/note"), b"from local").unwrap();
    initialize_research(&fx, rclone_adapter::InitPreference::Path1);

    let poll_at = fx.manager.snapshot().mappings[0].next_poll_at.unwrap();
    let events = fx.manager.subscribe_events(64);
    let lock = AccountConfigLock::acquire(&fx.state.join("accounts/drive/config.lock")).unwrap();
    fx.clock.set(poll_at);
    assert_eq!(wait_for_run_outcome(&events), "retryable");

    let snapshot = fx.manager.snapshot();
    let mapping = &snapshot.mappings[0];
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_eq!(mapping.last_run_outcome, Some(RunOutcome::RetryableFailure));
    assert_eq!(mapping.next_poll_at, None);
    let retry_at = mapping.retry_at.unwrap();
    assert!((30..=36).contains(&(retry_at - poll_at).num_seconds()));

    drop(lock);
    fx.listing(&RESEARCH);
    let Fixture {
        manager,
        clock,
        runner,
        secrets,
        state,
        _dir,
        ..
    } = fx;
    drop(manager);
    let manager = open_manager(
        &state,
        pinned_binary_path().unwrap(),
        &clock,
        &runner,
        &secrets,
    );
    assert_eq!(manager.snapshot().mappings[0].retry_at, Some(retry_at));

    let events = manager.subscribe_events(64);
    clock.set(retry_at);
    assert_eq!(wait_for_run_outcome(&events), "success");
    let mapping = &manager.snapshot().mappings[0];
    assert_eq!(mapping.retry_at, None);
    assert_eq!(
        mapping.next_poll_at,
        Some(retry_at + TimeDelta::seconds(300))
    );
}

fn wait_for_run_outcome(events: &mpsc::Receiver<ManagerEvent>) -> String {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let ManagerEvent::RunFinished { outcome, .. } = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("scheduled run finished")
        {
            return outcome;
        }
    }
}

#[test]
fn first_sync_creates_drive_marker_and_uploads_an_empty_collection() {
    let Some(fx) = Fixture::new(true) else { return };
    fs::create_dir(fx.root().join("local")).unwrap();
    fs::write(fx.root().join("local/note"), b"from local").unwrap();
    initialize_research(&fx, rclone_adapter::InitPreference::Path1);
    assert_eq!(
        fs::read(fx.root().join("Data/Research/note")).unwrap(),
        b"from local"
    );
    let marker = fs::read(fx.root().join("Data/Research/RCLONE_TEST")).unwrap();
    assert_eq!(
        marker,
        fs::read(fx.root().join("local/RCLONE_TEST")).unwrap()
    );
    assert_eq!(
        sync_core::AccessMarker::parse(std::str::from_utf8(&marker).unwrap())
            .unwrap()
            .collection_folder_id,
        "research-id"
    );

    // Detect a replacement on a normal run and put the mapping on hold.
    fx.listing(&[("Research", "replacement-id")]);
    assert!(fx.manager.run_now("local").is_err());
    assert_eq!(
        fx.manager.snapshot().mappings[0].readiness,
        Readiness::EndpointChanged
    );
    assert_eq!(
        fx.manager
            .list_runs(&MappingId::new("local"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn first_sync_downloads_and_reuses_existing_marker_bytes() {
    let Some(fx) = Fixture::new(true) else { return };
    let marker = sync_core::AccessMarker::from_collection(&CloudCollection {
        display_path: "Data/Research".into(),
        folder_id: "research-id".into(),
        ancestor_ids: Vec::new(),
    })
    .format()
    .replace('\n', "\r\n");
    fs::write(fx.root().join("Data/Research/RCLONE_TEST"), &marker).unwrap();
    fs::write(fx.root().join("Data/Research/note"), b"from Drive").unwrap();
    initialize_research(&fx, rclone_adapter::InitPreference::Path2);
    assert_eq!(
        fs::read(fx.root().join("local/note")).unwrap(),
        b"from Drive"
    );
    assert_eq!(
        fs::read(fx.root().join("local/RCLONE_TEST")).unwrap(),
        marker.as_bytes()
    );
    assert_eq!(
        fs::read(fx.root().join("Data/Research/RCLONE_TEST")).unwrap(),
        marker.as_bytes()
    );
}

#[test]
fn first_sync_refuses_to_overwrite_an_unrecognized_marker() {
    let Some(fx) = Fixture::new(true) else { return };
    let cloud_marker = fx.root().join("Data/Research/RCLONE_TEST");
    fs::write(&cloud_marker, b"user file").unwrap();
    fx.listing(&RESEARCH);
    fx.manager
        .add_mapping(fx.mapping("local", "research-id", "Data/Research"))
        .unwrap();
    fx.listing(&RESEARCH);
    let error = fx
        .manager
        .initialize(InitializeCommand {
            mapping_id: MappingId::new("local"),
            expected_direction: None,
        })
        .unwrap_err();
    assert!(matches!(
        error,
        sync_manager::ManagerError::AccessMarkerConflict { .. }
    ));
    assert_eq!(fs::read(cloud_marker).unwrap(), b"user file");
    assert!(!fx.root().join("local/RCLONE_TEST").exists());
    assert!(!fx.root().join("Recovery").exists());
}

#[test]
fn drive_preview_counts_excluded_files_in_subfolder_totals() {
    let Some(fx) = Fixture::new(true) else { return };
    let remote = fx.root().join("Data/Research");
    fs::create_dir_all(remote.join("Archive/Year")).unwrap();
    fs::write(remote.join("Archive/Year/notes"), b"archive").unwrap();
    fs::write(remote.join("active"), b"live").unwrap();
    let mut mapping = fx.mapping("local", "research-id", "Data/Research");
    mapping.extra_exclusions.insert("/Archive/**".into());
    fx.listing(&RESEARCH);
    let inventory = fx
        .manager
        .preflight_mapping_preview(&mapping, None)
        .unwrap()
        .remote_inventory
        .unwrap();
    assert_eq!((inventory.file_count, inventory.total_bytes), (1, 4));
    let archive = &inventory.subfolders[0];
    assert_eq!(archive.name, "Archive");
    assert_eq!(
        (
            archive.file_count,
            archive.total_bytes,
            archive.has_children
        ),
        (1, 7, true)
    );
}

const EXCLUDED_BUILD_FILES: [(&str, &[u8]); 2] = [
    ("proj/build/x.o", b"compiled"),
    ("proj/node_modules/m.js", b"dependency"),
];

fn filter_migration_fixture() -> Option<Fixture> {
    let fx = Fixture::new_with_drive_ids(true, true)?;
    let local = fx.root().join("local");
    for (relative, contents) in EXCLUDED_BUILD_FILES.into_iter().chain([
        ("proj/src/a.txt", b"source".as_slice()),
        ("proj/src/build", b"a file named build".as_slice()),
    ]) {
        let path = local.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    initialize_research(&fx, rclone_adapter::InitPreference::Path1);
    Some(fx)
}

fn add_invalid_excluded_name(fx: &Fixture) -> PathBuf {
    let file = fx.root().join("Data/Research/.Trashes/invalid?.txt");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, b"already excluded").unwrap();
    file
}

fn preview_build_exclusions(fx: &Fixture) -> FilterChangePreview {
    let preview = fx
        .manager
        .propose_filter_change(
            &MappingId::new("local"),
            ["build/**".to_owned(), "node_modules/**".to_owned()].into(),
        )
        .unwrap();
    assert!(preview.is_narrowing());
    assert_eq!(
        preview
            .cloud_move_counts()
            .unwrap()
            .iter()
            .map(|entry| (entry.pattern.as_str(), entry.file_count, entry.total_bytes))
            .collect::<Vec<_>>(),
        [("build/**", 1, 8), ("node_modules/**", 1, 10)]
    );
    preview
}

fn moved_build_root(fx: &Fixture) -> PathBuf {
    let device = SqliteStateStore::open(&fx.state.join("state.sqlite"))
        .unwrap()
        .get_default_device()
        .unwrap()
        .unwrap();
    let recovery = fx
        .root()
        .join("Recovery/research-id")
        .join(device.id.as_str());
    let paths: Vec<_> = fs::read_dir(recovery)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|run| run.join(EXCLUDED_BUILD_FILES[0].0).exists())
        .collect();
    assert_eq!(paths.len(), 1, "expected one move run, found {paths:?}");
    paths.into_iter().next().unwrap()
}

fn assert_build_files_moved(fx: &Fixture, moved: &Path) {
    let remote = fx.root().join("Data/Research");
    for (relative, bytes) in EXCLUDED_BUILD_FILES {
        assert_eq!(
            fs::read(fx.root().join("local").join(relative)).unwrap(),
            bytes
        );
        assert_eq!(fs::read(moved.join(relative)).unwrap(), bytes);
        assert!(!remote.join(relative).exists());
    }
    assert_eq!(fs::read(remote.join("proj/src/a.txt")).unwrap(), b"source");
    assert_eq!(
        fs::read(remote.join("proj/src/build")).unwrap(),
        b"a file named build"
    );
    assert!(!moved.join("proj/src/a.txt").exists());
    assert!(!moved.join("proj/src/build").exists());
}

#[test]
fn drive_narrowing_moves_excluded_objects_to_recovery_and_leaves_local_copies() {
    let Some(fx) = filter_migration_fixture() else {
        return;
    };
    let initial_generation = fx.manager.snapshot().mappings[0]
        .active_generation_id
        .clone();
    let remote = fx.root().join("Data/Research");
    let initial_ids = EXCLUDED_BUILD_FILES
        .map(|(relative, _)| fs::metadata(remote.join(relative)).unwrap().ino());
    let preview = preview_build_exclusions(&fx);
    let invalid = add_invalid_excluded_name(&fx);
    // This file is absent from the review inventory. The old-filter catch-up
    // uploads it, and the fresh move inventory must then include it.
    let pending = "proj/build/pending.o";
    fs::write(
        fx.root().join("local").join(pending),
        b"compiled after review",
    )
    .unwrap();
    fx.manager.apply_filter_change(preview).unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let mapping = &snapshot.mappings[0];
    assert_eq!(
        mapping.readiness,
        Readiness::Ready,
        "{:?}",
        mapping.last_run_summary
    );
    assert_ne!(mapping.active_generation_id, initial_generation);
    assert!(mapping.scheduling_enabled);
    assert!(!mapping.filter_migration_held);
    let moved = moved_build_root(&fx);
    assert_build_files_moved(&fx, &moved);
    assert_eq!(
        EXCLUDED_BUILD_FILES.map(|(relative, _)| fs::metadata(moved.join(relative)).unwrap().ino()),
        initial_ids,
        "server-side move preserves identity"
    );
    assert_eq!(
        fs::read(moved.join(pending)).unwrap(),
        b"compiled after review"
    );
    assert_eq!(
        fs::read(fx.root().join("local").join(pending)).unwrap(),
        b"compiled after review"
    );
    assert!(!remote.join(pending).exists());
    assert_eq!(fs::read(invalid).unwrap(), b"already excluded");
    fx.manager.run_now("local").unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert_eq!(
        snapshot.mappings[0].last_run_outcome,
        Some(RunOutcome::Success)
    );
    assert_eq!(
        snapshot.mappings[0].active_generation_id,
        mapping.active_generation_id
    );
    assert_build_files_moved(&fx, &moved);
    assert_eq!(
        fs::read(moved.join(pending)).unwrap(),
        b"compiled after review"
    );
    assert!(!remote.join(pending).exists());
}

fn assert_held_filter_change(fx: &Fixture, original: &MappingStatusSnapshot) {
    let snapshot = fx.manager.snapshot();
    let mapping = &snapshot.mappings[0];
    assert!(mapping.filter_migration_held);
    assert!(mapping.stopped_first_sync.is_none());
    assert_eq!(mapping.readiness, Readiness::RecoveryRequired);
    assert_eq!(mapping.active_generation_id, original.active_generation_id);
    assert_eq!(mapping.exclusions, original.exclusions);
    assert_eq!(mapping.intent, original.intent);
    assert_eq!(mapping.scheduling_enabled, original.scheduling_enabled);
    assert_eq!(mapping.next_poll_at, None);
    assert!(fx.manager.run_now("local").is_err());
    assert!(fx.manager.complete_recovery("local", true).is_err());
    let resume_error = fx.manager.resume_first_sync("local").unwrap_err();
    assert!(
        resume_error
            .cause()
            .contains("Finish or restore the subfolder update")
    );
    assert!(
        fx.manager
            .inspect_recovery("local")
            .unwrap()
            .filter_migration_held
    );
}

fn hold_filter_change_and_reopen(fx: Fixture) -> Fixture {
    let original = fx.manager.snapshot();
    *fx.runner.post_move_check_fault.lock().unwrap() = Some((0, CheckFault::Fail));
    let preview = preview_build_exclusions(&fx);
    fx.manager.apply_filter_change(preview).unwrap();
    fx.manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert!(fx.runner.post_move_check_fault.lock().unwrap().is_none());
    assert_build_files_moved(&fx, &moved_build_root(&fx));
    add_invalid_excluded_name(&fx);
    let fx = fx.reopen();
    assert_held_filter_change(&fx, &original.mappings[0]);
    fx
}

#[test]
fn drive_narrowing_can_finish_after_post_move_failure_and_restart() {
    let Some(fx) = filter_migration_fixture() else {
        return;
    };
    let old_generation = fx.manager.snapshot().mappings[0]
        .active_generation_id
        .clone();
    let fx = hold_filter_change_and_reopen(fx);
    let moved = moved_build_root(&fx);
    fx.manager.finish_filter_change("local").unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let mapping = &snapshot.mappings[0];
    assert!(
        !mapping.filter_migration_held,
        "{:?}",
        mapping.last_run_summary
    );
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_ne!(mapping.active_generation_id, old_generation);
    assert!(
        mapping
            .exclusions
            .iter()
            .any(|pattern| pattern == "build/**")
    );
    assert!(mapping.scheduling_enabled);
    assert_build_files_moved(&fx, &moved);
}

#[test]
fn drive_narrowing_can_restore_after_post_move_failure_and_restart() {
    let Some(fx) = filter_migration_fixture() else {
        return;
    };
    let original = fx.manager.snapshot();
    let fx = hold_filter_change_and_reopen(fx);
    let moved = moved_build_root(&fx);
    let (relative, original_bytes) = EXCLUDED_BUILD_FILES[0];
    let occupied = fx.root().join("Data/Research").join(relative);
    fs::create_dir_all(occupied.parent().unwrap()).unwrap();
    fs::write(&occupied, b"new file in Data").unwrap();
    if fx.manager.restore_filter_change("local").is_ok() {
        fx.manager
            .wait_until_idle(Duration::from_secs(120))
            .unwrap();
    }
    assert!(fx.manager.snapshot().mappings[0].filter_migration_held);
    assert_eq!(fs::read(&occupied).unwrap(), b"new file in Data");
    assert_eq!(fs::read(moved.join(relative)).unwrap(), original_bytes);
    fs::rename(occupied, fx.root().join("preserved-collision")).unwrap();
    fx.manager.restore_filter_change("local").unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let mapping = &snapshot.mappings[0];
    assert!(
        !mapping.filter_migration_held,
        "{:?}",
        mapping.last_run_summary
    );
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_eq!(
        mapping.active_generation_id,
        original.mappings[0].active_generation_id
    );
    assert_eq!(mapping.exclusions, original.mappings[0].exclusions);
    assert!(mapping.scheduling_enabled);
    for (relative, bytes) in EXCLUDED_BUILD_FILES {
        assert_eq!(
            fs::read(fx.root().join("local").join(relative)).unwrap(),
            bytes
        );
        assert_eq!(
            fs::read(fx.root().join("Data/Research").join(relative)).unwrap(),
            bytes
        );
        assert!(!moved.join(relative).exists());
    }
    fx.manager.run_now("local").unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert_eq!(
        snapshot.mappings[0].last_run_outcome,
        Some(RunOutcome::Success)
    );
}

#[test]
fn drive_narrowing_mkdir_failure_leaves_the_old_mapping_runnable() {
    let Some(fx) = filter_migration_fixture() else {
        return;
    };
    let preview = preview_build_exclusions(&fx);
    let original = fx.manager.snapshot();
    fx.runner.fail_next_mkdir.store(true, Ordering::SeqCst);
    fx.manager.apply_filter_change(preview).unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let mapping = &snapshot.mappings[0];
    assert!(!fx.runner.fail_next_mkdir.load(Ordering::SeqCst));
    assert!(!mapping.filter_migration_held);
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_eq!(
        mapping.active_generation_id,
        original.mappings[0].active_generation_id
    );
    assert_eq!(mapping.exclusions, original.mappings[0].exclusions);
    assert!(mapping.next_poll_at.is_some());
    assert_ne!(mapping.last_run_outcome, Some(RunOutcome::Success));
    assert!(
        mapping
            .last_run_summary
            .as_deref()
            .unwrap()
            .contains("injected recovery mkdir failure")
    );
    for (path, bytes) in EXCLUDED_BUILD_FILES {
        assert_eq!(
            fs::read(fx.root().join("Data/Research").join(path)).unwrap(),
            bytes
        );
    }
}

#[test]
fn drive_narrowing_can_move_all_included_content() {
    let Some(fx) = Fixture::new_with_drive_ids(true, true) else {
        return;
    };
    let local = fx.root().join("local");
    for (path, bytes) in EXCLUDED_BUILD_FILES {
        let file = local.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, bytes).unwrap();
    }
    initialize_research(&fx, rclone_adapter::InitPreference::Path1);
    let preview = preview_build_exclusions(&fx);
    assert_eq!(
        preview
            .preflight()
            .remote_inventory
            .as_ref()
            .unwrap()
            .file_count,
        0
    );
    fx.manager.apply_filter_change(preview).unwrap();
    let after = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert_eq!(after.mappings[0].readiness, Readiness::Ready);
    assert!(!after.mappings[0].filter_migration_held);
    let recovery = moved_build_root(&fx);
    for (path, bytes) in EXCLUDED_BUILD_FILES {
        assert_eq!(fs::read(local.join(path)).unwrap(), bytes);
        assert_eq!(fs::read(recovery.join(path)).unwrap(), bytes);
        assert!(!fx.root().join("Data/Research").join(path).exists());
    }
}

#[test]
fn drive_narrowing_pre_move_check_failure_preserves_old_schedule_and_data() {
    assert_pre_move_check_keeps_original_mapping(false);
}

#[test]
fn drive_narrowing_stop_during_check_preserves_old_schedule_and_data() {
    assert_pre_move_check_keeps_original_mapping(true);
}

fn assert_pre_move_check_keeps_original_mapping(stop: bool) {
    let Some(fx) = filter_migration_fixture() else {
        return;
    };
    let original = fx.manager.snapshot();
    let original = &original.mappings[0];
    let preview = preview_build_exclusions(&fx);
    let (fault, waiting) = if stop {
        let (started, waiting) = mpsc::channel();
        (CheckFault::WaitForStop(started), Some(waiting))
    } else {
        (CheckFault::Fail, None)
    };
    *fx.runner.pre_move_check_fault.lock().unwrap() = Some(fault);
    fx.manager.apply_filter_change(preview).unwrap();
    if let Some(waiting) = waiting {
        waiting.recv_timeout(Duration::from_secs(75)).unwrap();
        assert_eq!(
            fx.manager.snapshot().mappings[0].filter_migration_phase,
            Some(FilterMigrationPhase::Checking)
        );
        fx.manager.stop_run("local").unwrap();
    }
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let mapping = &snapshot.mappings[0];
    assert!(fx.runner.pre_move_check_fault.lock().unwrap().is_none());
    assert!(!mapping.filter_migration_held);
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_eq!(mapping.active_generation_id, original.active_generation_id);
    assert_eq!(mapping.exclusions, original.exclusions);
    assert_eq!(mapping.intent, original.intent);
    assert_eq!(mapping.scheduling_enabled, original.scheduling_enabled);
    assert!(mapping.next_poll_at.is_some());
    if !stop {
        assert!(
            mapping
                .last_run_summary
                .as_deref()
                .unwrap()
                .contains("injected check difference")
        );
    }
    for (relative, bytes) in EXCLUDED_BUILD_FILES {
        assert_eq!(
            fs::read(fx.root().join("local").join(relative)).unwrap(),
            bytes
        );
        assert_eq!(
            fs::read(fx.root().join("Data/Research").join(relative)).unwrap(),
            bytes
        );
    }
}

#[test]
fn drive_narrowing_stop_during_move_keeps_partial_recovery_and_can_restore() {
    let Some(fx) = filter_migration_fixture() else {
        return;
    };
    let original = fx.manager.snapshot();
    let original = &original.mappings[0];
    let preview = preview_build_exclusions(&fx);
    let (started, first_moved) = mpsc::channel();
    *fx.runner.stop_after_first_move.lock().unwrap() = Some(started);
    fx.manager.apply_filter_change(preview).unwrap();
    first_moved.recv_timeout(Duration::from_secs(75)).unwrap();
    assert_eq!(
        fx.manager.snapshot().mappings[0].filter_migration_phase,
        Some(FilterMigrationPhase::MovingToRecovery)
    );
    fx.manager.stop_run("local").unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    assert_held_filter_change(&fx, original);
    let summary = snapshot.mappings[0].last_run_summary.as_deref().unwrap();
    let (first_file, first_bytes) = EXCLUDED_BUILD_FILES[0];
    let (second_file, second_bytes) = EXCLUDED_BUILD_FILES[1];
    assert!(summary.contains(first_file), "{summary}");
    assert!(!summary.contains(second_file), "{summary}");
    let moved = moved_build_root(&fx);
    let data = fx.root().join("Data/Research");
    assert_eq!(fs::read(moved.join(first_file)).unwrap(), first_bytes);
    assert!(!data.join(first_file).exists());
    assert!(!moved.join(second_file).exists());
    assert_eq!(fs::read(data.join(second_file)).unwrap(), second_bytes);
    fx.manager.restore_filter_change("local").unwrap();
    let snapshot = fx
        .manager
        .wait_until_idle(Duration::from_secs(120))
        .unwrap();
    let mapping = &snapshot.mappings[0];
    assert!(
        !mapping.filter_migration_held,
        "{:?}",
        mapping.last_run_summary
    );
    assert_eq!(mapping.readiness, Readiness::Ready);
    assert_eq!(mapping.active_generation_id, original.active_generation_id);
    assert_eq!(mapping.exclusions, original.exclusions);
    for (relative, bytes) in EXCLUDED_BUILD_FILES {
        assert_eq!(
            fs::read(fx.root().join("local").join(relative)).unwrap(),
            bytes
        );
        assert_eq!(fs::read(data.join(relative)).unwrap(), bytes);
        assert!(!moved.join(relative).exists());
    }
}

#[test]
fn drive_narrowing_stop_during_recheck_or_final_verification_keeps_hold() {
    for (checks_before_stop, phase) in [
        (0, FilterMigrationPhase::MovingToRecovery),
        (1, FilterMigrationPhase::Verifying),
    ] {
        let Some(fx) = filter_migration_fixture() else {
            return;
        };
        fx.manager.pause("local").unwrap();
        let original = fx.manager.snapshot();
        let original = &original.mappings[0];
        let preview = preview_build_exclusions(&fx);
        let (started, checking) = mpsc::channel();
        *fx.runner.post_move_check_fault.lock().unwrap() =
            Some((checks_before_stop, CheckFault::WaitForStop(started)));
        fx.manager.apply_filter_change(preview).unwrap();
        checking.recv_timeout(Duration::from_secs(75)).unwrap();
        let in_progress = fx.manager.snapshot();
        assert_eq!(in_progress.mappings[0].filter_migration_phase, Some(phase));
        assert_eq!(
            in_progress.mappings[0].active_generation_id,
            original.active_generation_id
        );
        fx.manager.stop_run("local").unwrap();
        fx.manager
            .wait_until_idle(Duration::from_secs(120))
            .unwrap();
        assert_held_filter_change(&fx, original);
        assert_build_files_moved(&fx, &moved_build_root(&fx));
        fx.manager.finish_filter_change("local").unwrap();
        let finished = fx
            .manager
            .wait_until_idle(Duration::from_secs(120))
            .unwrap();
        let mapping = &finished.mappings[0];
        assert!(
            !mapping.filter_migration_held,
            "{:?}",
            mapping.last_run_summary
        );
        assert_ne!(mapping.active_generation_id, original.active_generation_id);
        assert_eq!(mapping.intent, original.intent);
        assert_eq!(mapping.scheduling_enabled, original.scheduling_enabled);
        assert_eq!(mapping.next_poll_at, None);
    }
}

fn put_mapping_on_authentication_hold(fx: &Fixture) -> (SqliteStateStore, AccountRecord) {
    initialize_research(fx, rclone_adapter::InitPreference::Path1);
    let store = SqliteStateStore::open(&fx.state.join("state.sqlite")).unwrap();
    let mut account = store.get_account(&fx.id).unwrap().unwrap();
    account.auth_status = "authentication_required".into();
    store.upsert_account(&account).unwrap();
    assert!(fx.manager.run_now("local").is_err());
    assert_eq!(
        fx.manager.snapshot().mappings[0].readiness,
        Readiness::AuthenticationRequired
    );
    (store, account)
}

#[test]
fn reconnect_restores_mapping_readiness_and_publishes_it() {
    let Some(fx) = Fixture::new(true) else { return };
    let (store, account) = put_mapping_on_authentication_hold(&fx);
    fs::write(
        &account.config_path,
        "RCLONE_ENCRYPT_V0:
FakeCiphertext==
",
    )
    .unwrap();
    fx.runner.fake.push_run(FakeRunScript::default()); // config reconnect
    fx.runner.fake.push_run(FakeRunScript::default()); // config encryption check
    fx.runner.fake.push_run(FakeRunScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stdout,
            text: "[household]
type = drive
token = XXX
"
            .into(),
            truncated: false,
        }],
        ..Default::default()
    });
    fx.dirs(&[("Data", "data-id")]);
    let snapshots = fx.manager.subscribe_snapshots(1);
    let result = fx
        .manager
        .start_drive_authorization(DriveAuthorization::Reconnect(fx.id.clone()), None)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(result.account.auth_status, "ready");
    assert_eq!(
        snapshots
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .mappings[0]
            .readiness,
        Readiness::Ready
    );
    assert_eq!(
        store
            .get_mapping(&MappingId::new("local"))
            .unwrap()
            .unwrap()
            .state
            .readiness(),
        Readiness::Ready
    );
}

#[test]
fn check_again_restores_readiness_after_verified_drive_sign_in() {
    let Some(fx) = Fixture::new(true) else { return };
    let (store, mut account) = put_mapping_on_authentication_hold(&fx);
    assert!(fx.manager.check_again("local").is_err());
    account.auth_status = "ready".into();
    store.upsert_account(&account).unwrap();

    fx.listing(&[("Research", "replacement-id")]);
    assert!(fx.manager.check_again("local").is_err());
    assert_eq!(
        fx.manager.snapshot().mappings[0].readiness,
        Readiness::AuthenticationRequired
    );
    fx.listing(&RESEARCH);
    let checked = fx.manager.check_again("local").unwrap();
    assert_eq!(checked.snapshot.mappings[0].readiness, Readiness::Ready);
    assert_eq!(
        store
            .get_mapping(&MappingId::new("local"))
            .unwrap()
            .unwrap()
            .state
            .readiness(),
        Readiness::Ready
    );
}

#[test]
fn authorization_cancel_stops_child_and_leaves_account_retryable() {
    let Some(fx) = Fixture::new(false) else {
        return;
    };
    for quit in [false, true] {
        fx.runner.fake.push_run(FakeRunScript {
            wait_for_stop: true,
            ..Default::default()
        });
        let handle = fx
            .manager
            .start_drive_authorization(
                DriveAuthorization::Reconnect(fx.id.clone()),
                Some(Box::new(|_| {})),
            )
            .unwrap();
        let expected_spawns = if quit { 2 } else { 1 };
        let started = Instant::now();
        while fx.runner.fake.spawned_specs().len() < expected_spawns {
            assert!(started.elapsed() < Duration::from_secs(120));
            thread::sleep(Duration::from_millis(10));
        }
        let specs = fx.runner.fake.spawned_specs();
        assert_eq!(
            specs
                .last()
                .unwrap()
                .env
                .get(std::ffi::OsStr::new(
                    "RCLONE_CONFIG_HOUSEHOLD_CONFIG_AUTH_NO_BROWSER"
                ))
                .unwrap(),
            "1"
        );
        if quit {
            fx.manager
                .controlled_shutdown(Duration::from_secs(5))
                .unwrap();
            assert!(handle.wait().is_err());
        } else {
            let canceller = handle.canceller().clone();
            let (finished, result) = mpsc::channel();
            let waiter = thread::spawn(move || finished.send(handle.wait()).unwrap());
            assert!(matches!(
                result.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            thread::spawn(move || canceller.cancel()).join().unwrap();
            assert!(
                result
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .is_err()
            );
            waiter.join().unwrap();
        }
        assert_eq!(fx.runner.fake.stop_calls(), expected_spawns);
        assert_eq!(
            fx.manager.list_accounts().unwrap()[0].auth_status,
            "authentication_required"
        );
    }
}

fn wait_for_spawn(runner: &FakeProcessRunner, command: &str) {
    let started = Instant::now();
    while !runner
        .spawned_specs()
        .iter()
        .any(|spec| spec.args[0] == command)
    {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "did not spawn {command}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
