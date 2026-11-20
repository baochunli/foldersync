//! Outcome classification combining the pinned rclone exit-code table with
//! parsed diagnostic log events (§14).

use crate::{
    log_parser::{BisyncMarker, LogEvent, LogLevel},
    process::ExitStatusInfo,
};

/// Rclone version that the exit-code table below is qualified against.
pub const EXIT_CODE_TABLE_VERSION: &str = crate::PINNED_RCLONE_VERSION;

/// Engine-specific exit code meanings for pinned rclone v1.76.0.
/// Kept in one table so engine upgrades require auditing a single place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcloneExitCode {
    Success,            // 0
    Error,              // 1: Error not otherwise categorised
    SyntaxOrUsage,      // 2: Syntax or usage error
    DirectoryNotFound,  // 3: Directory not found
    FileNotFound,       // 4: File not found
    TemporaryError,     // 5: Temporary error (retries might fix)
    LessSeriousError,   // 6: Less serious errors
    FatalError,         // 7: Fatal error
    TransferLimit,      // 8: --max-transfer reached
    NoFilesTransferred, // 9: Successful, no files transferred
    DurationLimit,      // 10: --max-duration reached
    InterruptedSigint,  // 130: 128 + SIGINT (2)
    KilledSigkill,      // 137: 128 + SIGKILL (9)
    TerminatedSigterm,  // 143: 128 + SIGTERM (15)
    Unrecognized(i32),
}

/// High-level run outcome consumed by the manager (§14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    SuccessWithConflicts {
        paths: Vec<String>,
    },
    Retryable {
        reason: String,
    },
    AuthFailure {
        reason: String,
    },
    AccessCheckFailed {
        reason: String,
    },
    EndpointMismatch {
        reason: String,
    },
    DeletionGuardAborted {
        reason: String,
    },
    InvalidConfig {
        reason: String,
    },
    Critical {
        requires_resync: bool,
        reason: String,
    },
    Interrupted {
        graceful: bool,
        needs_recovery: bool,
        reason: String,
    },
    Unknown {
        exit_status: ExitStatusInfo,
        reason: String,
    },
}

/// Maps a numeric rclone exit code using the pinned v1.76.0 table.
pub fn classify_exit_code(code: i32) -> RcloneExitCode {
    match code {
        0 => RcloneExitCode::Success,
        1 => RcloneExitCode::Error,
        2 => RcloneExitCode::SyntaxOrUsage,
        3 => RcloneExitCode::DirectoryNotFound,
        4 => RcloneExitCode::FileNotFound,
        5 => RcloneExitCode::TemporaryError,
        6 => RcloneExitCode::LessSeriousError,
        7 => RcloneExitCode::FatalError,
        8 => RcloneExitCode::TransferLimit,
        9 => RcloneExitCode::NoFilesTransferred,
        10 => RcloneExitCode::DurationLimit,
        130 => RcloneExitCode::InterruptedSigint,
        137 => RcloneExitCode::KilledSigkill,
        143 => RcloneExitCode::TerminatedSigterm,
        other => RcloneExitCode::Unrecognized(other),
    }
}

#[derive(Default)]
struct DiagnosticSummary {
    conflict_copy_paths: Vec<String>,
    conflict_detected_paths: Vec<String>,
    max_delete: Option<String>,
    access_check: Option<String>,
    auth_failure: Option<String>,
    invalid_config: Option<String>,
    endpoint_mismatch: Option<String>,
    must_resync: Option<String>,
    critical_error: Option<String>,
    resilient_retryable: Option<String>,
    signal_received: Option<String>,
    graceful_shutdown_started: bool,
    graceful_shutdown_completed: bool,
    stats_fatal_error: bool,
    stats_retry_error: bool,
    stats_last_error: Option<String>,
    last_error_message: Option<String>,
    raw_invalid_usage: Option<String>,
    last_raw_line: Option<String>,
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

impl DiagnosticSummary {
    fn from_events(events: &[LogEvent]) -> Self {
        let mut s = Self::default();
        for event in events {
            match event {
                LogEvent::Raw(raw) => {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        let lower = trimmed.to_ascii_lowercase();
                        if lower.contains("unknown flag:")
                            || lower.contains("invalid argument")
                            || lower.contains("accepts 2 arg(s)")
                        {
                            s.raw_invalid_usage
                                .get_or_insert_with(|| trimmed.to_string());
                        }
                        s.last_raw_line = Some(trimmed.to_string());
                    }
                }
                LogEvent::Structured(ev) => {
                    if let Some(stats) = &ev.stats {
                        if stats.fatal_error {
                            s.stats_fatal_error = true;
                        }
                        if stats.retry_error {
                            s.stats_retry_error = true;
                        }
                        if let Some(last) = &stats.last_error {
                            s.stats_last_error = Some(last.clone());
                        }
                    }
                    if matches!(ev.level, LogLevel::Error | LogLevel::Critical) {
                        s.last_error_message = Some(ev.plain_msg.trim().to_string());
                    }
                    if let Some(marker) = &ev.marker {
                        match marker {
                            BisyncMarker::ConflictCopyCreated { path, .. } => {
                                push_unique(&mut s.conflict_copy_paths, path.clone());
                            }
                            BisyncMarker::ConflictDetected { path } => {
                                push_unique(&mut s.conflict_detected_paths, path.clone());
                            }
                            BisyncMarker::MaxDeleteAborted { detail } => {
                                s.max_delete.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::AccessCheckFailed { detail } => {
                                s.access_check.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::AuthFailure { detail } => {
                                s.auth_failure.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::InvalidConfig { detail } => {
                                s.invalid_config.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::EndpointMismatch { detail } => {
                                s.endpoint_mismatch.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::MustResync { detail } => {
                                s.must_resync.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::CriticalError { detail } => {
                                s.critical_error.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::RetryableDueToResilient { detail } => {
                                s.resilient_retryable.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::SignalReceived { detail } => {
                                s.signal_received.get_or_insert_with(|| detail.clone());
                            }
                            BisyncMarker::GracefulShutdownStarted => {
                                s.graceful_shutdown_started = true;
                            }
                            BisyncMarker::GracefulShutdownCompleted => {
                                s.graceful_shutdown_completed = true;
                            }
                            BisyncMarker::BisyncSuccessful => {}
                        }
                    }
                }
            }
        }
        s
    }

    fn fallback_reason(&self, exit_status: ExitStatusInfo) -> String {
        self.last_error_message
            .clone()
            .or_else(|| self.stats_last_error.clone())
            .or_else(|| self.last_raw_line.clone())
            .unwrap_or_else(|| {
                format!(
                    "rclone {EXIT_CODE_TABLE_VERSION} exited with code={:?}, signal={:?}",
                    exit_status.code, exit_status.signal
                )
            })
    }
}

/// Classifies a completed run by combining exit status, parsed log diagnostics,
/// and whether cancellation was requested by the runner.
pub fn classify_outcome(
    exit_status: ExitStatusInfo,
    events: &[LogEvent],
    stop_requested: bool,
) -> Outcome {
    let diag = DiagnosticSummary::from_events(events);
    let mapped_code = exit_status.code.map(classify_exit_code);

    let killed_by_signal = exit_status.signal.is_some();
    let exit_indicates_interrupt = matches!(
        mapped_code,
        Some(
            RcloneExitCode::InterruptedSigint
                | RcloneExitCode::KilledSigkill
                | RcloneExitCode::TerminatedSigterm
        )
    );
    let log_indicates_interrupt = diag.signal_received.is_some()
        || diag.graceful_shutdown_started
        || diag.graceful_shutdown_completed;

    // 1. Interruption / cancellation (never report non-zero cancelled run as generic error).
    if killed_by_signal
        || exit_indicates_interrupt
        || (!exit_status.is_success() && (stop_requested || log_indicates_interrupt))
    {
        let abrupt_kill = exit_status.signal == Some(9)
            || matches!(mapped_code, Some(RcloneExitCode::KilledSigkill));
        let graceful = diag.graceful_shutdown_completed && !abrupt_kill;
        let reason = diag
            .signal_received
            .clone()
            .unwrap_or_else(|| diag.fallback_reason(exit_status));
        return Outcome::Interrupted {
            graceful,
            needs_recovery: true,
            reason,
        };
    }

    // 2. Clean exit codes (0 or 9) unless diagnostics indicate a fatal abort.
    if matches!(
        mapped_code,
        Some(RcloneExitCode::Success | RcloneExitCode::NoFilesTransferred)
    ) && !diag.stats_fatal_error
        && diag.must_resync.is_none()
        && diag.critical_error.is_none()
    {
        let conflict_paths = if !diag.conflict_copy_paths.is_empty() {
            diag.conflict_copy_paths
        } else {
            diag.conflict_detected_paths
        };
        return if conflict_paths.is_empty() {
            Outcome::Success
        } else {
            Outcome::SuccessWithConflicts {
                paths: conflict_paths,
            }
        };
    }

    // 3. Specific diagnostic markers override coarse exit codes (e.g. exit 1 / exit 7).
    if let Some(reason) = &diag.max_delete {
        return Outcome::DeletionGuardAborted {
            reason: reason.clone(),
        };
    }
    if let Some(reason) = &diag.access_check {
        return Outcome::AccessCheckFailed {
            reason: reason.clone(),
        };
    }
    if let Some(reason) = &diag.auth_failure {
        return Outcome::AuthFailure {
            reason: reason.clone(),
        };
    }
    if let Some(reason) = diag
        .invalid_config
        .as_ref()
        .or(diag.raw_invalid_usage.as_ref())
    {
        return Outcome::InvalidConfig {
            reason: reason.clone(),
        };
    }
    if matches!(mapped_code, Some(RcloneExitCode::SyntaxOrUsage)) {
        return Outcome::InvalidConfig {
            reason: diag.fallback_reason(exit_status),
        };
    }
    if let Some(reason) = &diag.endpoint_mismatch {
        return Outcome::EndpointMismatch {
            reason: reason.clone(),
        };
    }
    if matches!(mapped_code, Some(RcloneExitCode::DirectoryNotFound))
        && diag.must_resync.is_none()
        && diag.critical_error.is_none()
    {
        return Outcome::EndpointMismatch {
            reason: diag.fallback_reason(exit_status),
        };
    }

    // 4. Critical / must-resync states take precedence over `--resilient` retry notices (§10).
    if let Some(reason) = &diag.must_resync {
        return Outcome::Critical {
            requires_resync: true,
            reason: reason.clone(),
        };
    }
    if let Some(reason) = &diag.critical_error {
        return Outcome::Critical {
            requires_resync: false,
            reason: reason.clone(),
        };
    }
    if matches!(mapped_code, Some(RcloneExitCode::FatalError)) || diag.stats_fatal_error {
        return Outcome::Critical {
            requires_resync: false,
            reason: diag.fallback_reason(exit_status),
        };
    }

    // 5. Retryable transient / less-serious failures.
    if matches!(
        mapped_code,
        Some(RcloneExitCode::TemporaryError | RcloneExitCode::LessSeriousError)
    ) || diag.stats_retry_error
        || diag.resilient_retryable.is_some()
    {
        return Outcome::Retryable {
            reason: diag
                .resilient_retryable
                .clone()
                .unwrap_or_else(|| diag.fallback_reason(exit_status)),
        };
    }

    // 6. Unclassified failure — retain exit status and best diagnostic text.
    Outcome::Unknown {
        exit_status,
        reason: diag.fallback_reason(exit_status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_parser::parse_log_stream;

    #[test]
    fn maps_pinned_rclone_exit_codes() {
        assert_eq!(classify_exit_code(0), RcloneExitCode::Success);
        assert_eq!(classify_exit_code(1), RcloneExitCode::Error);
        assert_eq!(classify_exit_code(2), RcloneExitCode::SyntaxOrUsage);
        assert_eq!(classify_exit_code(3), RcloneExitCode::DirectoryNotFound);
        assert_eq!(classify_exit_code(5), RcloneExitCode::TemporaryError);
        assert_eq!(classify_exit_code(7), RcloneExitCode::FatalError);
        assert_eq!(classify_exit_code(9), RcloneExitCode::NoFilesTransferred);
        assert_eq!(classify_exit_code(130), RcloneExitCode::InterruptedSigint);
        assert_eq!(classify_exit_code(99), RcloneExitCode::Unrecognized(99));
    }

    #[test]
    fn classifies_success_with_conflicts_even_when_exit_code_is_zero() {
        let events = parse_log_stream([
            r#"{"level":"notice","msg":"- WARNING  New or changed in both paths - a.txt","source":"bisync/deltas.go:386"}"#,
            r#"{"level":"notice","msg":"- Path1    Renaming Path1 copy - /tmp/p1/a.txt.conflict1","source":"bisync/resolve.go:318"}"#,
            r#"{"level":"notice","msg":"- Path2    Renaming Path2 copy - /tmp/p2/a.txt.conflict2","source":"bisync/resolve.go:318"}"#,
            r#"{"level":"info","msg":"Bisync successful","source":"bisync/operations.go:234"}"#,
        ]);
        let outcome = classify_outcome(
            ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            &events,
            false,
        );
        assert_eq!(
            outcome,
            Outcome::SuccessWithConflicts {
                paths: vec![
                    "/tmp/p1/a.txt.conflict1".to_string(),
                    "/tmp/p2/a.txt.conflict2".to_string(),
                ]
            }
        );
    }

    #[test]
    fn classifies_max_delete_abort_above_generic_retry_flag() {
        let events = parse_log_stream([
            r#"{"level":"error","msg":"too many deletes (>10%, 4 of 8) on Path1 \"/tmp/p1/\". Run with --force if desired.","object":"Safety abort","source":"bisync/deltas.go:572"}"#,
            r#"{"level":"info","msg":"stats","stats":{"errors":1,"fatalError":false,"lastError":"too many deletes","retryError":true},"source":"accounting/stats.go:554"}"#,
        ]);
        let outcome = classify_outcome(
            ExitStatusInfo {
                code: Some(1),
                signal: None,
            },
            &events,
            false,
        );
        assert!(matches!(outcome, Outcome::DeletionGuardAborted { .. }));
    }

    #[test]
    fn classifies_access_check_failure_distinct_from_generic_critical() {
        let events = parse_log_stream([
            r#"{"level":"error","msg":"Access test failed: Path1 count 1, Path2 count 0 - RCLONE_TEST","source":"bisync/operations.go:535"}"#,
            r#"{"level":"error","msg":"\u001b[31mBisync critical error: check file check failed\u001b[0m","source":"bisync/operations.go:216"}"#,
            r#"{"level":"error","msg":"\u001b[33mBisync aborted. Error is retryable without --resync due to --resilient mode.\u001b[0m","source":"bisync/operations.go:217"}"#,
        ]);
        let outcome = classify_outcome(
            ExitStatusInfo {
                code: Some(7),
                signal: None,
            },
            &events,
            false,
        );
        assert!(matches!(outcome, Outcome::AccessCheckFailed { .. }));
    }

    #[test]
    fn classifies_must_resync_as_critical_even_when_resilient_prints_retryable() {
        let events = parse_log_stream([
            r#"{"level":"error","msg":"\u001b[31mBisync critical error: filters file md5 hash not found (must run --resync): /tmp/filters.txt\u001b[0m","source":"bisync/operations.go:216"}"#,
            r#"{"level":"error","msg":"\u001b[33mBisync aborted. Error is retryable without --resync due to --resilient mode.\u001b[0m","source":"bisync/operations.go:217"}"#,
        ]);
        let outcome = classify_outcome(
            ExitStatusInfo {
                code: Some(7),
                signal: None,
            },
            &events,
            false,
        );
        assert!(matches!(
            outcome,
            Outcome::Critical {
                requires_resync: true,
                ..
            }
        ));
    }

    #[test]
    fn classifies_auth_invalid_config_retryable_interrupted_and_unknown() {
        let auth_events = parse_log_stream([
            r#"{"level":"critical","msg":"Failed to load config file \"/tmp/rclone.conf\": password command failed: exit status 1","source":"config/config.go:379"}"#,
        ]);
        assert!(matches!(
            classify_outcome(
                ExitStatusInfo {
                    code: Some(1),
                    signal: None
                },
                &auth_events,
                false
            ),
            Outcome::AuthFailure { .. }
        ));

        let cfg_events = parse_log_stream([
            r#"{"level":"critical","msg":"Failed to create file system for destination \"missing:Data\": didn't find section in config file (\"missing\")","source":"cmd/cmd.go:208"}"#,
        ]);
        assert!(matches!(
            classify_outcome(
                ExitStatusInfo {
                    code: Some(1),
                    signal: None
                },
                &cfg_events,
                false
            ),
            Outcome::InvalidConfig { .. }
        ));

        assert!(matches!(
            classify_outcome(
                ExitStatusInfo {
                    code: Some(5),
                    signal: None
                },
                &[],
                false
            ),
            Outcome::Retryable { .. }
        ));

        let sigint_events = parse_log_stream([
            r#"{"level":"info","msg":"Signal received: interrupt","source":"atexit/atexit.go:51"}"#,
            r#"{"level":"notice","msg":"Graceful shutdown completed successfully.","source":"bisync/operations.go:177"}"#,
        ]);
        assert!(matches!(
            classify_outcome(
                ExitStatusInfo {
                    code: Some(130),
                    signal: None
                },
                &sigint_events,
                true
            ),
            Outcome::Interrupted {
                graceful: true,
                needs_recovery: true,
                ..
            }
        ));

        assert!(matches!(
            classify_outcome(
                ExitStatusInfo {
                    code: Some(42),
                    signal: None
                },
                &[],
                false
            ),
            Outcome::Unknown { .. }
        ));
    }
}
