//! Coordinates durable state, exclusive locking, run records, crash/orphan
//! recovery, initialization, manual runs, status snapshots, and Finder status
//! publication/action handling for FolderSync.

pub mod accounts;
mod archive_setup;
mod archive_transaction;
mod archive_writer;
pub mod clock;
pub mod error;
pub mod finder;
pub mod init;
pub mod launcher;
pub mod manager;
pub mod platform;
mod scheduler;
pub mod secrets;
pub mod snapshot;
pub mod store;

pub use accounts::{
    AccountCheckReport, AccountError, AccountService, AccountServiceConfig, AddDriveAccountRequest,
    CollectionCandidate, CollectionListing, CollectionSkipReason, DriveAccountResult,
    DriveAuthorization, ImportAccountRequest, PasswordCommandFactory,
    RESERVED_LOCAL_FIXTURE_ACCOUNT_ID, SkippedCollection,
};
pub use archive_transaction::ArchivedOriginal;
pub use clock::{Clock, FixedClock, SystemClock};
pub use error::ManagerError;
pub use finder::{
    ClaimedFinderAction, DARWIN_ACTION_REQUESTED_NOTIFICATION,
    DARWIN_SNAPSHOT_UPDATED_NOTIFICATION, DarwinFinderNotifier, FINDER_SNAPSHOT_VERSION,
    FinderActionFileOutcome, FinderActionKind, FinderActionRequest, FinderMappingSnapshot,
    FinderMappingState, FinderNotifier, FinderStatusSnapshot, FinderStatusWriter,
    NoopFinderNotifier, RecordingFinderNotifier, ValidatedFinderAction, claim_finder_action_files,
    derive_finder_state_with_pause, drain_finder_action_files, parse_and_validate_finder_action,
};
pub use init::{
    DetailedEndpointInventory, DetectedVolume, DiskSpaceInfo, EndpointInventory,
    SideClassification, SubfolderInventory, detect_volume_identity, disk_space_for_path,
    inspect_local_endpoint_detailed,
};
pub use launcher::{
    BisyncLauncher, FakeBisyncLauncher, FakeLaunchScript, LaunchPlan, LaunchedRun,
    RealBisyncLauncher,
};
pub use manager::{
    AddMappingCommand, CommandAcceptance, CommandOutcome, ControlledShutdownOutcome,
    ExclusionMatch, FilterChangePreview, FilterMigrationPhase, FinderActionDispatch,
    InitializeCommand, ManagerCommand, ManagerConfig, OperationCanceller, OperationHandle,
    PreflightPreview, RecoveryCaseView, RecoveryCopyEntry, RecoveryCopySide, RecoveryReport,
    RemoteEndpoint, SyncManager,
};
pub use platform::{
    FakeProcessInspector, LiveProcessInfo, MountedVolume, OsProcessInspector, OsVolumeProbe,
    ProcessInspector, ProcessProbeResult, StateLock, VolumeProbe, resolve_state_dir,
};
pub use scheduler::LocalChangeWatcher;
#[cfg(target_os = "macos")]
pub use secrets::KeychainSecretStore;
pub use secrets::{
    InMemorySecretStore, KEYCHAIN_SERVICE, SecretStore, SecretStoreError, SecretString,
    SystemSecretStore, run_credential_helper, scoped_credential_key, validate_credential_key,
};
pub use snapshot::{
    ArchiveState, ArchiveStatus, FolderSizeSnapshot, FolderSizeStatus, ManagerEvent,
    MappingCounters, MappingStatusSnapshot, SnapshotReceiver, StatusSnapshot,
};
pub use store::{
    AccountRecord, DeviceRecord, GenerationRecord, MappingRecord, RecoveryCaseRecord, RunRecord,
    SqliteStateStore,
};
