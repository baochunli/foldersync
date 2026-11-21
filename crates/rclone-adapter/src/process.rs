//! Process spawning, environment isolation, bounded output capture, secret
//! redaction, OS process identity, and graceful signal handling without
//! assuming a specific async runtime.

use std::{
    collections::{BTreeMap, VecDeque},
    env,
    ffi::OsString,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::log_parser::{LogEvent, LogLevel, parse_log_line, strip_ansi_codes};

const ENV_ALLOWLIST: &[&str] = &["PATH", "TMPDIR", "LANG", "LC_ALL", "TZ"];
const PW_CMD_STDERR_PREFIX: &str = "--password-command stderr:";

/// Builds a minimal isolated environment for child processes: starts from an
/// empty environment, forwards only a small allowlist (`PATH`, `TMPDIR`,
/// `LANG`, `LC_ALL`, `TZ`) from the parent if present, and optionally points
/// `HOME` to a lane- or run-owned directory. Ambient `RCLONE_*` and `XDG_*`
/// variables are never inherited.
pub fn minimal_isolated_env(home_dir: Option<&Path>) -> BTreeMap<OsString, OsString> {
    let mut env_map = BTreeMap::new();
    for key in ENV_ALLOWLIST {
        if let Some(val) = env::var_os(key) {
            env_map.insert(OsString::from(key), val);
        }
    }
    if let Some(home) = home_dir {
        env_map.insert(OsString::from("HOME"), home.as_os_str().to_os_string());
    }
    env_map
}

/// Specification of a child process spawned directly by argv (never via a shell)
/// with an explicit isolated environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    /// Exact environment applied after `Command::env_clear()`.
    pub env: BTreeMap<OsString, OsString>,
    /// Internal SHA-256 digest re-verified immediately before `exec` by [`RealProcessRunner`].
    pub(crate) expected_binary_sha256: Option<String>,
}

/// Which standard stream produced a captured line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// A single captured line from stdout or stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: StreamKind,
    pub text: String,
    pub truncated: bool,
}

/// Memory bounds for line-by-line stdout/stderr capture and retained diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    /// Maximum total retained bytes across all stored ordinary lines.
    pub max_total_bytes: usize,
    /// Maximum number of retained ordinary lines.
    pub max_lines: usize,
    /// Maximum bytes retained for any individual line.
    pub max_line_bytes: usize,
    /// Maximum number of recognized diagnostic events retained separately from
    /// ordinary lines so early conflicts/errors survive ring-buffer eviction.
    pub max_diagnostic_events: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 4 * 1024 * 1024,
            max_lines: 20_000,
            max_line_bytes: 64 * 1024,
            max_diagnostic_events: 4_096,
        }
    }
}

/// Spawn options combining memory bounds with an optional bounded, nonblocking
/// live line observer (`SyncSender`).
#[derive(Debug, Default)]
pub struct SpawnOptions {
    pub limits: OutputLimits,
    /// Bounded, nonblocking observer channel. Lines that cannot be sent
    /// immediately via `try_send` are dropped without blocking and counted in
    /// [`CapturedOutput::observer_dropped_lines`].
    pub line_observer: Option<mpsc::SyncSender<OutputLine>>,
}

/// Redacts credential-helper stderr payloads emitted by rclone's
/// `--password-command` error logger before any storage or observer delivery.
///
/// Matches `--password-command stderr:` only at the START of the decoded
/// message (or immediately after rclone's plaintext ` ERROR : ` prefix) so
/// benign conflict or transfer lines referencing filenames containing that
/// substring are never erased.
pub(crate) fn sanitize_captured_line(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{')
        && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(obj) = value.as_object_mut()
        && let Some(msg) = obj.get("msg").and_then(|v| v.as_str())
    {
        let plain = strip_ansi_codes(msg);
        if plain.trim_start().starts_with(PW_CMD_STDERR_PREFIX) {
            obj.insert(
                "msg".to_string(),
                serde_json::Value::String(format!("{PW_CMD_STDERR_PREFIX} [REDACTED]")),
            );
            if let Ok(serialized) = serde_json::to_string(&value) {
                return serialized;
            }
        }
        return text.to_string();
    }

    let plain_text = strip_ansi_codes(text);
    if plain_text.trim_start().starts_with(PW_CMD_STDERR_PREFIX) {
        return format!("{PW_CMD_STDERR_PREFIX} [REDACTED]");
    }
    if let Some((head, tail)) = plain_text.split_once(" ERROR : ")
        && !head.contains(" - ")
        && tail.trim_start().starts_with(PW_CMD_STDERR_PREFIX)
    {
        return format!("{head} ERROR : {PW_CMD_STDERR_PREFIX} [REDACTED]");
    }

    text.to_string()
}

fn is_diagnostic_event(event: &LogEvent) -> bool {
    match event {
        LogEvent::Raw(raw) => !raw.trim().is_empty(),
        LogEvent::Structured(ev) => {
            ev.marker.is_some()
                || matches!(ev.level, LogLevel::Error | LogLevel::Critical)
                || ev
                    .stats
                    .as_ref()
                    .is_some_and(|s| s.fatal_error || s.retry_error || s.errors > 0)
        }
    }
}

/// Bounded ring buffer of captured output lines plus separate bounded retention
/// of recognized diagnostic events and drop/truncation counters.
#[derive(Debug, Clone)]
pub struct BoundedLineBuffer {
    limits: OutputLimits,
    lines: VecDeque<OutputLine>,
    retained_bytes: usize,
    dropped_lines: u64,
    dropped_bytes: u64,
    truncated_lines: u64,
    diagnostic_events: Vec<LogEvent>,
    dropped_diagnostic_events: u64,
    observer_dropped_lines: u64,
}

fn truncate_utf8(s: &mut String, max_bytes: usize) -> bool {
    if s.len() <= max_bytes {
        return false;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    true
}

impl BoundedLineBuffer {
    pub fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            lines: VecDeque::new(),
            retained_bytes: 0,
            dropped_lines: 0,
            dropped_bytes: 0,
            truncated_lines: 0,
            diagnostic_events: Vec::new(),
            dropped_diagnostic_events: 0,
            observer_dropped_lines: 0,
        }
    }

    pub fn record_observer_drop(&mut self) {
        self.observer_dropped_lines += 1;
    }

    pub fn push(&mut self, mut line: OutputLine) {
        line.text = sanitize_captured_line(&line.text);
        if truncate_utf8(&mut line.text, self.limits.max_line_bytes) {
            line.truncated = true;
        }
        if line.truncated {
            self.truncated_lines += 1;
        }

        // Retain recognized diagnostics separately before ordinary-line eviction
        // so early conflicts/critical errors survive high-volume INFO output.
        if line.stream == StreamKind::Stderr {
            let parsed = parse_log_line(&line.text);
            if is_diagnostic_event(&parsed) {
                if self.diagnostic_events.len() < self.limits.max_diagnostic_events {
                    self.diagnostic_events.push(parsed);
                } else {
                    self.dropped_diagnostic_events += 1;
                }
            }
        }

        let line_len = line.text.len();
        if self.limits.max_lines == 0 || line_len > self.limits.max_total_bytes {
            self.dropped_lines += 1;
            self.dropped_bytes += line_len as u64;
            return;
        }

        while self.lines.len() >= self.limits.max_lines
            || self.retained_bytes + line_len > self.limits.max_total_bytes
        {
            if let Some(evicted) = self.lines.pop_front() {
                let evicted_len = evicted.text.len();
                self.retained_bytes = self.retained_bytes.saturating_sub(evicted_len);
                self.dropped_lines += 1;
                self.dropped_bytes += evicted_len as u64;
            } else {
                break;
            }
        }

        self.retained_bytes += line_len;
        self.lines.push_back(line);
    }

    pub fn into_captured(self) -> CapturedOutput {
        CapturedOutput {
            lines: self.lines.into_iter().collect(),
            retained_bytes: self.retained_bytes,
            dropped_lines: self.dropped_lines,
            dropped_bytes: self.dropped_bytes,
            truncated_lines: self.truncated_lines,
            diagnostic_events: self.diagnostic_events,
            dropped_diagnostic_events: self.dropped_diagnostic_events,
            observer_dropped_lines: self.observer_dropped_lines,
        }
    }
}

/// Bounded stdout/stderr lines, separately retained diagnostics, and accounting
/// counters captured from a run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CapturedOutput {
    pub lines: Vec<OutputLine>,
    pub retained_bytes: usize,
    pub dropped_lines: u64,
    pub dropped_bytes: u64,
    pub truncated_lines: u64,
    pub diagnostic_events: Vec<LogEvent>,
    pub dropped_diagnostic_events: u64,
    pub observer_dropped_lines: u64,
}

impl CapturedOutput {
    /// Concatenates retained stdout lines separated by `\n`.
    pub fn stdout_text(&self) -> String {
        self.lines
            .iter()
            .filter(|l| l.stream == StreamKind::Stdout)
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Concatenates retained stderr lines separated by `\n`.
    pub fn stderr_text(&self) -> String {
        self.lines
            .iter()
            .filter(|l| l.stream == StreamKind::Stderr)
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Returns `true` when diagnostic capture was incomplete (recognized
    /// diagnostic events were dropped or lines were truncated).
    pub fn diagnosis_incomplete(&self) -> bool {
        self.dropped_diagnostic_events > 0 || self.truncated_lines > 0
    }
}

/// OS-reported process start time for PID-reuse detection (§14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStartTime {
    /// Microseconds since Unix epoch (macOS `proc_pidinfo(PROC_PIDTBSDINFO)`).
    UnixMicros(u64),
    /// Clock ticks since system boot (Linux `/proc/<pid>/stat` field 22).
    BootTicks(u64),
}

/// Child identity recorded alongside a run so restarts can reconcile orphans
/// without trusting a bare PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildIdentity {
    pub pid: u32,
    pub start_time: Option<ProcessStartTime>,
    pub executable: PathBuf,
}

/// Portable exit status summary (exit code and/or terminating Unix signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatusInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitStatusInfo {
    pub fn is_success(self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }

    fn from_std(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;

        Self {
            code: status.code(),
            signal,
        }
    }
}

/// Final result returned by [`RunHandle::wait`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutput {
    pub identity: ChildIdentity,
    pub exit_status: ExitStatusInfo,
    pub output: CapturedOutput,
    pub stop_requested: bool,
}

/// Errors raised while spawning, signaling, or awaiting a child process.
#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("failed to spawn process `{program}`: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "refused to spawn `{program}`: SHA-256 mismatch immediately before launch (expected {expected}, got {actual})"
    )]
    BinaryIntegrityMismatch {
        program: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("failed to verify SHA-256 of `{program}` before launch: {reason}")]
    IntegrityCheck { program: PathBuf, reason: String },
    #[error("failed to send signal {signal} to pid {pid}: {source}")]
    Signal {
        pid: u32,
        signal: i32,
        #[source]
        source: std::io::Error,
    },
    #[error("failed while waiting for pid {pid}: {source}")]
    Wait {
        pid: u32,
        #[source]
        source: std::io::Error,
    },
    #[error("fake process runner has no scripted run queued for `{program}`")]
    UnexpectedSpawn { program: PathBuf },
}

/// Reads the OS start time for `pid` to distinguish live children from reused PIDs.
#[cfg(target_os = "macos")]
pub fn read_process_start_time(pid: u32) -> Option<ProcessStartTime> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected_size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
            expected_size,
        )
    };
    if ret == expected_size {
        let micros = info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec);
        Some(ProcessStartTime::UnixMicros(micros))
    } else {
        None
    }
}

/// Reads the OS start time for `pid` from `/proc/<pid>/stat` field 22 (`starttime`).
#[cfg(target_os = "linux")]
pub fn read_process_start_time(pid: u32) -> Option<ProcessStartTime> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // After field 2 `(comm)`, field 3 (`state`) is index 0, so field 22 (`starttime`) is index 19.
    let starttime = after_comm
        .split_ascii_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    Some(ProcessStartTime::BootTicks(starttime))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read_process_start_time(_pid: u32) -> Option<ProcessStartTime> {
    None
}

/// Abstraction over child process execution so manager/adapter logic can be tested
/// deterministically with [`FakeProcessRunner`].
pub trait ProcessRunner: Send + Sync {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError>;
}

/// Real OS process runner using `std::process::Command`, `env_clear()`,
/// `Stdio::null()` stdin, and dedicated reader threads for bounded stdout/stderr
/// capture.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealProcessRunner;

struct RealChildControl {
    pid: u32,
    child: Mutex<Option<Child>>,
    exit_status: Mutex<Option<ExitStatusInfo>>,
    sigint_count: AtomicU32,
    stop_requested: AtomicBool,
}

impl RealChildControl {
    fn poll_reaped(&self) -> Result<Option<ExitStatusInfo>, ProcessError> {
        let mut child_guard = self.child.lock().unwrap();
        let Some(child) = child_guard.as_mut() else {
            return Ok(*self.exit_status.lock().unwrap());
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let info = ExitStatusInfo::from_std(status);
                *self.exit_status.lock().unwrap() = Some(info);
                *child_guard = None;
                Ok(Some(info))
            }
            Ok(None) => Ok(None),
            Err(source) => Err(ProcessError::Wait {
                pid: self.pid,
                source,
            }),
        }
    }

    #[cfg(unix)]
    fn send_signal_while_unreaped(&self, signal: i32) -> Result<bool, ProcessError> {
        let mut child_guard = self.child.lock().unwrap();
        let Some(child) = child_guard.as_mut() else {
            return Ok(false);
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                *self.exit_status.lock().unwrap() = Some(ExitStatusInfo::from_std(status));
                *child_guard = None;
                return Ok(false);
            }
            Ok(None) => {}
            Err(source) => {
                return Err(ProcessError::Wait {
                    pid: self.pid,
                    source,
                });
            }
        }
        let res = unsafe { libc::kill(self.pid as libc::pid_t, signal) };
        if res == 0 {
            Ok(true)
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(false)
            } else {
                Err(ProcessError::Signal {
                    pid: self.pid,
                    signal,
                    source: err,
                })
            }
        }
    }
}

impl StopBackend for RealChildControl {
    fn request_stop(&self) -> Result<(), ProcessError> {
        self.stop_requested.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        {
            if self
                .sigint_count
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let _ = self.send_signal_while_unreaped(libc::SIGINT)?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let mut child_guard = self.child.lock().unwrap();
            if let Some(child) = child_guard.as_mut() {
                let _ = child.kill();
            }
            Ok(())
        }
    }

    fn force_stop(&self, kill_timeout: Duration) -> Result<(), ProcessError> {
        self.stop_requested.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        {
            let prev = self.sigint_count.fetch_add(1, Ordering::SeqCst);
            if !self.send_signal_while_unreaped(libc::SIGINT)? {
                return Ok(());
            }
            if prev == 0 {
                thread::sleep(Duration::from_millis(50));
                if self.poll_reaped()?.is_some() {
                    return Ok(());
                }
                self.sigint_count.fetch_add(1, Ordering::SeqCst);
                if !self.send_signal_while_unreaped(libc::SIGINT)? {
                    return Ok(());
                }
            }

            let deadline = Instant::now() + kill_timeout;
            while Instant::now() < deadline {
                if self.poll_reaped()?.is_some() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.send_signal_while_unreaped(libc::SIGKILL)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = kill_timeout;
            let mut child_guard = self.child.lock().unwrap();
            if let Some(child) = child_guard.as_mut() {
                let _ = child.kill();
            }
            Ok(())
        }
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }
}

fn spawn_stream_reader<R: Read + Send + 'static>(
    reader: R,
    stream: StreamKind,
    max_line_bytes: usize,
    buffer: Arc<Mutex<BoundedLineBuffer>>,
    observer: Option<mpsc::SyncSender<OutputLine>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf_reader = BufReader::new(reader);
        let mut current = Vec::new();
        let mut truncated = false;

        loop {
            let chunk = match buf_reader.fill_buf() {
                Ok([]) => {
                    if !current.is_empty() || truncated {
                        emit_line(&mut current, &mut truncated, stream, &buffer, &observer);
                    }
                    break;
                }
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    if !current.is_empty() || truncated {
                        emit_line(&mut current, &mut truncated, stream, &buffer, &observer);
                    }
                    break;
                }
            };

            let mut consumed = 0;
            while consumed < chunk.len() {
                let remaining = &chunk[consumed..];
                if let Some(pos) = remaining.iter().position(|&b| b == b'\n') {
                    append_bounded(
                        &mut current,
                        &remaining[..pos],
                        max_line_bytes,
                        &mut truncated,
                    );
                    emit_line(&mut current, &mut truncated, stream, &buffer, &observer);
                    consumed += pos + 1;
                } else {
                    append_bounded(&mut current, remaining, max_line_bytes, &mut truncated);
                    consumed = chunk.len();
                }
            }
            buf_reader.consume(consumed);
        }
    })
}

fn append_bounded(dst: &mut Vec<u8>, src: &[u8], max_line_bytes: usize, truncated: &mut bool) {
    if *truncated {
        return;
    }
    let space = max_line_bytes.saturating_sub(dst.len());
    if src.len() <= space {
        dst.extend_from_slice(src);
    } else {
        dst.extend_from_slice(&src[..space]);
        *truncated = true;
    }
}

fn emit_line(
    raw: &mut Vec<u8>,
    truncated: &mut bool,
    stream: StreamKind,
    buffer: &Arc<Mutex<BoundedLineBuffer>>,
    observer: &Option<mpsc::SyncSender<OutputLine>>,
) {
    if raw.last() == Some(&b'\r') {
        raw.pop();
    }
    let text = sanitize_captured_line(&String::from_utf8_lossy(raw));
    let line = OutputLine {
        stream,
        text,
        truncated: *truncated,
    };
    raw.clear();
    *truncated = false;

    let mut observer_dropped = false;
    if let Some(tx) = observer
        && tx.try_send(line.clone()).is_err()
    {
        observer_dropped = true;
    }
    let mut guard = buffer.lock().unwrap();
    if observer_dropped {
        guard.record_observer_drop();
    }
    guard.push(line);
}

struct RealRunHandleBackend {
    identity: ChildIdentity,
    control: Arc<RealChildControl>,
    buffer: Arc<Mutex<BoundedLineBuffer>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl RunHandleBackend for RealRunHandleBackend {
    fn stop_handle(&self) -> StopHandle {
        StopHandle {
            inner: Arc::clone(&self.control) as Arc<dyn StopBackend>,
        }
    }

    fn stop_requested(&self) -> bool {
        self.control.stop_requested()
    }

    fn wait(mut self: Box<Self>) -> Result<RunOutput, ProcessError> {
        let exit_status = loop {
            if let Some(status) = self.control.poll_reaped()? {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };

        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }

        let captured = {
            let mut guard = self.buffer.lock().unwrap();
            std::mem::replace(&mut *guard, BoundedLineBuffer::new(OutputLimits::default()))
                .into_captured()
        };

        Ok(RunOutput {
            identity: self.identity,
            exit_status,
            output: captured,
            stop_requested: self.control.stop_requested(),
        })
    }
}

impl ProcessRunner for RealProcessRunner {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        // Re-hash immediately before `exec` whenever the spec carries a pinned digest.
        if let Some(expected_sha256) = &spec.expected_binary_sha256 {
            let actual = crate::qualification::sha256_file(&spec.program).map_err(|err| {
                ProcessError::IntegrityCheck {
                    program: spec.program.clone(),
                    reason: err.to_string(),
                }
            })?;
            if &actual != expected_sha256 {
                return Err(ProcessError::BinaryIntegrityMismatch {
                    program: spec.program.clone(),
                    expected: expected_sha256.clone(),
                    actual,
                });
            }
        }

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &spec.current_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn().map_err(|source| ProcessError::Spawn {
            program: spec.program.clone(),
            source,
        })?;

        let pid = child.id();
        let start_time = read_process_start_time(pid);
        let identity = ChildIdentity {
            pid,
            start_time,
            executable: spec.program.clone(),
        };

        let buffer = Arc::new(Mutex::new(BoundedLineBuffer::new(options.limits)));
        let stdout_thread = child.stdout.take().map(|out| {
            spawn_stream_reader(
                out,
                StreamKind::Stdout,
                options.limits.max_line_bytes,
                Arc::clone(&buffer),
                options.line_observer.clone(),
            )
        });
        let stderr_thread = child.stderr.take().map(|err| {
            spawn_stream_reader(
                err,
                StreamKind::Stderr,
                options.limits.max_line_bytes,
                Arc::clone(&buffer),
                options.line_observer,
            )
        });

        let control = Arc::new(RealChildControl {
            pid,
            child: Mutex::new(Some(child)),
            exit_status: Mutex::new(None),
            sigint_count: AtomicU32::new(0),
            stop_requested: AtomicBool::new(false),
        });

        Ok(RunHandle {
            identity: identity.clone(),
            backend: Box::new(RealRunHandleBackend {
                identity,
                control,
                buffer,
                stdout_thread,
                stderr_thread,
            }),
        })
    }
}

trait RunHandleBackend: Send {
    fn stop_handle(&self) -> StopHandle;
    fn stop_requested(&self) -> bool;
    fn wait(self: Box<Self>) -> Result<RunOutput, ProcessError>;
}

trait StopBackend: Send + Sync {
    fn request_stop(&self) -> Result<(), ProcessError>;
    fn force_stop(&self, kill_timeout: Duration) -> Result<(), ProcessError>;
    fn stop_requested(&self) -> bool;
}

/// Cloneable control handle allowing another thread to request graceful or
/// forced termination of a running child without holding [`RunHandle`].
#[derive(Clone)]
pub struct StopHandle {
    inner: Arc<dyn StopBackend>,
}

impl std::fmt::Debug for StopHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopHandle")
            .field("stop_requested", &self.stop_requested())
            .finish()
    }
}

impl StopHandle {
    /// Sends SIGINT once for graceful rclone shutdown (idempotent).
    pub fn request_stop(&self) -> Result<(), ProcessError> {
        self.inner.request_stop()
    }

    /// Sends a second SIGINT (or two SIGINTs if none sent yet), waits up to
    /// `kill_timeout`, and escalates to SIGKILL if the child is still alive.
    pub fn force_stop(&self, kill_timeout: Duration) -> Result<(), ProcessError> {
        self.inner.force_stop(kill_timeout)
    }

    pub fn stop_requested(&self) -> bool {
        self.inner.stop_requested()
    }
}

/// Handle to a running child process exposing identity, stop controls, and `wait()`.
pub struct RunHandle {
    identity: ChildIdentity,
    backend: Box<dyn RunHandleBackend>,
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle")
            .field("identity", &self.identity)
            .finish()
    }
}

impl RunHandle {
    pub fn identity(&self) -> &ChildIdentity {
        &self.identity
    }

    pub fn stop_handle(&self) -> StopHandle {
        self.backend.stop_handle()
    }

    pub fn request_stop(&self) -> Result<(), ProcessError> {
        self.stop_handle().request_stop()
    }

    pub fn force_stop(&self, kill_timeout: Duration) -> Result<(), ProcessError> {
        self.stop_handle().force_stop(kill_timeout)
    }

    pub fn stop_requested(&self) -> bool {
        self.backend.stop_requested()
    }

    pub fn wait(self) -> Result<RunOutput, ProcessError> {
        self.backend.wait()
    }
}

/// Scripted behavior for a single [`FakeProcessRunner`] spawn.
#[derive(Debug, Clone)]
pub struct FakeRunScript {
    pub identity: ChildIdentity,
    pub lines: Vec<OutputLine>,
    pub exit_status: ExitStatusInfo,
    pub exit_status_on_stop: Option<ExitStatusInfo>,
    pub exit_status_on_force_stop: Option<ExitStatusInfo>,
    pub wait_for_stop: bool,
}

impl Default for FakeRunScript {
    fn default() -> Self {
        Self {
            identity: ChildIdentity {
                pid: 1000,
                start_time: Some(ProcessStartTime::UnixMicros(1_700_000_000_000_000)),
                executable: PathBuf::from("/fake/rclone"),
            },
            lines: Vec::new(),
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            exit_status_on_stop: None,
            exit_status_on_force_stop: None,
            wait_for_stop: false,
        }
    }
}

/// Deterministic test double for [`ProcessRunner`].
#[derive(Debug, Default, Clone)]
pub struct FakeProcessRunner {
    state: Arc<Mutex<FakeRunnerState>>,
}

#[derive(Debug, Default)]
struct FakeRunnerState {
    scripts: VecDeque<FakeRunScript>,
    spawned: Vec<ProcessSpec>,
    stop_calls: usize,
    force_stop_calls: usize,
}

struct FakeChildControl {
    runner_state: Arc<Mutex<FakeRunnerState>>,
    stop_state: Mutex<FakeStopState>,
    condvar: Condvar,
}

#[derive(Default)]
struct FakeStopState {
    stop_requested: bool,
    force_stopped: bool,
}

impl StopBackend for FakeChildControl {
    fn request_stop(&self) -> Result<(), ProcessError> {
        self.runner_state.lock().unwrap().stop_calls += 1;
        let mut st = self.stop_state.lock().unwrap();
        st.stop_requested = true;
        self.condvar.notify_all();
        Ok(())
    }

    fn force_stop(&self, _kill_timeout: Duration) -> Result<(), ProcessError> {
        self.runner_state.lock().unwrap().force_stop_calls += 1;
        let mut st = self.stop_state.lock().unwrap();
        st.stop_requested = true;
        st.force_stopped = true;
        self.condvar.notify_all();
        Ok(())
    }

    fn stop_requested(&self) -> bool {
        self.stop_state.lock().unwrap().stop_requested
    }
}

struct FakeRunHandleBackend {
    script: FakeRunScript,
    limits: OutputLimits,
    line_observer: Option<mpsc::SyncSender<OutputLine>>,
    control: Arc<FakeChildControl>,
}

impl RunHandleBackend for FakeRunHandleBackend {
    fn stop_handle(&self) -> StopHandle {
        StopHandle {
            inner: Arc::clone(&self.control) as Arc<dyn StopBackend>,
        }
    }

    fn stop_requested(&self) -> bool {
        self.control.stop_requested()
    }

    fn wait(self: Box<Self>) -> Result<RunOutput, ProcessError> {
        let mut buf = BoundedLineBuffer::new(self.limits);
        for mut line in self.script.lines {
            line.text = sanitize_captured_line(&line.text);
            if let Some(tx) = &self.line_observer
                && tx.try_send(line.clone()).is_err()
            {
                buf.record_observer_drop();
            }
            buf.push(line);
        }

        let (stop_requested, force_stopped) = {
            let mut st = self.control.stop_state.lock().unwrap();
            if self.script.wait_for_stop {
                while !st.stop_requested {
                    st = self.control.condvar.wait(st).unwrap();
                }
            }
            (st.stop_requested, st.force_stopped)
        };

        let exit_status = if force_stopped {
            self.script
                .exit_status_on_force_stop
                .or(self.script.exit_status_on_stop)
                .unwrap_or(self.script.exit_status)
        } else if stop_requested {
            self.script
                .exit_status_on_stop
                .unwrap_or(self.script.exit_status)
        } else {
            self.script.exit_status
        };

        Ok(RunOutput {
            identity: self.script.identity,
            exit_status,
            output: buf.into_captured(),
            stop_requested,
        })
    }
}

impl FakeProcessRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_run(&self, script: FakeRunScript) {
        self.state.lock().unwrap().scripts.push_back(script);
    }

    pub fn spawned_specs(&self) -> Vec<ProcessSpec> {
        self.state.lock().unwrap().spawned.clone()
    }

    pub fn stop_calls(&self) -> usize {
        self.state.lock().unwrap().stop_calls
    }

    pub fn force_stop_calls(&self) -> usize {
        self.state.lock().unwrap().force_stop_calls
    }
}

impl ProcessRunner for FakeProcessRunner {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        let script = {
            let mut st = self.state.lock().unwrap();
            st.spawned.push(spec.clone());
            st.scripts
                .pop_front()
                .ok_or_else(|| ProcessError::UnexpectedSpawn {
                    program: spec.program.clone(),
                })?
        };
        let identity = script.identity.clone();
        let control = Arc::new(FakeChildControl {
            runner_state: Arc::clone(&self.state),
            stop_state: Mutex::new(FakeStopState::default()),
            condvar: Condvar::new(),
        });

        Ok(RunHandle {
            identity,
            backend: Box::new(FakeRunHandleBackend {
                script,
                limits: options.limits,
                line_observer: options.line_observer,
                control,
            }),
        })
    }
}

// Atomic flag used by the test child's SIGINT handler (no shell scripts anywhere).
#[cfg(all(test, unix))]
static TEST_CHILD_GOT_SIGINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(all(test, unix))]
extern "C" fn test_child_sigint_handler(_sig: libc::c_int) {
    TEST_CHILD_GOT_SIGINT.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper entry point launched directly by argv (`current_exe()`) by runner
    /// unit tests so no `/bin/sh -c` shell strings are ever used.
    #[test]
    fn child_helper_entry() {
        let Ok(mode) = env::var("FOLDERSYNC_TEST_CHILD_MODE") else {
            return;
        };
        match mode.as_str() {
            "stdin_and_lines" => {
                let mut buf = Vec::new();
                let _ = std::io::stdin().read_to_end(&mut buf);
                if !buf.is_empty() {
                    println!("GOT_STDIN");
                }
                eprintln!("err1");
                eprintln!("err2");
                eprintln!("err3");
                eprintln!("err4");
                std::process::exit(0);
            }
            #[cfg(unix)]
            "graceful_sigint" => {
                TEST_CHILD_GOT_SIGINT.store(0, Ordering::SeqCst);
                unsafe {
                    libc::signal(
                        libc::SIGINT,
                        test_child_sigint_handler as *const () as libc::sighandler_t,
                    );
                }
                eprintln!("READY");
                let _ = std::io::stderr().flush();
                let deadline = Instant::now() + Duration::from_secs(10);
                while TEST_CHILD_GOT_SIGINT.load(Ordering::SeqCst) == 0 && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(10));
                }
                let count = TEST_CHILD_GOT_SIGINT.load(Ordering::SeqCst);
                eprintln!("GOT_SIGINT:{count}");
                std::process::exit(if count == 1 { 42 } else { 99 });
            }
            #[cfg(unix)]
            "stubborn_sigint" => {
                unsafe {
                    libc::signal(libc::SIGINT, libc::SIG_IGN);
                }
                eprintln!("STUBBORN_READY");
                let _ = std::io::stderr().flush();
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(50));
                }
                std::process::exit(0);
            }
            "dump_env" => {
                for (k, v) in env::vars() {
                    eprintln!("ENV:{k}={v}");
                }
                std::process::exit(0);
            }
            "verify_env_cleared_in_subproc" => {
                let runner = RealProcessRunner;
                let mut child_env = minimal_isolated_env(None);
                child_env.insert(
                    OsString::from("FOLDERSYNC_TEST_CHILD_MODE"),
                    OsString::from("dump_env"),
                );
                let spec = ProcessSpec {
                    program: env::current_exe().unwrap(),
                    args: vec![
                        OsString::from("--exact"),
                        OsString::from("process::tests::child_helper_entry"),
                        OsString::from("--nocapture"),
                    ],
                    current_dir: None,
                    env: child_env,
                    expected_binary_sha256: None,
                };
                let out = runner
                    .spawn(&spec, SpawnOptions::default())
                    .unwrap()
                    .wait()
                    .unwrap();
                let stderr = out.output.stderr_text();
                assert!(out.exit_status.is_success());
                assert!(
                    !stderr.contains("RCLONE_FORCE"),
                    "RCLONE_FORCE leaked: {stderr}"
                );
                assert!(
                    !stderr.contains("RCLONE_RESYNC"),
                    "RCLONE_RESYNC leaked: {stderr}"
                );
                std::process::exit(0);
            }
            other => panic!("unknown FOLDERSYNC_TEST_CHILD_MODE: {other}"),
        }
    }

    fn helper_spec(mode: &str) -> ProcessSpec {
        let mut child_env = minimal_isolated_env(None);
        child_env.insert(
            OsString::from("FOLDERSYNC_TEST_CHILD_MODE"),
            OsString::from(mode),
        );
        ProcessSpec {
            program: env::current_exe().unwrap(),
            args: vec![
                OsString::from("--exact"),
                OsString::from("process::tests::child_helper_entry"),
                OsString::from("--nocapture"),
            ],
            current_dir: None,
            env: child_env,
            expected_binary_sha256: None,
        }
    }

    #[test]
    fn bounded_buffer_caps_lines_and_bytes_and_counts_drops() {
        let mut buf = BoundedLineBuffer::new(OutputLimits {
            max_total_bytes: 20,
            max_lines: 3,
            max_line_bytes: 10,
            max_diagnostic_events: 10,
        });
        for i in 0..5 {
            buf.push(OutputLine {
                stream: StreamKind::Stderr,
                text: format!("line-{i:02}"), // 7 bytes each
                truncated: false,
            });
        }
        let captured = buf.into_captured();
        assert!(captured.retained_bytes <= 20);
        assert_eq!(captured.lines.len(), 2);
        assert_eq!(captured.lines[0].text, "line-03");
        assert_eq!(captured.lines[1].text, "line-04");
        assert_eq!(captured.dropped_lines, 3);
        assert_eq!(captured.dropped_bytes, 21);
    }

    #[test]
    fn retains_early_conflict_diagnostics_even_when_ordinary_lines_are_evicted() {
        let mut buf = BoundedLineBuffer::new(OutputLimits {
            max_total_bytes: 4096,
            max_lines: 5,
            max_line_bytes: 1024,
            max_diagnostic_events: 10,
        });
        buf.push(OutputLine {
            stream: StreamKind::Stderr,
            text: r#"{"level":"notice","msg":"- Path1    Renaming Path1 copy - /tmp/p1/a.txt.conflict1","source":"bisync/resolve.go:318"}"#.to_string(),
            truncated: false,
        });
        for i in 0..25 {
            buf.push(OutputLine {
                stream: StreamKind::Stderr,
                text: format!(r#"{{"level":"info","msg":"ordinary info line {i}"}}"#),
                truncated: false,
            });
        }
        let captured = buf.into_captured();
        assert_eq!(captured.lines.len(), 5);
        assert_eq!(captured.dropped_lines, 21);
        assert_eq!(captured.diagnostic_events.len(), 1);
        assert_eq!(captured.dropped_diagnostic_events, 0);
        assert!(!captured.diagnosis_incomplete());
    }

    #[test]
    fn bounded_nonblocking_observer_drops_without_blocking_and_counts_drops() {
        let runner = FakeProcessRunner::new();
        let lines: Vec<OutputLine> = (0..10)
            .map(|i| OutputLine {
                stream: StreamKind::Stderr,
                text: format!(r#"{{"level":"info","msg":"line {i}"}}"#),
                truncated: false,
            })
            .collect();
        runner.push_run(FakeRunScript {
            lines,
            ..Default::default()
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let spec = ProcessSpec {
            program: PathBuf::from("/fake/rclone"),
            args: vec![OsString::from("bisync")],
            current_dir: None,
            env: BTreeMap::new(),
            expected_binary_sha256: None,
        };
        // Never drain `rx` while `wait()` runs: must not block and must count 9 drops.
        let out = runner
            .spawn(
                &spec,
                SpawnOptions {
                    limits: OutputLimits::default(),
                    line_observer: Some(tx),
                },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(out.output.observer_dropped_lines, 9);
        assert_eq!(
            rx.try_recv().unwrap().text,
            r#"{"level":"info","msg":"line 0"}"#
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reads_current_process_start_time_from_os() {
        let start = read_process_start_time(std::process::id());
        assert!(
            start.is_some(),
            "OS should report start time for current process"
        );
    }

    #[test]
    fn fake_runner_records_specs_and_stop_signals() {
        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            exit_status_on_stop: Some(ExitStatusInfo {
                code: Some(130),
                signal: None,
            }),
            wait_for_stop: true,
            ..Default::default()
        });

        let spec = ProcessSpec {
            program: PathBuf::from("/fake/rclone"),
            args: vec![OsString::from("version")],
            current_dir: None,
            env: BTreeMap::new(),
            expected_binary_sha256: None,
        };
        let handle = runner.spawn(&spec, SpawnOptions::default()).unwrap();
        let stopper = handle.stop_handle();
        let waiter = std::thread::spawn(move || handle.wait().unwrap());
        stopper.request_stop().unwrap();
        let out = waiter.join().unwrap();

        assert_eq!(out.exit_status.code, Some(130));
        assert!(out.stop_requested);
        assert_eq!(runner.spawned_specs(), vec![spec]);
        assert_eq!(runner.stop_calls(), 1);
    }

    #[test]
    fn real_runner_clears_ambient_rclone_env_variables() {
        // Spawn a subprocess whose parent env explicitly contains RCLONE_FORCE and RCLONE_RESYNC;
        // inside that subprocess, RealProcessRunner spawns a child with minimal_isolated_env and
        // asserts neither variable reaches the child.
        let status = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::child_helper_entry",
                "--nocapture",
            ])
            .env("RCLONE_FORCE", "true")
            .env("RCLONE_RESYNC", "true")
            .env(
                "FOLDERSYNC_TEST_CHILD_MODE",
                "verify_env_cleared_in_subproc",
            )
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "child env isolation check failed with status {status:?}"
        );
    }

    #[test]
    fn real_runner_closes_stdin_captures_bounded_output_and_reports_identity() {
        let runner = RealProcessRunner;
        let spec = helper_spec("stdin_and_lines");
        let handle = runner
            .spawn(
                &spec,
                SpawnOptions {
                    limits: OutputLimits {
                        max_total_bytes: 1024,
                        max_lines: 2,
                        max_line_bytes: 64,
                        max_diagnostic_events: 16,
                    },
                    line_observer: None,
                },
            )
            .unwrap();
        assert!(handle.identity().pid > 0);
        assert!(handle.identity().start_time.is_some());
        let out = handle.wait().unwrap();
        assert!(out.exit_status.is_success());
        assert!(!out.output.stdout_text().contains("GOT_STDIN"));
        assert_eq!(out.output.lines.len(), 2);
        assert!(out.output.dropped_lines >= 2);
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_request_stop_sends_sigint_once_and_force_stop_kills_stubborn_child() {
        let runner = RealProcessRunner;

        let spec_graceful = helper_spec("graceful_sigint");
        let (tx, rx) = mpsc::sync_channel(64);
        let handle = runner
            .spawn(
                &spec_graceful,
                SpawnOptions {
                    limits: OutputLimits::default(),
                    line_observer: Some(tx),
                },
            )
            .unwrap();
        loop {
            let line = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if line.text == "READY" {
                break;
            }
        }

        handle.request_stop().unwrap();
        // Second call must be idempotent (does not send a second SIGINT).
        handle.request_stop().unwrap();
        let out = handle.wait().unwrap();
        assert_eq!(out.exit_status.code, Some(42));
        assert!(out.stop_requested);

        let spec_stubborn = helper_spec("stubborn_sigint");
        let (tx2, rx2) = mpsc::sync_channel(64);
        let handle2 = runner
            .spawn(
                &spec_stubborn,
                SpawnOptions {
                    limits: OutputLimits::default(),
                    line_observer: Some(tx2),
                },
            )
            .unwrap();
        loop {
            let line2 = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
            if line2.text == "STUBBORN_READY" {
                break;
            }
        }

        handle2.force_stop(Duration::from_millis(150)).unwrap();
        let out2 = handle2.wait().unwrap();
        assert_eq!(out2.exit_status.signal, Some(libc::SIGKILL));
        assert!(out2.stop_requested);
    }

    #[test]
    fn redacts_password_command_stderr_payloads_from_capture_and_observer() {
        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            lines: vec![
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: r#"{"level":"error","msg":"--password-command stderr: SUPER_SECRET_PASS","source":"config/crypt.go:199"}"#.to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: "2026/11/20 19:00:00 ERROR : --password-command stderr: RAW_SECRET_999"
                        .to_string(),
                    truncated: false,
                },
            ],
            exit_status: ExitStatusInfo {
                code: Some(1),
                signal: None,
            },
            ..Default::default()
        });
        let (tx, rx) = mpsc::sync_channel(8);
        let spec = ProcessSpec {
            program: PathBuf::from("/fake/rclone"),
            args: vec![OsString::from("bisync")],
            current_dir: None,
            env: BTreeMap::new(),
            expected_binary_sha256: None,
        };
        let out = runner
            .spawn(
                &spec,
                SpawnOptions {
                    limits: OutputLimits::default(),
                    line_observer: Some(tx),
                },
            )
            .unwrap()
            .wait()
            .unwrap();
        let all_stderr = out.output.stderr_text();
        assert!(
            !all_stderr.contains("SUPER_SECRET_PASS"),
            "secret leaked to capture: {all_stderr}"
        );
        assert!(
            !all_stderr.contains("RAW_SECRET_999"),
            "secret leaked to capture: {all_stderr}"
        );
        assert!(all_stderr.contains("[REDACTED]"));

        // Verify live observer also received redacted lines only.
        let observed: Vec<String> = rx.try_iter().map(|l| l.text).collect();
        assert_eq!(observed.len(), 2);
        for line in &observed {
            assert!(
                !line.contains("SUPER_SECRET_PASS"),
                "secret leaked to observer: {line}"
            );
            assert!(
                !line.contains("RAW_SECRET_999"),
                "secret leaked to observer: {line}"
            );
            assert!(line.contains("[REDACTED]"));
        }
    }

    #[test]
    fn benign_conflict_filename_containing_helper_prefix_is_not_redacted() {
        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            lines: vec![
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: r#"{"level":"notice","msg":"- \u001b[34mWARNING\u001b[0m  \u001b[35mNew or changed in both paths\u001b[0m       - \u001b[36mnotes/--password-command stderr: draft.txt\u001b[0m","source":"bisync/deltas.go:386"}"#.to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: r#"{"level":"notice","msg":"- \u001b[36mPath1\u001b[0m    \u001b[35mRenaming Path1 copy\u001b[0m                - \u001b[36m/tmp/p1/notes/--password-command stderr: draft.txt.conflict1\u001b[0m","source":"bisync/resolve.go:318"}"#.to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: r#"{"level":"notice","msg":"- \u001b[34mPath2\u001b[0m    \u001b[35mRenaming Path2 copy\u001b[0m                - \u001b[36m/tmp/p2/notes/--password-command stderr: draft.txt.conflict2\u001b[0m","source":"bisync/resolve.go:318"}"#.to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: r#"{"level":"info","msg":"\u001b[32mBisync successful\u001b[0m","source":"bisync/operations.go:234"}"#.to_string(),
                    truncated: false,
                },
            ],
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            ..Default::default()
        });
        let spec = ProcessSpec {
            program: PathBuf::from("/fake/rclone"),
            args: vec![OsString::from("bisync")],
            current_dir: None,
            env: BTreeMap::new(),
            expected_binary_sha256: None,
        };
        let out = runner
            .spawn(&spec, SpawnOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        let outcome = crate::outcome::classify_run_outcome(&out);
        assert_eq!(
            outcome,
            crate::outcome::Outcome::SuccessWithConflicts {
                paths: vec![
                    "/tmp/p1/notes/--password-command stderr: draft.txt.conflict1".to_string(),
                    "/tmp/p2/notes/--password-command stderr: draft.txt.conflict2".to_string(),
                ]
            }
        );
    }
}
