//! Coordinates durable state, exclusive locking, run records, crash/orphan
//! recovery, initialization, manual runs, status snapshots, and Finder status
//! publication for FolderSync.

pub mod clock;
pub mod error;
pub mod finder;
pub mod init;
pub mod launcher;
pub mod manager;
pub mod platform;
pub mod snapshot;
pub mod store;

pub use clock::{Clock, FixedClock, SystemClock};
pub use error::ManagerError;
pub use finder::{
    FINDER_SNAPSHOT_VERSION, FinderMappingSnapshot, FinderMappingState, FinderStatusSnapshot,
    FinderStatusWriter,
};
pub use init::{EndpointInventory, SideClassification};
pub use launcher::{
    BisyncLauncher, FakeBisyncLauncher, FakeLaunchScript, LaunchPlan, LaunchedRun,
    RealBisyncLauncher,
};
pub use manager::{
    AddMappingCommand, CommandAcceptance, CommandOutcome, InitializeCommand, ManagerCommand,
    ManagerConfig, RecoveryCaseView, RecoveryReport, RemoteEndpoint, SyncManager,
};
pub use platform::{
    FakeProcessInspector, LiveProcessInfo, OsProcessInspector, ProcessInspector,
    ProcessProbeResult, StateLock, resolve_state_dir,
};
pub use snapshot::{ManagerEvent, MappingCounters, MappingStatusSnapshot, StatusSnapshot};
pub use store::{
    AccountRecord, DeviceRecord, GenerationRecord, MappingRecord, RecoveryCaseRecord, RunRecord,
    SqliteStateStore,
};
