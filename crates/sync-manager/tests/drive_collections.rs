use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use rclone_adapter::{
    FakeProcessRunner, FakeRunScript, OutputLine, PasswordCommand, ProcessError, ProcessRunner,
    ProcessSpec, RealProcessRunner, RunHandle, SpawnOptions, StreamKind, pinned_binary_path,
};
use sync_core::{AccountId, CloudCollection, MappingId, Readiness};
use sync_manager::{
    AccountRecord, AccountServiceConfig, AddMappingCommand, DriveAuthorization, FolderSizeStatus,
    InMemorySecretStore, InitializeCommand, ManagerConfig, RealBisyncLauncher, SecretStore,
    SecretString, SqliteStateStore, SyncManager, scoped_credential_key,
};

struct FixtureRunner {
    fake: FakeProcessRunner,
    root: PathBuf,
}

impl ProcessRunner for FixtureRunner {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        let args = &spec.args;
        let command = args[0].to_str().unwrap();
        let directory_listing = args.iter().any(|arg| arg == "--dirs-only");
        if directory_listing
            || command == "config"
            || command == "size" && args.iter().any(|arg| arg == "household:Data/Slow")
        {
            return self.fake.spawn(spec, options);
        }
        let mut spec = spec.clone();
        spec.current_dir = Some(self.root.clone());
        RealProcessRunner.spawn(&spec, options)
    }
}

struct Fixture {
    manager: SyncManager,
    runner: Arc<FixtureRunner>,
    id: AccountId,
    secrets: Arc<InMemorySecretStore>,
    state: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(ready: bool) -> Option<Self> {
        let binary = pinned_binary_path()?;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let runner = Arc::new(FixtureRunner {
            fake: FakeProcessRunner::new(),
            root: root.clone(),
        });
        let state = root.join("state");
        let account_dir = state.join("accounts/drive");
        fs::create_dir_all(&account_dir).unwrap();
        let config_path = account_dir.join("rclone.conf");
        fs::write(&config_path, "[household]\ntype = local\n").unwrap();
        let mut config = ManagerConfig::with_state_dir(&state);
        config.launcher = Arc::new(RealBisyncLauncher::with_process_runner(
            Some(binary.clone()),
            runner.clone(),
        ));
        let secrets = Arc::new(InMemorySecretStore::new());
        let mut account_config = AccountServiceConfig::with_state_dir(&state);
        account_config.secret_store = secrets.clone();
        account_config.process_runner = runner.clone();
        account_config.rclone_binary_override = Some(binary);
        account_config.password_command_factory = Some(Arc::new(|_| {
            Ok(PasswordCommand::new("/usr/bin/false", vec![]).unwrap())
        }));
        let manager = SyncManager::open_with_account_config(config, account_config).unwrap();
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
}

#[test]
fn browse_lists_only_data_children_and_rejects_duplicate_names() {
    let Some(fx) = Fixture::new(true) else { return };
    fx.listing(&[("Research", "research-id")]);
    let candidates = fx.manager.list_collection_candidates(&fx.id).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].collection.folder_id, "research-id");
    assert_eq!(
        candidates[0].collection.ancestor_ids,
        ["parent-id", "data-id"]
    );
    assert_eq!(candidates[0].name, "Research");
    assert_eq!(candidates[0].modified_at, "2026-01-02T03:04:05Z");

    fx.listing(&[("Research", "research-id"), ("Research", "another-id")]);
    assert!(
        fx.manager
            .list_collection_candidates(&fx.id)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
}

#[test]
fn create_refuses_duplicate_and_resolves_created_folder() {
    let Some(fx) = Fixture::new(true) else { return };
    fx.dirs(&[("Data", "data-id")]);
    assert!(
        fx.manager
            .create_collection_folder(&fx.id, "Research")
            .is_err()
    );
    fx.dirs(&[("Data", "data-id")]);
    fx.listing(&[("Research", "research-id"), ("Notes", "notes-id")]);
    let created = fx
        .manager
        .create_collection_folder(&fx.id, "Notes")
        .unwrap();
    assert!(fx.root().join("Data/Notes").is_dir());
    assert_eq!(created.collection.folder_id, "notes-id");
}

#[test]
fn size_is_lazy_cancellable_and_cached() {
    let Some(fx) = Fixture::new(true) else { return };
    fs::write(fx.root().join("Data/Research/note"), b"12345").unwrap();
    assert!(fx.manager.snapshot().folder_sizes.is_empty());
    for _ in 0..2 {
        fx.listing(&[("Research", "research-id")]);
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
    let snapshot = fx.manager.wait_until_idle(Duration::from_secs(90)).unwrap();
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
            assert!(started.elapsed() < Duration::from_secs(10));
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
        } else {
            handle.cancel();
        }
        assert!(handle.wait().is_err());
        assert_eq!(fx.runner.fake.stop_calls(), expected_spawns);
    }

    assert_eq!(
        fx.manager.list_accounts().unwrap()[0].auth_status,
        "authentication_required"
    );
}

fn wait_for_spawn(runner: &FakeProcessRunner, command: &str) {
    let started = Instant::now();
    while !runner
        .spawned_specs()
        .iter()
        .any(|spec| spec.args[0] == command)
    {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "did not spawn {command}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
