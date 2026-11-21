//! Coordinates durable state, exclusive locking, run records, crash/orphan
//! recovery, initialization, manual runs, status snapshots, and Finder status
//! publication/action handling for FolderSync.

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
    DARWIN_ACTION_REQUESTED_NOTIFICATION, DARWIN_SNAPSHOT_UPDATED_NOTIFICATION,
    DarwinFinderNotifier, FINDER_SNAPSHOT_VERSION, FinderActionFileOutcome, FinderActionKind,
    FinderActionRequest, FinderMappingSnapshot, FinderMappingState, FinderNotifier,
    FinderStatusSnapshot, FinderStatusWriter, RecordingFinderNotifier, ValidatedFinderAction,
    drain_finder_action_files, parse_and_validate_finder_action,
};
pub use init::{
    DetailedEndpointInventory, DiskSpaceInfo, EndpointInventory, SideClassification,
    SubfolderInventory, disk_space_for_path, inspect_local_endpoint_detailed,
};
pub use launcher::{
    BisyncLauncher, FakeBisyncLauncher, FakeLaunchScript, LaunchPlan, LaunchedRun,
    RealBisyncLauncher,
};
pub use manager::{
    AddMappingCommand, CommandAcceptance, CommandOutcome, ControlledShutdownOutcome,
    FinderActionDispatch, InitializeCommand, ManagerCommand, ManagerConfig, RecoveryCaseView,
    RecoveryCopyEntry, RecoveryCopySide, RecoveryReport, RemoteEndpoint, SyncManager,
};
pub use platform::{
    FakeProcessInspector, LiveProcessInfo, OsProcessInspector, ProcessInspector,
    ProcessProbeResult, StateLock, resolve_state_dir,
};
pub use snapshot::{
    ManagerEvent, MappingCounters, MappingStatusSnapshot, SnapshotReceiver, StatusSnapshot,
};
pub use store::{
    AccountRecord, DeviceRecord, GenerationRecord, MappingRecord, RecoveryCaseRecord, RunRecord,
    SqliteStateStore,
};
