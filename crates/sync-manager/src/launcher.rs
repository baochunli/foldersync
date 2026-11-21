//! Sync execution launcher abstraction bridging `rclone-adapter` and deterministic test fakes.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use rclone_adapter::{
    BackupDir2, BisyncRequest, BoundedLineBuffer, ChildIdentity, EngineIdentity, ExitStatusInfo,
    OutputLine, Path2, RealProcessRunner, RunHandle, RunMode, RunOutput, SpawnOptions, StopHandle,
    qualify_binary, qualify_pinned_binary,
};

use crate::error::ManagerError;

const FORCE_STOP_KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// Validated launch parameters handed by the manager to a [`BisyncLauncher`].
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub run_id: String,
    pub mode: RunMode,
    pub path1: PathBuf,
    pub path2: Path2,
    pub config_path: PathBuf,
    pub workdir: PathBuf,
    pub filters_file: PathBuf,
    pub check_filename: String,
    pub max_delete_percent: u8,
    pub backup_dir1: PathBuf,
    pub backup_dir2: BackupDir2,
}

/// Engine identity summary recorded in run artifacts and SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineMetadata {
    pub version: String,
    pub sha256: String,
    pub platform: String,
    pub executable: PathBuf,
}

impl From<&EngineIdentity> for EngineMetadata {
    fn from(id: &EngineIdentity) -> Self {
        Self {
            version: id.version().to_string(),
            sha256: id.sha256().to_string(),
            platform: id.platform().to_string(),
            executable: id.path().to_path_buf(),
        }
    }
}

/// Active launched run handle returned immediately after spawning.
pub struct LaunchedRun {
    pub identity: ChildIdentity,
    pub sanitized_argv: Vec<String>,
    pub engine: EngineMetadata,
    pub simulate_crash_before_pid_record: bool,
    backend: Box<dyn LaunchedRunBackend>,
}

pub(crate) trait LaunchedRunBackend: Send {
    fn stop_handle(&self) -> Arc<dyn LaunchStopper>;
    fn wait(self: Box<Self>) -> Result<RunOutput, ManagerError>;
}

pub(crate) trait LaunchStopper: Send + Sync {
    fn request_stop(&self) -> Result<(), ManagerError>;
    fn force_stop(&self) -> Result<(), ManagerError>;
}

impl LaunchedRun {
    pub(crate) fn stop_handle(&self) -> Arc<dyn LaunchStopper> {
        self.backend.stop_handle()
    }

    pub fn wait(self) -> Result<RunOutput, ManagerError> {
        self.backend.wait()
    }
}

/// Prepared launch ready for immediate spawning once final pre-spawn checks pass.
pub struct PreparedLaunch {
    backend: Box<dyn PreparedLaunchBackend>,
}

trait PreparedLaunchBackend: Send {
    fn spawn(self: Box<Self>, options: SpawnOptions) -> Result<LaunchedRun, ManagerError>;
}

impl PreparedLaunch {
    /// Spawns the prepared child process.
    pub fn spawn(self, options: SpawnOptions) -> Result<LaunchedRun, ManagerError> {
        self.backend.spawn(options)
    }
}

/// Abstraction preparing and launching a validated bisync invocation.
pub trait BisyncLauncher: Send + Sync {
    /// Engine version/profile metadata for recording a configuration generation.
    fn engine_metadata(&self) -> Result<EngineMetadata, ManagerError>;

    /// Qualifies the engine and validates the launch plan BEFORE the final
    /// pre-spawn root/marker and cancellation check.
    fn prepare(&self, plan: &LaunchPlan) -> Result<PreparedLaunch, ManagerError>;

    /// Convenience helper combining `prepare` and `spawn`.
    fn launch(
        &self,
        plan: &LaunchPlan,
        options: SpawnOptions,
    ) -> Result<LaunchedRun, ManagerError> {
        self.prepare(plan)?.spawn(options)
    }
}

/// Real launcher backed by `rclone-adapter`'s pinned binary qualification,
/// `BisyncRequest`, and `RealProcessRunner`.
#[derive(Debug, Default, Clone)]
pub struct RealBisyncLauncher {
    binary_override: Option<PathBuf>,
    qualified: Arc<Mutex<Option<EngineIdentity>>>,
}

impl RealBisyncLauncher {
    pub fn new(binary_override: Option<PathBuf>) -> Self {
        Self {
            binary_override,
            ..Self::default()
        }
    }

    fn resolve_and_qualify(&self) -> Result<EngineIdentity, ManagerError> {
        let mut qualified = self.qualified.lock().unwrap();
        if let Some(engine) = qualified.as_ref() {
            return Ok(engine.clone());
        }
        let engine = match self.binary_override.as_deref() {
            Some(path) => qualify_binary(path)?,
            None => qualify_pinned_binary()?.ok_or_else(|| ManagerError::Engine {
                cause: "Pinned rclone binary is not installed.".to_string(),
                next_step:
                    "Run scripts/fetch-rclone.sh to install the verified development engine."
                        .to_string(),
            })?,
        };
        *qualified = Some(engine.clone());
        Ok(engine)
    }
}

struct RealStopper {
    handle: StopHandle,
}

impl LaunchStopper for RealStopper {
    fn request_stop(&self) -> Result<(), ManagerError> {
        self.handle.request_stop().map_err(|err| ManagerError::Io {
            cause: format!("Failed to send graceful stop signal to rclone: {err}."),
            next_step: "Check process status or use Force stop if the child is unresponsive."
                .to_string(),
        })
    }

    fn force_stop(&self) -> Result<(), ManagerError> {
        self.handle
            .force_stop(FORCE_STOP_KILL_TIMEOUT)
            .map_err(|err| ManagerError::Io {
                cause: format!("Failed to force-stop rclone child: {err}."),
                next_step: "Inspect recovery status and verify no child process remains."
                    .to_string(),
            })
    }
}

struct RealLaunchedBackend {
    handle: RunHandle,
}

impl LaunchedRunBackend for RealLaunchedBackend {
    fn stop_handle(&self) -> Arc<dyn LaunchStopper> {
        Arc::new(RealStopper {
            handle: self.handle.stop_handle(),
        })
    }

    fn wait(self: Box<Self>) -> Result<RunOutput, ManagerError> {
        self.handle.wait().map_err(|err| ManagerError::Io {
            cause: format!("Failed waiting for rclone child process: {err}."),
            next_step: "Inspect recovery status and run logs.".to_string(),
        })
    }
}

struct RealPreparedBackend {
    req: BisyncRequest,
    engine: EngineIdentity,
    sanitized_argv: Vec<String>,
}

impl PreparedLaunchBackend for RealPreparedBackend {
    fn spawn(self: Box<Self>, options: SpawnOptions) -> Result<LaunchedRun, ManagerError> {
        let runner = RealProcessRunner;
        let handle = self.req.spawn(&runner, options)?;
        let identity = handle.identity().clone();
        Ok(LaunchedRun {
            identity,
            sanitized_argv: self.sanitized_argv,
            engine: EngineMetadata::from(&self.engine),
            simulate_crash_before_pid_record: false,
            backend: Box::new(RealLaunchedBackend { handle }),
        })
    }
}

impl BisyncLauncher for RealBisyncLauncher {
    fn engine_metadata(&self) -> Result<EngineMetadata, ManagerError> {
        let engine = self.resolve_and_qualify()?;
        Ok(EngineMetadata::from(&engine))
    }

    fn prepare(&self, plan: &LaunchPlan) -> Result<PreparedLaunch, ManagerError> {
        let engine = self.resolve_and_qualify()?;
        let req = BisyncRequest::builder()
            .engine(engine.clone())
            .mode(plan.mode)
            .path1(&plan.path1)
            .path2(plan.path2.clone())
            .config_path(&plan.config_path)
            .workdir(&plan.workdir)
            .filters_file(&plan.filters_file)
            .check_filename(&plan.check_filename)
            .max_delete_percent(plan.max_delete_percent)
            .backup_dir1(&plan.backup_dir1)
            .backup_dir2(plan.backup_dir2.clone())
            .build()?;

        let sanitized_argv = req
            .to_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        Ok(PreparedLaunch {
            backend: Box::new(RealPreparedBackend {
                req,
                engine,
                sanitized_argv,
            }),
        })
    }
}

/// Callback invoked inside [`FakeBisyncLauncher`] during `prepare` or `spawn`.
pub type SpawnCallback = Arc<dyn Fn(&LaunchPlan) + Send + Sync>;

/// Scripted fake run for deterministic manager tests.
#[derive(Clone)]
pub struct FakeLaunchScript {
    pub identity: ChildIdentity,
    pub lines: Vec<OutputLine>,
    pub exit_status: ExitStatusInfo,
    pub exit_status_on_stop: Option<ExitStatusInfo>,
    pub exit_status_on_force_stop: Option<ExitStatusInfo>,
    pub wait_for_stop: bool,
    pub simulate_crash_before_pid_record: bool,
    pub on_prepare: Option<SpawnCallback>,
    pub on_spawn: Option<SpawnCallback>,
    /// Optional hook invoked inside `wait()` (outside manager mutex) to simulate slow/hung children.
    pub on_wait: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for FakeLaunchScript {
    fn default() -> Self {
        Self {
            identity: ChildIdentity {
                pid: 4242,
                start_time: Some(rclone_adapter::ProcessStartTime::UnixMicros(
                    1_700_000_000_000_000,
                )),
                executable: PathBuf::from("/fake/rclone"),
            },
            lines: Vec::new(),
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            exit_status_on_stop: Some(ExitStatusInfo {
                code: Some(130),
                signal: None,
            }),
            exit_status_on_force_stop: Some(ExitStatusInfo {
                code: None,
                signal: Some(9),
            }),
            wait_for_stop: false,
            simulate_crash_before_pid_record: false,
            on_prepare: None,
            on_spawn: None,
            on_wait: None,
        }
    }
}

/// Deterministic fake [`BisyncLauncher`] for manager unit/integration tests.
#[derive(Default, Clone)]
pub struct FakeBisyncLauncher {
    state: Arc<Mutex<FakeLauncherState>>,
}

#[derive(Default)]
struct FakeLauncherState {
    scripts: VecDeque<FakeLaunchScript>,
    launched: Vec<LaunchPlan>,
    stop_calls: usize,
    force_stop_calls: usize,
}

impl FakeBisyncLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_script(&self, script: FakeLaunchScript) {
        self.state.lock().unwrap().scripts.push_back(script);
    }

    pub fn launched_plans(&self) -> Vec<LaunchPlan> {
        self.state.lock().unwrap().launched.clone()
    }

    pub fn stop_calls(&self) -> usize {
        self.state.lock().unwrap().stop_calls
    }

    pub fn force_stop_calls(&self) -> usize {
        self.state.lock().unwrap().force_stop_calls
    }
}

struct FakeStopper {
    launcher_state: Arc<Mutex<FakeLauncherState>>,
    stop_state: Mutex<(bool, bool)>,
    condvar: Condvar,
}

impl LaunchStopper for FakeStopper {
    fn request_stop(&self) -> Result<(), ManagerError> {
        self.launcher_state.lock().unwrap().stop_calls += 1;
        let mut st = self.stop_state.lock().unwrap();
        st.0 = true;
        self.condvar.notify_all();
        Ok(())
    }

    fn force_stop(&self) -> Result<(), ManagerError> {
        self.launcher_state.lock().unwrap().force_stop_calls += 1;
        let mut st = self.stop_state.lock().unwrap();
        st.0 = true;
        st.1 = true;
        self.condvar.notify_all();
        Ok(())
    }
}

struct FakeLaunchedBackend {
    script: FakeLaunchScript,
    stopper: Arc<FakeStopper>,
    options: SpawnOptions,
}

impl LaunchedRunBackend for FakeLaunchedBackend {
    fn stop_handle(&self) -> Arc<dyn LaunchStopper> {
        Arc::clone(&self.stopper) as Arc<dyn LaunchStopper>
    }

    fn wait(self: Box<Self>) -> Result<RunOutput, ManagerError> {
        let mut buf = BoundedLineBuffer::new(self.options.limits);
        for line in self.script.lines {
            if let Some(tx) = &self.options.line_observer
                && tx.try_send(line.clone()).is_err()
            {
                buf.record_observer_drop();
            }
            buf.push(line);
        }

        if let Some(cb) = &self.script.on_wait {
            cb();
        }
        let (stop_requested, force_stopped) = {
            let mut st = self.stopper.stop_state.lock().unwrap();
            if self.script.wait_for_stop {
                while !st.0 {
                    st = self.stopper.condvar.wait(st).unwrap();
                }
            }
            (st.0, st.1)
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

struct FakePreparedBackend {
    launcher_state: Arc<Mutex<FakeLauncherState>>,
    plan: LaunchPlan,
    script: FakeLaunchScript,
    engine: EngineMetadata,
}

impl PreparedLaunchBackend for FakePreparedBackend {
    fn spawn(self: Box<Self>, options: SpawnOptions) -> Result<LaunchedRun, ManagerError> {
        self.launcher_state
            .lock()
            .unwrap()
            .launched
            .push(self.plan.clone());

        if let Some(cb) = &self.script.on_spawn {
            cb(&self.plan);
        }

        let identity = self.script.identity.clone();
        let simulate_crash = self.script.simulate_crash_before_pid_record;
        let stopper = Arc::new(FakeStopper {
            launcher_state: Arc::clone(&self.launcher_state),
            stop_state: Mutex::new((false, false)),
            condvar: Condvar::new(),
        });

        let sanitized_argv = vec![
            "bisync".to_string(),
            self.plan.path1.to_string_lossy().into_owned(),
            format!("{:?}", self.plan.path2),
            "--workdir".to_string(),
            self.plan.workdir.to_string_lossy().into_owned(),
        ];

        Ok(LaunchedRun {
            identity,
            sanitized_argv,
            engine: self.engine,
            simulate_crash_before_pid_record: simulate_crash,
            backend: Box::new(FakeLaunchedBackend {
                script: self.script,
                stopper,
                options,
            }),
        })
    }
}

impl BisyncLauncher for FakeBisyncLauncher {
    fn engine_metadata(&self) -> Result<EngineMetadata, ManagerError> {
        Ok(EngineMetadata {
            version: rclone_adapter::PINNED_RCLONE_VERSION.to_string(),
            sha256: "0".repeat(64),
            platform: rclone_adapter::host_platform_key()
                .unwrap_or("osx-arm64")
                .to_string(),
            executable: PathBuf::from("/fake/rclone"),
        })
    }

    fn prepare(&self, plan: &LaunchPlan) -> Result<PreparedLaunch, ManagerError> {
        let script = {
            let mut st = self.state.lock().unwrap();
            st.scripts.pop_front().ok_or_else(|| ManagerError::Engine {
                cause: format!(
                    "FakeBisyncLauncher has no scripted run queued for `{}`.",
                    plan.run_id
                ),
                next_step: "Push a FakeLaunchScript before invoking this run in tests.".to_string(),
            })?
        };

        if let Some(cb) = &script.on_prepare {
            cb(plan);
        }

        Ok(PreparedLaunch {
            backend: Box::new(FakePreparedBackend {
                launcher_state: Arc::clone(&self.state),
                plan: plan.clone(),
                script,
                engine: self.engine_metadata()?,
            }),
        })
    }
}
