use std::{
    fs,
    path::Path,
    sync::{Arc, Barrier},
    thread,
};

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

use rclone_adapter::{
    FakeProcessRunner, FakeRunScript, OutputLine, PasswordCommand, StreamKind, pinned_binary_path,
};
use sync_core::AccountId;
use sync_manager::{
    AccountError, AccountService, AccountServiceConfig, AddDriveAccountRequest,
    ImportAccountRequest, InMemorySecretStore, PasswordCommandFactory,
    RESERVED_LOCAL_FIXTURE_ACCOUNT_ID, SecretStore, SecretStoreError, SecretString,
    SqliteStateStore, StateLock,
};
use tempfile::TempDir;

#[cfg(unix)]
fn helper_factory_for_store(
    helper_root: &Path,
    store: Arc<InMemorySecretStore>,
) -> PasswordCommandFactory {
    let dir = helper_root.to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    Arc::new(move |cred_key: &str| {
        let secret = store.get_secret(cred_key).map_err(AccountError::from)?;
        let safe_name = cred_key.replace('/', "__");
        let sec_file = dir.join(format!("{safe_name}.sec"));
        fs::write(&sec_file, secret.expose().as_bytes()).unwrap();
        fs::set_permissions(&sec_file, fs::Permissions::from_mode(0o600)).unwrap();

        let bin = dir.join(format!("helper-{safe_name}"));
        fs::write(&bin, format!("#!/bin/sh\ncat \"{}\"\n", sec_file.display())).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        Ok(PasswordCommand::new(bin, vec!["--account".to_string(), cred_key.to_string()]).unwrap())
    })
}

/// Finding 1: Two disposable state dirs sharing ONE fake SecretStore must not
/// overwrite or delete each other's Keychain credentials, and create_secret is create-only.
#[cfg(unix)]
#[test]
fn two_state_dirs_sharing_one_fake_store_isolate_credentials_and_create_is_exclusive() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let shared_store = Arc::new(InMemorySecretStore::new());

    // Direct create_secret exclusivity check.
    shared_store
        .create_secret("inst-a/acc-1", &SecretString::new("first-secret"))
        .unwrap();
    assert!(matches!(
        shared_store.create_secret("inst-a/acc-1", &SecretString::new("second-secret")),
        Err(SecretStoreError::AlreadyExists { .. })
    ));
    assert_eq!(
        shared_store.get_secret("inst-a/acc-1").unwrap().expose(),
        "first-secret"
    );

    let state_a = tmp.path().join("state-a");
    let state_b = tmp.path().join("state-b");
    let tree = tmp.path().join("tree");
    fs::create_dir_all(tree.join("Data")).unwrap();
    let src = tmp.path().join("src.conf");
    fs::write(&src, "[demo]\ntype = local\n").unwrap();

    let make_svc = |state_dir: &Path, helper_sub: &str| {
        let mut cfg = AccountServiceConfig::with_state_dir(state_dir);
        cfg.secret_store = Arc::clone(&shared_store) as Arc<dyn SecretStore>;
        cfg.password_command_factory = Some(helper_factory_for_store(
            &tmp.path().join(helper_sub),
            Arc::clone(&shared_store),
        ));
        AccountService::new(cfg).unwrap()
    };

    let svc_a = make_svc(&state_a, "helper-a");
    let svc_b = make_svc(&state_b, "helper-b");

    // Both state directories import the SAME account ID `household-a`.
    svc_a
        .import_account(ImportAccountRequest {
            id: AccountId::new("household-a"),
            display_name: "State A".to_string(),
            source_config_path: src.clone(),
            source_remote_name: "demo".to_string(),
            target_remote_name: "household".to_string(),
            root_folder_id: Some(tree.to_string_lossy().into_owned()),
            allow_global_default_source: false,
        })
        .unwrap();

    svc_b
        .import_account(ImportAccountRequest {
            id: AccountId::new("household-a"),
            display_name: "State B".to_string(),
            source_config_path: src.clone(),
            source_remote_name: "demo".to_string(),
            target_remote_name: "household".to_string(),
            root_folder_id: Some(tree.to_string_lossy().into_owned()),
            allow_global_default_source: false,
        })
        .unwrap();

    // A failing import in state B for a missing remote with the same ID must NOT touch state A.
    let _ = svc_b.import_account(ImportAccountRequest {
        id: AccountId::new("household-a"),
        display_name: "Fail".to_string(),
        source_config_path: src.clone(),
        source_remote_name: "nonexistent".to_string(),
        target_remote_name: "household".to_string(),
        root_folder_id: None,
        allow_global_default_source: false,
    });

    // Both state A and state B still decrypt and check cleanly!
    assert!(
        svc_a
            .check_account(&AccountId::new("household-a"))
            .unwrap()
            .encrypted_config_verified
    );
    assert!(
        svc_b
            .check_account(&AccountId::new("household-a"))
            .unwrap()
            .encrypted_config_verified
    );
}

/// Finding 2: Leftover setup temp files and rclone plaintext backup files are
/// cleaned up on startup and on interrupted setup without publishing final rclone.conf.
#[cfg(unix)]
#[test]
fn startup_cleans_leftover_temp_and_rclone_backup_files_in_account_dir() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let acct_dir = state_dir.join("accounts/drive-a");
    fs::create_dir_all(&acct_dir).unwrap();

    let leftover_tmp = acct_dir.join(".foldersync-setup-tmp-deadbeef.conf");
    let rclone_old_bak = acct_dir.join("rclone.conf.old987654321");
    let real_cfg = acct_dir.join("rclone.conf");
    fs::write(&leftover_tmp, b"[household]\nclient_secret = leaked\n").unwrap();
    fs::write(&rclone_old_bak, b"RCLONE_ENCRYPT_V0:\nEncryptedBackup==\n").unwrap();
    fs::write(
        &real_cfg,
        b"# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nCipher==\n",
    )
    .unwrap();
    fs::set_permissions(&real_cfg, fs::Permissions::from_mode(0o600)).unwrap();

    let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
    cfg.secret_store = Arc::new(InMemorySecretStore::new());
    let _svc = AccountService::new(cfg).unwrap();

    assert!(
        !leftover_tmp.exists(),
        "leftover FolderSync setup temp file must be removed on startup"
    );
    assert!(
        rclone_old_bak.exists(),
        "rclone's own .old* encrypted backup must be preserved on startup"
    );
    assert!(
        real_cfg.exists(),
        "real encrypted rclone.conf must be preserved"
    );
}

/// Finding 3: Concurrent add/import for the same account ID under manager-callable mode
/// must serialize under one per-account lock so the loser never overwrites or deletes the winner's secret.
#[cfg(unix)]
#[test]
fn concurrent_import_in_locked_manager_mode_serializes_and_preserves_winner_credential() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let tree = tmp.path().join("tree");
    fs::create_dir_all(tree.join("Data")).unwrap();
    let src = tmp.path().join("src.conf");
    fs::write(&src, "[demo]\ntype = local\n").unwrap();

    let shared_store = Arc::new(InMemorySecretStore::new());
    let helper_factory =
        helper_factory_for_store(&tmp.path().join("helpers"), Arc::clone(&shared_store));

    let make_mgr_svc = || {
        let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
        cfg.secret_store = Arc::clone(&shared_store) as Arc<dyn SecretStore>;
        cfg.password_command_factory = Some(Arc::clone(&helper_factory));
        AccountService::for_locked_manager(cfg).unwrap()
    };

    // Pre-initialize DB & device ID once before racing.
    let svc_init = make_mgr_svc();
    assert!(svc_init.list_accounts().unwrap().is_empty());

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let b = Arc::clone(&barrier);
        let svc = make_mgr_svc();
        let src_c = src.clone();
        let tree_c = tree.clone();
        handles.push(thread::spawn(move || {
            b.wait();
            svc.import_account(ImportAccountRequest {
                id: AccountId::new("race-acct"),
                display_name: "Race Account".to_string(),
                source_config_path: src_c,
                source_remote_name: "demo".to_string(),
                target_remote_name: "household".to_string(),
                root_folder_id: Some(tree_c.to_string_lossy().into_owned()),
                allow_global_default_source: false,
            })
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let already_exists_count = results
        .iter()
        .filter(|r| matches!(r, Err(AccountError::AlreadyExists { .. })))
        .count();
    assert_eq!(
        ok_count, 1,
        "exactly one concurrent import must win: {results:?}"
    );
    assert_eq!(
        already_exists_count, 1,
        "loser must fail with AlreadyExists without deleting winner credential: {results:?}"
    );

    // Winner's encrypted config and credential must remain intact and verifiable.
    let check = svc_init
        .check_account(&AccountId::new("race-acct"))
        .unwrap();
    assert!(check.encrypted_config_verified);
}

/// Finding 6: Symlinks at `manager.lock` and `state.sqlite` must be refused
/// before any mutation, leaving outside target files byte-identical.
#[cfg(unix)]
#[test]
fn refuses_symlinks_at_manager_lock_and_state_sqlite_leaving_outside_targets_untouched() {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();

    // Case 1: symlink at manager.lock
    let outside_lock_target = outside.join("precious-lock.txt");
    fs::write(&outside_lock_target, b"DO-NOT-TRUNCATE").unwrap();
    let state1 = tmp.path().join("state-lock-symlink");
    fs::create_dir_all(&state1).unwrap();
    symlink(&outside_lock_target, state1.join("manager.lock")).unwrap();

    assert!(StateLock::acquire(&state1).is_err());
    assert_eq!(
        fs::read(&outside_lock_target).unwrap(),
        b"DO-NOT-TRUNCATE",
        "outside target of manager.lock symlink must remain byte-identical"
    );

    let mut cfg1 = AccountServiceConfig::with_state_dir(&state1);
    cfg1.secret_store = Arc::new(InMemorySecretStore::new());
    let svc1 = AccountService::new(cfg1).unwrap();
    assert!(svc1.list_accounts().is_err());
    assert_eq!(fs::read(&outside_lock_target).unwrap(), b"DO-NOT-TRUNCATE");

    // Case 2: symlink at state.sqlite
    let outside_db_target = outside.join("precious-db.bin");
    fs::write(&outside_db_target, b"DO-NOT-OPEN-AS-SQLITE").unwrap();
    let state2 = tmp.path().join("state-db-symlink");
    fs::create_dir_all(&state2).unwrap();
    symlink(&outside_db_target, state2.join("state.sqlite")).unwrap();

    assert!(SqliteStateStore::open(&state2.join("state.sqlite")).is_err());
    assert_eq!(
        fs::read(&outside_db_target).unwrap(),
        b"DO-NOT-OPEN-AS-SQLITE",
        "outside target of state.sqlite symlink must remain byte-identical"
    );

    let mut cfg2 = AccountServiceConfig::with_state_dir(&state2);
    cfg2.secret_store = Arc::new(InMemorySecretStore::new());
    let svc2 = AccountService::new(cfg2).unwrap();
    assert!(svc2.list_accounts().is_err());
    assert_eq!(
        fs::read(&outside_db_target).unwrap(),
        b"DO-NOT-OPEN-AS-SQLITE"
    );
}

/// Finding 8: `local-fixture` account ID is reserved; add-drive and import refuse it.
#[test]
fn reserved_local_fixture_id_is_refused_by_add_and_import() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
    cfg.secret_store = Arc::new(InMemorySecretStore::new());
    let svc = AccountService::new(cfg).unwrap();

    let err_add = svc
        .add_drive_account(
            AddDriveAccountRequest {
                id: AccountId::new(RESERVED_LOCAL_FIXTURE_ACCOUNT_ID),
                display_name: "Bad".to_string(),
                remote_name: "household".to_string(),
                root_folder_id: "0ARoot".to_string(),
                client_id: "cid".to_string(),
                client_secret: SecretString::new("csec"),
                scope: "drive".to_string(),
                run_oauth_flow: false,
            },
            None,
        )
        .unwrap_err();
    assert!(matches!(err_add, AccountError::InvalidInput { .. }));

    let src = tmp.path().join("src.conf");
    fs::write(&src, "[local-fixture]\ntype = local\n").unwrap();
    let err_imp = svc
        .import_account(ImportAccountRequest {
            id: AccountId::new(RESERVED_LOCAL_FIXTURE_ACCOUNT_ID),
            display_name: "Bad".to_string(),
            source_config_path: src,
            source_remote_name: "local-fixture".to_string(),
            target_remote_name: "local-fixture".to_string(),
            root_folder_id: None,
            allow_global_default_source: false,
        })
        .unwrap_err();
    assert!(matches!(err_imp, AccountError::InvalidInput { .. }));
}

/// Finding 7: Auth command exiting 0 without saving a token must NOT mark the account ready,
/// and leaves a retryable state that `reconnect_drive_account` (or re-running `add_drive_account`) can resume.
#[cfg(unix)]
#[test]
fn drive_auth_without_saved_token_is_not_ready_and_can_resume() {
    let Some(rclone_path) = pinned_binary_path() else {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let secret_store = Arc::new(InMemorySecretStore::new());
    let helper_factory =
        helper_factory_for_store(&tmp.path().join("helpers"), Arc::clone(&secret_store));

    // Hybrid runner: real pinned rclone for config encryption/check/redacted, fake for reconnect & lsjson.
    struct MockReconnectRunner {
        fake_reconnect: FakeProcessRunner,
        redacted_override: std::sync::Mutex<Option<String>>,
    }
    impl rclone_adapter::ProcessRunner for MockReconnectRunner {
        fn spawn(
            &self,
            spec: &rclone_adapter::ProcessSpec,
            options: rclone_adapter::SpawnOptions,
        ) -> Result<rclone_adapter::RunHandle, rclone_adapter::ProcessError> {
            let args: Vec<String> = spec
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            if args.first().map(String::as_str) == Some("config")
                && args.get(1).map(String::as_str) == Some("reconnect")
            {
                return self.fake_reconnect.spawn(spec, options);
            }
            if args.first().map(String::as_str) == Some("config")
                && args.get(1).map(String::as_str) == Some("redacted")
                && let Some(ini) = self.redacted_override.lock().unwrap().clone()
            {
                let f = FakeProcessRunner::new();
                f.push_run(FakeRunScript {
                    lines: vec![OutputLine {
                        stream: StreamKind::Stdout,
                        text: ini,
                        truncated: false,
                    }],
                    ..Default::default()
                });
                return f.spawn(spec, options);
            }
            if args.first().map(String::as_str) == Some("lsjson") {
                let f = FakeProcessRunner::new();
                f.push_run(FakeRunScript {
                    lines: vec![OutputLine {
                        stream: StreamKind::Stdout,
                        text: r#"[{"Path":"Data","Name":"Data","ID":"1Data","IsDir":true}]"#
                            .to_string(),
                        truncated: false,
                    }],
                    ..Default::default()
                });
                return f.spawn(spec, options);
            }
            rclone_adapter::RealProcessRunner.spawn(spec, options)
        }
    }

    let fake_reconnect = FakeProcessRunner::new();
    // Attempt 1: exits 0 without saving a token in the encrypted config.
    fake_reconnect.push_run(FakeRunScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=attempt1".to_string(),
            truncated: false,
        }],
        ..Default::default()
    });
    // Attempt 2: reconnect succeeds and config redacted now shows token = XXX!
    fake_reconnect.push_run(FakeRunScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=attempt2".to_string(),
            truncated: false,
        }],
        ..Default::default()
    });

    let runner = Arc::new(MockReconnectRunner {
        fake_reconnect,
        redacted_override: std::sync::Mutex::new(None),
    });

    let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
    cfg.secret_store = Arc::clone(&secret_store) as Arc<dyn SecretStore>;
    cfg.process_runner = Arc::clone(&runner) as Arc<dyn rclone_adapter::ProcessRunner>;
    cfg.rclone_binary_override = Some(rclone_path);
    cfg.password_command_factory = Some(helper_factory);
    let svc = AccountService::new(cfg).unwrap();

    // Attempt 1: real config redacted has NO token -> MUST fail with AuthenticationRequired!
    let err = svc
        .add_drive_account(
            AddDriveAccountRequest {
                id: AccountId::new("drive-retry"),
                display_name: "Retry Drive".to_string(),
                remote_name: "household".to_string(),
                root_folder_id: "0ASharedParent".to_string(),
                client_id: "cid".to_string(),
                client_secret: SecretString::new("csec"),
                scope: "drive".to_string(),
                run_oauth_flow: true,
            },
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, AccountError::AuthenticationRequired { .. }),
        "missing saved token must return AuthenticationRequired, got {err:?}"
    );
    let rec1 = svc
        .get_account(&AccountId::new("drive-retry"))
        .unwrap()
        .unwrap();
    assert_eq!(rec1.auth_status, "authentication_required");

    // Attempt 2: simulate rclone saving a token and resume via reconnect_drive_account!
    *runner.redacted_override.lock().unwrap() =
        Some("[household]\ntype = drive\nroot_folder_id = XXX\ntoken = XXX\n".to_string());
    let res = svc
        .reconnect_drive_account(&AccountId::new("drive-retry"), None)
        .unwrap();
    assert_eq!(res.account.auth_status, "ready");
    assert_eq!(
        res.auth_url.as_deref(),
        Some("http://127.0.0.1:53682/auth?state=attempt2")
    );
}

/// Finding 4: Importing a Drive remote rewrites `root_folder_id`, strips
/// `shared_with_me`, and verifies the effective root by listing it before
/// recording in SQLite (rolling back config + credential if listing fails).
#[cfg(unix)]
#[test]
fn drive_import_verifies_effective_root_before_recording_and_rolls_back_on_failure() {
    let Some(rclone_path) = pinned_binary_path() else {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let src_cfg = tmp.path().join("chosen/drive-src.conf");
    fs::create_dir_all(src_cfg.parent().unwrap()).unwrap();
    fs::write(
        &src_cfg,
        "\
[my_drive]
type = drive
client_id = cid
client_secret = csec
root_folder_id = OldWrongRoot
token = {\"access_token\":\"tok\",\"refresh_token\":\"ref\"}
",
    )
    .unwrap();

    let secret_store = Arc::new(InMemorySecretStore::new());
    let helper_factory =
        helper_factory_for_store(&tmp.path().join("helpers"), Arc::clone(&secret_store));

    struct HybridLsJsonRunner {
        fake_lsjson: FakeProcessRunner,
    }
    impl rclone_adapter::ProcessRunner for HybridLsJsonRunner {
        fn spawn(
            &self,
            spec: &rclone_adapter::ProcessSpec,
            options: rclone_adapter::SpawnOptions,
        ) -> Result<rclone_adapter::RunHandle, rclone_adapter::ProcessError> {
            if spec.args.first().and_then(|s| s.to_str()) == Some("lsjson") {
                self.fake_lsjson.spawn(spec, options)
            } else {
                rclone_adapter::RealProcessRunner.spawn(spec, options)
            }
        }
    }

    let fake_lsjson = FakeProcessRunner::new();
    // Run 1: effective root listing fails -> import must roll back!
    fake_lsjson.push_run(FakeRunScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: "ERROR : directory not found".to_string(),
            truncated: false,
        }],
        exit_status: rclone_adapter::ExitStatusInfo {
            code: Some(3),
            signal: None,
        },
        ..Default::default()
    });
    // Run 2: effective root listing succeeds -> account recorded!
    fake_lsjson.push_run(FakeRunScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stdout,
            text: r#"[{"Path":"Data","Name":"Data","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T00:00:00Z","IsDir":true,"ID":"1DataId"}]"#.to_string(),
            truncated: false,
        }],
        ..Default::default()
    });

    let runner = Arc::new(HybridLsJsonRunner { fake_lsjson });
    let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
    cfg.secret_store = Arc::clone(&secret_store) as Arc<dyn SecretStore>;
    cfg.process_runner = Arc::clone(&runner) as Arc<dyn rclone_adapter::ProcessRunner>;
    cfg.rclone_binary_override = Some(rclone_path);
    cfg.password_command_factory = Some(helper_factory);
    let svc = AccountService::new(cfg).unwrap();

    // 1. Failing root verification rolls back SQLite, config file, and owned credential.
    let err = svc
        .import_account(ImportAccountRequest {
            id: AccountId::new("drive-imp"),
            display_name: "Imported Drive".to_string(),
            source_config_path: src_cfg.clone(),
            source_remote_name: "my_drive".to_string(),
            target_remote_name: "household".to_string(),
            root_folder_id: Some("0ARequestedRoot".to_string()),
            allow_global_default_source: false,
        })
        .unwrap_err();
    assert!(!matches!(err, AccountError::AlreadyExists { .. }));
    assert!(
        svc.get_account(&AccountId::new("drive-imp"))
            .unwrap()
            .is_none()
    );
    assert!(!state_dir.join("accounts/drive-imp/rclone.conf").exists());
    assert!(secret_store.is_empty());

    // 2. Succeeding root verification records the account with the requested shared parent ID.
    let rec = svc
        .import_account(ImportAccountRequest {
            id: AccountId::new("drive-imp"),
            display_name: "Imported Drive".to_string(),
            source_config_path: src_cfg,
            source_remote_name: "my_drive".to_string(),
            target_remote_name: "household".to_string(),
            root_folder_id: Some("0ARequestedRoot".to_string()),
            allow_global_default_source: false,
        })
        .unwrap();
    assert_eq!(rec.kind, "drive");
    assert_eq!(rec.remote_root_folder_id, "0ARequestedRoot");
    assert!(state_dir.join("accounts/drive-imp/rclone.conf").is_file());
    assert_eq!(secret_store.len(), 1);
}
