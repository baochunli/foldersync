//! Core domain model, policies, configuration generation fingerprinting,
//! validation rules, and mapping state machine for FolderSync.
//!
//! `sync-core` contains pure logic only: it performs no process spawning,
//! filesystem writes, database access, or UI operations. Filesystem path
//! canonicalization is injected through [`PathResolver`].

#![deny(missing_docs)]

pub mod generation;
pub mod model;
pub mod policy;
pub mod state;
pub mod validation;

pub use generation::{
    ConfigGeneration, EngineCompatibility, GENERATION_FINGERPRINT_VERSION, GenerationFingerprint,
};
pub use model::{
    AccountId, CloudCollection, DEFAULT_ACTIVE_SCHEDULE_INTERVAL,
    DEFAULT_ARCHIVE_SCHEDULE_INTERVAL, DeviceId, GenerationId, LocalBinding, Mapping, MappingId,
    RunId, VolumeIdentity, VolumeKind,
};
pub use policy::{
    ACCESS_MARKER_FILENAME, ACCESS_MARKER_HEADER, AccessMarker, AccessMarkerError, ComparePolicy,
    ConflictLoserNaming, ConflictPolicy, ConflictResolution, DEFAULT_EXCLUSIONS,
    DEFAULT_MAX_DELETE_PERCENT, DefaultExclusion, MaxDeletePercent, RecoveryPolicy,
    SUPPORTED_FILE_POLICY_TABLE, SupportDisposition, SupportedFilePolicyEntry, SupportedItemKind,
    SupportedTypesPolicy, default_exclusion_patterns,
};
pub use state::{
    AvailableAction, Execution, Intent, LastRunSummary, MappingState, ObservedEvent,
    PostRecoveryReadiness, Readiness, RunOutcome, TransitionEffect, TransitionResult, UserAction,
    WallTime, apply_event, apply_user_action, available_actions, status_text,
};
pub use validation::{
    DeviceCloudCollection, LocalLayoutEntry, MAX_COMPONENT_BYTES, NameFinding, NameFindingKind,
    PathResolutionError, PathResolver, RESERVED_FILENAME_CHARS, StdPathResolver, ValidationError,
    ValidationErrorKind, validate_cloud_collection_pair, validate_cloud_collections,
    validate_local_binding, validate_local_layout, validate_local_roots_disjoint, validate_names,
};
