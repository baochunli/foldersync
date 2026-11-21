use std::{fs, process::Command, thread, time::Duration};

use rclone_adapter::pinned_binary_path;
use sync_manager::StateLock;
use tempfile::TempDir;

fn cli_cmd(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sync-cli"));
    cmd.env("HOME", home)
        .env("FOLDERSYNC_STATE_DIR", home.join("environment-state"));
    cmd
}

#[test]
fn scheduling_pause_and_resume_persist_without_using_environment_state() {
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state");
    let cli = |args: &[&str]| {
        cli_cmd(tmp.path())
            .arg("--state-dir")
            .arg(&state)
            .args(args)
            .output()
            .unwrap()
    };
    for (action, paused) in [("pause", true), ("resume", false)] {
        let result = cli(&["scheduling", action]);
        assert!(result.status.success(), "{:?}", result);
        let status = cli(&["status", "--json"]);
        let snapshot: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(snapshot["scheduling_paused"], paused);
    }
    assert!(!tmp.path().join("environment-state").exists());
}

#[test]
fn drive_commands_require_explicit_state_and_reject_mixed_endpoints() {
    let tmp = TempDir::new().unwrap();
    for args in [
        &["collections", "list", "--account", "drive"][..],
        &[
            "collections",
            "size",
            "--account",
            "drive",
            "--folder-id",
            "folder",
        ],
        &[
            "add-mapping",
            "--account",
            "drive",
            "--folder-id",
            "folder",
            "--local",
            "local",
            "--local-recovery",
            "recovery",
        ],
        &["scheduling", "pause"],
    ] {
        let output = cli_cmd(tmp.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("required non-empty --state-dir"));
    }
    let state = tmp.path().join("state");
    let output = cli_cmd(tmp.path())
        .args([
            "add-mapping",
            "--account",
            "drive",
            "--folder-id",
            "folder",
            "--local",
            "local",
            "--local-recovery",
            "recovery",
            "--remote-dir",
            "fixture",
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("local fixture"));
    assert!(!state.exists());
    assert!(!tmp.path().join("environment-state").exists());
}

#[test]
fn prints_version() {
    let tmp = TempDir::new().unwrap();
    for args in [&[][..], &["version"][..], &["--version"][..]] {
        let output = cli_cmd(tmp.path()).args(args).output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            concat!("FolderSync ", env!("CARGO_PKG_VERSION"), "\n")
        );
    }
}

#[test]
fn prints_help() {
    let tmp = TempDir::new().unwrap();
    let output = cli_cmd(tmp.path()).arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Usage: sync-cli"));
    assert!(help.contains("version"));
    assert!(help.contains("status"));
    assert!(help.contains("add-mapping"));
    assert!(help.contains("init"));
    assert!(help.contains("run-once"));
    assert!(help.contains("pause"));
    assert!(help.contains("resume"));
    assert!(help.contains("credential-helper"));
    assert!(help.contains("account add-drive"));
    assert!(help.contains("account reconnect"));
    assert!(help.contains("account import"));
    assert!(help.contains("account list"));
    assert!(help.contains("account check"));
    assert!(help.contains("remote resolve"));
}

#[test]
fn rejects_unknown_arguments() {
    let tmp = TempDir::new().unwrap();
    let output = cli_cmd(tmp.path()).arg("unknown").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("--help"));
}

#[test]
fn cli_add_mapping_status_pause_resume_and_lock_refusal() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_docs");
    let remote_dir = tmp.path().join("remote_docs");
    let local_recovery = tmp.path().join("local_recovery");
    let remote_recovery = tmp.path().join("remote_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();
    fs::create_dir_all(&remote_recovery).unwrap();

    // 1. add-mapping
    let out = cli_cmd(tmp.path())
        .args([
            "add-mapping",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--id",
            "docs",
            "--local-dir",
            local_dir.to_str().unwrap(),
            "--remote-dir",
            remote_dir.to_str().unwrap(),
            "--recovery-dir",
            local_recovery.to_str().unwrap(),
            "--remote-recovery-dir",
            remote_recovery.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "add-mapping failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. status --json
    let out = cli_cmd(tmp.path())
        .args([
            "status",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(snap["mappings"][0]["id"], "docs");
    assert_eq!(snap["mappings"][0]["intent"], "Enabled");

    // 3. pause
    let out = cli_cmd(tmp.path())
        .args(["pause", "docs", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = cli_cmd(tmp.path())
        .args([
            "status",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    let snap: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(snap["mappings"][0]["intent"], "Paused");

    // 4. resume
    let out = cli_cmd(tmp.path())
        .args(["resume", "docs", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = cli_cmd(tmp.path())
        .args([
            "status",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    let snap: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(snap["mappings"][0]["intent"], "Enabled");

    // 5. Refuse to run while another instance holds the exclusive lock!
    let _lock = StateLock::acquire(&state_dir).unwrap();
    let out = cli_cmd(tmp.path())
        .args(["status", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "sync-cli must refuse to run while state lock is held"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("lock") || stderr.contains("another"),
        "expected clear lock refusal message, got: {stderr}"
    );
}

#[test]
fn cli_init_and_run_once_end_to_end_with_pinned_rclone() {
    if pinned_binary_path().is_none() {
        eprintln!(
            "Skipping cli_init_and_run_once_end_to_end_with_pinned_rclone: pinned rclone binary is absent"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_docs");
    let remote_dir = tmp.path().join("remote_docs");
    let local_recovery = tmp.path().join("local_recovery");
    let remote_recovery = tmp.path().join("remote_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();
    fs::create_dir_all(&remote_recovery).unwrap();

    fs::write(local_dir.join("hello.txt"), b"cli-v1").unwrap();

    let add = cli_cmd(tmp.path())
        .args([
            "add-mapping",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--id",
            "docs",
            "--local-dir",
            local_dir.to_str().unwrap(),
            "--remote-dir",
            remote_dir.to_str().unwrap(),
            "--recovery-dir",
            local_recovery.to_str().unwrap(),
            "--remote-recovery-dir",
            remote_recovery.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );

    let init = cli_cmd(tmp.path())
        .args(["init", "docs", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert_eq!(fs::read(remote_dir.join("hello.txt")).unwrap(), b"cli-v1");

    thread::sleep(Duration::from_millis(1100));
    fs::write(local_dir.join("hello.txt"), b"cli-v2-edited").unwrap();

    let run = cli_cmd(tmp.path())
        .args([
            "run-once",
            "docs",
            "--state-dir",
            state_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "run-once failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        fs::read(remote_dir.join("hello.txt")).unwrap(),
        b"cli-v2-edited"
    );
}

#[test]
fn cli_requires_non_empty_state_dir_and_never_uses_default() {
    let tmp = TempDir::new().unwrap();

    // Missing --state-dir must fail with usage error (exit 2) even for `status`.
    let out = cli_cmd(tmp.path()).args(["status"]).output().unwrap();
    assert!(
        !out.status.success(),
        "status without --state-dir must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--state-dir"),
        "stderr should explain --state-dir is required: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Empty --state-dir "" must also fail.
    let out_empty = cli_cmd(tmp.path())
        .args(["status", "--state-dir", ""])
        .output()
        .unwrap();
    assert!(
        !out_empty.status.success(),
        "status with empty --state-dir must be rejected"
    );
}

#[test]
fn cli_init_failure_exits_nonzero_without_printing_initialized() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_docs");
    let remote_dir = tmp.path().join("remote_docs");
    let local_recovery = tmp.path().join("local_recovery");
    let remote_recovery = tmp.path().join("remote_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();
    fs::create_dir_all(&remote_recovery).unwrap();

    // Populate both sides so init fails with BothSidesPopulated.
    fs::write(local_dir.join("a.txt"), b"local").unwrap();
    fs::write(remote_dir.join("b.txt"), b"remote").unwrap();

    let add = cli_cmd(tmp.path())
        .args([
            "add-mapping",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--id",
            "docs",
            "--local-dir",
            local_dir.to_str().unwrap(),
            "--remote-dir",
            remote_dir.to_str().unwrap(),
            "--recovery-dir",
            local_recovery.to_str().unwrap(),
            "--remote-recovery-dir",
            remote_recovery.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(add.status.success());

    let init = cli_cmd(tmp.path())
        .args(["init", "docs", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!init.status.success());
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(
        !stdout.contains("Initialized"),
        "failed init must not print 'Initialized', got stdout: {stdout:?}"
    );
}

#[test]
fn cli_add_mapping_without_remote_recovery_dir_uses_sibling_and_succeeds() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_docs");
    let remote_dir = tmp.path().join("remote_docs");
    let local_recovery = tmp.path().join("local_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();

    let add = cli_cmd(tmp.path())
        .args([
            "add-mapping",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--id",
            "docs",
            "--local-dir",
            local_dir.to_str().unwrap(),
            "--remote-dir",
            remote_dir.to_str().unwrap(),
            "--recovery-dir",
            local_recovery.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add-mapping without --remote-recovery-dir must succeed with sibling default: {}",
        String::from_utf8_lossy(&add.stderr)
    );
}

#[test]
fn cli_run_once_fails_when_terminal_db_commit_fails_and_never_reuses_old_success() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_docs");
    let remote_dir = tmp.path().join("remote_docs");
    let local_recovery = tmp.path().join("local_recovery");
    let remote_recovery = tmp.path().join("remote_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();
    fs::create_dir_all(&remote_recovery).unwrap();

    // Initialize cleanly using FakeBisyncLauncher so the mapping is Ready with a prior Success run.
    {
        let launcher = std::sync::Arc::new(sync_manager::FakeBisyncLauncher::new());
        launcher.push_script(sync_manager::FakeLaunchScript::default());
        let mut cfg = sync_manager::ManagerConfig::with_state_dir(&state_dir);
        cfg.launcher = launcher;
        let mgr = sync_manager::SyncManager::open(cfg).unwrap();
        mgr.register_local_fixture_account("local-fixture", "Local Fixture")
            .unwrap();
        mgr.add_mapping(
            sync_manager::AddMappingCommand::local_fixture(
                "docs",
                "local-fixture",
                "Data/docs",
                &local_dir,
                &remote_dir,
                &local_recovery,
                &remote_recovery,
            )
            .unwrap(),
        )
        .unwrap();
        mgr.initialize(sync_manager::InitializeCommand {
            mapping_id: sync_core::MappingId::new("docs"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(120)).unwrap();
        mgr.shutdown().unwrap();
    }

    // Inject a failing terminal UPDATE trigger via sqlite3 (when available) AND
    // point FOLDERSYNC_RCLONE to a non-existent binary so `sync-cli run-once`
    // MUST exit non-zero and never mistake the prior Success run for a new success!
    let trigger_sql = "CREATE TRIGGER fail_cli_terminal BEFORE UPDATE ON runs \
         WHEN NEW.status IN ('completed', 'failed', 'interrupted') \
         BEGIN SELECT RAISE(ABORT, 'injected cli terminal DB failure'); END;";
    if let Ok(status) = Command::new("sqlite3")
        .arg(state_dir.join("state.sqlite"))
        .arg(trigger_sql)
        .status()
    {
        assert!(status.success(), "sqlite3 trigger setup failed");
    }

    let out = cli_cmd(tmp.path())
        .env("FOLDERSYNC_RCLONE", tmp.path().join("missing-rclone"))
        .args([
            "run-once",
            "docs",
            "--state-dir",
            state_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "run-once must fail when terminal DB commit fails (stdout={}, stderr={})",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Run completed"),
        "failed run-once must not print 'Run completed'"
    );
}

#[test]
fn account_and_remote_commands_require_state_dir_and_list_empty_without_touching_keychain() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");

    // Every account/remote subcommand must reject missing --state-dir.
    for args in [
        &["account", "list"][..],
        &["account", "reconnect", "--account", "a1"][..],
        &["account", "check", "--account", "a1"][..],
        &[
            "account",
            "import",
            "--from",
            "/tmp/src.conf",
            "--remote",
            "demo",
        ][..],
        &[
            "account",
            "add-drive",
            "--id",
            "d1",
            "--root-folder-id",
            "0ARoot",
            "--client-id",
            "cid",
        ][..],
        &["remote", "resolve", "Data/Research", "--account", "a1"][..],
    ] {
        let out = cli_cmd(tmp.path()).args(args).output().unwrap();
        assert!(
            !out.status.success(),
            "expected failure without --state-dir for {args:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--state-dir"),
            "stderr for {args:?} should mention --state-dir: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // `account list --state-dir <dir> --json` succeeds on a clean state dir without touching Keychain.
    let out = cli_cmd(tmp.path())
        .args([
            "account",
            "list",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "account list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed, serde_json::json!([]));

    // `account import` must refuse $HOME/.config/rclone/rclone.conf without --allow-global-config
    // (before touching Keychain or creating account files).
    let global_cfg = tmp.path().join(".config/rclone/rclone.conf");
    fs::create_dir_all(global_cfg.parent().unwrap()).unwrap();
    fs::write(&global_cfg, "[demo]\ntype = local\n").unwrap();
    let refused = cli_cmd(tmp.path())
        .args([
            "account",
            "import",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--from",
            global_cfg.to_str().unwrap(),
            "--remote",
            "demo",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("global rclone configuration") || stderr.contains("--allow-global-config"),
        "expected global-config refusal message, got: {stderr}"
    );
    assert!(!state_dir.join("accounts/demo").exists());
}

#[test]
fn add_mapping_refuses_to_overwrite_existing_non_fixture_local_fixture_account() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let local_dir = tmp.path().join("local_docs");
    let remote_dir = tmp.path().join("remote_docs");
    let recovery_dir = tmp.path().join("local_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&recovery_dir).unwrap();

    // Seed a non-fixture `drive` account claiming the reserved `local-fixture` ID.
    let acct_dir = state_dir.join("accounts/local-fixture");
    fs::create_dir_all(&acct_dir).unwrap();
    let enc_bytes = b"# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nProtected==\n";
    fs::write(acct_dir.join("rclone.conf"), enc_bytes).unwrap();
    let store = sync_manager::SqliteStateStore::open(&state_dir.join("state.sqlite")).unwrap();
    store
        .upsert_account(&sync_manager::AccountRecord {
            id: sync_core::AccountId::new("local-fixture"),
            display_name: "Existing Drive".to_string(),
            kind: "drive".to_string(),
            remote_name: "household".to_string(),
            remote_root_folder_id: "0ARoot".to_string(),
            config_path: acct_dir.join("rclone.conf"),
            auth_status: "ready".to_string(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
            updated_at: "2026-11-20T00:00:00Z".to_string(),
        })
        .unwrap();
    drop(store);

    let out = cli_cmd(tmp.path())
        .args([
            "add-mapping",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--id",
            "docs",
            "--local-dir",
            local_dir.to_str().unwrap(),
            "--remote-dir",
            remote_dir.to_str().unwrap(),
            "--recovery-dir",
            recovery_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "add-mapping must refuse to overwrite a non-fixture `local-fixture` account"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("non-fixture"),
        "stderr should explain refusal: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Confirm existing account row and encrypted config are untouched.
    let store2 = sync_manager::SqliteStateStore::open(&state_dir.join("state.sqlite")).unwrap();
    let rec = store2
        .get_account(&sync_core::AccountId::new("local-fixture"))
        .unwrap()
        .unwrap();
    assert_eq!(rec.kind, "drive");
    assert_eq!(fs::read(acct_dir.join("rclone.conf")).unwrap(), enc_bytes);
}
