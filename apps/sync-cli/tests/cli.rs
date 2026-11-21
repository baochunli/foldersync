use std::{fs, process::Command, thread, time::Duration};

use rclone_adapter::pinned_binary_path;
use sync_manager::StateLock;
use tempfile::TempDir;

#[test]
fn prints_version() {
    for args in [&[][..], &["version"][..], &["--version"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_sync-cli"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            concat!("FolderSync ", env!("CARGO_PKG_VERSION"), "\n")
        );
    }
}

#[test]
fn prints_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_sync-cli"))
        .arg("--help")
        .output()
        .unwrap();
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
    let output = Command::new(env!("CARGO_BIN_EXE_sync-cli"))
        .arg("unknown")
        .output()
        .unwrap();
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

    let bin = env!("CARGO_BIN_EXE_sync-cli");

    // 1. add-mapping
    let out = Command::new(bin)
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
    let out = Command::new(bin)
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
    let out = Command::new(bin)
        .args(["pause", "docs", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = Command::new(bin)
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
    let out = Command::new(bin)
        .args(["resume", "docs", "--state-dir", state_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = Command::new(bin)
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
    let out = Command::new(bin)
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
    let bin = env!("CARGO_BIN_EXE_sync-cli");

    let add = Command::new(bin)
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

    let init = Command::new(bin)
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

    let run = Command::new(bin)
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
    let bin = env!("CARGO_BIN_EXE_sync-cli");

    // Missing --state-dir must fail with usage error (exit 2) even for `status`.
    let out = Command::new(bin)
        .args(["status"])
        .env("HOME", tmp.path())
        .env_remove("FOLDERSYNC_STATE_DIR")
        .output()
        .unwrap();
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
    let out_empty = Command::new(bin)
        .args(["status", "--state-dir", ""])
        .env("HOME", tmp.path())
        .env_remove("FOLDERSYNC_STATE_DIR")
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

    let bin = env!("CARGO_BIN_EXE_sync-cli");
    let add = Command::new(bin)
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

    let init = Command::new(bin)
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
