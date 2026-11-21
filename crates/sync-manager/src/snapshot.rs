//! Immutable status snapshots and bounded manager event stream (§16).

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
};

use serde::{Deserialize, Serialize};
use sync_core::{
    AccountId, AvailableAction, Execution, GenerationId, Intent, MappingId, Readiness, RunId,
    RunOutcome,
};

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

/// Immutable per-mapping view published in [`StatusSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingStatusSnapshot {
    pub id: MappingId,
    pub display_name: String,
    pub account_id: AccountId,
    pub collection_display_path: String,
    pub local_root: PathBuf,
    pub volume_name: String,
    pub readiness: Readiness,
    pub execution: Execution,
    pub intent: Intent,
    pub scheduling_enabled: bool,
    pub active_generation_id: Option<GenerationId>,
    pub active_generation_fingerprint: Option<String>,
    pub last_run_id: Option<RunId>,
    pub last_run_outcome: Option<RunOutcome>,
    pub last_run_completed_at: Option<String>,
    pub last_run_summary: Option<String>,
    pub counters: MappingCounters,
    pub available_actions: Vec<AvailableAction>,
    pub status_text: String,
    pub local_recovery_root: PathBuf,
    pub cloud_recovery_path: String,
}

/// Immutable manager-wide snapshot published to UI/CLI subscribers (§16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub sequence: u64,
    pub generated_at: String,
    pub active_job_mapping_id: Option<MappingId>,
    pub mappings: Vec<MappingStatusSnapshot>,
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
    snapshot_subscribers: Vec<mpsc::SyncSender<Arc<StatusSnapshot>>>,
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

    pub(crate) fn publish_snapshot(&self, snapshot: Arc<StatusSnapshot>) {
        let mut guard = self.inner.lock().unwrap();
        guard.snapshot_subscribers.retain(|tx| {
            !matches!(
                tx.try_send(Arc::clone(&snapshot)),
                Err(mpsc::TrySendError::Disconnected(_))
            )
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

    pub(crate) fn subscribe_snapshots(&self, bound: usize) -> mpsc::Receiver<Arc<StatusSnapshot>> {
        let (tx, rx) = mpsc::sync_channel(bound.max(1));
        self.inner.lock().unwrap().snapshot_subscribers.push(tx);
        rx
    }
}
