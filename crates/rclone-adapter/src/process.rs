//! Process spawning, bounded output capture, OS process identity, and graceful
//! signal handling without assuming a specific async runtime.

use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
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

/// Specification of a child process spawned directly by argv (never via a shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
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

/// Memory bounds for line-by-line stdout/stderr capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    /// Maximum total retained bytes across all stored lines.
    pub max_total_bytes: usize,
    /// Maximum number of retained lines.
    pub max_lines: usize,
    /// Maximum bytes retained for any individual line.
    pub max_line_bytes: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 4 * 1024 * 1024,
            max_lines: 20_000,
            max_line_bytes: 64 * 1024,
        }
    }
}

/// Spawn options combining memory bounds with an optional live line observer.
#[derive(Debug, Default)]
pub struct SpawnOptions {
    pub limits: OutputLimits,
    pub line_observer: Option<mpsc::Sender<OutputLine>>,
}

/// Bounded ring buffer of captured output lines with drop/truncation counters.
#[derive(Debug, Clone)]
pub struct BoundedLineBuffer {
    limits: OutputLimits,
    lines: VecDeque<OutputLine>,
    retained_bytes: usize,
    dropped_lines: u64,
    dropped_bytes: u64,
    truncated_lines: u64,
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
        }
    }

    pub fn push(&mut self, mut line: OutputLine) {
        if truncate_utf8(&mut line.text, self.limits.max_line_bytes) {
            line.truncated = true;
        }
        if line.truncated {
            self.truncated_lines += 1;
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
        }
    }
}

/// Bounded stdout/stderr lines and accounting counters captured from a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    pub lines: Vec<OutputLine>,
    pub retained_bytes: usize,
    pub dropped_lines: u64,
    pub dropped_bytes: u64,
    pub truncated_lines: u64,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Real OS process runner using `std::process::Command`, `Stdio::null()` stdin,
/// and dedicated reader threads for bounded stdout/stderr capture.
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
        // Re-check before signaling so we never signal a zombie we are about to reap.
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
                // First SIGINT starts graceful shutdown; deliver a second SIGINT briefly
                // afterward to trigger rclone's immediate un-graceful shutdown path.
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
    observer: Option<mpsc::Sender<OutputLine>>,
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
    observer: &Option<mpsc::Sender<OutputLine>>,
) {
    if raw.last() == Some(&b'\r') {
        raw.pop();
    }
    let text = String::from_utf8_lossy(raw).into_owned();
    let line = OutputLine {
        stream,
        text,
        truncated: *truncated,
    };
    raw.clear();
    *truncated = false;

    if let Some(tx) = observer {
        let _ = tx.send(line.clone());
    }
    buffer.lock().unwrap().push(line);
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
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
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
    line_observer: Option<mpsc::Sender<OutputLine>>,
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
        for line in self.script.lines {
            if let Some(tx) = &self.line_observer {
                let _ = tx.send(line.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_buffer_caps_lines_and_bytes_and_counts_drops() {
        let mut buf = BoundedLineBuffer::new(OutputLimits {
            max_total_bytes: 20,
            max_lines: 3,
            max_line_bytes: 10,
        });
        for i in 0..5 {
            buf.push(OutputLine {
                stream: StreamKind::Stderr,
                text: format!("line-{i:02}"), // 7 bytes each
                truncated: false,
            });
        }
        let captured = buf.into_captured();
        // 20 bytes holds at most two 7-byte lines (14 bytes <= 20; 3 lines = 21 > 20).
        assert!(captured.retained_bytes <= 20);
        assert_eq!(captured.lines.len(), 2);
        assert_eq!(captured.lines[0].text, "line-03");
        assert_eq!(captured.lines[1].text, "line-04");
        assert_eq!(captured.dropped_lines, 3);
        assert_eq!(captured.dropped_bytes, 21);
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

    #[cfg(unix)]
    #[test]
    fn real_runner_closes_stdin_captures_bounded_output_and_reports_identity() {
        let runner = RealProcessRunner;
        // `sh -c` is used ONLY with `/bin/sh` in this unit test to test stdin EOF + bounded capture.
        let spec = ProcessSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(
                    "read x && echo GOT_STDIN; echo out1; echo err1 >&2; echo err2 >&2; echo err3 >&2",
                ),
            ],
            current_dir: None,
        };
        let handle = runner
            .spawn(
                &spec,
                SpawnOptions {
                    limits: OutputLimits {
                        max_total_bytes: 1024,
                        max_lines: 2,
                        max_line_bytes: 64,
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
        assert_eq!(out.output.dropped_lines, 2);
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_request_stop_sends_sigint_once_and_force_stop_kills_stubborn_child() {
        let runner = RealProcessRunner;

        // Child traps SIGINT once and exits with code 42 on first SIGINT.
        let spec_graceful = ProcessSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(
                    "trap 'echo GOT_SIGINT; exit 42' INT; echo READY; while true; do sleep 1; done",
                ),
            ],
            current_dir: None,
        };
        let (tx, rx) = mpsc::channel();
        let handle = runner
            .spawn(
                &spec_graceful,
                SpawnOptions {
                    limits: OutputLimits::default(),
                    line_observer: Some(tx),
                },
            )
            .unwrap();
        // Wait until child prints READY so trap is installed.
        let line = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(line.text, "READY");

        handle.request_stop().unwrap();
        // Second call must be idempotent (does not send a second SIGINT).
        handle.request_stop().unwrap();
        let out = handle.wait().unwrap();
        assert_eq!(out.exit_status.code, Some(42));
        assert!(out.stop_requested);

        // Stubborn child ignores SIGINT completely; force_stop must escalate to SIGKILL.
        let spec_stubborn = ProcessSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from("trap '' INT; echo STUBBORN_READY; while true; do sleep 1; done"),
            ],
            current_dir: None,
        };
        let (tx2, rx2) = mpsc::channel();
        let handle2 = runner
            .spawn(
                &spec_stubborn,
                SpawnOptions {
                    limits: OutputLimits::default(),
                    line_observer: Some(tx2),
                },
            )
            .unwrap();
        let line2 = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(line2.text, "STUBBORN_READY");

        handle2.force_stop(Duration::from_millis(150)).unwrap();
        let out2 = handle2.wait().unwrap();
        assert_eq!(out2.exit_status.signal, Some(libc::SIGKILL));
        assert!(out2.stop_requested);
    }
}
