use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use chrono::{TimeZone, Utc};
use sync_manager::{
    AddMappingCommand, FINDER_SNAPSHOT_VERSION, FakeBisyncLauncher, FakeLaunchScript,
    FinderMappingState, FinderStatusSnapshot, FixedClock, InitializeCommand, ManagerConfig,
    SyncManager,
};
use tempfile::TempDir;

#[test]
fn finder_status_json_is_valid_v1_atomic_and_updated_on_state_changes() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let finder_dir = tmp.path().join("custom_finder");
    let local_dir = tmp.path().join("local_research");
    let remote_dir = tmp.path().join("remote_research");
    let local_recovery = tmp.path().join("local_recovery");
    let remote_recovery = tmp.path().join("remote_recovery");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_dir).unwrap();
    fs::create_dir_all(&local_recovery).unwrap();
    fs::create_dir_all(&remote_recovery).unwrap();

    let clock = Arc::new(FixedClock::new(
        Utc.with_ymd_and_hms(2026, 11, 20, 18, 45, 0).unwrap(),
    ));
    let launcher = Arc::new(FakeBisyncLauncher::new());
    launcher.push_script(FakeLaunchScript::default());

    let mut cfg = ManagerConfig::with_state_dir(&state_dir);
    cfg.finder_status_dir = Some(finder_dir.clone());
    cfg.clock = clock;
    cfg.launcher = launcher;

    let mgr = SyncManager::open(cfg).unwrap();
    let status_path = finder_dir.join("status.json");
    assert!(
        status_path.is_file(),
        "status.json must be written on startup"
    );

    // Check atomic write left no temp files and used private permissions.
    let entries: Vec<String> = fs::read_dir(&finder_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["status.json".to_string()]);
    let mode = fs::metadata(&status_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let initial: FinderStatusSnapshot =
        serde_json::from_str(&fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(initial.version, FINDER_SNAPSHOT_VERSION);
    assert_eq!(initial.generated_at, "2026-11-20T18:45:00Z");
    assert_eq!(initial.stale_after_seconds, 300);
    assert!(initial.mappings.is_empty());

    // Add draft mapping -> Finder state is "attention" (NeedsInitialization).
    mgr.register_local_fixture_account("local-1", "Local Fixture")
        .unwrap();
    let cmd = AddMappingCommand::local_fixture(
        "research",
        "local-1",
        "Data/Research",
        &local_dir,
        &remote_dir,
        &local_recovery,
        &remote_recovery,
    )
    .unwrap();
    mgr.add_mapping(cmd).unwrap();

    let after_add: FinderStatusSnapshot =
        serde_json::from_str(&fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(after_add.mappings.len(), 1);
    assert_eq!(after_add.mappings[0].id, "research");
    assert_eq!(after_add.mappings[0].state, FinderMappingState::Attention);
    assert_eq!(
        after_add.mappings[0].root_path,
        fs::canonicalize(&local_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );

    // Initialize -> Finder state transitions to "synced".
    mgr.initialize(InitializeCommand {
        mapping_id: "research".into(),
        expected_direction: None,
    })
    .unwrap();
    mgr.wait_until_idle(Duration::from_secs(5)).unwrap();

    let after_init: FinderStatusSnapshot =
        serde_json::from_str(&fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(after_init.mappings[0].state, FinderMappingState::Synced);

    // Pause -> Finder state transitions to "paused".
    mgr.pause("research").unwrap();
    let after_pause: FinderStatusSnapshot =
        serde_json::from_str(&fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(after_pause.mappings[0].state, FinderMappingState::Paused);

    // Verify exact serialized JSON keys match ADR 0002.
    let raw_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(raw_json["version"], 1);
    assert_eq!(raw_json["stale_after_seconds"], 300);
    assert_eq!(raw_json["mappings"][0]["state"], "paused");
    assert!(raw_json["mappings"][0]["label"].is_string());

    mgr.shutdown().unwrap();
}

#[test]
fn finder_status_defaults_to_state_dir_subdirectory() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().join("state");
    let mut cfg = ManagerConfig::with_state_dir(&state_dir);
    cfg.launcher = Arc::new(FakeBisyncLauncher::new());

    let mgr = SyncManager::open(cfg).unwrap();
    assert_eq!(mgr.finder_status_dir(), state_dir.join("finder"));
    assert!(state_dir.join("finder/status.json").is_file());
    mgr.shutdown().unwrap();
}
