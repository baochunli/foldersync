use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};

use rclone_adapter::{ExitStatusInfo, OutputLine, StreamKind};
use sync_core::{Execution, Intent, MappingId, Readiness};
use sync_manager::{
    AddMappingCommand, FakeBisyncLauncher, FakeLaunchScript, InitializeCommand, ManagerConfig,
    ManagerError, SqliteStateStore, StateLock, SyncManager, resolve_state_dir,
};
use tempfile::TempDir;

struct FixtureDirs {
    _tmp: TempDir,
    state_dir: PathBuf,
    local_dir: PathBuf,
    remote_dir: PathBuf,
    local_recovery: PathBuf,
    remote_recovery: PathBuf,
}

impl FixtureDirs {
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
            _tmp: tmp,
            state_dir,
            local_dir,
            remote_dir,
            local_recovery,
            remote_recovery,
        }
    }

    fn config_with_fake(&self, launcher: Arc<FakeBisyncLauncher>) -> ManagerConfig {
        let mut cfg = ManagerConfig::with_state_dir(&self.state_dir);
        cfg.launcher = launcher;
        cfg
    }
}

#[test]
fn state_lock_is_exclusive_and_uses_private_permissions() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");

    // Explicit override always wins (canonicalized) and never touches real Application Support.
    let resolved = resolve_state_dir(Some(&state_dir)).unwrap();
    assert_eq!(
        resolved,
        fs::canonicalize(tmp.path()).unwrap().join("state")
    );

    let lock1 = StateLock::acquire(&state_dir).expect("first lock should succeed");
    let dir_mode = fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "state dir must use 0700 private permissions"
    );
    let lock_mode = fs::metadata(lock1.lock_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        lock_mode, 0o600,
        "lock file must use 0600 private permissions"
    );

    let err = StateLock::acquire(&state_dir).expect_err("second lock must fail");
    assert!(
        matches!(err, ManagerError::StateLocked { .. }),
        "expected StateLocked, got {err:?}"
    );

    drop(lock1);
    let _lock2 = StateLock::acquire(&state_dir).expect("lock should succeed after release");
}

#[test]
fn migrations_from_empty_db_create_all_section6_tables_and_reject_downgrade() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("state.sqlite");

    let store = SqliteStateStore::open(&db_path).expect("initial open should run migrations");
    let migrations = store.applied_migrations().unwrap();
    assert!(!migrations.is_empty());
    assert_eq!(migrations[0].0, 1);

    let mut tables = store.table_names().unwrap();
    tables.sort();
    for expected in [
        "accounts",
        "devices",
        "mapping_generations",
        "mappings",
        "recovery_cases",
        "runs",
        "schema_migrations",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing expected §6 table `{expected}` in {tables:?}"
        );
    }
    // No per-file sync table.
    assert!(!tables.iter().any(|t| t.contains("file")));

    let db_mode = fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(db_mode, 0o600, "state.sqlite must use 0600 permissions");

    drop(store);

    // Reopening is idempotent.
    let store2 = SqliteStateStore::open(&db_path).expect("reopen should be idempotent");
    assert_eq!(store2.applied_migrations().unwrap(), migrations);
}

#[test]
fn pause_and_resume_intent_survive_restart() {
    let fx = FixtureDirs::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    {
        let mgr = SyncManager::open(fx.config_with_fake(Arc::clone(&launcher))).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        let cmd = AddMappingCommand::local_fixture(
            "research",
            "local-1",
            "Data/Research",
            &fx.local_dir,
            &fx.remote_dir,
            &fx.local_recovery,
            &fx.remote_recovery,
        )
        .unwrap();
        mgr.add_mapping(cmd).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

        let ack = mgr.pause("research").unwrap();
        let m = &ack.snapshot.mappings[0];
        assert_eq!(m.intent, Intent::Paused);
        assert_eq!(m.readiness, Readiness::Ready);
        assert!(!m.scheduling_enabled);

        // RunNow must be rejected while paused.
        assert!(mgr.run_now("research").is_err());
        mgr.shutdown().unwrap();
    }

    // Reopen fresh manager and confirm pause intent survived restart.
    {
        let mgr = SyncManager::open(fx.config_with_fake(Arc::clone(&launcher))).unwrap();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings.len(), 1);
        assert_eq!(snap.mappings[0].intent, Intent::Paused);
        assert_eq!(snap.mappings[0].readiness, Readiness::Ready);
        assert!(mgr.run_now("research").is_err());

        // Resume persists Intent::Enabled across restart without spawning (scheduling disabled).
        let ack = mgr.resume("research").unwrap();
        assert_eq!(ack.snapshot.mappings[0].intent, Intent::Enabled);
        assert_eq!(ack.snapshot.mappings[0].execution, Execution::Idle);
        mgr.shutdown().unwrap();
    }

    {
        let mgr = SyncManager::open(fx.config_with_fake(Arc::clone(&launcher))).unwrap();
        assert_eq!(mgr.snapshot().mappings[0].intent, Intent::Enabled);
        mgr.shutdown().unwrap();
    }
}

#[test]
fn recovery_hold_survives_restart_and_blocks_run_now() {
    let fx = FixtureDirs::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    // 1. Successful initialization
    launcher.push_script(FakeLaunchScript::default());
    // 2. Normal run aborts on deletion guard -> latches RecoveryRequired
    launcher.push_script(FakeLaunchScript {
        lines: vec![OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"error","msg":"too many deletes (>10%, 5 of 10) on Path1 \"/tmp/p1/\". Run with --force if desired.","object":"Safety abort","source":"bisync/deltas.go:572"}"#.to_string(),
            truncated: false,
        }],
        exit_status: ExitStatusInfo {
            code: Some(1),
            signal: None,
        },
        ..Default::default()
    });

    {
        let mgr = SyncManager::open(fx.config_with_fake(Arc::clone(&launcher))).unwrap();
        mgr.register_local_fixture_account("local-1", "Local Fixture")
            .unwrap();
        let cmd = AddMappingCommand::local_fixture(
            "research",
            "local-1",
            "Data/Research",
            &fx.local_dir,
            &fx.remote_dir,
            &fx.local_recovery,
            &fx.remote_recovery,
        )
        .unwrap();
        mgr.add_mapping(cmd).unwrap();
        mgr.initialize(InitializeCommand {
            mapping_id: MappingId::new("research"),
            expected_direction: None,
        })
        .unwrap();
        mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

        mgr.run_now("research").unwrap();
        let snap = mgr.wait_until_idle(Duration::from_secs(5)).unwrap();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].counters.open_recovery_cases, 1);
        assert_eq!(snap.mappings[0].status_text, "Recovery required");
        assert!(mgr.run_now("research").is_err());
        mgr.shutdown().unwrap();
    }

    // Reopen and confirm recovery hold and open recovery case survive restart.
    {
        let mgr = SyncManager::open(fx.config_with_fake(Arc::clone(&launcher))).unwrap();
        let snap = mgr.snapshot();
        assert_eq!(snap.mappings[0].readiness, Readiness::RecoveryRequired);
        assert_eq!(snap.mappings[0].counters.open_recovery_cases, 1);
        assert!(mgr.run_now("research").is_err());

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
fn add_mapping_rejects_overlapping_local_roots_and_missing_dirs() {
    let fx = FixtureDirs::new();
    let launcher = Arc::new(FakeBisyncLauncher::new());
    let mgr = SyncManager::open(fx.config_with_fake(launcher)).unwrap();
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();

    // Missing local dir must be rejected and never auto-created.
    let missing = fx._tmp.path().join("does_not_exist");
    let err = AddMappingCommand::local_fixture(
        "missing",
        "local-1",
        "Data/Missing",
        &missing,
        &fx.remote_dir,
        &fx.local_recovery,
        &fx.remote_recovery,
    )
    .unwrap_err();
    assert!(!missing.exists());
    assert!(!err.cause().is_empty());
    assert!(!err.next_step().is_empty());

    // Add first valid mapping.
    let cmd1 = AddMappingCommand::local_fixture(
        "research",
        "local-1",
        "Data/Research",
        &fx.local_dir,
        &fx.remote_dir,
        &fx.local_recovery,
        &fx.remote_recovery,
    )
    .unwrap();
    mgr.add_mapping(cmd1).unwrap();

    // Overlapping nested local dir must be rejected by sync-core validation.
    let nested = fx.local_dir.join("sub");
    fs::create_dir_all(&nested).unwrap();
    let remote2 = fx._tmp.path().join("remote_sub");
    fs::create_dir_all(&remote2).unwrap();
    let cmd2 = AddMappingCommand::local_fixture(
        "sub",
        "local-1",
        "Data/Other",
        &nested,
        &remote2,
        &fx.local_recovery,
        &fx.remote_recovery,
    )
    .unwrap();
    let err = mgr.add_mapping(cmd2).unwrap_err();
    assert!(
        matches!(
            err,
            ManagerError::Validation(_) | ManagerError::ValidationList(_)
        ),
        "expected validation error for overlapping local root, got {err:?}"
    );
    mgr.shutdown().unwrap();
}
