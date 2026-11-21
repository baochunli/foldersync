//! Outcome classification combining the pinned rclone exit-code table with
//! parsed diagnostic log events (§14).
//!
//! Exit-code definitions follow pinned rclone `v1.76.0`:
//! <https://github.com/rclone/rclone/blob/v1.76.0/lib/exitcode/exitcode.go#L13-L18>

use crate::{
    log_parser::{BisyncMarker, LogEvent, LogLevel, parse_log_line},
    process::{ExitStatusInfo, RunOutput, StreamKind},
};

/// Rclone version that the exit-code table below is qualified against.
pub const EXIT_CODE_TABLE_VERSION: &str = crate::PINNED_RCLONE_VERSION;

/// Engine-specific exit code meanings for pinned rclone `v1.76.0`
/// (`lib/exitcode/exitcode.go`). Kept in one table so engine upgrades require
/// auditing a single place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcloneExitCode {
    Success,            // 0: Success
    Error,              // 1: Uncategorized error
    SyntaxOrUsage,      // 2: Syntax or usage error
    DirectoryNotFound,  // 3: Directory not found
    FileNotFound,       // 4: File not found
    TemporaryError,     // 5: RetryError (temporary error that retries might fix)
    NoRetryError,       // 6: NoRetryError (less serious / non-retryable error)
    FatalError,         // 7: FatalError
    TransferLimit,      // 8: TransferExceeded (--max-transfer reached)
    NoFilesTransferred, // 9: NoFilesTransferred (successful, no files transferred)
    DurationLimit,      // 10: DurationExceeded (--max-duration reached)
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
    /// Diagnostic capture dropped recognized diagnostic events or truncated
    /// lines, so the preliminary outcome cannot be treated as complete proof.
    IncompleteDiagnosis {
        preliminary: Box<Outcome>,
        dropped_diagnostic_events: u64,
        truncated_lines: u64,
        reason: String,
    },
    Unknown {
        exit_status: ExitStatusInfo,
        reason: String,
    },
}

/// Maps a numeric rclone exit code using the pinned `v1.76.0` table
/// (`lib/exitcode/exitcode.go`).
pub fn classify_exit_code(code: i32) -> RcloneExitCode {
    match code {
        0 => RcloneExitCode::Success,
        1 => RcloneExitCode::Error,
        2 => RcloneExitCode::SyntaxOrUsage,
        3 => RcloneExitCode::DirectoryNotFound,
        4 => RcloneExitCode::FileNotFound,
        5 => RcloneExitCode::TemporaryError,
        6 => RcloneExitCode::NoRetryError,
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

    // 1. Interruption / cancellation.
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

    // 3. Holds that must NEVER be overridden by resilient-retry or generic exit codes:
    // deletion guard, access check, auth, invalid config, endpoint mismatch, unusable baseline.
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
    if let Some(reason) = &diag.must_resync {
        return Outcome::Critical {
            requires_resync: true,
            reason: reason.clone(),
        };
    }

    // 4. Honor rclone's explicit resilient-retry diagnostic when no hold condition
    // (access, identity/endpoint, auth, deletion guard, config, or unusable baseline)
    // is present — e.g. transient listing failures that exit 7 with a generic
    // Bisync critical error plus the resilient retryable notice (§14).
    // Never retry exit 6 (NoRetryError).
    let is_no_retry_exit = matches!(mapped_code, Some(RcloneExitCode::NoRetryError));
    if !is_no_retry_exit && let Some(resilient_msg) = &diag.resilient_retryable {
        let reason = diag
            .critical_error
            .clone()
            .unwrap_or_else(|| resilient_msg.clone());
        return Outcome::Retryable { reason };
    }

    // 5. Non-resilient critical / fatal errors.
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

    // 6. Retryable transient errors (exit 5 RetryError or stats.retry_error).
    // Exit 6 is NoRetryError in rclone v1.76.0 (lib/exitcode/exitcode.go) and
    // is NEVER retryable, regardless of stats.retryError.
    if !is_no_retry_exit
        && (matches!(mapped_code, Some(RcloneExitCode::TemporaryError)) || diag.stats_retry_error)
    {
        return Outcome::Retryable {
            reason: diag.fallback_reason(exit_status),
        };
    }

    // 7. Unclassified / non-retryable failure — retain exit status and best diagnostic text.
    Outcome::Unknown {
        exit_status,
        reason: diag.fallback_reason(exit_status),
    }
}

/// Classifies a completed [`RunOutput`] using its separately retained
/// diagnostic events plus retained stderr lines, and reports
/// [`Outcome::IncompleteDiagnosis`] whenever diagnostic events were dropped or
/// lines were truncated.
pub fn classify_run_outcome(run: &RunOutput) -> Outcome {
    let mut events = run.output.diagnostic_events.clone();
    // Append parsed retained stderr lines so non-diagnostic lines like final
    // clean stats or BisyncSuccessful are also visible when diagnostic_events is empty.
    for line in &run.output.lines {
        if line.stream == StreamKind::Stderr {
            let parsed = parse_log_line(&line.text);
            if !events.contains(&parsed) {
                events.push(parsed);
            }
        }
    }

    let preliminary = classify_outcome(run.exit_status, &events, run.stop_requested);
    if run.output.diagnosis_incomplete() {
        Outcome::IncompleteDiagnosis {
            preliminary: Box::new(preliminary),
            dropped_diagnostic_events: run.output.dropped_diagnostic_events,
            truncated_lines: run.output.truncated_lines,
            reason: format!(
                "diagnostic capture incomplete (dropped_diagnostic_events={}, truncated_lines={})",
                run.output.dropped_diagnostic_events, run.output.truncated_lines
            ),
        }
    } else {
        preliminary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        log_parser::parse_log_stream,
        process::{BoundedLineBuffer, ChildIdentity, OutputLimits, OutputLine},
    };
    use std::path::PathBuf;

    #[test]
    fn maps_pinned_rclone_exit_codes() {
        assert_eq!(classify_exit_code(0), RcloneExitCode::Success);
        assert_eq!(classify_exit_code(1), RcloneExitCode::Error);
        assert_eq!(classify_exit_code(2), RcloneExitCode::SyntaxOrUsage);
        assert_eq!(classify_exit_code(3), RcloneExitCode::DirectoryNotFound);
        assert_eq!(classify_exit_code(5), RcloneExitCode::TemporaryError);
        assert_eq!(classify_exit_code(6), RcloneExitCode::NoRetryError);
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

    #[test]
    fn exit_6_is_not_retryable_and_resilient_listing_failure_is_retryable() {
        let exit6 = classify_outcome(
            ExitStatusInfo {
                code: Some(6),
                signal: None,
            },
            &[],
            false,
        );
        assert!(
            !matches!(exit6, Outcome::Retryable { .. }),
            "exit 6 (NoRetryError) must not be classified as Retryable, got {exit6:?}"
        );

        let transient_events = parse_log_stream([
            r#"{"level":"error","msg":"\u001b[31mBisync critical error: error listing Path2: temporary network failure\u001b[0m","source":"bisync/operations.go:216"}"#,
            r#"{"level":"error","msg":"\u001b[33mBisync aborted. Error is retryable without --resync due to --resilient mode.\u001b[0m","source":"bisync/operations.go:217"}"#,
        ]);
        let outcome = classify_outcome(
            ExitStatusInfo {
                code: Some(7),
                signal: None,
            },
            &transient_events,
            false,
        );
        assert!(
            matches!(outcome, Outcome::Retryable { .. }),
            "resilient transient listing failure should be Retryable, got {outcome:?}"
        );
    }

    #[test]
    fn classify_run_outcome_preserves_evicted_conflicts_and_flags_incomplete_diagnosis() {
        // Case 1: Early conflict followed by many INFO lines that evict the ordinary buffer.
        let mut buf = BoundedLineBuffer::new(OutputLimits {
            max_total_bytes: 2048,
            max_lines: 4,
            max_line_bytes: 512,
            max_diagnostic_events: 16,
        });
        buf.push(OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"notice","msg":"- Path1    Renaming Path1 copy - /tmp/p1/a.txt.conflict1","source":"bisync/resolve.go:318"}"#.to_string(),
            truncated: false,
        });
        for i in 0..20 {
            buf.push(OutputLine {
                stream: StreamKind::Stderr,
                text: format!(r#"{{"level":"info","msg":"copied file {i}"}}"#),
                truncated: false,
            });
        }
        let run = RunOutput {
            identity: ChildIdentity {
                pid: 1,
                start_time: None,
                executable: PathBuf::from("/fake/rclone"),
            },
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            output: buf.into_captured(),
            stop_requested: false,
        };
        assert_eq!(
            classify_run_outcome(&run),
            Outcome::SuccessWithConflicts {
                paths: vec!["/tmp/p1/a.txt.conflict1".to_string()]
            }
        );

        // Case 2: Diagnostic capacity exceeded -> IncompleteDiagnosis (never silent plain Success).
        let mut buf2 = BoundedLineBuffer::new(OutputLimits {
            max_total_bytes: 4096,
            max_lines: 10,
            max_line_bytes: 512,
            max_diagnostic_events: 1,
        });
        buf2.push(OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"notice","msg":"- Path1    Renaming Path1 copy - /tmp/p1/a.txt.conflict1"}"#.to_string(),
            truncated: false,
        });
        buf2.push(OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"notice","msg":"- Path2    Renaming Path2 copy - /tmp/p2/a.txt.conflict2"}"#.to_string(),
            truncated: false,
        });
        let run2 = RunOutput {
            identity: ChildIdentity {
                pid: 2,
                start_time: None,
                executable: PathBuf::from("/fake/rclone"),
            },
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            output: buf2.into_captured(),
            stop_requested: false,
        };
        assert!(matches!(
            classify_run_outcome(&run2),
            Outcome::IncompleteDiagnosis {
                dropped_diagnostic_events: 1,
                ..
            }
        ));
    }

    #[test]
    fn baseline_errors_listing_auth_failure_and_exit_6_with_retry_stats_never_retry() {
        // 1. Unreadable prior listing with resilient-retry notice -> Critical { requires_resync: true }.
        let unreadable_events = parse_log_stream([
            r#"{"level":"error","msg":"\u001b[31mBisync critical error: cannot read prior listing of Path1: open /tmp/wd/a.path1.lst: permission denied\u001b[0m","source":"bisync/operations.go:216"}"#,
            r#"{"level":"error","msg":"\u001b[33mBisync aborted. Error is retryable without --resync due to --resilient mode.\u001b[0m","source":"bisync/operations.go:217"}"#,
        ]);
        let unreadable_out = classify_outcome(
            ExitStatusInfo {
                code: Some(7),
                signal: None,
            },
            &unreadable_events,
            false,
        );
        assert!(
            matches!(
                unreadable_out,
                Outcome::Critical {
                    requires_resync: true,
                    ..
                }
            ),
            "unreadable prior listing must hold as Critical {{ requires_resync: true }}, got {unreadable_out:?}"
        );

        // 2. Out-of-sync prior listings with resilient-retry notice -> Critical { requires_resync: true }.
        let out_of_sync_events = parse_log_stream([
            r#"{"level":"error","msg":"\u001b[31mBisync critical error: path1 and path2 are out of sync, run --resync to recover\u001b[0m","source":"bisync/operations.go:216"}"#,
            r#"{"level":"error","msg":"\u001b[33mBisync aborted. Error is retryable without --resync due to --resilient mode.\u001b[0m","source":"bisync/operations.go:217"}"#,
        ]);
        let out_of_sync_out = classify_outcome(
            ExitStatusInfo {
                code: Some(7),
                signal: None,
            },
            &out_of_sync_events,
            false,
        );
        assert!(
            matches!(
                out_of_sync_out,
                Outcome::Critical {
                    requires_resync: true,
                    ..
                }
            ),
            "out-of-sync listings must hold as Critical {{ requires_resync: true }}, got {out_of_sync_out:?}"
        );

        // 3. Listing-time OAuth failure (expired token / invalid_grant) with resilient notice + fatal stats -> AuthFailure.
        let listing_auth_events = parse_log_stream([
            r#"{"level":"error","msg":"error reading destination root directory: couldn't list directory: Get \"https://www.googleapis.com/drive/v3/files\": couldn't fetch token: invalid_grant: Token has been expired or revoked.","source":"bisync/march.go:45"}"#,
            r#"{"level":"error","msg":"\u001b[31mBisync critical error: error listing Path2: oauth2: token expired and refresh token is not set\u001b[0m","source":"bisync/operations.go:216"}"#,
            r#"{"level":"error","msg":"\u001b[33mBisync aborted. Error is retryable without --resync due to --resilient mode.\u001b[0m","source":"bisync/operations.go:217"}"#,
            r#"{"level":"info","msg":"stats","stats":{"errors":1,"fatalError":true,"lastError":"bisync aborted","retryError":false},"source":"accounting/stats.go:554"}"#,
        ]);
        let listing_auth_out = classify_outcome(
            ExitStatusInfo {
                code: Some(7),
                signal: None,
            },
            &listing_auth_events,
            false,
        );
        assert!(
            matches!(listing_auth_out, Outcome::AuthFailure { .. }),
            "listing-time OAuth failure must hold as AuthFailure, got {listing_auth_out:?}"
        );

        // 4. Exit 6 (NoRetryError) WITH stats.retryError == true must NEVER be Retryable.
        let exit6_retry_stats = parse_log_stream([
            r#"{"level":"info","msg":"stats","stats":{"errors":1,"fatalError":false,"lastError":"non-retryable backend error","retryError":true},"source":"accounting/stats.go:554"}"#,
        ]);
        let exit6_out = classify_outcome(
            ExitStatusInfo {
                code: Some(6),
                signal: None,
            },
            &exit6_retry_stats,
            false,
        );
        assert!(
            !matches!(exit6_out, Outcome::Retryable { .. }),
            "exit 6 with stats.retryError=true must never be Retryable, got {exit6_out:?}"
        );
    }
}
