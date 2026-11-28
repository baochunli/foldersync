//! Immutable status snapshots, coalescing snapshot subscriptions, and bounded
//! manager event stream (§16).

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sync_core::{
    AccountId, AvailableAction, Execution, GenerationId, Intent, MappingId, Readiness, RunId,
    RunOutcome, VolumeKind,
};

use crate::platform::DiskSpaceInfo;

/// Default capacity of the bounded in-memory event ring buffer.
pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 512;

/// Summary counters for a mapping snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingCounters {
    pub files_checked: u64,
    pub files_transferred: u64,
    pub bytes_transferred: u64,
    pub conflict_count: u64,
    pub open_recovery_cases: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedFirstSync {
    pub files_copied: u64,
    pub total_files: Option<u64>,
    pub cause: Option<String>,
}

impl StoppedFirstSync {
    pub fn status_text(&self) -> String {
        match self.total_files {
            Some(total) => format!(
                "First sync stopped · {} of {total} files copied",
                self.files_copied.min(total)
            ),
            None => format!("First sync stopped · {} files copied", self.files_copied),
        }
    }
}

/// Immutable per-mapping view published in [`StatusSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingStatusSnapshot {
    pub id: MappingId,
    pub display_name: String,
    pub account_id: AccountId,
    pub collection_display_path: String,
    #[serde(default)]
    pub collection_folder_id: String,
    pub local_root: PathBuf,
    pub volume_name: String,
    #[serde(default = "default_volume_kind")]
    pub volume_kind: VolumeKind,
    #[serde(default)]
    pub volume_stable_id: String,
    /// Capacity of the matching mounted volume, if it is currently visible.
    #[serde(default)]
    pub volume_space: Option<DiskSpaceInfo>,
    #[serde(default)]
    pub volume_filesystem: Option<String>,
    pub readiness: Readiness,
    pub execution: Execution,
    pub intent: Intent,
    pub scheduling_enabled: bool,
    #[serde(default)]
    pub filter_migration_held: bool,
    #[serde(default)]
    pub filter_migration_phase: Option<crate::manager::FilterMigrationPhase>,
    #[serde(default = "default_schedule_interval_secs")]
    pub schedule_interval_secs: u64,
    #[serde(default)]
    pub next_poll_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub retry_at: Option<DateTime<Utc>>,
    pub active_generation_id: Option<GenerationId>,
    pub active_generation_fingerprint: Option<String>,
    pub last_run_id: Option<RunId>,
    pub last_run_outcome: Option<RunOutcome>,
    pub last_run_completed_at: Option<String>,
    pub last_run_summary: Option<String>,
    #[serde(default)]
    pub stopped_first_sync: Option<StoppedFirstSync>,
    /// Run ID whose conflict copies have not yet been marked reviewed by the user.
    #[serde(default)]
    pub unreviewed_conflict_run_id: Option<RunId>,
    /// Observed conflict copy paths from the unreviewed conflict run (if any).
    #[serde(default)]
    pub unreviewed_conflict_paths: Vec<String>,
    #[serde(default)]
    pub exclusions: Vec<String>,
    pub counters: MappingCounters,
    pub available_actions: Vec<AvailableAction>,
    pub status_text: String,
    pub local_recovery_root: PathBuf,
    pub cloud_recovery_path: String,
}

fn default_volume_kind() -> VolumeKind {
    VolumeKind::Internal
}

fn default_schedule_interval_secs() -> u64 {
    300
}

/// Latest result for a requested Drive folder size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSizeSnapshot {
    pub account_id: AccountId,
    pub folder_id: String,
    pub updated_at: DateTime<Utc>,
    pub status: FolderSizeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderSizeStatus {
    Calculating,
    Known { bytes: u64, files: u64 },
    Failed { cause: String },
}

/// Local archive preparation and retained-original status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveState {
    Pending,
    Creating { bytes: u64, total: u64 },
    Failed { cause: String },
    Complete,
}

/// One confirmed archive and its retained original, where available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveStatus {
    pub mapping_id: MappingId,
    pub folder: String,
    pub state: ArchiveState,
    pub holding_path: Option<PathBuf>,
    pub original_bytes: Option<u64>,
}

/// Immutable manager-wide snapshot published to UI/CLI subscribers (§16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub sequence: u64,
    pub generated_at: String,
    pub active_job_mapping_id: Option<MappingId>,
    /// Ordered list of queued mapping IDs (front of queue first).
    #[serde(default)]
    pub queue_order: Vec<MappingId>,
    /// Global automatic-scheduling pause hold (D1: manual Sync now still works).
    #[serde(default)]
    pub scheduling_paused: bool,
    pub mappings: Vec<MappingStatusSnapshot>,
    #[serde(default)]
    pub folder_sizes: Vec<FolderSizeSnapshot>,
    #[serde(default)]
    pub archives: Vec<ArchiveStatus>,
}

/// Bounded activity and diagnostic events emitted by the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerEvent {
    SnapshotUpdated {
        sequence: u64,
        timestamp: String,
    },
    MappingAdded {
        mapping_id: MappingId,
        timestamp: String,
    },
    RunStarted {
        mapping_id: MappingId,
        run_id: RunId,
        mode: String,
        timestamp: String,
    },
    RunOutputLine {
        mapping_id: MappingId,
        run_id: RunId,
        line: String,
        timestamp: String,
    },
    RunProgress {
        mapping_id: MappingId,
        run_id: RunId,
        bytes_transferred: u64,
        #[serde(default)]
        bytes_already_present: u64,
        #[serde(default)]
        expected_bytes: Option<u64>,
        #[serde(default)]
        expected_files: Option<u64>,
        files_transferred: u64,
        #[serde(default)]
        files_already_present: u64,
        #[serde(default)]
        first_sync_total_files: Option<u64>,
        files_checked: u64,
        timestamp: String,
    },
    RunFinished {
        mapping_id: MappingId,
        run_id: RunId,
        outcome: String,
        timestamp: String,
    },
    RecoveryOpened {
        mapping_id: MappingId,
        case_id: String,
        reason_kind: String,
        cause: String,
        timestamp: String,
    },
    PreflightFailed {
        mapping_id: MappingId,
        cause: String,
        next_step: String,
        timestamp: String,
    },
    ShutdownTimedOut {
        mapping_id: MappingId,
        run_id: RunId,
        timestamp: String,
    },
}

/// Coalescing snapshot subscription that NEVER drops the newest snapshot.
///
/// When a subscriber falls behind and its bounded buffer is full, the oldest
/// superseded snapshot is evicted so the latest [`StatusSnapshot`] is always
/// delivered promptly.
#[derive(Debug)]
pub struct SnapshotReceiver {
    shared: Arc<SnapshotSubShared>,
}

#[derive(Debug)]
struct SnapshotSubShared {
    state: Mutex<SnapshotSubState>,
    cv: Condvar,
}

#[derive(Debug)]
struct SnapshotSubState {
    pending: VecDeque<Arc<StatusSnapshot>>,
    latest: Option<Arc<StatusSnapshot>>,
    capacity: usize,
    sender_closed: bool,
    receiver_dropped: bool,
}

impl SnapshotReceiver {
    fn new(capacity: usize) -> (Arc<SnapshotSubShared>, Self) {
        let shared = Arc::new(SnapshotSubShared {
            state: Mutex::new(SnapshotSubState {
                pending: VecDeque::with_capacity(capacity.max(1)),
                latest: None,
                capacity: capacity.max(1),
                sender_closed: false,
                receiver_dropped: false,
            }),
            cv: Condvar::new(),
        });
        (
            Arc::clone(&shared),
            Self {
                shared: Arc::clone(&shared),
            },
        )
    }

    /// Blocks until the next snapshot arrives or the manager closes.
    pub fn recv(&self) -> Result<Arc<StatusSnapshot>, mpsc::RecvError> {
        let mut guard = self.shared.state.lock().unwrap();
        loop {
            if let Some(snap) = guard.pending.pop_front() {
                return Ok(snap);
            }
            if guard.sender_closed {
                return Err(mpsc::RecvError);
            }
            guard = self.shared.cv.wait(guard).unwrap();
        }
    }

    /// Waits up to `timeout` for the next snapshot.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Arc<StatusSnapshot>, mpsc::RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.shared.state.lock().unwrap();
        loop {
            if let Some(snap) = guard.pending.pop_front() {
                return Ok(snap);
            }
            if guard.sender_closed {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            let (next_guard, wait_res) = self.shared.cv.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if let Some(snap) = guard.pending.pop_front() {
                return Ok(snap);
            }
            if guard.sender_closed {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            if wait_res.timed_out() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
        }
    }

    /// Returns the next buffered snapshot immediately without blocking.
    pub fn try_recv(&self) -> Result<Arc<StatusSnapshot>, mpsc::TryRecvError> {
        let mut guard = self.shared.state.lock().unwrap();
        if let Some(snap) = guard.pending.pop_front() {
            Ok(snap)
        } else if guard.sender_closed {
            Err(mpsc::TryRecvError::Disconnected)
        } else {
            Err(mpsc::TryRecvError::Empty)
        }
    }

    /// Drains any older buffered snapshots and returns the newest available snapshot, if any.
    pub fn drain_latest(&self) -> Option<Arc<StatusSnapshot>> {
        let mut guard = self.shared.state.lock().unwrap();
        let last = guard.pending.back().cloned();
        guard.pending.clear();
        last
    }

    /// Returns the most recently published snapshot ever seen by this subscription.
    pub fn latest(&self) -> Option<Arc<StatusSnapshot>> {
        self.shared.state.lock().unwrap().latest.clone()
    }
}

impl Drop for SnapshotReceiver {
    fn drop(&mut self) {
        let mut guard = self.shared.state.lock().unwrap();
        guard.receiver_dropped = true;
    }
}

#[derive(Debug)]
pub(crate) struct EventBus {
    capacity: usize,
    inner: Mutex<EventBusInner>,
}

#[derive(Debug)]
struct EventBusInner {
    recent: VecDeque<ManagerEvent>,
    event_subscribers: Vec<mpsc::SyncSender<ManagerEvent>>,
    snapshot_subscribers: Vec<Arc<SnapshotSubShared>>,
}

impl EventBus {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(EventBusInner {
                recent: VecDeque::with_capacity(capacity.max(1)),
                event_subscribers: Vec::new(),
                snapshot_subscribers: Vec::new(),
            }),
        }
    }

    pub(crate) fn publish_event(&self, event: ManagerEvent) {
        let mut guard = self.inner.lock().unwrap();
        while guard.recent.len() >= self.capacity {
            guard.recent.pop_front();
        }
        guard.recent.push_back(event.clone());
        guard.event_subscribers.retain(|tx| {
            !matches!(
                tx.try_send(event.clone()),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        });
    }

    /// Publishes `snapshot` to all subscribers, coalescing by dropping the
    /// oldest buffered snapshot when a subscriber's queue is full so the latest
    /// snapshot is NEVER dropped.
    pub(crate) fn publish_snapshot(&self, snapshot: Arc<StatusSnapshot>) {
        let mut guard = self.inner.lock().unwrap();
        guard.snapshot_subscribers.retain(|sub| {
            let mut st = sub.state.lock().unwrap();
            if st.receiver_dropped {
                return false;
            }
            while st.pending.len() >= st.capacity {
                st.pending.pop_front();
            }
            st.pending.push_back(Arc::clone(&snapshot));
            st.latest = Some(Arc::clone(&snapshot));
            sub.cv.notify_one();
            true
        });
    }

    pub(crate) fn recent_events(&self) -> Vec<ManagerEvent> {
        self.inner.lock().unwrap().recent.iter().cloned().collect()
    }

    pub(crate) fn subscribe_events(&self, bound: usize) -> mpsc::Receiver<ManagerEvent> {
        let (tx, rx) = mpsc::sync_channel(bound.max(1));
        self.inner.lock().unwrap().event_subscribers.push(tx);
        rx
    }

    pub(crate) fn subscribe_snapshots(&self, bound: usize) -> SnapshotReceiver {
        let (shared, rx) = SnapshotReceiver::new(bound.max(1));
        self.inner.lock().unwrap().snapshot_subscribers.push(shared);
        rx
    }
}

impl Drop for EventBus {
    fn drop(&mut self) {
        if let Ok(guard) = self.inner.lock() {
            for sub in &guard.snapshot_subscribers {
                if let Ok(mut st) = sub.state.lock() {
                    st.sender_closed = true;
                    sub.cv.notify_all();
                }
            }
        }
    }
}
