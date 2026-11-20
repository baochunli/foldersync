//! Parser for rclone `--use-json-log` NDJSON output streams.

use serde::Deserialize;

/// Parsed log severity emitted by rclone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Other(String),
}

impl LogLevel {
    fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "debug" => Self::Debug,
            "info" => Self::Info,
            "notice" => Self::Notice,
            "warning" | "warn" => Self::Warning,
            "error" => Self::Error,
            "critical" => Self::Critical,
            _ => Self::Other(raw.to_string()),
        }
    }
}

/// Which side of a bisync pair created a conflict copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    Path1,
    Path2,
}

/// Recognized bisync / rclone operational markers extracted from log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BisyncMarker {
    BisyncSuccessful,
    ConflictDetected {
        path: String,
    },
    ConflictCopyCreated {
        side: Option<ConflictSide>,
        path: String,
    },
    MaxDeleteAborted {
        detail: String,
    },
    AccessCheckFailed {
        detail: String,
    },
    MustResync {
        detail: String,
    },
    CriticalError {
        detail: String,
    },
    RetryableDueToResilient {
        detail: String,
    },
    SignalReceived {
        detail: String,
    },
    GracefulShutdownStarted,
    GracefulShutdownCompleted,
    AuthFailure {
        detail: String,
    },
    InvalidConfig {
        detail: String,
    },
    EndpointMismatch {
        detail: String,
    },
}

/// Transfer statistics embedded in rclone `--stats` JSON log events.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TransferStats {
    pub bytes: u64,
    pub total_bytes: u64,
    pub checks: u64,
    pub total_checks: u64,
    pub transfers: u64,
    pub total_transfers: u64,
    pub deletes: u64,
    pub deleted_dirs: u64,
    pub renames: u64,
    pub listed: u64,
    pub errors: u64,
    pub fatal_error: bool,
    pub retry_error: bool,
    pub last_error: Option<String>,
    pub elapsed_time: f64,
    pub transfer_time: f64,
    pub speed: f64,
    pub eta: Option<f64>,
    pub server_side_copies: u64,
    pub server_side_copy_bytes: u64,
    pub server_side_moves: u64,
    pub server_side_move_bytes: u64,
    pub updated_dirs: u64,
}

/// A structured JSON event emitted by rclone `--use-json-log`.
#[derive(Debug, Clone, PartialEq)]
pub struct StructuredLogEvent {
    pub level: LogLevel,
    pub msg: String,
    pub plain_msg: String,
    pub time: Option<String>,
    pub source: Option<String>,
    pub object: Option<String>,
    pub object_type: Option<String>,
    pub stats: Option<TransferStats>,
    pub marker: Option<BisyncMarker>,
}

/// Parsed event from a single rclone output line.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEvent {
    Structured(Box<StructuredLogEvent>),
    /// Any non-JSON or unrecognized line, preserved verbatim for diagnostics.
    Raw(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJsonLog {
    level: String,
    msg: String,
    time: Option<String>,
    source: Option<String>,
    object: Option<String>,
    object_type: Option<String>,
    stats: Option<TransferStats>,
}

/// Removes ANSI CSI escape sequences (e.g. `\u{1b}[31m`) that rclone v1.76.0
/// embeds inside JSON `msg` strings.
pub fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7E).contains(&b) {
                    break;
                }
            }
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn extract_trailing_dash_field(plain_msg: &str) -> Option<String> {
    let (_, rhs) = plain_msg.rsplit_once(" - ")?;
    let trimmed = rhs.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn detect_marker(plain_msg: &str, object: Option<&str>, level: &LogLevel) -> Option<BisyncMarker> {
    let trimmed = plain_msg.trim();
    let lower = trimmed.to_ascii_lowercase();

    // Conflict markers.
    if lower.contains("renaming path1 copy")
        && let Some(path) = extract_trailing_dash_field(trimmed)
    {
        return Some(BisyncMarker::ConflictCopyCreated {
            side: Some(ConflictSide::Path1),
            path,
        });
    }
    if lower.contains("renaming path2 copy")
        && let Some(path) = extract_trailing_dash_field(trimmed)
    {
        return Some(BisyncMarker::ConflictCopyCreated {
            side: Some(ConflictSide::Path2),
            path,
        });
    }
    if lower.contains("new or changed in both paths")
        && let Some(path) = extract_trailing_dash_field(trimmed)
    {
        return Some(BisyncMarker::ConflictDetected { path });
    }

    // Deletion guard abort.
    if lower.contains("too many deletes") || object == Some("Safety abort") {
        return Some(BisyncMarker::MaxDeleteAborted {
            detail: trimmed.to_string(),
        });
    }

    // Access check failure (check before generic Bisync critical error).
    if lower.contains("access test failed") || lower.contains("check file check failed") {
        return Some(BisyncMarker::AccessCheckFailed {
            detail: trimmed.to_string(),
        });
    }

    // Resync-required errors (check before generic Bisync critical error and resilient note).
    if lower.contains("must run --resync")
        || lower.contains("cannot find prior path1 or path2 listings")
    {
        return Some(BisyncMarker::MustResync {
            detail: trimmed.to_string(),
        });
    }

    // Authentication / encrypted config password-command errors.
    if lower.contains("password command failed")
        || lower.contains("using --password-command returned")
        || lower.contains("unable to decrypt configuration")
        || lower.contains("oauth2: cannot fetch token")
        || lower.contains("couldn't fetch token")
        || lower.contains("invalid_grant")
        || lower.contains("401 unauthorized")
        || lower.contains("invalid credentials")
        || lower.contains("token expired")
    {
        return Some(BisyncMarker::AuthFailure {
            detail: trimmed.to_string(),
        });
    }

    // Invalid configuration / arguments.
    if lower.contains("didn't find section in config file")
        || lower.contains("failed to load config file")
        || lower.contains("unknown flag:")
        || lower.contains("must not overlap")
    {
        return Some(BisyncMarker::InvalidConfig {
            detail: trimmed.to_string(),
        });
    }

    // Endpoint / path mismatch or duplicate names.
    if lower.contains("directory not found")
        || lower.contains("duplicate object found")
        || lower.contains("duplicate directory found")
    {
        return Some(BisyncMarker::EndpointMismatch {
            detail: trimmed.to_string(),
        });
    }

    // Generic Bisync critical error.
    if lower.contains("bisync critical error")
        || (*level == LogLevel::Critical && lower.contains("bisync"))
    {
        return Some(BisyncMarker::CriticalError {
            detail: trimmed.to_string(),
        });
    }

    // Resilient retryable notice.
    if lower.contains("error is retryable without --resync due to --resilient") {
        return Some(BisyncMarker::RetryableDueToResilient {
            detail: trimmed.to_string(),
        });
    }

    // Interrupt / graceful shutdown markers.
    if lower.contains("signal received:") {
        return Some(BisyncMarker::SignalReceived {
            detail: trimmed.to_string(),
        });
    }
    if lower.contains("attempting to gracefully shutdown") {
        return Some(BisyncMarker::GracefulShutdownStarted);
    }
    if lower.contains("graceful shutdown completed successfully") {
        return Some(BisyncMarker::GracefulShutdownCompleted);
    }

    // Success marker.
    if lower == "bisync successful" {
        return Some(BisyncMarker::BisyncSuccessful);
    }

    None
}

/// Parses a single NDJSON line from rclone into a typed [`LogEvent`].
pub fn parse_log_line(line: &str) -> LogEvent {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return LogEvent::Raw(line.to_string());
    }
    let Ok(raw) = serde_json::from_str::<RawJsonLog>(trimmed) else {
        return LogEvent::Raw(line.to_string());
    };

    let level = LogLevel::parse(&raw.level);
    let plain_msg = strip_ansi_codes(&raw.msg);
    let marker = detect_marker(&plain_msg, raw.object.as_deref(), &level);

    LogEvent::Structured(Box::new(StructuredLogEvent {
        level,
        msg: raw.msg,
        plain_msg,
        time: raw.time,
        source: raw.source,
        object: raw.object,
        object_type: raw.object_type,
        stats: raw.stats,
        marker,
    }))
}

/// Parses an iterator of lines into typed [`LogEvent`] values.
pub fn parse_log_stream<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<LogEvent> {
    lines.into_iter().map(parse_log_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_escape_sequences() {
        let input = "\u{1b}[31mBisync aborted. Must run --resync to recover.\u{1b}[0m";
        assert_eq!(
            strip_ansi_codes(input),
            "Bisync aborted. Must run --resync to recover."
        );
    }

    #[test]
    fn parses_stats_object_from_json_log_line() {
        let line = r#"{"time":"2026-11-20T18:39:50.603925-05:00","level":"info","msg":"Transferred: 102 B","stats":{"bytes":102,"checks":4,"deletedDirs":0,"deletes":1,"elapsedTime":0.5,"errors":0,"eta":null,"fatalError":false,"listed":6,"renames":2,"retryError":false,"speed":204.0,"totalBytes":102,"totalChecks":4,"totalTransfers":4,"transferTime":0.1,"transfers":4,"updatedDirs":0},"source":"accounting/stats.go:554"}"#;
        let LogEvent::Structured(ev) = parse_log_line(line) else {
            panic!("expected structured event");
        };
        assert_eq!(ev.level, LogLevel::Info);
        assert_eq!(ev.time.as_deref(), Some("2026-11-20T18:39:50.603925-05:00"));
        assert_eq!(ev.source.as_deref(), Some("accounting/stats.go:554"));
        let stats = ev.stats.expect("stats should be parsed");
        assert_eq!(stats.bytes, 102);
        assert_eq!(stats.total_bytes, 102);
        assert_eq!(stats.transfers, 4);
        assert_eq!(stats.deletes, 1);
        assert_eq!(stats.renames, 2);
        assert!(!stats.fatal_error);
    }

    #[test]
    fn recognizes_conflict_markers_with_ansi_escapes() {
        let warn = r#"{"time":"2026-11-20T18:39:50Z","level":"notice","msg":"- \u001b[34mWARNING\u001b[0m  \u001b[35mNew or changed in both paths\u001b[0m       - \u001b[36mnotes/a.txt\u001b[0m","source":"bisync/deltas.go:386"}"#;
        let rename1 = r#"{"time":"2026-11-20T18:39:50Z","level":"notice","msg":"- \u001b[36mPath1\u001b[0m    \u001b[35mRenaming Path1 copy\u001b[0m                - \u001b[36m/tmp/p1/notes/a.txt.conflict1\u001b[0m","source":"bisync/resolve.go:318"}"#;
        let rename2 = r#"{"time":"2026-11-20T18:39:50Z","level":"notice","msg":"- \u001b[34mPath2\u001b[0m    \u001b[35mRenaming Path2 copy\u001b[0m                - \u001b[36m/tmp/p2/notes/a.txt.conflict2\u001b[0m","source":"bisync/resolve.go:318"}"#;

        let LogEvent::Structured(e0) = parse_log_line(warn) else {
            panic!()
        };
        assert_eq!(
            e0.marker,
            Some(BisyncMarker::ConflictDetected {
                path: "notes/a.txt".to_string()
            })
        );

        let LogEvent::Structured(e1) = parse_log_line(rename1) else {
            panic!()
        };
        assert_eq!(
            e1.marker,
            Some(BisyncMarker::ConflictCopyCreated {
                side: Some(ConflictSide::Path1),
                path: "/tmp/p1/notes/a.txt.conflict1".to_string()
            })
        );

        let LogEvent::Structured(e2) = parse_log_line(rename2) else {
            panic!()
        };
        assert_eq!(
            e2.marker,
            Some(BisyncMarker::ConflictCopyCreated {
                side: Some(ConflictSide::Path2),
                path: "/tmp/p2/notes/a.txt.conflict2".to_string()
            })
        );
    }

    #[test]
    fn recognizes_max_delete_access_check_must_resync_and_interrupt_markers() {
        let max_del = r#"{"level":"error","msg":"too many deletes (>10%, 4 of 8) on Path1 \"/tmp/p1/\". Run with --force if desired.","object":"Safety abort","source":"bisync/deltas.go:572"}"#;
        let access = r#"{"level":"error","msg":"Access test failed: Path1 count 1, Path2 count 0 - RCLONE_TEST","source":"bisync/operations.go:535"}"#;
        let must_resync = r#"{"level":"error","msg":"\u001b[31mBisync aborted. Must run --resync to recover.\u001b[0m","source":"bisync/operations.go:226"}"#;
        let sigint =
            r#"{"level":"info","msg":"Signal received: interrupt","source":"atexit/atexit.go:51"}"#;
        let graceful = r#"{"level":"notice","msg":"\u001b[32mGraceful shutdown completed successfully.\u001b[0m","source":"bisync/operations.go:177"}"#;

        assert!(matches!(
            parse_log_line(max_del),
            LogEvent::Structured(ev) if matches!(ev.marker, Some(BisyncMarker::MaxDeleteAborted { .. }))
        ));
        assert!(matches!(
            parse_log_line(access),
            LogEvent::Structured(ev) if matches!(ev.marker, Some(BisyncMarker::AccessCheckFailed { .. }))
        ));
        assert!(matches!(
            parse_log_line(must_resync),
            LogEvent::Structured(ev) if matches!(ev.marker, Some(BisyncMarker::MustResync { .. }))
        ));
        assert!(matches!(
            parse_log_line(sigint),
            LogEvent::Structured(ev) if matches!(ev.marker, Some(BisyncMarker::SignalReceived { .. }))
        ));
        assert!(matches!(
            parse_log_line(graceful),
            LogEvent::Structured(ev) if matches!(ev.marker, Some(BisyncMarker::GracefulShutdownCompleted))
        ));
    }

    #[test]
    fn preserves_non_json_and_unstructured_lines_verbatim_as_raw() {
        let plain = "Error: unknown flag: --bogus";
        assert_eq!(parse_log_line(plain), LogEvent::Raw(plain.to_string()));

        let partial_json = r#"{"foo":"bar"}"#;
        assert_eq!(
            parse_log_line(partial_json),
            LogEvent::Raw(partial_json.to_string())
        );
    }
}
