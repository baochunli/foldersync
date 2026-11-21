//! Regression tests for `drive-t4` codex review round 2 (6 HIGH findings).
//!
//! All tests use isolated temp directories, in-memory `SecretStore` fakes,
//! and fake/fixture runners or pinned local rclone — never touching real
//! Keychain, real Google Drive, `~/.config/rclone`, or
//! `~/Library/Application Support/FolderSync`.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use rclone_adapter::{
    ConfigError, ExitStatusInfo, FakeProcessRunner, FakeRunScript, OutputLine, PasswordCommand,
    ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, RunHandle, SpawnOptions,
    StreamKind, pinned_binary_path,
};
use sync_core::AccountId;
use sync_manager::{
    AccountError, AccountRecord, AccountService, AccountServiceConfig, AddDriveAccountRequest,
    ImportAccountRequest, InMemorySecretStore, ManagerError, PasswordCommandFactory, SecretStore,
    SecretString, SqliteStateStore, SystemClock, scoped_credential_key,
};
use tempfile::TempDir;

#[cfg(unix)]
fn fake_password_factory(
    store: Arc<InMemorySecretStore>,
    helper_dir: &Path,
) -> PasswordCommandFactory {
    let helper_dir = helper_dir.to_path_buf();
    Arc::new(move |cred_key: &str| {
        let secret = store.get_secret(cred_key).map_err(AccountError::from)?;
        let safe = cred_key.replace('/', "__");
        let sec_file = helper_dir.join(format!("{safe}.sec"));
        std::fs::write(&sec_file, secret.expose().as_bytes()).unwrap();
        std::fs::set_permissions(&sec_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let bin = helper_dir.join(format!("helper-{safe}"));
        std::fs::write(&bin, format!("#!/bin/sh\ncat \"{}\"\n", sec_file.display())).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        Ok(PasswordCommand::new(bin, vec!["--account".to_string(), cred_key.to_string()]).unwrap())
    })
}

fn service_with(
    state_dir: &Path,
    store: Arc<InMemorySecretStore>,
    runner: Arc<dyn ProcessRunner>,
    factory: Option<PasswordCommandFactory>,
) -> AccountService {
    AccountService::new(AccountServiceConfig {
        state_dir: state_dir.to_path_buf(),
        secret_store: store as Arc<dyn SecretStore>,
        process_runner: runner,
        clock: Arc::new(SystemClock),
        rclone_binary_override: None,
        credential_helper_binary: None,
        password_command_factory: factory,
        acquire_state_lock: true,
    })
    .unwrap()
}

/// Finding (#1): Cleanup deletes ONLY our unique `.foldersync-setup-tmp-*` prefix —
/// never `rclone.conf`, never rclone's `rclone.conf.old*` backups, and never
/// unrelated user files like `saved-remote.bak`.
#[test]
fn cleanup_only_removes_setup_temp_prefix_never_rclone_conf_old_backups_or_user_files() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let acct_dir = state_dir.join("accounts/a1");
    std::fs::create_dir_all(&acct_dir).unwrap();

    let active_cfg = acct_dir.join("rclone.conf");
    let rclone_backup = acct_dir.join("rclone.conf.old123456");
    let user_bak = acct_dir.join("saved-remote.bak");
    let user_tmp = acct_dir.join("notes.tmp");
    let setup_tmp = acct_dir.join(".foldersync-setup-tmp-0123456789abcdef.conf");
    let setup_tmp_old = acct_dir.join(".foldersync-setup-tmp-0123456789abcdef.conf.old1");

    let active_bytes = b"RCLONE_ENCRYPT_V0:\nactive-ciphertext\n";
    let backup_bytes = b"RCLONE_ENCRYPT_V0:\nprevious-encrypted-backup\n";
    let user_bak_bytes = b"[user_saved]\ntype = local\n";
    let user_tmp_bytes = b"user scratch notes\n";

    std::fs::write(&active_cfg, active_bytes).unwrap();
    std::fs::write(&rclone_backup, backup_bytes).unwrap();
    std::fs::write(&user_bak, user_bak_bytes).unwrap();
    std::fs::write(&user_tmp, user_tmp_bytes).unwrap();
    std::fs::write(&setup_tmp, b"client_secret = temp-secret").unwrap();
    std::fs::write(&setup_tmp_old, b"client_secret = temp-backup-secret").unwrap();

    let store = Arc::new(InMemorySecretStore::new());
    let _svc = service_with(&state_dir, store, Arc::new(FakeProcessRunner::new()), None);

    assert!(
        !setup_tmp.exists(),
        "our setup temp file must be cleaned up on startup"
    );
    assert!(
        !setup_tmp_old.exists(),
        "rclone backup of our setup temp file must be cleaned up on startup"
    );
    assert_eq!(
        std::fs::read(&active_cfg).unwrap(),
        active_bytes,
        "active rclone.conf must stay intact"
    );
    assert_eq!(
        std::fs::read(&rclone_backup).unwrap(),
        backup_bytes,
        "rclone.conf.old* backup must NEVER be deleted by cleanup"
    );
    assert_eq!(
        std::fs::read(&user_bak).unwrap(),
        user_bak_bytes,
        "unrelated user .bak file must NEVER be deleted by cleanup"
    );
    assert_eq!(
        std::fs::read(&user_tmp).unwrap(),
        user_tmp_bytes,
        "unrelated user .tmp file must NEVER be deleted by cleanup"
    );
}

/// Finding (#1): If the final `rclone.conf` is missing while an `rclone.conf.old*`
/// backup exists, report "recovery needed" and leave all backup files untouched.
#[test]
fn missing_final_config_with_rclone_backup_reports_recovery_needed_and_leaves_files_untouched() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let acct_dir = state_dir.join("accounts/rec1");
    std::fs::create_dir_all(&acct_dir).unwrap();

    let backup_path = acct_dir.join("rclone.conf.old987654");
    let backup_bytes = b"RCLONE_ENCRYPT_V0:\nonly-recoverable-ciphertext\n";
    std::fs::write(&backup_path, backup_bytes).unwrap();

    // Seed account row and credential in state.sqlite + fake store.
    let db = SqliteStateStore::open(&state_dir.join("state.sqlite")).unwrap();
    let dev = db
        .ensure_device(&sync_manager::DeviceRecord {
            id: sync_core::DeviceId::new("device-rec-test"),
            name: "test".to_string(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
        })
        .unwrap();
    db.upsert_account(&AccountRecord {
        id: AccountId::new("rec1"),
        display_name: "Recovery Test".to_string(),
        kind: "drive".to_string(),
        remote_name: "household".to_string(),
        remote_root_folder_id: "0ARoot".to_string(),
        config_path: acct_dir.join("rclone.conf"),
        auth_status: "ready".to_string(),
        created_at: "2026-11-20T00:00:00Z".to_string(),
        updated_at: "2026-11-20T00:00:00Z".to_string(),
    })
    .unwrap();
    drop(db);

    let store = Arc::new(InMemorySecretStore::new());
    let cred_key = scoped_credential_key(dev.id.as_str(), "rec1").unwrap();
    store
        .create_secret(&cred_key, &SecretString::new("rec-passphrase"))
        .unwrap();

    let svc = service_with(&state_dir, store, Arc::new(FakeProcessRunner::new()), None);
    assert!(
        svc.startup_warnings()
            .iter()
            .any(|w| w.to_ascii_lowercase().contains("recovery needed") && w.contains("rec1")),
        "startup must report recovery needed for rec1, got: {:?}",
        svc.startup_warnings()
    );
    assert_eq!(
        std::fs::read(&backup_path).unwrap(),
        backup_bytes,
        "startup must leave backup file untouched"
    );

    let check_err = svc.check_account(&AccountId::new("rec1")).unwrap_err();
    assert!(
        matches!(check_err, AccountError::RecoveryNeeded { .. }),
        "check_account must report RecoveryNeeded, got: {check_err:?}"
    );
    assert!(
        check_err
            .cause()
            .to_ascii_lowercase()
            .contains("recovery needed"),
        "error cause must say recovery needed: {}",
        check_err.cause()
    );
    assert_eq!(
        std::fs::read(&backup_path).unwrap(),
        backup_bytes,
        "check_account must leave backup file untouched"
    );

    let reconnect_err = svc
        .reconnect_drive_account(&AccountId::new("rec1"), None)
        .unwrap_err();
    assert!(
        matches!(reconnect_err, AccountError::RecoveryNeeded { .. }),
        "reconnect must report RecoveryNeeded, got: {reconnect_err:?}"
    );
    assert_eq!(
        std::fs::read(&backup_path).unwrap(),
        backup_bytes,
        "reconnect must leave backup file untouched"
    );
}

/// Finding (#2): A credential created for THIS install with no SQLite account row
/// (e.g. crash after create_secret or after config publish before row upsert) is
/// reported at startup as stale and cleanly replaced when retrying the same ID,
/// without affecting another state dir's credential in the shared store.
#[cfg(unix)]
#[test]
fn interrupted_registration_after_credential_creation_is_reported_at_startup_and_recoverable_on_retry()
 {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let state_a = tmp.path().join("state-a");
    let state_b = tmp.path().join("state-b");
    let tree = tmp.path().join("tree");
    std::fs::create_dir_all(tree.join("Data")).unwrap();
    let src_cfg = tmp.path().join("source.conf");
    std::fs::write(&src_cfg, "[loc]\ntype = local\n").unwrap();

    let shared_store = Arc::new(InMemorySecretStore::new());
    let helper_dir = tmp.path().join("helpers");
    std::fs::create_dir_all(&helper_dir).unwrap();

    // First set up state_b with the same ID `crash-acct` to prove state_a recovery
    // never touches state_b's credential.
    let svc_b = service_with(
        &state_b,
        Arc::clone(&shared_store),
        Arc::new(RealProcessRunner),
        Some(fake_password_factory(
            Arc::clone(&shared_store),
            &helper_dir,
        )),
    );
    svc_b
        .import_account(ImportAccountRequest {
            id: AccountId::new("crash-acct"),
            display_name: "State B".to_string(),
            source_config_path: src_cfg.clone(),
            source_remote_name: "loc".to_string(),
            target_remote_name: "household".to_string(),
            root_folder_id: Some(tree.to_string_lossy().into_owned()),
            allow_global_default_source: false,
        })
        .unwrap();

    // Simulate crash in state_a: device row exists, accounts/crash-acct/ exists,
    // scoped credential exists in shared_store, orphaned rclone.conf exists,
    // but NO account row in state_a/state.sqlite.
    let db_a = SqliteStateStore::open(&state_a.join("state.sqlite")).unwrap();
    let dev_a = db_a
        .ensure_device(&sync_manager::DeviceRecord {
            id: sync_core::DeviceId::new("device-state-a-crash"),
            name: "state-a".to_string(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
        })
        .unwrap();
    drop(db_a);
    let acct_dir_a = state_a.join("accounts/crash-acct");
    std::fs::create_dir_all(&acct_dir_a).unwrap();
    std::fs::write(
        acct_dir_a.join("rclone.conf"),
        b"RCLONE_ENCRYPT_V0:\norphaned-pre-crash\n",
    )
    .unwrap();
    let stale_key = scoped_credential_key(dev_a.id.as_str(), "crash-acct").unwrap();
    shared_store
        .create_secret(
            &stale_key,
            &SecretString::new("stale-passphrase-before-crash"),
        )
        .unwrap();

    // Startup in state_a must report the stale credential.
    let svc_a = service_with(
        &state_a,
        Arc::clone(&shared_store),
        Arc::new(RealProcessRunner),
        Some(fake_password_factory(
            Arc::clone(&shared_store),
            &helper_dir,
        )),
    );
    assert!(
        svc_a
            .startup_warnings()
            .iter()
            .any(
                |w| w.to_ascii_lowercase().contains("stale credential") && w.contains("crash-acct")
            ),
        "startup must report stale credential for crash-acct, got: {:?}",
        svc_a.startup_warnings()
    );

    // Retry with the same ID must recover cleanly and replace the stale credential.
    let rec = svc_a
        .import_account(ImportAccountRequest {
            id: AccountId::new("crash-acct"),
            display_name: "State A Recovered".to_string(),
            source_config_path: src_cfg,
            source_remote_name: "loc".to_string(),
            target_remote_name: "household".to_string(),
            root_folder_id: Some(tree.to_string_lossy().into_owned()),
            allow_global_default_source: false,
        })
        .unwrap();
    assert_eq!(rec.id.as_str(), "crash-acct");
    assert_ne!(
        shared_store.get_secret(&stale_key).unwrap().expose(),
        "stale-passphrase-before-crash",
        "stale credential must be replaced with a fresh passphrase on retry"
    );

    // Both state_a AND state_b check cleanly!
    svc_a.check_account(&AccountId::new("crash-acct")).unwrap();
    svc_b.check_account(&AccountId::new("crash-acct")).unwrap();
}

/// Finding (#3): Drive import accepts ONLY unquoted allowlisted keys/values and
/// rejects starred_only, shared_with_me, team_drive, quoted keys/values, and
/// unknown options with actionable errors.
#[cfg(unix)]
#[test]
fn drive_import_rejects_disallowed_options_quoted_syntax_and_unknown_keys() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let helper_dir = tmp.path().join("helpers");
    std::fs::create_dir_all(&helper_dir).unwrap();
    let store = Arc::new(InMemorySecretStore::new());

    let cases: &[(&str, &str)] = &[
        (
            "starred",
            "[d]\ntype = drive\nroot_folder_id = 0ARoot\nstarred_only = true\n",
        ),
        (
            "shared",
            "[d]\ntype = drive\nroot_folder_id = 0ARoot\nshared_with_me = true\n",
        ),
        (
            "team",
            "[d]\ntype = drive\nroot_folder_id = 0ARoot\nteam_drive = 0ATeamDrive\n",
        ),
        (
            "quoted-key",
            "[d]\ntype = drive\nroot_folder_id = 0ARoot\n\"shared_with_me\" = true\n",
        ),
        (
            "quoted-type",
            "[d]\ntype = \"\"\"drive\"\"\"\nroot_folder_id = 0ARoot\n",
        ),
        (
            "quoted-val",
            "[d]\ntype = drive\nroot_folder_id = 0ARoot\nscope = \"drive\"\n",
        ),
        (
            "unknown-opt",
            "[d]\ntype = drive\nroot_folder_id = 0ARoot\nexport_formats = docx\n",
        ),
    ];

    let svc = service_with(
        &state_dir,
        Arc::clone(&store),
        Arc::new(RealProcessRunner),
        Some(fake_password_factory(Arc::clone(&store), &helper_dir)),
    );

    for (label, content) in cases {
        let src_cfg = tmp.path().join(format!("{label}.conf"));
        std::fs::write(&src_cfg, content).unwrap();
        let err = svc
            .import_account(ImportAccountRequest {
                id: AccountId::new(format!("bad-{label}")),
                display_name: label.to_string(),
                source_config_path: src_cfg,
                source_remote_name: "d".to_string(),
                target_remote_name: "household".to_string(),
                root_folder_id: Some("0ARoot".to_string()),
                allow_global_default_source: false,
            })
            .unwrap_err();
        assert!(
            matches!(err, AccountError::InvalidInput { .. }),
            "case `{label}` must be rejected with InvalidInput, got: {err:?}"
        );
        assert!(
            !state_dir
                .join(format!("accounts/bad-{label}/rclone.conf"))
                .exists(),
            "case `{label}` must not publish rclone.conf"
        );
        assert!(
            svc.get_account(&AccountId::new(format!("bad-{label}")))
                .unwrap()
                .is_none(),
            "case `{label}` must not record a DB row"
        );
    }
}

/// Finding (#4): SQLite uses `SQLITE_OPEN_NOFOLLOW` directly without raw pre-open,
/// refuses symlinks pointing to a REAL valid SQLite database without modifying its
/// bytes or permissions, queries `list_accounts` via `SqliteStateStore`, and does
/// not drop POSIX locks on an open connection when opening another store.
#[cfg(unix)]
#[test]
fn sqlite_nofollow_refuses_symlink_to_valid_db_without_modifying_bytes_or_mode_and_lists_via_store()
{
    let tmp = TempDir::new().unwrap();

    // 1. Create a REAL valid SQLite database outside state_dir with mode 0644.
    let outside_db = tmp.path().join("outside-valid.sqlite");
    {
        let conn = rusqlite::Connection::open(&outside_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sentinel (v TEXT); INSERT INTO sentinel VALUES ('intact');",
        )
        .unwrap();
    }
    std::fs::set_permissions(&outside_db, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before_bytes = std::fs::read(&outside_db).unwrap();

    let state_sym = tmp.path().join("state-sym");
    std::fs::create_dir_all(&state_sym).unwrap();
    std::os::unix::fs::symlink(&outside_db, state_sym.join("state.sqlite")).unwrap();

    let err = match SqliteStateStore::open(&state_sym.join("state.sqlite")) {
        Err(e) => e,
        Ok(_) => panic!("expected PreflightRejected on symlinked valid SQLite DB"),
    };
    assert!(
        matches!(err, ManagerError::PreflightRejected { .. }),
        "expected PreflightRejected on symlinked valid SQLite DB, got: {err:?}"
    );
    let svc_sym = service_with(
        &state_sym,
        Arc::new(InMemorySecretStore::new()),
        Arc::new(FakeProcessRunner::new()),
        None,
    );
    let list_err = svc_sym.list_accounts().unwrap_err();
    assert!(
        matches!(
            list_err,
            AccountError::Manager(ref m) if matches!(**m, ManagerError::PreflightRejected { .. })
        ),
        "expected PreflightRejected from list_accounts, got: {list_err:?}"
    );
    assert_eq!(
        std::fs::read(&outside_db).unwrap(),
        before_bytes,
        "outside valid SQLite DB bytes must stay untouched"
    );
    let after_mode = std::fs::symlink_metadata(&outside_db)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        after_mode, 0o644,
        "outside valid SQLite DB permissions must NOT be chmodded"
    );

    // 2. Fresh state_dir creates state.sqlite with 0600 and lists accounts via store.
    let state_ok = tmp.path().join("state-ok");
    let db_path = state_ok.join("state.sqlite");
    let store = SqliteStateStore::open(&db_path).unwrap();
    let db_mode = std::fs::symlink_metadata(&db_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(db_mode, 0o600);
    store
        .upsert_account(&AccountRecord {
            id: AccountId::new("acct-store"),
            display_name: "Via Store".to_string(),
            kind: "local".to_string(),
            remote_name: "household".to_string(),
            remote_root_folder_id: "/tmp/x".to_string(),
            config_path: state_ok.join("accounts/acct-store/rclone.conf"),
            auth_status: "ready".to_string(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
            updated_at: "2026-11-20T00:00:00Z".to_string(),
        })
        .unwrap();
    let listed = store.list_accounts().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id.as_str(), "acct-store");
}

/// Finding (#5): After add or reconnect, the account is marked `ready` ONLY after
/// a successful `lsjson --max-depth 1 --dirs-only` listing of the configured root
/// under the per-account lock. A retained old/revoked token keeps the account unready.
#[cfg(unix)]
#[test]
fn add_and_reconnect_stay_unready_when_retained_revoked_token_fails_root_listing() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    }

    struct RevokedTokenRunner {
        lsjson_attempt: AtomicUsize,
        recorded_argvs: Mutex<Vec<Vec<String>>>,
    }

    impl ProcessRunner for RevokedTokenRunner {
        fn spawn(
            &self,
            spec: &ProcessSpec,
            options: SpawnOptions,
        ) -> Result<RunHandle, ProcessError> {
            let argv: Vec<String> = spec
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            self.recorded_argvs.lock().unwrap().push(argv.clone());

            let cmd0 = argv.first().map(String::as_str);
            let cmd1 = argv.get(1).map(String::as_str);
            assert!(
                !(cmd0 == Some("config") && cmd1 == Some("dump")),
                "config dump must NEVER be invoked: {argv:?}"
            );

            if cmd0 == Some("config") && cmd1 == Some("reconnect") {
                let fake = FakeProcessRunner::new();
                fake.push_run(FakeRunScript {
                    lines: vec![OutputLine {
                        stream: StreamKind::Stderr,
                        text: "NOTICE: Go to http://127.0.0.1:53682/auth?state=revokedTest"
                            .to_string(),
                        truncated: false,
                    }],
                    ..Default::default()
                });
                fake.spawn(spec, options)
            } else if cmd0 == Some("config") && cmd1 == Some("redacted") {
                // Simulate an old/retained token still sitting in the config file.
                let fake = FakeProcessRunner::new();
                fake.push_run(FakeRunScript {
                    lines: vec![OutputLine {
                        stream: StreamKind::Stdout,
                        text: "[household]\ntype = drive\nclient_id = XXX\nclient_secret = XXX\nscope = drive\nroot_folder_id = XXX\ntoken = XXX\n".to_string(),
                        truncated: false,
                    }],
                    ..Default::default()
                });
                fake.spawn(spec, options)
            } else if cmd0 == Some("lsjson") {
                let n = self.lsjson_attempt.fetch_add(1, Ordering::SeqCst);
                let fake = FakeProcessRunner::new();
                if n < 2 {
                    // Attempts 0 (add) and 1 (first reconnect): retained token is revoked!
                    fake.push_run(FakeRunScript {
                        lines: vec![OutputLine {
                            stream: StreamKind::Stderr,
                            text: "Failed to lsjson: couldn't list directory: oauth2: \"invalid_grant\" \"Token has been expired or revoked.\"".to_string(),
                            truncated: false,
                        }],
                        exit_status: ExitStatusInfo {
                            code: Some(1),
                            signal: None,
                        },
                        ..Default::default()
                    });
                } else {
                    // Attempt 2 (second reconnect): fresh token works!
                    fake.push_run(FakeRunScript {
                        lines: vec![OutputLine {
                            stream: StreamKind::Stdout,
                            text: r#"[{"Path":"Data","Name":"Data","ID":"id-data","IsDir":true}]"#
                                .to_string(),
                            truncated: false,
                        }],
                        ..Default::default()
                    });
                }
                fake.spawn(spec, options)
            } else {
                RealProcessRunner.spawn(spec, options)
            }
        }
    }

    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let helper_dir = tmp.path().join("helpers");
    std::fs::create_dir_all(&helper_dir).unwrap();
    let store = Arc::new(InMemorySecretStore::new());
    let runner = Arc::new(RevokedTokenRunner {
        lsjson_attempt: AtomicUsize::new(0),
        recorded_argvs: Mutex::new(Vec::new()),
    });

    let svc = service_with(
        &state_dir,
        Arc::clone(&store),
        Arc::clone(&runner) as Arc<dyn ProcessRunner>,
        Some(fake_password_factory(Arc::clone(&store), &helper_dir)),
    );

    // 1. add_drive_account: reconnect exits 0, config redacted shows token=XXX,
    //    but lsjson fails with invalid_grant -> MUST stay unready!
    let err1 = svc
        .add_drive_account(
            AddDriveAccountRequest {
                id: AccountId::new("rev-1"),
                display_name: "Revoked Test".to_string(),
                remote_name: "household".to_string(),
                root_folder_id: "0ARoot".to_string(),
                client_id: "cid".to_string(),
                client_secret: SecretString::new("csec"),
                scope: "drive".to_string(),
                run_oauth_flow: true,
            },
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err1, AccountError::AuthenticationRequired { .. }),
        "expected AuthenticationRequired when root listing fails, got: {err1:?}"
    );
    assert_eq!(
        svc.get_account(&AccountId::new("rev-1"))
            .unwrap()
            .unwrap()
            .auth_status,
        "authentication_required"
    );

    // 2. reconnect_drive_account with retained revoked token -> MUST stay unready!
    let err2 = svc
        .reconnect_drive_account(&AccountId::new("rev-1"), None)
        .unwrap_err();
    assert!(
        matches!(err2, AccountError::AuthenticationRequired { .. }),
        "reconnect with retained revoked token must stay AuthenticationRequired, got: {err2:?}"
    );
    assert_eq!(
        svc.get_account(&AccountId::new("rev-1"))
            .unwrap()
            .unwrap()
            .auth_status,
        "authentication_required"
    );

    // 3. reconnect_drive_account once root listing succeeds -> NOW marks ready!
    let ok = svc
        .reconnect_drive_account(&AccountId::new("rev-1"), None)
        .unwrap();
    assert_eq!(ok.account.auth_status, "ready");

    // Confirm lsjson used `--max-depth 1 --dirs-only` on the remote root `household:`.
    let argvs = runner.recorded_argvs.lock().unwrap();
    let lsjson_calls: Vec<&Vec<String>> = argvs
        .iter()
        .filter(|a| a.first().map(String::as_str) == Some("lsjson"))
        .collect();
    assert_eq!(lsjson_calls.len(), 3);
    for call in lsjson_calls {
        assert!(
            call.windows(2).any(|w| w == ["--max-depth", "1"]),
            "expected --max-depth 1 in lsjson call: {call:?}"
        );
        assert!(call.iter().any(|a| a == "--dirs-only"));
        assert!(call.iter().any(|a| a == "household:"));
    }
}

/// Finding (#6): Config commands NEVER run `config dump` and NEVER include child
/// stdout in errors on nonzero exit or signal termination (no credential canaries
/// leak into errors).
#[cfg(unix)]
#[test]
fn config_commands_never_run_dump_and_never_leak_stdout_canaries_on_nonzero_or_signal() {
    if pinned_binary_path().is_none() {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let cfg_path = tmp.path().join("accounts/c1/rclone.conf");
    std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    std::fs::write(&cfg_path, "RCLONE_ENCRYPT_V0:\nfake-encrypted\n").unwrap();
    std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let helper_bin = tmp.path().join("fake-helper");
    std::fs::write(&helper_bin, "#!/bin/sh\necho pw\n").unwrap();
    std::fs::set_permissions(&helper_bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let pw_cmd = PasswordCommand::new(helper_bin, vec![]).unwrap();
    let engine = rclone_adapter::qualify_binary(&pinned_binary_path().unwrap()).unwrap();

    let canary_secret = "GOCSPX-CANARY-CLIENT-SECRET-999";
    let canary_token = "ya29.CANARY-ACCESS-TOKEN-888";

    for (label, exit_status) in [
        (
            "nonzero",
            ExitStatusInfo {
                code: Some(1),
                signal: None,
            },
        ),
        (
            "signaled",
            ExitStatusInfo {
                code: None,
                signal: Some(9),
            },
        ),
    ] {
        let fake = FakeProcessRunner::new();
        // 1. config encryption check succeeds
        fake.push_run(FakeRunScript::default());
        // 2. config redacted prints credential canaries to stdout with EMPTY stderr and fails
        fake.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: format!(
                    "[household]\nclient_secret = {canary_secret}\ntoken = {canary_token}\n"
                ),
                truncated: false,
            }],
            exit_status,
            ..Default::default()
        });

        let err = rclone_adapter::encrypted_remote_has_token(
            &engine,
            &fake,
            &cfg_path,
            "household",
            &pw_cmd,
        )
        .unwrap_err();
        let rendered = err.to_string();
        assert!(
            matches!(err, ConfigError::RcloneCommandFailed { .. }),
            "case `{label}` expected RcloneCommandFailed, got: {err:?}"
        );
        assert!(
            !rendered.contains(canary_secret) && !rendered.contains(canary_token),
            "case `{label}` leaked stdout credential canary into error: {rendered}"
        );

        let recorded = fake.spawned_specs();
        for spec in &recorded {
            let args: Vec<String> = spec
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                !(args.first().map(String::as_str) == Some("config")
                    && args.get(1).map(String::as_str) == Some("dump")),
                "config dump must never be invoked: {args:?}"
            );
        }
    }
}
