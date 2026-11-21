//! Injectable OS/system actions boundary (`reveal_path`, `open_url`,
//! `set_launch_at_login`, `notify`) with a real macOS implementation (used ONLY
//! in the real app run) and a recording fake for headless/unit/demo tests.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use nsh_core::{desktop::LaunchAtLoginError, notifications::Notification};

/// Result of toggling "Open at login".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAtLoginOutcome {
    #[default]
    Enabled,
    Disabled,
    /// Enabled, waiting for user approval in System Settings -> Login Items.
    PendingApproval,
    /// Not available (e.g. unpackaged `nsh run` or unsupported platform).
    Unavailable,
}

/// Injectable interface for platform/system side effects.
///
/// Tests and `FOLDERSYNC_DEMO=1` always inject [`RecordingSystemActions`] so
/// Finder, browser URLs, macOS Login Items, and Notification Center are never
/// touched outside a real interactive app run.
pub trait SystemActions: Send + Sync {
    /// Reveals a local file or folder in Finder.
    fn reveal_path(&self, path: &Path) -> Result<(), String>;

    /// Opens an allow-listed URL (`https://` or `x-apple.systempreferences:`).
    fn open_url(&self, url: &str) -> Result<(), String>;

    /// Updates the system Launch-at-Login registration.
    fn set_launch_at_login(&self, enabled: bool) -> Result<LaunchAtLoginOutcome, String>;

    /// Posts a user notification (only works in a signed bundle on macOS).
    fn notify(&self, id: &str, title: &str, body: &str) -> Result<bool, String>;
}

/// Request dispatched from the manager worker thread to the NSH main thread
/// so platform desktop APIs run strictly on the main reactive thread (HIGH #8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemActionRequest {
    RevealPath(PathBuf),
    OpenUrl(String),
    SetLaunchAtLogin(bool),
    Notify {
        id: String,
        title: String,
        body: String,
    },
}

/// Real outcome returned from the NSH main thread back to the manager worker (HIGH #8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemActionOutcome {
    RevealPath(Result<(), String>),
    OpenUrl(Result<(), String>),
    SetLaunchAtLogin(Result<LaunchAtLoginOutcome, String>),
    Notify(Result<bool, String>),
}

/// Executes one real macOS system action strictly on the NSH main thread (inside
/// `App()`'s async runtime) and returns the actual result — never panics off-thread
/// and never invents a fake launch-at-login outcome (HIGH #8).
pub async fn execute_system_action_on_main_thread(req: SystemActionRequest) -> SystemActionOutcome {
    match req {
        SystemActionRequest::RevealPath(path) => {
            if !path.exists() {
                return SystemActionOutcome::RevealPath(Err(format!(
                    "Folder or file `{}` does not exist on this Mac right now.",
                    path.display()
                )));
            }
            match nsh_core::prelude::reveal_path(path).await {
                Ok(()) => SystemActionOutcome::RevealPath(Ok(())),
                Err(err) => SystemActionOutcome::RevealPath(Err(format!(
                    "Could not reveal item in Finder: {err}."
                ))),
            }
        }
        SystemActionRequest::OpenUrl(url) => match validate_open_url(&url) {
            Ok(safe) => {
                if nsh_core::prelude::open_url(safe.to_owned()).await {
                    SystemActionOutcome::OpenUrl(Ok(()))
                } else {
                    SystemActionOutcome::OpenUrl(Err(format!("Could not open URL `{safe}`.")))
                }
            }
            Err(err) => SystemActionOutcome::OpenUrl(Err(err)),
        },
        SystemActionRequest::SetLaunchAtLogin(enabled) => {
            match nsh_core::element::desktop::set_launch_at_login(enabled).await {
                Ok(()) => SystemActionOutcome::SetLaunchAtLogin(Ok(if enabled {
                    LaunchAtLoginOutcome::Enabled
                } else {
                    LaunchAtLoginOutcome::Disabled
                })),
                Err(LaunchAtLoginError::Unavailable) => {
                    SystemActionOutcome::SetLaunchAtLogin(Ok(LaunchAtLoginOutcome::Unavailable))
                }
                Err(err) => SystemActionOutcome::SetLaunchAtLogin(Err(format!(
                    "Could not update Open at login: {err}."
                ))),
            }
        }
        SystemActionRequest::Notify { id, title, body } => {
            let notif = Notification::new(id, title, body);
            let delivered = nsh_core::prelude::show_notification(notif).await;
            SystemActionOutcome::Notify(Ok(delivered))
        }
    }
}

/// Validates that `url` uses an explicit safe scheme before handing it to macOS.
pub fn validate_open_url(url: &str) -> Result<&str, String> {
    let trimmed = url.trim();
    if trimmed.starts_with("https://") || trimmed.starts_with("x-apple.systempreferences:") {
        Ok(trimmed)
    } else {
        Err(format!(
            "Refused to open unsupported URL scheme: `{trimmed}`."
        ))
    }
}

/// One recorded call captured by [`RecordingSystemActions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedSystemAction {
    RevealPath(PathBuf),
    OpenUrl(String),
    SetLaunchAtLogin(bool),
    Notify {
        id: String,
        title: String,
        body: String,
    },
}

/// Recording fake [`SystemActions`] used in all tests and `FOLDERSYNC_DEMO=1`.
#[derive(Debug, Default)]
pub struct RecordingSystemActions {
    calls: Mutex<Vec<RecordedSystemAction>>,
    login_outcome_on_enable: Mutex<LaunchAtLoginOutcome>,
    fail_reveal: Mutex<Option<String>>,
}

impl RecordingSystemActions {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            login_outcome_on_enable: Mutex::new(LaunchAtLoginOutcome::Enabled),
            fail_reveal: Mutex::new(None),
        }
    }

    pub fn set_login_outcome_on_enable(&self, outcome: LaunchAtLoginOutcome) {
        *self.login_outcome_on_enable.lock().unwrap() = outcome;
    }

    pub fn set_fail_reveal(&self, err: Option<String>) {
        *self.fail_reveal.lock().unwrap() = err;
    }

    pub fn recorded(&self) -> Vec<RecordedSystemAction> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }
}

impl SystemActions for RecordingSystemActions {
    fn reveal_path(&self, path: &Path) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedSystemAction::RevealPath(path.to_path_buf()));
        if let Some(err) = self.fail_reveal.lock().unwrap().clone() {
            return Err(err);
        }
        Ok(())
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        let safe = validate_open_url(url)?;
        self.calls
            .lock()
            .unwrap()
            .push(RecordedSystemAction::OpenUrl(safe.to_owned()));
        Ok(())
    }

    fn set_launch_at_login(&self, enabled: bool) -> Result<LaunchAtLoginOutcome, String> {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedSystemAction::SetLaunchAtLogin(enabled));
        if enabled {
            Ok(*self.login_outcome_on_enable.lock().unwrap())
        } else {
            Ok(LaunchAtLoginOutcome::Disabled)
        }
    }

    fn notify(&self, id: &str, title: &str, body: &str) -> Result<bool, String> {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedSystemAction::Notify {
                id: id.to_owned(),
                title: title.to_owned(),
                body: body.to_owned(),
            });
        Ok(true)
    }
}
