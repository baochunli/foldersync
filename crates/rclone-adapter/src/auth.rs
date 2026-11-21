//! Google Drive OAuth authorization wrapper around pinned rclone's documented
//! `rclone config reconnect` flow (§8).
//!
//! Credentials (`client_secret`, config passphrase, and refreshed tokens) are
//! stored only in the encrypted `accounts/<id>/rclone.conf` and the OS secret
//! store (`--password-command`); secrets never appear in argv or logs.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use thiserror::Error;

use crate::{
    config::{
        AccountConfigLock, ConfigError, enforce_private_config_permissions,
        is_encrypted_config_content, stderr_indicates_auth_failure,
    },
    log_parser::strip_ansi_codes,
    process::{
        CapturedOutput, ExitStatusInfo, OutputLimits, OutputLine, ProcessError, ProcessRunner,
        ProcessSpec, SpawnOptions, minimal_isolated_env,
    },
    qualification::{
        EngineIdentity, QualificationError, QualificationManifest,
        verify_binary_integrity_with_manifest,
    },
    request::{PasswordCommand, RequestError, validate_remote_name_str},
};

/// Validated request to run pinned rclone's browser OAuth flow against an
/// already-created encrypted Drive remote configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveAuthRequest {
    engine: EngineIdentity,
    config_path: PathBuf,
    lock_path: PathBuf,
    remote_name: String,
    password_command: PasswordCommand,
}

/// Completed OAuth authorization outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveAuthOutcome {
    pub auth_url: Option<String>,
    pub exit_status: ExitStatusInfo,
    pub output: CapturedOutput,
}

/// Errors returned while constructing or running a Drive OAuth flow.
#[derive(Debug, Error)]
pub enum DriveAuthError {
    #[error("invalid authorization request: {0}")]
    Request(#[from] RequestError),
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("engine integrity check failed: {0}")]
    Qualification(#[from] QualificationError),
    #[error("authentication required or credential store locked: {reason}")]
    AuthenticationRequired { reason: String },
    #[error("rclone authorization command failed (code={code:?}, signal={signal:?}): {detail}")]
    AuthorizationFailed {
        code: Option<i32>,
        signal: Option<i32>,
        detail: String,
    },
    #[error("process error during authorization: {0}")]
    Process(#[from] ProcessError),
}

impl DriveAuthRequest {
    /// Constructs a validated Drive authorization request.
    pub fn new(
        engine: EngineIdentity,
        config_path: PathBuf,
        lock_path: PathBuf,
        remote_name: impl Into<String>,
        password_command: PasswordCommand,
    ) -> Result<Self, DriveAuthError> {
        let remote_name = remote_name.into();
        validate_remote_name_str(&remote_name)?;
        if !config_path.is_absolute() {
            return Err(DriveAuthError::Request(RequestError::RelativePath {
                field: "config_path",
                path: config_path,
            }));
        }
        if !lock_path.is_absolute() {
            return Err(DriveAuthError::Request(RequestError::RelativePath {
                field: "lock_path",
                path: lock_path,
            }));
        }
        Ok(Self {
            engine,
            config_path,
            lock_path,
            remote_name,
            password_command,
        })
    }

    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    pub fn password_command(&self) -> &PasswordCommand {
        &self.password_command
    }

    /// Builds the pinned `rclone config reconnect` argument vector.
    ///
    /// Never carries OAuth `client_secret` or config passphrase in argv;
    /// always passes `--ask-password=false` and `--auto-confirm` with stdin closed.
    pub fn to_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("config"),
            OsString::from("reconnect"),
            OsString::from(format!("{}:", self.remote_name)),
            OsString::from("--config"),
            self.config_path.as_os_str().to_os_string(),
            OsString::from("--password-command"),
            OsString::from(self.password_command.to_rclone_space_sep_list()),
            OsString::from("--ask-password=false"),
            OsString::from("--auto-confirm"),
        ]
    }

    fn build_verified_spec(
        &self,
        manifest: &QualificationManifest,
    ) -> Result<ProcessSpec, DriveAuthError> {
        verify_binary_integrity_with_manifest(&self.engine, manifest)?;
        let account_dir = self.config_path.parent().ok_or(ConfigError::InvalidInput {
            field: "config_path",
            reason: "must have a parent directory",
        })?;
        Ok(ProcessSpec {
            program: self.engine.path().to_path_buf(),
            args: self.to_args(),
            current_dir: None,
            env: minimal_isolated_env(Some(account_dir)),
            expected_binary_sha256: Some(self.engine.sha256().to_string()),
        })
    }
}

/// Extracts the OAuth browser authorization URL from a single line of rclone
/// output (plain text, ANSI-colored, or `--use-json-log` JSON).
pub fn extract_auth_url(line: &str) -> Option<String> {
    let candidate_text = {
        let trimmed = line.trim();
        if trimmed.starts_with('{')
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
            && let Some(msg) = val.get("msg").and_then(|v| v.as_str())
        {
            strip_ansi_codes(msg)
        } else {
            strip_ansi_codes(line)
        }
    };

    const MARKERS: &[&str] = &[
        "go to the following link:",
        "visit the following url:",
        "open the following link:",
    ];

    let lower = candidate_text.to_ascii_lowercase();
    for marker in MARKERS {
        if let Some(pos) = lower.find(marker) {
            let after = candidate_text[pos + marker.len()..].trim();
            if let Some(url) = extract_first_http_url(after) {
                return Some(url);
            }
        }
    }

    // Fallback: explicit loopback rclone auth callback URL on its own line.
    if candidate_text.contains("http://127.0.0.1:") && candidate_text.contains("/auth") {
        return extract_first_http_url(&candidate_text);
    }

    None
}

fn extract_first_http_url(text: &str) -> Option<String> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let slice = &text[start..];
    let end = slice
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
        .unwrap_or(slice.len());
    let url = slice[..end].trim_end_matches(['.', ',', ';']);
    if url.len() > "http://a".len() && !url.chars().any(|c| c.is_control()) {
        Some(url.to_string())
    } else {
        None
    }
}

/// Callback invoked as soon as pinned rclone emits the OAuth browser URL.
pub type AuthUrlCallback = Box<dyn Fn(&str) + Send>;

/// Runs the pinned rclone OAuth browser authorization flow under the account's
/// config mutation lock with stdin closed, surfacing the browser URL as soon as
/// rclone emits it.
pub fn run_drive_auth(
    req: &DriveAuthRequest,
    runner: &dyn ProcessRunner,
    on_auth_url: Option<AuthUrlCallback>,
) -> Result<DriveAuthOutcome, DriveAuthError> {
    let manifest = QualificationManifest::embedded()?;
    run_drive_auth_with_manifest(req, &manifest, runner, on_auth_url)
}

pub(crate) fn run_drive_auth_with_manifest(
    req: &DriveAuthRequest,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    on_auth_url: Option<AuthUrlCallback>,
) -> Result<DriveAuthOutcome, DriveAuthError> {
    let _lock = AccountConfigLock::acquire(req.lock_path())?;
    enforce_private_config_permissions(req.config_path())?;
    let spec = req.build_verified_spec(manifest)?;

    let (tx, rx) = mpsc::sync_channel::<OutputLine>(128);
    let handle = runner.spawn(
        &spec,
        SpawnOptions {
            limits: OutputLimits::default(),
            line_observer: Some(tx),
        },
    )?;

    let observer_thread = thread::spawn(move || {
        let mut first_url: Option<String> = None;
        while let Ok(line) = rx.recv() {
            if first_url.is_none()
                && let Some(url) = extract_auth_url(&line.text)
            {
                if let Some(cb) = &on_auth_url {
                    cb(&url);
                }
                first_url = Some(url);
            }
        }
        first_url
    });

    let run_out = handle.wait()?;
    let observed_url = observer_thread.join().unwrap_or(None);

    // Always re-enforce 0600 permissions on config_path if it exists after mutation.
    if req.config_path().exists() {
        enforce_private_config_permissions(req.config_path())?;
    }

    if !run_out.exit_status.is_success() {
        let stderr = run_out.output.stderr_text();
        let stdout = run_out.output.stdout_text();
        let combined = if stderr.is_empty() { stdout } else { stderr };
        if stderr_indicates_auth_failure(&combined) {
            return Err(DriveAuthError::AuthenticationRequired {
                reason: combined.trim().to_string(),
            });
        }
        return Err(DriveAuthError::AuthorizationFailed {
            code: run_out.exit_status.code,
            signal: run_out.exit_status.signal,
            detail: combined.trim().to_string(),
        });
    }

    // Confirm config remained encrypted after rclone persisted the OAuth token.
    let on_disk = fs::read_to_string(req.config_path()).map_err(|source| ConfigError::Io {
        path: req.config_path().to_path_buf(),
        source,
    })?;
    if !is_encrypted_config_content(&on_disk) {
        return Err(DriveAuthError::Config(ConfigError::NotEncrypted {
            path: req.config_path().to_path_buf(),
        }));
    }

    let auth_url = observed_url.or_else(|| {
        run_out
            .output
            .lines
            .iter()
            .find_map(|l| extract_auth_url(&l.text))
    });

    Ok(DriveAuthOutcome {
        auth_url,
        exit_status: run_out.exit_status,
        output: run_out.output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        process::{FakeProcessRunner, FakeRunScript, StreamKind},
        qualification::{host_platform_key, qualify_binary_with_runner, sha256_file},
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    fn unique_tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "foldersync-auth-unit-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extracts_auth_url_from_recorded_rclone_plain_ansi_and_json_fixtures() {
        let plain = "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=Q4f8xK9mN2pL0vB7cR1tYw";
        assert_eq!(
            extract_auth_url(plain).as_deref(),
            Some("http://127.0.0.1:53682/auth?state=Q4f8xK9mN2pL0vB7cR1tYw")
        );

        let ansi = "\u{1b}[33mNOTICE: If your browser doesn't open automatically go to the following link: \u{1b}[36mhttp://127.0.0.1:53682/auth?state=ansiState99\u{1b}[0m";
        assert_eq!(
            extract_auth_url(ansi).as_deref(),
            Some("http://127.0.0.1:53682/auth?state=ansiState99")
        );

        let json = r#"{"level":"notice","msg":"If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=jsonState42","source":"oauthutil/oauthutil.go:577"}"#;
        assert_eq!(
            extract_auth_url(json).as_deref(),
            Some("http://127.0.0.1:53682/auth?state=jsonState42")
        );

        let remote_google = "Please go to the following link: https://accounts.google.com/o/oauth2/auth?client_id=123.apps.googleusercontent.com&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive";
        assert_eq!(
            extract_auth_url(remote_google).as_deref(),
            Some(
                "https://accounts.google.com/o/oauth2/auth?client_id=123.apps.googleusercontent.com&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive"
            )
        );

        assert_eq!(extract_auth_url("NOTICE: Waiting for code..."), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_drive_auth_builds_reconnect_argv_surfaces_url_and_verifies_encrypted_config() {
        let tmp = unique_tmp("reconnect-fixture");
        let fake_bin = tmp.join("rclone");
        fs::write(&fake_bin, b"fake rclone binary").unwrap();
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let digest = sha256_file(&fake_bin).unwrap();
        let platform = host_platform_key().unwrap();
        let manifest = QualificationManifest::from_toml_str(&format!(
            "[rclone]\nversion = \"1.76.0\"\n[rclone.{platform}]\nurl = \"https://example.invalid/rclone.zip\"\narchive_sha256 = \"{digest}\"\nbinary_sha256 = \"{digest}\"\n"
        ))
        .unwrap();

        let qual_runner = FakeProcessRunner::new();
        qual_runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: "rclone v1.76.0".to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        let engine = qualify_binary_with_runner(&fake_bin, &manifest, &qual_runner).unwrap();

        let acct_dir = tmp.join("accounts/drive1");
        fs::create_dir_all(&acct_dir).unwrap();
        let cfg = acct_dir.join("rclone.conf");
        let lock = acct_dir.join("config.lock");
        fs::write(
            &cfg,
            "# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nFakeCiphertext==\n",
        )
        .unwrap();
        fs::set_permissions(&cfg, fs::Permissions::from_mode(0o600)).unwrap();

        let pw = PasswordCommand::new(
            tmp.join("bin/sync-cli"),
            vec![
                "credential-helper".to_string(),
                "--account".to_string(),
                "drive1".to_string(),
            ],
        )
        .unwrap();
        let req = DriveAuthRequest::new(engine, cfg.clone(), lock, "household", pw).unwrap();

        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            lines: vec![
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=fixture123".to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: "NOTICE: Log in and authorize rclone for access".to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: "NOTICE: Got code".to_string(),
                    truncated: false,
                },
            ],
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            ..Default::default()
        });

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_cb = Arc::clone(&seen);
        let outcome = run_drive_auth_with_manifest(
            &req,
            &manifest,
            &runner,
            Some(Box::new(move |url| {
                seen_cb.lock().unwrap().push(url.to_string());
            })),
        )
        .unwrap();

        assert_eq!(
            outcome.auth_url.as_deref(),
            Some("http://127.0.0.1:53682/auth?state=fixture123")
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["http://127.0.0.1:53682/auth?state=fixture123"]
        );

        let specs = runner.spawned_specs();
        assert_eq!(specs.len(), 1);
        let argv: Vec<String> = specs[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv[0], "config");
        assert_eq!(argv[1], "reconnect");
        assert_eq!(argv[2], "household:");
        assert!(argv.contains(&"--ask-password=false".to_string()));
        assert!(argv.contains(&"--auto-confirm".to_string()));
        assert!(argv.contains(&"--password-command".to_string()));
        assert!(specs[0].env.contains_key(&OsString::from("HOME")));
        assert!(
            !specs[0]
                .env
                .keys()
                .any(|k| k.to_string_lossy().starts_with("RCLONE_"))
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
