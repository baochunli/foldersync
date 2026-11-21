use std::{fs, process::Command, thread, time::Duration};

use rclone_adapter::pinned_binary_path;
use sync_manager::StateLock;
use tempfile::TempDir;

fn cli_cmd(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sync-cli"));
    cmd.env("HOME", home).env_remove("FOLDERSYNC_STATE_DIR");
    cmd
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
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
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
