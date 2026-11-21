//! Durable SQLite state store with forward-only `schema_migrations` and the §6 entities.

use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use rclone_adapter::ProcessStartTime;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sync_core::{
    AccountId, CloudCollection, ConfigGeneration, DeviceId, Execution, GenerationId, Intent,
    LocalBinding, MappingId, MappingState, Readiness, RunId, RunOutcome, VolumeIdentity,
    VolumeKind,
};

use crate::{
    error::ManagerError,
    platform::{ensure_private_dir, set_private_file_permissions},
};

/// Current highest SQLite schema migration version supported by this build.
pub const LATEST_SCHEMA_VERSION: i64 = 2;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_section6_schema",
        sql: r#"
CREATE TABLE devices (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    remote_name TEXT NOT NULL,
    remote_root_folder_id TEXT NOT NULL,
    config_path TEXT NOT NULL,
    auth_status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mappings (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES devices(id),
    account_id TEXT NOT NULL REFERENCES accounts(id),
    collection_display_path TEXT NOT NULL,
    collection_folder_id TEXT NOT NULL,
    collection_ancestor_ids_json TEXT NOT NULL,
    local_volume_kind TEXT NOT NULL,
    local_volume_stable_id TEXT NOT NULL,
    local_mount_path TEXT NOT NULL,
    local_relative_dir TEXT NOT NULL,
    remote_endpoint_kind TEXT NOT NULL,
    remote_endpoint_value TEXT NOT NULL,
    local_recovery_root TEXT NOT NULL,
    cloud_recovery_path TEXT NOT NULL,
    schedule_interval_secs INTEGER NOT NULL,
    scheduling_enabled INTEGER NOT NULL,
    state_json TEXT NOT NULL,
    intent TEXT NOT NULL,
    readiness TEXT NOT NULL,
    execution TEXT NOT NULL,
    active_generation_id TEXT,
    exclusions_json TEXT NOT NULL,
    max_delete_percent INTEGER NOT NULL,
    last_run_id TEXT,
    last_run_outcome TEXT,
    last_run_completed_at TEXT,
    last_run_summary_text TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mapping_generations (
    id TEXT PRIMARY KEY,
    mapping_id TEXT NOT NULL REFERENCES mappings(id),
    generation_number INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    config_json TEXT NOT NULL,
    initial_direction TEXT,
    workdir_path TEXT NOT NULL,
    filters_file_path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(mapping_id, generation_number)
);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    mapping_id TEXT NOT NULL REFERENCES mappings(id),
    generation_id TEXT NOT NULL REFERENCES mapping_generations(id),
    mode TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    workdir_path TEXT NOT NULL,
    backup_dir1 TEXT NOT NULL,
    backup_dir2 TEXT NOT NULL,
    run_dir TEXT NOT NULL,
    log_path TEXT NOT NULL,
    child_pid INTEGER,
    child_start_time_kind TEXT,
    child_start_time_value INTEGER,
    child_executable TEXT,
    engine_version TEXT,
    engine_sha256 TEXT,
    engine_platform TEXT,
    exit_code INTEGER,
    exit_signal INTEGER,
    outcome_kind TEXT,
    outcome_detail TEXT,
    conflict_paths_json TEXT NOT NULL,
    last_verified_phase TEXT NOT NULL,
    files_checked INTEGER NOT NULL DEFAULT 0,
    files_transferred INTEGER NOT NULL DEFAULT 0,
    bytes_transferred INTEGER NOT NULL DEFAULT 0,
    conflict_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_runs_mapping_started ON runs(mapping_id, started_at DESC);
CREATE INDEX idx_runs_status ON runs(status);

CREATE TABLE recovery_cases (
    id TEXT PRIMARY KEY,
    mapping_id TEXT NOT NULL REFERENCES mappings(id),
    run_id TEXT REFERENCES runs(id),
    status TEXT NOT NULL,
    reason_kind TEXT NOT NULL,
    cause TEXT NOT NULL,
    next_step TEXT NOT NULL,
    workdir_path TEXT,
    backup_dir1 TEXT,
    backup_dir2 TEXT,
    log_path TEXT,
    orphan_pid INTEGER,
    created_at TEXT NOT NULL,
    resolved_at TEXT,
    resolution_note TEXT
);

CREATE INDEX idx_recovery_cases_mapping_status ON recovery_cases(mapping_id, status);
"#,
    },
    Migration {
        version: 2,
        name: "unreviewed_conflict_acknowledgment",
        sql: r#"
ALTER TABLE mappings ADD COLUMN unreviewed_conflict_run_id TEXT;
UPDATE mappings
SET unreviewed_conflict_run_id = last_run_id
WHERE last_run_outcome IN ('"SuccessWithConflicts"', 'SuccessWithConflicts')
  AND last_run_id IS NOT NULL;
"#,
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: DeviceId,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: AccountId,
    pub display_name: String,
    pub kind: String,
    pub remote_name: String,
    pub remote_root_folder_id: String,
    pub config_path: PathBuf,
    pub auth_status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingRecord {
    pub id: MappingId,
    pub device_id: DeviceId,
    pub account_id: AccountId,
    pub collection: CloudCollection,
    pub local_binding: LocalBinding,
    pub remote_endpoint_kind: String,
    pub remote_endpoint_value: String,
    pub local_recovery_root: PathBuf,
    pub cloud_recovery_path: String,
    pub schedule_interval_secs: u64,
    pub scheduling_enabled: bool,
    pub state: MappingState,
    pub active_generation_id: Option<GenerationId>,
    pub exclusions: Vec<String>,
    pub max_delete_percent: u8,
    pub last_run_id: Option<RunId>,
    pub last_run_outcome: Option<RunOutcome>,
    pub last_run_completed_at: Option<String>,
    pub last_run_summary_text: Option<String>,
    /// Run ID whose conflict copies have not yet been marked reviewed by the user.
    #[serde(default)]
    pub unreviewed_conflict_run_id: Option<RunId>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub id: GenerationId,
    pub mapping_id: MappingId,
    pub generation_number: i64,
    pub fingerprint: String,
    pub config: ConfigGeneration,
    pub initial_direction: Option<String>,
    pub workdir_path: PathBuf,
    pub filters_file_path: PathBuf,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub mapping_id: MappingId,
    pub generation_id: GenerationId,
    pub mode: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub workdir_path: PathBuf,
    pub backup_dir1: PathBuf,
    pub backup_dir2: String,
    pub run_dir: PathBuf,
    pub log_path: PathBuf,
    pub child_pid: Option<u32>,
    #[serde(with = "opt_process_start_time_serde")]
    pub child_start_time: Option<ProcessStartTime>,
    pub child_executable: Option<PathBuf>,
    pub engine_version: Option<String>,
    pub engine_sha256: Option<String>,
    pub engine_platform: Option<String>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub outcome_kind: Option<String>,
    pub outcome_detail: Option<String>,
    pub conflict_paths: Vec<String>,
    pub last_verified_phase: String,
    pub files_checked: u64,
    pub files_transferred: u64,
    pub bytes_transferred: u64,
    pub conflict_count: u64,
}

mod opt_process_start_time_serde {
    use rclone_adapter::ProcessStartTime;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
    enum WireStartTime {
        UnixMicros(u64),
        BootTicks(u64),
    }

    pub fn serialize<S: Serializer>(
        value: &Option<ProcessStartTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let wire = value.map(|v| match v {
            ProcessStartTime::UnixMicros(n) => WireStartTime::UnixMicros(n),
            ProcessStartTime::BootTicks(n) => WireStartTime::BootTicks(n),
        });
        wire.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<ProcessStartTime>, D::Error> {
        let wire = Option::<WireStartTime>::deserialize(deserializer)?;
        Ok(wire.map(|w| match w {
            WireStartTime::UnixMicros(n) => ProcessStartTime::UnixMicros(n),
            WireStartTime::BootTicks(n) => ProcessStartTime::BootTicks(n),
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCaseRecord {
    pub id: String,
    pub mapping_id: MappingId,
    pub run_id: Option<RunId>,
    pub status: String,
    pub reason_kind: String,
    pub cause: String,
    pub next_step: String,
    pub workdir_path: Option<PathBuf>,
    pub backup_dir1: Option<PathBuf>,
    pub backup_dir2: Option<String>,
    pub log_path: Option<PathBuf>,
    pub orphan_pid: Option<u32>,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolution_note: Option<String>,
}

fn storage_err(context: &str, err: impl std::fmt::Display) -> ManagerError {
    ManagerError::Storage {
        cause: format!("{context}: {err}."),
        next_step: "Verify the FolderSync state directory is intact and writable.".to_string(),
    }
}

fn intent_str(intent: Intent) -> &'static str {
    match intent {
        Intent::Enabled => "enabled",
        Intent::Paused => "paused",
    }
}

fn readiness_str(readiness: Readiness) -> &'static str {
    match readiness {
        Readiness::Unconfigured => "unconfigured",
        Readiness::NeedsInitialization => "needs_initialization",
        Readiness::Ready => "ready",
        Readiness::VolumeUnavailable => "volume_unavailable",
        Readiness::AuthenticationRequired => "authentication_required",
        Readiness::EndpointChanged => "endpoint_changed",
        Readiness::RecoveryRequired => "recovery_required",
    }
}

fn execution_str(execution: Execution) -> &'static str {
    match execution {
        Execution::Idle => "idle",
        Execution::Queued => "queued",
        Execution::Running => "running",
        Execution::Stopping => "stopping",
        Execution::Interrupted => "interrupted",
    }
}

fn volume_kind_str(kind: VolumeKind) -> &'static str {
    match kind {
        VolumeKind::Internal => "internal",
        VolumeKind::External => "external",
    }
}

fn parse_volume_kind(s: &str) -> VolumeKind {
    match s {
        "external" => VolumeKind::External,
        _ => VolumeKind::Internal,
    }
}

fn start_time_to_cols(st: Option<ProcessStartTime>) -> (Option<&'static str>, Option<i64>) {
    match st {
        Some(ProcessStartTime::UnixMicros(v)) => (Some("unix_micros"), Some(v as i64)),
        Some(ProcessStartTime::BootTicks(v)) => (Some("boot_ticks"), Some(v as i64)),
        None => (None, None),
    }
}

fn cols_to_start_time(kind: Option<String>, value: Option<i64>) -> Option<ProcessStartTime> {
    match (kind.as_deref(), value) {
        (Some("unix_micros"), Some(v)) => Some(ProcessStartTime::UnixMicros(v as u64)),
        (Some("boot_ticks"), Some(v)) => Some(ProcessStartTime::BootTicks(v as u64)),
        _ => None,
    }
}

/// Bundled SQLite store owning the §6 tables and forward-only `schema_migrations`.
pub struct SqliteStateStore {
    conn: Connection,
    path: PathBuf,
}

impl SqliteStateStore {
    /// Opens or creates `db_path`, enforces `0600` private permissions, and runs
    /// forward-only migrations.
    pub fn open(db_path: &Path) -> Result<Self, ManagerError> {
        if let Some(parent) = db_path.parent() {
            ensure_private_dir(parent)?;
        }

        let conn = Connection::open(db_path).map_err(|e| {
            storage_err(
                &format!("Failed to open SQLite DB `{}`", db_path.display()),
                e,
            )
        })?;
        set_private_file_permissions(db_path)?;

        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| storage_err("Failed to configure SQLite pragmas", e))?;

        let mut store = Self {
            conn,
            path: db_path.to_path_buf(),
        };
        store.run_migrations()?;
        set_private_file_permissions(db_path)?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn run_migrations(&mut self) -> Result<(), ManagerError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    applied_at TEXT NOT NULL
                );",
            )
            .map_err(|e| storage_err("Failed to initialize schema_migrations table", e))?;

        let current_max: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(|e| storage_err("Failed to query schema_migrations", e))?;

        if current_max > LATEST_SCHEMA_VERSION {
            return Err(ManagerError::UnsupportedSchemaVersion {
                found: current_max,
                max_supported: LATEST_SCHEMA_VERSION,
                cause: format!(
                    "Database schema version {current_max} is newer than this version of FolderSync supports (max {LATEST_SCHEMA_VERSION})."
                ),
                next_step:
                    "Upgrade FolderSync to a compatible version; automatic downgrade is not supported."
                        .to_string(),
            });
        }

        for migration in MIGRATIONS {
            if migration.version <= current_max {
                continue;
            }
            let tx = self
                .conn
                .transaction()
                .map_err(|e| storage_err("Failed to start migration transaction", e))?;
            tx.execute_batch(migration.sql).map_err(|e| {
                storage_err(
                    &format!(
                        "Failed to apply schema migration v{} (`{}`)",
                        migration.version, migration.name
                    ),
                    e,
                )
            })?;
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            tx.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![migration.version, migration.name, now],
            )
            .map_err(|e| storage_err("Failed to record schema migration", e))?;
            tx.commit()
                .map_err(|e| storage_err("Failed to commit schema migration", e))?;
        }

        Ok(())
    }

    pub fn applied_migrations(&self) -> Result<Vec<(i64, String)>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare("SELECT version, name FROM schema_migrations ORDER BY version ASC")
            .map_err(|e| storage_err("Failed to query applied migrations", e))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| storage_err("Failed to read applied migrations", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect applied migrations", e))
    }

    pub fn table_names(&self) -> Result<Vec<String>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name ASC",
            )
            .map_err(|e| storage_err("Failed to list tables", e))?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| storage_err("Failed to read tables", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect tables", e))
    }

    pub fn ensure_device(&self, device: &DeviceRecord) -> Result<DeviceRecord, ManagerError> {
        if let Some(existing) = self.get_default_device()? {
            return Ok(existing);
        }
        self.conn
            .execute(
                "INSERT INTO devices (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![device.id.as_str(), device.name, device.created_at],
            )
            .map_err(|e| storage_err("Failed to insert device record", e))?;
        Ok(device.clone())
    }

    pub fn get_default_device(&self) -> Result<Option<DeviceRecord>, ManagerError> {
        self.conn
            .query_row(
                "SELECT id, name, created_at FROM devices ORDER BY created_at ASC LIMIT 1",
                [],
                |row| {
                    Ok(DeviceRecord {
                        id: DeviceId::new(row.get::<_, String>(0)?),
                        name: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| storage_err("Failed to read device record", e))
    }

    pub fn upsert_account(&self, account: &AccountRecord) -> Result<(), ManagerError> {
        self.conn
            .execute(
                "INSERT INTO accounts (
                    id, display_name, kind, remote_name, remote_root_folder_id,
                    config_path, auth_status, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(id) DO UPDATE SET
                    display_name = excluded.display_name,
                    kind = excluded.kind,
                    remote_name = excluded.remote_name,
                    remote_root_folder_id = excluded.remote_root_folder_id,
                    config_path = excluded.config_path,
                    auth_status = excluded.auth_status,
                    updated_at = excluded.updated_at",
                params![
                    account.id.as_str(),
                    account.display_name,
                    account.kind,
                    account.remote_name,
                    account.remote_root_folder_id,
                    account.config_path.to_string_lossy().as_ref(),
                    account.auth_status,
                    account.created_at,
                    account.updated_at,
                ],
            )
            .map_err(|e| storage_err("Failed to persist account record", e))?;
        Ok(())
    }

    pub fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>, ManagerError> {
        self.conn
            .query_row(
                "SELECT id, display_name, kind, remote_name, remote_root_folder_id,
                        config_path, auth_status, created_at, updated_at
                 FROM accounts WHERE id = ?1",
                params![id.as_str()],
                |row| {
                    Ok(AccountRecord {
                        id: AccountId::new(row.get::<_, String>(0)?),
                        display_name: row.get(1)?,
                        kind: row.get(2)?,
                        remote_name: row.get(3)?,
                        remote_root_folder_id: row.get(4)?,
                        config_path: PathBuf::from(row.get::<_, String>(5)?),
                        auth_status: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(|e| storage_err("Failed to query account", e))
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, display_name, kind, remote_name, remote_root_folder_id,
                        config_path, auth_status, created_at, updated_at
                 FROM accounts ORDER BY created_at ASC, id ASC",
            )
            .map_err(|e| storage_err("Failed to prepare list_accounts", e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AccountRecord {
                    id: AccountId::new(row.get::<_, String>(0)?),
                    display_name: row.get(1)?,
                    kind: row.get(2)?,
                    remote_name: row.get(3)?,
                    remote_root_folder_id: row.get(4)?,
                    config_path: PathBuf::from(row.get::<_, String>(5)?),
                    auth_status: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })
            .map_err(|e| storage_err("Failed to read accounts", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect accounts", e))
    }

    pub fn delete_account(&self, id: &AccountId) -> Result<(), ManagerError> {
        self.conn
            .execute("DELETE FROM accounts WHERE id = ?1", params![id.as_str()])
            .map_err(|e| storage_err("Failed to delete account record", e))?;
        Ok(())
    }

    pub fn insert_mapping(&self, m: &MappingRecord) -> Result<(), ManagerError> {
        let ancestors_json = serde_json::to_string(&m.collection.ancestor_ids)
            .map_err(|e| storage_err("Failed to serialize ancestor_ids", e))?;
        let state_json = serde_json::to_string(&m.state)
            .map_err(|e| storage_err("Failed to serialize MappingState", e))?;
        let exclusions_json = serde_json::to_string(&m.exclusions)
            .map_err(|e| storage_err("Failed to serialize exclusions", e))?;
        let last_outcome_json = m
            .last_run_outcome
            .map(|o| serde_json::to_string(&o))
            .transpose()
            .map_err(|e| storage_err("Failed to serialize last_run_outcome", e))?;

        self.conn
            .execute(
                "INSERT INTO mappings (
                    id, device_id, account_id,
                    collection_display_path, collection_folder_id, collection_ancestor_ids_json,
                    local_volume_kind, local_volume_stable_id, local_mount_path, local_relative_dir,
                    remote_endpoint_kind, remote_endpoint_value,
                    local_recovery_root, cloud_recovery_path,
                    schedule_interval_secs, scheduling_enabled,
                    state_json, intent, readiness, execution,
                    active_generation_id, exclusions_json, max_delete_percent,
                    last_run_id, last_run_outcome, last_run_completed_at, last_run_summary_text,
                    unreviewed_conflict_run_id,
                    created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                    ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30
                )",
                params![
                    m.id.as_str(),
                    m.device_id.as_str(),
                    m.account_id.as_str(),
                    m.collection.display_path,
                    m.collection.folder_id,
                    ancestors_json,
                    volume_kind_str(m.local_binding.volume.kind),
                    m.local_binding.volume.stable_id,
                    m.local_binding.mount_path.to_string_lossy().as_ref(),
                    m.local_binding.relative_dir.to_string_lossy().as_ref(),
                    m.remote_endpoint_kind,
                    m.remote_endpoint_value,
                    m.local_recovery_root.to_string_lossy().as_ref(),
                    m.cloud_recovery_path,
                    m.schedule_interval_secs as i64,
                    i64::from(m.scheduling_enabled),
                    state_json,
                    intent_str(m.state.intent),
                    readiness_str(m.state.readiness()),
                    execution_str(m.state.execution),
                    m.active_generation_id.as_ref().map(|g| g.as_str()),
                    exclusions_json,
                    i64::from(m.max_delete_percent),
                    m.last_run_id.as_ref().map(|r| r.as_str()),
                    last_outcome_json,
                    m.last_run_completed_at,
                    m.last_run_summary_text,
                    m.unreviewed_conflict_run_id.as_ref().map(|r| r.as_str()),
                    m.created_at,
                    m.updated_at,
                ],
            )
            .map_err(|e| storage_err("Failed to insert mapping record", e))?;
        Ok(())
    }

    pub fn update_mapping_draft(&self, m: &MappingRecord) -> Result<(), ManagerError> {
        let ancestors_json = serde_json::to_string(&m.collection.ancestor_ids)
            .map_err(|e| storage_err("Failed to serialize ancestor_ids", e))?;
        let state_json = serde_json::to_string(&m.state)
            .map_err(|e| storage_err("Failed to serialize MappingState", e))?;
        let exclusions_json = serde_json::to_string(&m.exclusions)
            .map_err(|e| storage_err("Failed to serialize exclusions", e))?;
        self.conn
            .execute(
                "UPDATE mappings SET
                    account_id = ?2,
                    collection_display_path = ?3,
                    collection_folder_id = ?4,
                    collection_ancestor_ids_json = ?5,
                    local_volume_kind = ?6,
                    local_volume_stable_id = ?7,
                    local_mount_path = ?8,
                    local_relative_dir = ?9,
                    remote_endpoint_kind = ?10,
                    remote_endpoint_value = ?11,
                    local_recovery_root = ?12,
                    cloud_recovery_path = ?13,
                    schedule_interval_secs = ?14,
                    state_json = ?15,
                    intent = ?16,
                    readiness = ?17,
                    execution = ?18,
                    exclusions_json = ?19,
                    max_delete_percent = ?20,
                    updated_at = ?21
                 WHERE id = ?1",
                params![
                    m.id.as_str(),
                    m.account_id.as_str(),
                    m.collection.display_path,
                    m.collection.folder_id,
                    ancestors_json,
                    volume_kind_str(m.local_binding.volume.kind),
                    m.local_binding.volume.stable_id,
                    m.local_binding.mount_path.to_string_lossy().as_ref(),
                    m.local_binding.relative_dir.to_string_lossy().as_ref(),
                    m.remote_endpoint_kind,
                    m.remote_endpoint_value,
                    m.local_recovery_root.to_string_lossy().as_ref(),
                    m.cloud_recovery_path,
                    m.schedule_interval_secs as i64,
                    state_json,
                    intent_str(m.state.intent),
                    readiness_str(m.state.readiness()),
                    execution_str(m.state.execution),
                    exclusions_json,
                    i64::from(m.max_delete_percent),
                    m.updated_at,
                ],
            )
            .map_err(|e| storage_err("Failed to update draft mapping record", e))?;
        Ok(())
    }

    pub fn update_mapping_schedule_interval(
        &self,
        mapping_id: &MappingId,
        interval_secs: u64,
        updated_at: &str,
    ) -> Result<(), ManagerError> {
        self.conn
            .execute(
                "UPDATE mappings SET schedule_interval_secs = ?2, updated_at = ?3 WHERE id = ?1",
                params![mapping_id.as_str(), interval_secs as i64, updated_at],
            )
            .map_err(|e| storage_err("Failed to update schedule interval", e))?;
        Ok(())
    }

    pub fn update_mapping_runtime(&self, m: &MappingRecord) -> Result<(), ManagerError> {
        let state_json = serde_json::to_string(&m.state)
            .map_err(|e| storage_err("Failed to serialize MappingState", e))?;
        let last_outcome_json = m
            .last_run_outcome
            .map(|o| serde_json::to_string(&o))
            .transpose()
            .map_err(|e| storage_err("Failed to serialize last_run_outcome", e))?;

        self.conn
            .execute(
                "UPDATE mappings SET
                    scheduling_enabled = ?2,
                    state_json = ?3,
                    intent = ?4,
                    readiness = ?5,
                    execution = ?6,
                    active_generation_id = ?7,
                    last_run_id = ?8,
                    last_run_outcome = ?9,
                    last_run_completed_at = ?10,
                    last_run_summary_text = ?11,
                    unreviewed_conflict_run_id = ?12,
                    updated_at = ?13
                 WHERE id = ?1",
                params![
                    m.id.as_str(),
                    i64::from(m.scheduling_enabled),
                    state_json,
                    intent_str(m.state.intent),
                    readiness_str(m.state.readiness()),
                    execution_str(m.state.execution),
                    m.active_generation_id.as_ref().map(|g| g.as_str()),
                    m.last_run_id.as_ref().map(|r| r.as_str()),
                    last_outcome_json,
                    m.last_run_completed_at,
                    m.last_run_summary_text,
                    m.unreviewed_conflict_run_id.as_ref().map(|r| r.as_str()),
                    m.updated_at,
                ],
            )
            .map_err(|e| storage_err("Failed to update mapping state", e))?;
        Ok(())
    }

    /// Removes a mapping and its database metadata in a single SQLite transaction.
    /// Never touches local synced folders, cloud collections, or recovery copies (§7).
    pub fn delete_mapping_cascade(&mut self, mapping_id: &MappingId) -> Result<(), ManagerError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("Failed to start remove-mapping transaction", e))?;
        tx.execute(
            "DELETE FROM recovery_cases WHERE mapping_id = ?1",
            params![mapping_id.as_str()],
        )
        .map_err(|e| storage_err("Failed to delete recovery cases for mapping", e))?;
        tx.execute(
            "DELETE FROM runs WHERE mapping_id = ?1",
            params![mapping_id.as_str()],
        )
        .map_err(|e| storage_err("Failed to delete runs for mapping", e))?;
        tx.execute(
            "DELETE FROM mapping_generations WHERE mapping_id = ?1",
            params![mapping_id.as_str()],
        )
        .map_err(|e| storage_err("Failed to delete generations for mapping", e))?;
        tx.execute(
            "DELETE FROM mappings WHERE id = ?1",
            params![mapping_id.as_str()],
        )
        .map_err(|e| storage_err("Failed to delete mapping record", e))?;
        tx.commit()
            .map_err(|e| storage_err("Failed to commit remove-mapping transaction", e))?;
        Ok(())
    }

    fn map_mapping_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MappingRecord> {
        let ancestors_json: String = row.get(5)?;
        let ancestor_ids: Vec<String> = serde_json::from_str(&ancestors_json).unwrap_or_default();
        let state_json: String = row.get(16)?;
        let state: MappingState = serde_json::from_str(&state_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(16, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let exclusions_json: String = row.get(21)?;
        let exclusions: Vec<String> = serde_json::from_str(&exclusions_json).unwrap_or_default();
        let last_outcome_json: Option<String> = row.get(24)?;
        let last_run_outcome = last_outcome_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<RunOutcome>(s).ok());

        Ok(MappingRecord {
            id: MappingId::new(row.get::<_, String>(0)?),
            device_id: DeviceId::new(row.get::<_, String>(1)?),
            account_id: AccountId::new(row.get::<_, String>(2)?),
            collection: CloudCollection {
                display_path: row.get(3)?,
                folder_id: row.get(4)?,
                ancestor_ids,
            },
            local_binding: LocalBinding {
                volume: VolumeIdentity::new(
                    parse_volume_kind(&row.get::<_, String>(6)?),
                    row.get::<_, String>(7)?,
                ),
                mount_path: PathBuf::from(row.get::<_, String>(8)?),
                relative_dir: PathBuf::from(row.get::<_, String>(9)?),
            },
            remote_endpoint_kind: row.get(10)?,
            remote_endpoint_value: row.get(11)?,
            local_recovery_root: PathBuf::from(row.get::<_, String>(12)?),
            cloud_recovery_path: row.get(13)?,
            schedule_interval_secs: row.get::<_, i64>(14)? as u64,
            scheduling_enabled: row.get::<_, i64>(15)? != 0,
            state,
            active_generation_id: row.get::<_, Option<String>>(20)?.map(GenerationId::new),
            exclusions,
            max_delete_percent: row.get::<_, i64>(22)? as u8,
            last_run_id: row.get::<_, Option<String>>(23)?.map(RunId::new),
            last_run_outcome,
            last_run_completed_at: row.get(25)?,
            last_run_summary_text: row.get(26)?,
            unreviewed_conflict_run_id: row.get::<_, Option<String>>(27)?.map(RunId::new),
            created_at: row.get(28)?,
            updated_at: row.get(29)?,
        })
    }

    pub fn get_mapping(
        &self,
        mapping_id: &MappingId,
    ) -> Result<Option<MappingRecord>, ManagerError> {
        self.conn
            .query_row(
                "SELECT id, device_id, account_id,
                        collection_display_path, collection_folder_id, collection_ancestor_ids_json,
                        local_volume_kind, local_volume_stable_id, local_mount_path, local_relative_dir,
                        remote_endpoint_kind, remote_endpoint_value,
                        local_recovery_root, cloud_recovery_path,
                        schedule_interval_secs, scheduling_enabled,
                        state_json, intent, readiness, execution,
                        active_generation_id, exclusions_json, max_delete_percent,
                        last_run_id, last_run_outcome, last_run_completed_at, last_run_summary_text,
                        unreviewed_conflict_run_id,
                        created_at, updated_at
                 FROM mappings WHERE id = ?1",
                params![mapping_id.as_str()],
                Self::map_mapping_row,
            )
            .optional()
            .map_err(|e| storage_err("Failed to query mapping", e))
    }

    pub fn list_mappings(&self) -> Result<Vec<MappingRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, device_id, account_id,
                        collection_display_path, collection_folder_id, collection_ancestor_ids_json,
                        local_volume_kind, local_volume_stable_id, local_mount_path, local_relative_dir,
                        remote_endpoint_kind, remote_endpoint_value,
                        local_recovery_root, cloud_recovery_path,
                        schedule_interval_secs, scheduling_enabled,
                        state_json, intent, readiness, execution,
                        active_generation_id, exclusions_json, max_delete_percent,
                        last_run_id, last_run_outcome, last_run_completed_at, last_run_summary_text,
                        unreviewed_conflict_run_id,
                        created_at, updated_at
                 FROM mappings ORDER BY created_at ASC, id ASC",
            )
            .map_err(|e| storage_err("Failed to prepare list_mappings", e))?;
        let rows = stmt
            .query_map([], Self::map_mapping_row)
            .map_err(|e| storage_err("Failed to read mappings", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect mappings", e))
    }

    pub fn next_generation_number(&self, mapping_id: &MappingId) -> Result<i64, ManagerError> {
        let current: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(generation_number), 0) FROM mapping_generations WHERE mapping_id = ?1",
                params![mapping_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| storage_err("Failed to query next generation number", e))?;
        Ok(current + 1)
    }

    pub fn insert_generation(&self, gen_rec: &GenerationRecord) -> Result<(), ManagerError> {
        let config_json = serde_json::to_string(&gen_rec.config)
            .map_err(|e| storage_err("Failed to serialize ConfigGeneration", e))?;
        self.conn
            .execute(
                "INSERT INTO mapping_generations (
                    id, mapping_id, generation_number, fingerprint, config_json,
                    initial_direction, workdir_path, filters_file_path, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    gen_rec.id.as_str(),
                    gen_rec.mapping_id.as_str(),
                    gen_rec.generation_number,
                    gen_rec.fingerprint,
                    config_json,
                    gen_rec.initial_direction,
                    gen_rec.workdir_path.to_string_lossy().as_ref(),
                    gen_rec.filters_file_path.to_string_lossy().as_ref(),
                    gen_rec.created_at,
                ],
            )
            .map_err(|e| storage_err("Failed to insert mapping_generations row", e))?;
        Ok(())
    }

    pub fn get_generation(
        &self,
        generation_id: &GenerationId,
    ) -> Result<Option<GenerationRecord>, ManagerError> {
        self.conn
            .query_row(
                "SELECT id, mapping_id, generation_number, fingerprint, config_json,
                        initial_direction, workdir_path, filters_file_path, created_at
                 FROM mapping_generations WHERE id = ?1",
                params![generation_id.as_str()],
                |row| {
                    let config_json: String = row.get(4)?;
                    let config: ConfigGeneration =
                        serde_json::from_str(&config_json).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    Ok(GenerationRecord {
                        id: GenerationId::new(row.get::<_, String>(0)?),
                        mapping_id: MappingId::new(row.get::<_, String>(1)?),
                        generation_number: row.get(2)?,
                        fingerprint: row.get(3)?,
                        config,
                        initial_direction: row.get(5)?,
                        workdir_path: PathBuf::from(row.get::<_, String>(6)?),
                        filters_file_path: PathBuf::from(row.get::<_, String>(7)?),
                        created_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(|e| storage_err("Failed to query generation", e))
    }

    pub fn insert_run_start(&self, run: &RunRecord) -> Result<(), ManagerError> {
        let (st_kind, st_val) = start_time_to_cols(run.child_start_time);
        let conflict_json = serde_json::to_string(&run.conflict_paths)
            .map_err(|e| storage_err("Failed to serialize conflict_paths", e))?;
        self.conn
            .execute(
                "INSERT INTO runs (
                    id, mapping_id, generation_id, mode, status, started_at, finished_at,
                    workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                    child_pid, child_start_time_kind, child_start_time_value, child_executable,
                    engine_version, engine_sha256, engine_platform,
                    exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                    last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                    ?25, ?26, ?27, ?28, ?29
                )",
                params![
                    run.id.as_str(),
                    run.mapping_id.as_str(),
                    run.generation_id.as_str(),
                    run.mode,
                    run.status,
                    run.started_at,
                    run.finished_at,
                    run.workdir_path.to_string_lossy().as_ref(),
                    run.backup_dir1.to_string_lossy().as_ref(),
                    run.backup_dir2,
                    run.run_dir.to_string_lossy().as_ref(),
                    run.log_path.to_string_lossy().as_ref(),
                    run.child_pid.map(i64::from),
                    st_kind,
                    st_val,
                    run.child_executable
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    run.engine_version,
                    run.engine_sha256,
                    run.engine_platform,
                    run.exit_code,
                    run.exit_signal,
                    run.outcome_kind,
                    run.outcome_detail,
                    conflict_json,
                    run.last_verified_phase,
                    run.files_checked as i64,
                    run.files_transferred as i64,
                    run.bytes_transferred as i64,
                    run.conflict_count as i64,
                ],
            )
            .map_err(|e| storage_err("Failed to persist run-start row", e))?;
        Ok(())
    }

    pub fn update_run_child_identity(
        &self,
        run_id: &RunId,
        child: &rclone_adapter::ChildIdentity,
        engine: &crate::launcher::EngineMetadata,
    ) -> Result<(), ManagerError> {
        let (st_kind, st_val) = start_time_to_cols(child.start_time);
        self.conn
            .execute(
                "UPDATE runs SET
                    status = 'running',
                    child_pid = ?2,
                    child_start_time_kind = ?3,
                    child_start_time_value = ?4,
                    child_executable = ?5,
                    engine_version = ?6,
                    engine_sha256 = ?7,
                    engine_platform = ?8,
                    last_verified_phase = 'spawned'
                 WHERE id = ?1",
                params![
                    run_id.as_str(),
                    i64::from(child.pid),
                    st_kind,
                    st_val,
                    child.executable.to_string_lossy().as_ref(),
                    engine.version,
                    engine.sha256,
                    engine.platform,
                ],
            )
            .map_err(|e| storage_err("Failed to record child identity on run row", e))?;
        Ok(())
    }

    pub fn update_run_finished(&self, run: &RunRecord) -> Result<(), ManagerError> {
        let conflict_json = serde_json::to_string(&run.conflict_paths)
            .map_err(|e| storage_err("Failed to serialize conflict_paths", e))?;
        self.conn
            .execute(
                "UPDATE runs SET
                    status = ?2,
                    finished_at = ?3,
                    exit_code = ?4,
                    exit_signal = ?5,
                    outcome_kind = ?6,
                    outcome_detail = ?7,
                    conflict_paths_json = ?8,
                    last_verified_phase = ?9,
                    files_checked = ?10,
                    files_transferred = ?11,
                    bytes_transferred = ?12,
                    conflict_count = ?13
                 WHERE id = ?1",
                params![
                    run.id.as_str(),
                    run.status,
                    run.finished_at,
                    run.exit_code,
                    run.exit_signal,
                    run.outcome_kind,
                    run.outcome_detail,
                    conflict_json,
                    run.last_verified_phase,
                    run.files_checked as i64,
                    run.files_transferred as i64,
                    run.bytes_transferred as i64,
                    run.conflict_count as i64,
                ],
            )
            .map_err(|e| storage_err("Failed to update finished run row", e))?;
        Ok(())
    }

    fn map_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
        let st_kind: Option<String> = row.get(13)?;
        let st_val: Option<i64> = row.get(14)?;
        let conflict_json: String = row.get(23)?;
        let conflict_paths: Vec<String> = serde_json::from_str(&conflict_json).unwrap_or_default();

        Ok(RunRecord {
            id: RunId::new(row.get::<_, String>(0)?),
            mapping_id: MappingId::new(row.get::<_, String>(1)?),
            generation_id: GenerationId::new(row.get::<_, String>(2)?),
            mode: row.get(3)?,
            status: row.get(4)?,
            started_at: row.get(5)?,
            finished_at: row.get(6)?,
            workdir_path: PathBuf::from(row.get::<_, String>(7)?),
            backup_dir1: PathBuf::from(row.get::<_, String>(8)?),
            backup_dir2: row.get(9)?,
            run_dir: PathBuf::from(row.get::<_, String>(10)?),
            log_path: PathBuf::from(row.get::<_, String>(11)?),
            child_pid: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
            child_start_time: cols_to_start_time(st_kind, st_val),
            child_executable: row.get::<_, Option<String>>(15)?.map(PathBuf::from),
            engine_version: row.get(16)?,
            engine_sha256: row.get(17)?,
            engine_platform: row.get(18)?,
            exit_code: row.get(19)?,
            exit_signal: row.get(20)?,
            outcome_kind: row.get(21)?,
            outcome_detail: row.get(22)?,
            conflict_paths,
            last_verified_phase: row.get(24)?,
            files_checked: row.get::<_, i64>(25)? as u64,
            files_transferred: row.get::<_, i64>(26)? as u64,
            bytes_transferred: row.get::<_, i64>(27)? as u64,
            conflict_count: row.get::<_, i64>(28)? as u64,
        })
    }

    pub fn get_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, ManagerError> {
        self.conn
            .query_row(
                "SELECT id, mapping_id, generation_id, mode, status, started_at, finished_at,
                        workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                        child_pid, child_start_time_kind, child_start_time_value, child_executable,
                        engine_version, engine_sha256, engine_platform,
                        exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                        last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                 FROM runs WHERE id = ?1",
                params![run_id.as_str()],
                Self::map_run_row,
            )
            .optional()
            .map_err(|e| storage_err("Failed to query run", e))
    }

    pub fn list_unfinished_runs(&self) -> Result<Vec<RunRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mapping_id, generation_id, mode, status, started_at, finished_at,
                        workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                        child_pid, child_start_time_kind, child_start_time_value, child_executable,
                        engine_version, engine_sha256, engine_platform,
                        exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                        last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                 FROM runs WHERE status IN ('starting', 'running') ORDER BY started_at ASC",
            )
            .map_err(|e| storage_err("Failed to query unfinished runs", e))?;
        let rows = stmt
            .query_map([], Self::map_run_row)
            .map_err(|e| storage_err("Failed to read unfinished runs", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect unfinished runs", e))
    }

    pub fn list_terminal_runs(&self) -> Result<Vec<RunRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mapping_id, generation_id, mode, status, started_at, finished_at,
                        workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                        child_pid, child_start_time_kind, child_start_time_value, child_executable,
                        engine_version, engine_sha256, engine_platform,
                        exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                        last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                 FROM runs WHERE status NOT IN ('starting', 'running') ORDER BY started_at DESC",
            )
            .map_err(|e| storage_err("Failed to query terminal runs", e))?;
        let rows = stmt
            .query_map([], Self::map_run_row)
            .map_err(|e| storage_err("Failed to read terminal runs", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect terminal runs", e))
    }

    pub fn list_runs_for_mapping(
        &self,
        mapping_id: &MappingId,
    ) -> Result<Vec<RunRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mapping_id, generation_id, mode, status, started_at, finished_at,
                        workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                        child_pid, child_start_time_kind, child_start_time_value, child_executable,
                        engine_version, engine_sha256, engine_platform,
                        exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                        last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                 FROM runs WHERE mapping_id = ?1 ORDER BY started_at DESC, rowid DESC",
            )
            .map_err(|e| storage_err("Failed to query runs for mapping", e))?;
        let rows = stmt
            .query_map(params![mapping_id.as_str()], Self::map_run_row)
            .map_err(|e| storage_err("Failed to read runs for mapping", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect runs for mapping", e))
    }

    /// Returns only the newest `limit` runs for `mapping_id` (bounded query).
    pub fn list_recent_runs_for_mapping(
        &self,
        mapping_id: &MappingId,
        limit: usize,
    ) -> Result<Vec<RunRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mapping_id, generation_id, mode, status, started_at, finished_at,
                        workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                        child_pid, child_start_time_kind, child_start_time_value, child_executable,
                        engine_version, engine_sha256, engine_platform,
                        exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                        last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                 FROM runs WHERE mapping_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2",
            )
            .map_err(|e| storage_err("Failed to query recent runs for mapping", e))?;
        let rows = stmt
            .query_map(
                params![mapping_id.as_str(), limit.max(1) as i64],
                Self::map_run_row,
            )
            .map_err(|e| storage_err("Failed to read recent runs for mapping", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect recent runs for mapping", e))
    }

    /// Returns only the single newest run for `mapping_id` (O(1) index probe via `LIMIT 1`).
    pub fn get_latest_run_for_mapping(
        &self,
        mapping_id: &MappingId,
    ) -> Result<Option<RunRecord>, ManagerError> {
        self.conn
            .query_row(
                "SELECT id, mapping_id, generation_id, mode, status, started_at, finished_at,
                        workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                        child_pid, child_start_time_kind, child_start_time_value, child_executable,
                        engine_version, engine_sha256, engine_platform,
                        exit_code, exit_signal, outcome_kind, outcome_detail, conflict_paths_json,
                        last_verified_phase, files_checked, files_transferred, bytes_transferred, conflict_count
                 FROM runs WHERE mapping_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                params![mapping_id.as_str()],
                Self::map_run_row,
            )
            .optional()
            .map_err(|e| storage_err("Failed to query latest run for mapping", e))
    }

    pub fn insert_recovery_case(&self, case: &RecoveryCaseRecord) -> Result<(), ManagerError> {
        self.conn
            .execute(
                "INSERT INTO recovery_cases (
                    id, mapping_id, run_id, status, reason_kind, cause, next_step,
                    workdir_path, backup_dir1, backup_dir2, log_path, orphan_pid,
                    created_at, resolved_at, resolution_note
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    case.id,
                    case.mapping_id.as_str(),
                    case.run_id.as_ref().map(|r| r.as_str()),
                    case.status,
                    case.reason_kind,
                    case.cause,
                    case.next_step,
                    case.workdir_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.backup_dir1
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.backup_dir2,
                    case.log_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.orphan_pid.map(i64::from),
                    case.created_at,
                    case.resolved_at,
                    case.resolution_note,
                ],
            )
            .map_err(|e| storage_err("Failed to persist recovery case", e))?;
        Ok(())
    }

    fn map_recovery_case_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecoveryCaseRecord> {
        Ok(RecoveryCaseRecord {
            id: row.get(0)?,
            mapping_id: MappingId::new(row.get::<_, String>(1)?),
            run_id: row.get::<_, Option<String>>(2)?.map(RunId::new),
            status: row.get(3)?,
            reason_kind: row.get(4)?,
            cause: row.get(5)?,
            next_step: row.get(6)?,
            workdir_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
            backup_dir1: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
            backup_dir2: row.get(9)?,
            log_path: row.get::<_, Option<String>>(10)?.map(PathBuf::from),
            orphan_pid: row.get::<_, Option<i64>>(11)?.map(|v| v as u32),
            created_at: row.get(12)?,
            resolved_at: row.get(13)?,
            resolution_note: row.get(14)?,
        })
    }

    pub fn list_open_recovery_cases(
        &self,
        mapping_id: &MappingId,
    ) -> Result<Vec<RecoveryCaseRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mapping_id, run_id, status, reason_kind, cause, next_step,
                        workdir_path, backup_dir1, backup_dir2, log_path, orphan_pid,
                        created_at, resolved_at, resolution_note
                 FROM recovery_cases
                 WHERE mapping_id = ?1 AND status = 'open'
                 ORDER BY created_at DESC, rowid DESC",
            )
            .map_err(|e| storage_err("Failed to query open recovery cases", e))?;
        let rows = stmt
            .query_map(params![mapping_id.as_str()], Self::map_recovery_case_row)
            .map_err(|e| storage_err("Failed to read recovery cases", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect recovery cases", e))
    }

    pub fn list_all_open_recovery_cases(&self) -> Result<Vec<RecoveryCaseRecord>, ManagerError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mapping_id, run_id, status, reason_kind, cause, next_step,
                        workdir_path, backup_dir1, backup_dir2, log_path, orphan_pid,
                        created_at, resolved_at, resolution_note
                 FROM recovery_cases
                 WHERE status = 'open'
                 ORDER BY created_at DESC, rowid DESC",
            )
            .map_err(|e| storage_err("Failed to query all open recovery cases", e))?;
        let rows = stmt
            .query_map([], Self::map_recovery_case_row)
            .map_err(|e| storage_err("Failed to read all recovery cases", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("Failed to collect all recovery cases", e))
    }

    /// Atomically commits a run's terminal status, optional updated mapping
    /// state/hold, and optional recovery case in a single SQLite transaction.
    pub fn commit_run_terminal(
        &mut self,
        run: &RunRecord,
        mapping: Option<&MappingRecord>,
        recovery_case: Option<&RecoveryCaseRecord>,
    ) -> Result<(), ManagerError> {
        let conflict_json = serde_json::to_string(&run.conflict_paths)
            .map_err(|e| storage_err("Failed to serialize conflict_paths", e))?;
        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("Failed to start terminal commit transaction", e))?;

        tx.execute(
            "UPDATE runs SET
                status = ?2,
                finished_at = ?3,
                exit_code = ?4,
                exit_signal = ?5,
                outcome_kind = ?6,
                outcome_detail = ?7,
                conflict_paths_json = ?8,
                last_verified_phase = ?9,
                files_checked = ?10,
                files_transferred = ?11,
                bytes_transferred = ?12,
                conflict_count = ?13
             WHERE id = ?1",
            params![
                run.id.as_str(),
                run.status,
                run.finished_at,
                run.exit_code,
                run.exit_signal,
                run.outcome_kind,
                run.outcome_detail,
                conflict_json,
                run.last_verified_phase,
                run.files_checked as i64,
                run.files_transferred as i64,
                run.bytes_transferred as i64,
                run.conflict_count as i64,
            ],
        )
        .map_err(|e| storage_err("Failed to update finished run row in transaction", e))?;

        if let Some(m) = mapping {
            let state_json = serde_json::to_string(&m.state)
                .map_err(|e| storage_err("Failed to serialize MappingState", e))?;
            let last_outcome_json = m
                .last_run_outcome
                .map(|o| serde_json::to_string(&o))
                .transpose()
                .map_err(|e| storage_err("Failed to serialize last_run_outcome", e))?;
            tx.execute(
                "UPDATE mappings SET
                    scheduling_enabled = ?2,
                    state_json = ?3,
                    intent = ?4,
                    readiness = ?5,
                    execution = ?6,
                    active_generation_id = ?7,
                    last_run_id = ?8,
                    last_run_outcome = ?9,
                    last_run_completed_at = ?10,
                    last_run_summary_text = ?11,
                    unreviewed_conflict_run_id = ?12,
                    updated_at = ?13
                 WHERE id = ?1",
                params![
                    m.id.as_str(),
                    i64::from(m.scheduling_enabled),
                    state_json,
                    intent_str(m.state.intent),
                    readiness_str(m.state.readiness()),
                    execution_str(m.state.execution),
                    m.active_generation_id.as_ref().map(|g| g.as_str()),
                    m.last_run_id.as_ref().map(|r| r.as_str()),
                    last_outcome_json,
                    m.last_run_completed_at,
                    m.last_run_summary_text,
                    m.unreviewed_conflict_run_id.as_ref().map(|r| r.as_str()),
                    m.updated_at,
                ],
            )
            .map_err(|e| storage_err("Failed to update mapping state in transaction", e))?;
        }

        if let Some(case) = recovery_case {
            tx.execute(
                "INSERT INTO recovery_cases (
                    id, mapping_id, run_id, status, reason_kind, cause, next_step,
                    workdir_path, backup_dir1, backup_dir2, log_path, orphan_pid,
                    created_at, resolved_at, resolution_note
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    case.id,
                    case.mapping_id.as_str(),
                    case.run_id.as_ref().map(|r| r.as_str()),
                    case.status,
                    case.reason_kind,
                    case.cause,
                    case.next_step,
                    case.workdir_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.backup_dir1
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.backup_dir2,
                    case.log_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.orphan_pid.map(i64::from),
                    case.created_at,
                    case.resolved_at,
                    case.resolution_note,
                ],
            )
            .map_err(|e| storage_err("Failed to insert recovery case in transaction", e))?;
        }

        tx.commit()
            .map_err(|e| storage_err("Failed to commit terminal transaction", e))?;
        Ok(())
    }

    /// Atomically commits a mapping runtime update and optional recovery case
    /// in a single SQLite transaction.
    pub fn commit_mapping_and_recovery(
        &mut self,
        m: &MappingRecord,
        recovery_case: Option<&RecoveryCaseRecord>,
    ) -> Result<(), ManagerError> {
        let state_json = serde_json::to_string(&m.state)
            .map_err(|e| storage_err("Failed to serialize MappingState", e))?;
        let last_outcome_json = m
            .last_run_outcome
            .map(|o| serde_json::to_string(&o))
            .transpose()
            .map_err(|e| storage_err("Failed to serialize last_run_outcome", e))?;
        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("Failed to start mapping/recovery transaction", e))?;

        tx.execute(
            "UPDATE mappings SET
                scheduling_enabled = ?2,
                state_json = ?3,
                intent = ?4,
                readiness = ?5,
                execution = ?6,
                active_generation_id = ?7,
                last_run_id = ?8,
                last_run_outcome = ?9,
                last_run_completed_at = ?10,
                last_run_summary_text = ?11,
                unreviewed_conflict_run_id = ?12,
                updated_at = ?13
             WHERE id = ?1",
            params![
                m.id.as_str(),
                i64::from(m.scheduling_enabled),
                state_json,
                intent_str(m.state.intent),
                readiness_str(m.state.readiness()),
                execution_str(m.state.execution),
                m.active_generation_id.as_ref().map(|g| g.as_str()),
                m.last_run_id.as_ref().map(|r| r.as_str()),
                last_outcome_json,
                m.last_run_completed_at,
                m.last_run_summary_text,
                m.unreviewed_conflict_run_id.as_ref().map(|r| r.as_str()),
                m.updated_at,
            ],
        )
        .map_err(|e| storage_err("Failed to update mapping state in transaction", e))?;

        if let Some(case) = recovery_case {
            tx.execute(
                "INSERT INTO recovery_cases (
                    id, mapping_id, run_id, status, reason_kind, cause, next_step,
                    workdir_path, backup_dir1, backup_dir2, log_path, orphan_pid,
                    created_at, resolved_at, resolution_note
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    case.id,
                    case.mapping_id.as_str(),
                    case.run_id.as_ref().map(|r| r.as_str()),
                    case.status,
                    case.reason_kind,
                    case.cause,
                    case.next_step,
                    case.workdir_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.backup_dir1
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.backup_dir2,
                    case.log_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    case.orphan_pid.map(i64::from),
                    case.created_at,
                    case.resolved_at,
                    case.resolution_note,
                ],
            )
            .map_err(|e| storage_err("Failed to insert recovery case in transaction", e))?;
        }

        tx.commit()
            .map_err(|e| storage_err("Failed to commit mapping/recovery transaction", e))?;
        Ok(())
    }

    /// Atomically marks all open recovery cases for `m.id` resolved and updates
    /// the mapping's runtime state in a single SQLite transaction.
    pub fn resolve_recovery_and_update_mapping(
        &mut self,
        m: &MappingRecord,
        resolved_at: &str,
        resolution_note: &str,
    ) -> Result<(), ManagerError> {
        let state_json = serde_json::to_string(&m.state)
            .map_err(|e| storage_err("Failed to serialize MappingState", e))?;
        let last_outcome_json = m
            .last_run_outcome
            .map(|o| serde_json::to_string(&o))
            .transpose()
            .map_err(|e| storage_err("Failed to serialize last_run_outcome", e))?;
        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("Failed to start resolve-recovery transaction", e))?;

        tx.execute(
            "UPDATE recovery_cases
             SET status = 'resolved', resolved_at = ?2, resolution_note = ?3
             WHERE mapping_id = ?1 AND status = 'open'",
            params![m.id.as_str(), resolved_at, resolution_note],
        )
        .map_err(|e| storage_err("Failed to resolve open recovery cases", e))?;

        tx.execute(
            "UPDATE mappings SET
                scheduling_enabled = ?2,
                state_json = ?3,
                intent = ?4,
                readiness = ?5,
                execution = ?6,
                active_generation_id = ?7,
                last_run_id = ?8,
                last_run_outcome = ?9,
                last_run_completed_at = ?10,
                last_run_summary_text = ?11,
                unreviewed_conflict_run_id = ?12,
                updated_at = ?13
             WHERE id = ?1",
            params![
                m.id.as_str(),
                i64::from(m.scheduling_enabled),
                state_json,
                intent_str(m.state.intent),
                readiness_str(m.state.readiness()),
                execution_str(m.state.execution),
                m.active_generation_id.as_ref().map(|g| g.as_str()),
                m.last_run_id.as_ref().map(|r| r.as_str()),
                last_outcome_json,
                m.last_run_completed_at,
                m.last_run_summary_text,
                m.unreviewed_conflict_run_id.as_ref().map(|r| r.as_str()),
                m.updated_at,
            ],
        )
        .map_err(|e| storage_err("Failed to update mapping state after recovery", e))?;

        tx.commit()
            .map_err(|e| storage_err("Failed to commit resolve-recovery transaction", e))?;
        Ok(())
    }

    /// Test helper to insert a synthetic schema version row for downgrade checks.
    #[cfg(test)]
    pub(crate) fn insert_test_schema_version(&self, version: i64, name: &str) {
        self.conn
            .execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, '2026-11-20T00:00:00Z')",
                params![version, name],
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn refuses_newer_schema_version_on_open() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("state.sqlite");
        {
            let store = SqliteStateStore::open(&db).unwrap();
            store.insert_test_schema_version(LATEST_SCHEMA_VERSION + 1, "future_migration");
        }
        let err = SqliteStateStore::open(&db).err().unwrap();
        assert!(matches!(
            err,
            ManagerError::UnsupportedSchemaVersion { found, max_supported, .. }
            if found == LATEST_SCHEMA_VERSION + 1 && max_supported == LATEST_SCHEMA_VERSION
        ));
    }

    #[test]
    fn commit_run_terminal_rolls_back_atomically_on_constraint_error() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("state.sqlite");
        let mut store = SqliteStateStore::open(&db).unwrap();

        let dev = DeviceRecord {
            id: DeviceId::new("dev-1"),
            name: "test".to_string(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
        };
        store.ensure_device(&dev).unwrap();
        store
            .upsert_account(&AccountRecord {
                id: AccountId::new("acc-1"),
                display_name: "Local".to_string(),
                kind: "local_fixture".to_string(),
                remote_name: "local".to_string(),
                remote_root_folder_id: "root".to_string(),
                config_path: tmp.path().join("rclone.conf"),
                auth_status: "ready".to_string(),
                created_at: "2026-11-20T00:00:00Z".to_string(),
                updated_at: "2026-11-20T00:00:00Z".to_string(),
            })
            .unwrap();
        let mapping = MappingRecord {
            id: MappingId::new("m1"),
            device_id: dev.id.clone(),
            account_id: AccountId::new("acc-1"),
            collection: CloudCollection {
                display_path: "Data/Research".to_string(),
                folder_id: "col-1".to_string(),
                ancestor_ids: vec![],
            },
            local_binding: LocalBinding {
                volume: VolumeIdentity::new(VolumeKind::Internal, "vol-1"),
                mount_path: PathBuf::from("/tmp"),
                relative_dir: PathBuf::from("research"),
            },
            remote_endpoint_kind: "local_dir".to_string(),
            remote_endpoint_value: "/tmp/remote".to_string(),
            local_recovery_root: PathBuf::from("/tmp/rec1"),
            cloud_recovery_path: "/tmp/rec2".to_string(),
            schedule_interval_secs: 300,
            scheduling_enabled: false,
            state: MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled),
            active_generation_id: None,
            exclusions: vec![],
            max_delete_percent: 10,
            last_run_id: None,
            last_run_outcome: None,
            last_run_completed_at: None,
            last_run_summary_text: None,
            unreviewed_conflict_run_id: None,
            created_at: "2026-11-20T00:00:00Z".to_string(),
            updated_at: "2026-11-20T00:00:00Z".to_string(),
        };
        store.insert_mapping(&mapping).unwrap();
        let gen_rec = GenerationRecord {
            id: GenerationId::new("g1"),
            mapping_id: mapping.id.clone(),
            generation_number: 1,
            fingerprint: "0".repeat(64),
            config: ConfigGeneration {
                path1: PathBuf::from("/tmp/research"),
                path2: "/tmp/remote".to_string(),
                remote_root_folder_id: "root".to_string(),
                selected_folder_id: "col-1".to_string(),
                local_binding: mapping.local_binding.clone(),
                effective_filters: Default::default(),
                compare_policy: sync_core::ComparePolicy::SizeModtime,
                supported_types: sync_core::SupportedTypesPolicy::default(),
                conflict_policy: sync_core::ConflictPolicy::default(),
                recovery_policy: sync_core::RecoveryPolicy::default(),
                engine_compatibility: sync_core::EngineCompatibility {
                    engine_name: "rclone".to_string(),
                    engine_version: "1.76.0".to_string(),
                    profile_version: "v1".to_string(),
                },
            },
            initial_direction: Some("path1".to_string()),
            workdir_path: tmp.path().join("wd"),
            filters_file_path: tmp.path().join("filters.txt"),
            created_at: "2026-11-20T00:00:00Z".to_string(),
        };
        store.insert_generation(&gen_rec).unwrap();
        let mut run = RunRecord {
            id: RunId::new("run-1"),
            mapping_id: mapping.id.clone(),
            generation_id: gen_rec.id.clone(),
            mode: "normal".to_string(),
            status: "running".to_string(),
            started_at: "2026-11-20T00:00:00Z".to_string(),
            finished_at: None,
            workdir_path: gen_rec.workdir_path,
            backup_dir1: tmp.path().join("b1"),
            backup_dir2: tmp.path().join("b2").to_string_lossy().into_owned(),
            run_dir: tmp.path().join("r1"),
            log_path: tmp.path().join("r1/output.ndjson"),
            child_pid: Some(1234),
            child_start_time: None,
            child_executable: None,
            engine_version: None,
            engine_sha256: None,
            engine_platform: None,
            exit_code: None,
            exit_signal: None,
            outcome_kind: None,
            outcome_detail: None,
            conflict_paths: vec![],
            last_verified_phase: "spawned".to_string(),
            files_checked: 0,
            files_transferred: 0,
            bytes_transferred: 0,
            conflict_count: 0,
        };
        store.insert_run_start(&run).unwrap();

        // Force a foreign-key violation on recovery_case.mapping_id -> entire transaction must roll back!
        run.status = "interrupted".to_string();
        let mut updated_mapping = mapping.clone();
        updated_mapping.state = MappingState::new(
            Readiness::RecoveryRequired,
            Execution::Interrupted,
            Intent::Enabled,
        );
        let bad_case = RecoveryCaseRecord {
            id: "rc-bad".to_string(),
            mapping_id: MappingId::new("nonexistent-mapping"),
            run_id: Some(run.id.clone()),
            status: "open".to_string(),
            reason_kind: "interrupted".to_string(),
            cause: "test".to_string(),
            next_step: "test".to_string(),
            workdir_path: None,
            backup_dir1: None,
            backup_dir2: None,
            log_path: None,
            orphan_pid: None,
            created_at: "2026-11-20T00:01:00Z".to_string(),
            resolved_at: None,
            resolution_note: None,
        };
        assert!(
            store
                .commit_run_terminal(&run, Some(&updated_mapping), Some(&bad_case))
                .is_err()
        );

        // Verify neither `runs` nor `mappings` was partially mutated.
        assert_eq!(store.get_run(&run.id).unwrap().unwrap().status, "running");
        assert_eq!(
            store
                .get_mapping(&mapping.id)
                .unwrap()
                .unwrap()
                .state
                .readiness(),
            Readiness::Ready
        );
    }
}
