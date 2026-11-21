//! Version and binary-integrity qualification against
//! `packaging/qualification-manifest.toml`.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::process::{
    ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, SpawnOptions, minimal_isolated_env,
};

const EMBEDDED_MANIFEST_TOML: &str = include_str!("../../../packaging/qualification-manifest.toml");
static PROBE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Verified identity of the pinned rclone executable, stored with each run.
///
/// Fields are private so callers cannot forge or mutate an identity after
/// qualification; launch boundaries also re-verify the binary's SHA-256 on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineIdentity {
    version: String,
    sha256: String,
    path: PathBuf,
    platform: String,
}

impl EngineIdentity {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    #[cfg(test)]
    pub(crate) fn for_unit_test(
        version: impl Into<String>,
        sha256: impl Into<String>,
        path: impl Into<PathBuf>,
        platform: impl Into<String>,
    ) -> Self {
        Self {
            version: version.into(),
            sha256: sha256.into(),
            path: path.into(),
            platform: platform.into(),
        }
    }
}

/// Platform-specific rclone release archive and binary digests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlatformPin {
    pub url: String,
    pub archive_sha256: String,
    pub binary_sha256: String,
}

/// Parsed rclone pin section from `packaging/qualification-manifest.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationManifest {
    rclone_version: String,
    platforms: BTreeMap<String, PlatformPin>,
}

/// Failures encountered while qualifying an rclone binary.
#[derive(Debug, Error)]
pub enum QualificationError {
    #[error("failed to read qualification manifest at {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid qualification manifest: {0}")]
    InvalidManifest(String),
    #[error("unsupported host platform for pinned rclone: {os}-{arch}")]
    UnsupportedPlatform { os: String, arch: String },
    #[error("missing platform pin `{platform}` in qualification manifest")]
    MissingPlatformPin { platform: String },
    #[error("engine identity platform `{actual}` does not match host platform `{expected}`")]
    PlatformMismatch { expected: String, actual: String },
    #[error("rclone binary not found or not a regular file: {0}")]
    BinaryNotFound(PathBuf),
    #[error("rclone binary is not executable: {0}")]
    NotExecutable(PathBuf),
    #[error("failed to create isolated throwaway config for version probe: {0}")]
    ProbeTempDir(#[source] std::io::Error),
    #[error("failed to read rclone binary {path} for SHA-256 verification: {source}")]
    HashIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "rclone binary SHA-256 mismatch for {path}: expected {expected}, got {actual} (possible replacement)"
    )]
    Sha256Mismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("failed to execute `{path} version`: {source}")]
    SpawnVersion {
        path: PathBuf,
        #[source]
        source: ProcessError,
    },
    #[error("`{path} version` exited with failure (code={code:?}, signal={signal:?})")]
    VersionCommandFailed {
        path: PathBuf,
        code: Option<i32>,
        signal: Option<i32>,
    },
    #[error("could not parse rclone version from output: {output:?}")]
    UnparseableVersion { output: String },
    #[error("rclone version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: String, actual: String },
}

#[derive(Deserialize)]
struct RawManifest {
    rclone: RawRcloneSection,
}

#[derive(Deserialize)]
struct RawRcloneSection {
    version: String,
    #[serde(flatten)]
    platforms: BTreeMap<String, toml::Value>,
}

fn is_valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl QualificationManifest {
    /// Parses the manifest checked into `packaging/qualification-manifest.toml`.
    pub fn embedded() -> Result<Self, QualificationError> {
        Self::from_toml_str_internal(EMBEDDED_MANIFEST_TOML)
    }

    /// Parses a qualification manifest from TOML text (test-only).
    #[cfg(test)]
    pub(crate) fn from_toml_str(toml_text: &str) -> Result<Self, QualificationError> {
        Self::from_toml_str_internal(toml_text)
    }

    fn from_toml_str_internal(toml_text: &str) -> Result<Self, QualificationError> {
        let raw: RawManifest = toml::from_str(toml_text)
            .map_err(|err| QualificationError::InvalidManifest(err.to_string()))?;

        let version = raw.rclone.version.trim().to_string();
        if version.is_empty() {
            return Err(QualificationError::InvalidManifest(
                "rclone.version must not be empty".to_string(),
            ));
        }

        let mut platforms = BTreeMap::new();
        for (platform, value) in raw.rclone.platforms {
            let pin: PlatformPin = value.try_into().map_err(|err: toml::de::Error| {
                QualificationError::InvalidManifest(format!("rclone.{platform}: {err}"))
            })?;
            if !is_valid_sha256_hex(&pin.binary_sha256) {
                return Err(QualificationError::InvalidManifest(format!(
                    "rclone.{platform}.binary_sha256 must be 64 lowercase hex characters"
                )));
            }
            if !is_valid_sha256_hex(&pin.archive_sha256) {
                return Err(QualificationError::InvalidManifest(format!(
                    "rclone.{platform}.archive_sha256 must be 64 lowercase hex characters"
                )));
            }
            platforms.insert(platform, pin);
        }

        if platforms.is_empty() {
            return Err(QualificationError::InvalidManifest(
                "manifest defines no platform pins under [rclone.*]".to_string(),
            ));
        }

        Ok(Self {
            rclone_version: version,
            platforms,
        })
    }

    /// Returns the pinned rclone version string.
    pub fn rclone_version(&self) -> &str {
        &self.rclone_version
    }

    /// Returns the pin for the given platform key (e.g. `osx-arm64`).
    pub fn platform_pin(&self, platform: &str) -> Result<&PlatformPin, QualificationError> {
        self.platforms
            .get(platform)
            .ok_or_else(|| QualificationError::MissingPlatformPin {
                platform: platform.to_string(),
            })
    }

    /// Returns the pin for the current host platform.
    pub fn host_platform_pin(&self) -> Result<(&'static str, &PlatformPin), QualificationError> {
        let platform = host_platform_key()?;
        let pin = self.platform_pin(platform)?;
        Ok((platform, pin))
    }
}

/// Returns the qualification manifest platform key for the current build target.
pub fn host_platform_key() -> Result<&'static str, QualificationError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("osx-arm64"),
        ("macos", "x86_64") => Ok("osx-amd64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        (os, arch) => Err(QualificationError::UnsupportedPlatform {
            os: os.to_string(),
            arch: arch.to_string(),
        }),
    }
}

/// Computes the lowercase hex SHA-256 digest of a file using streaming reads.
pub fn sha256_file(path: &Path) -> Result<String, QualificationError> {
    let mut file = File::open(path).map_err(|source| QualificationError::HashIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|source| QualificationError::HashIo {
                path: path.to_path_buf(),
                source,
            })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Extracts the rclone version string from `<binary> version` stdout.
pub fn parse_rclone_version(stdout: &str) -> Result<String, QualificationError> {
    let first_line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let Some(rest) = first_line.strip_prefix("rclone v") else {
        return Err(QualificationError::UnparseableVersion {
            output: stdout.to_string(),
        });
    };
    let version = rest.trim();
    if version.is_empty() || version.chars().any(char::is_whitespace) {
        return Err(QualificationError::UnparseableVersion {
            output: stdout.to_string(),
        });
    }
    Ok(version.to_string())
}

struct ProbeTempDir {
    dir: PathBuf,
    config_path: PathBuf,
}

impl ProbeTempDir {
    fn create() -> Result<Self, QualificationError> {
        let base = std::env::temp_dir();
        for _ in 0..64 {
            let seq = PROBE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!(
                "foldersync-rclone-probe-{}-{seq}",
                std::process::id()
            ));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    let config_path = dir.join("rclone.conf");
                    if let Err(err) = File::create(&config_path) {
                        let _ = fs::remove_dir_all(&dir);
                        return Err(QualificationError::ProbeTempDir(err));
                    }
                    return Ok(Self { dir, config_path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(QualificationError::ProbeTempDir(err)),
            }
        }
        Err(QualificationError::ProbeTempDir(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temporary directory attempts",
        )))
    }
}

impl Drop for ProbeTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Re-hashes the binary referenced by `identity` on disk and confirms its
/// version, platform, executable metadata, and SHA-256 still match the host
/// platform's pin in the checked-in qualification manifest (replacement
/// detection at the launch boundary, §19).
pub fn verify_binary_integrity(identity: &EngineIdentity) -> Result<(), QualificationError> {
    let manifest = QualificationManifest::embedded()?;
    verify_binary_integrity_with_manifest(identity, &manifest)
}

pub(crate) fn verify_binary_integrity_with_manifest(
    identity: &EngineIdentity,
    manifest: &QualificationManifest,
) -> Result<(), QualificationError> {
    let (host_platform, pin) = manifest.host_platform_pin()?;
    if identity.platform != host_platform {
        return Err(QualificationError::PlatformMismatch {
            expected: host_platform.to_string(),
            actual: identity.platform.clone(),
        });
    }
    if identity.version != manifest.rclone_version {
        return Err(QualificationError::VersionMismatch {
            expected: manifest.rclone_version.clone(),
            actual: identity.version.clone(),
        });
    }
    if identity.sha256 != pin.binary_sha256 {
        return Err(QualificationError::Sha256Mismatch {
            path: identity.path.clone(),
            expected: pin.binary_sha256.clone(),
            actual: identity.sha256.clone(),
        });
    }

    let meta = fs::metadata(&identity.path)
        .map_err(|_| QualificationError::BinaryNotFound(identity.path.clone()))?;
    if !meta.is_file() {
        return Err(QualificationError::BinaryNotFound(identity.path.clone()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(QualificationError::NotExecutable(identity.path.clone()));
        }
    }

    let actual_sha256 = sha256_file(&identity.path)?;
    if actual_sha256 != pin.binary_sha256 {
        return Err(QualificationError::Sha256Mismatch {
            path: identity.path.clone(),
            expected: pin.binary_sha256.clone(),
            actual: actual_sha256,
        });
    }
    Ok(())
}

/// Verifies binary SHA-256 and `<binary> version` against the checked-in
/// qualification manifest using the real process runner.
pub fn qualify_binary(binary_path: &Path) -> Result<EngineIdentity, QualificationError> {
    let manifest = QualificationManifest::embedded()?;
    qualify_binary_with_runner(binary_path, &manifest, &RealProcessRunner)
}

/// Verifies binary SHA-256 (before execution) and `<binary> version` using a
/// supplied [`ProcessRunner`] with an explicit throwaway `--config` and cleared
/// isolated environment.
pub(crate) fn qualify_binary_with_runner(
    binary_path: &Path,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
) -> Result<EngineIdentity, QualificationError> {
    let canonical = binary_path
        .canonicalize()
        .map_err(|_| QualificationError::BinaryNotFound(binary_path.to_path_buf()))?;
    let meta = fs::metadata(&canonical)
        .map_err(|_| QualificationError::BinaryNotFound(canonical.clone()))?;
    if !meta.is_file() {
        return Err(QualificationError::BinaryNotFound(canonical));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(QualificationError::NotExecutable(canonical));
        }
    }

    let (platform, pin) = manifest.host_platform_pin()?;

    // Compute and verify SHA-256 BEFORE executing the binary so a replaced binary is never run.
    let actual_sha256 = sha256_file(&canonical)?;
    if actual_sha256 != pin.binary_sha256 {
        return Err(QualificationError::Sha256Mismatch {
            path: canonical,
            expected: pin.binary_sha256.clone(),
            actual: actual_sha256,
        });
    }

    // Always isolate startup and pass an explicit throwaway --config so rclone never probes
    // or creates ~/.config/rclone during the version probe.
    let probe_tmp = ProbeTempDir::create()?;
    let spec = ProcessSpec {
        program: canonical.clone(),
        args: vec![
            OsString::from("version"),
            OsString::from("--config"),
            probe_tmp.config_path.as_os_str().to_os_string(),
            OsString::from("--ask-password=false"),
        ],
        current_dir: None,
        env: minimal_isolated_env(Some(&probe_tmp.dir)),
        expected_binary_sha256: Some(pin.binary_sha256.clone()),
    };
    let handle = runner
        .spawn(&spec, SpawnOptions::default())
        .map_err(|source| QualificationError::SpawnVersion {
            path: canonical.clone(),
            source,
        })?;
    let run_out = handle
        .wait()
        .map_err(|source| QualificationError::SpawnVersion {
            path: canonical.clone(),
            source,
        })?;
    if !run_out.exit_status.is_success() {
        return Err(QualificationError::VersionCommandFailed {
            path: canonical,
            code: run_out.exit_status.code,
            signal: run_out.exit_status.signal,
        });
    }

    let actual_version = parse_rclone_version(&run_out.output.stdout_text())?;
    if actual_version != manifest.rclone_version {
        return Err(QualificationError::VersionMismatch {
            expected: manifest.rclone_version.clone(),
            actual: actual_version,
        });
    }

    Ok(EngineIdentity {
        version: actual_version,
        sha256: actual_sha256,
        path: canonical,
        platform: platform.to_string(),
    })
}

/// Resolves and qualifies the workspace/overridden pinned rclone binary against
/// the embedded qualification manifest.
pub fn qualify_pinned_binary() -> Result<Option<EngineIdentity>, QualificationError> {
    let Some(path) = crate::pinned_binary_path() else {
        return Ok(None);
    };
    qualify_binary(&path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{
        ChildIdentity, ExitStatusInfo, FakeProcessRunner, FakeRunScript, OutputLine, StreamKind,
    };
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "foldersync-qual-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn embedded_manifest_contains_pinned_version_and_all_platforms() {
        let manifest = QualificationManifest::embedded().unwrap();
        assert_eq!(manifest.rclone_version, crate::PINNED_RCLONE_VERSION);
        assert!(manifest.platform_pin("osx-arm64").is_ok());
        assert!(manifest.platform_pin("osx-amd64").is_ok());
        assert!(manifest.platform_pin("linux-amd64").is_ok());
    }

    #[test]
    fn parses_standard_rclone_version_output() {
        let stdout =
            "rclone v1.76.0\n- os/version: darwin 27.2.1 (64 bit)\n- go/version: go1.27.3\n";
        assert_eq!(parse_rclone_version(stdout).unwrap(), "1.76.0");
    }

    #[test]
    fn rejects_unparseable_version_output() {
        assert!(matches!(
            parse_rclone_version("not rclone at all\n"),
            Err(QualificationError::UnparseableVersion { .. })
        ));
        assert!(matches!(
            parse_rclone_version("rclone v\n"),
            Err(QualificationError::UnparseableVersion { .. })
        ));
    }

    #[test]
    fn detects_replaced_binary_before_executing_it() {
        let dir = unique_temp_dir("replaced");
        let fake_bin = dir.join("rclone");
        fs::write(&fake_bin, b"tampered binary contents").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let manifest = QualificationManifest::embedded().unwrap();
        let runner = FakeProcessRunner::new();

        let err = qualify_binary_with_runner(&fake_bin, &manifest, &runner).unwrap_err();
        assert!(matches!(err, QualificationError::Sha256Mismatch { .. }));
        // Crucial: tampered binary must NEVER be spawned.
        assert!(runner.spawned_specs().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_mismatched_version_and_isolates_version_probe_with_throwaway_config() {
        let dir = unique_temp_dir("ver-mismatch");
        let fake_bin = dir.join("rclone");
        fs::write(&fake_bin, b"custom test binary").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let digest = sha256_file(&fake_bin).unwrap();
        let platform = host_platform_key().unwrap();
        let toml_text = format!(
            "[rclone]\nversion = \"1.76.0\"\n[rclone.{platform}]\nurl = \"https://example.invalid/rclone.zip\"\narchive_sha256 = \"{digest}\"\nbinary_sha256 = \"{digest}\"\n"
        );
        let manifest = QualificationManifest::from_toml_str(&toml_text).unwrap();

        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            identity: ChildIdentity {
                pid: 42,
                start_time: None,
                executable: fake_bin.clone(),
            },
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: "rclone v1.75.0".to_string(),
                truncated: false,
            }],
            exit_status: ExitStatusInfo {
                code: Some(0),
                signal: None,
            },
            ..Default::default()
        });

        let err = qualify_binary_with_runner(&fake_bin, &manifest, &runner).unwrap_err();
        assert!(matches!(
            err,
            QualificationError::VersionMismatch {
                expected,
                actual
            } if expected == "1.76.0" && actual == "1.75.0"
        ));

        let specs = runner.spawned_specs();
        assert_eq!(specs.len(), 1);
        let args: Vec<String> = specs[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "version");
        assert!(
            args.contains(&"--config".to_string()),
            "version probe must pass explicit throwaway --config, got {args:?}"
        );
        assert!(args.contains(&"--ask-password=false".to_string()));
        assert!(
            specs[0].env.contains_key(&OsString::from("HOME")),
            "version probe must isolate HOME to throwaway temp dir"
        );
        assert!(
            !specs[0]
                .env
                .keys()
                .any(|k| k.to_string_lossy().starts_with("RCLONE_")),
            "version probe must not pass ambient RCLONE_* variables"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_binary_integrity_detects_replacement_after_qualification() {
        let dir = unique_temp_dir("post-qual-replace");
        let fake_bin = dir.join("rclone");
        fs::write(&fake_bin, b"original qualified bytes").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let digest = sha256_file(&fake_bin).unwrap();
        let platform = host_platform_key().unwrap();
        let toml_text = format!(
            "[rclone]\nversion = \"1.76.0\"\n[rclone.{platform}]\nurl = \"https://example.invalid/rclone.zip\"\narchive_sha256 = \"{digest}\"\nbinary_sha256 = \"{digest}\"\n"
        );
        let manifest = QualificationManifest::from_toml_str(&toml_text).unwrap();

        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: "rclone v1.76.0".to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        let identity = qualify_binary_with_runner(&fake_bin, &manifest, &runner).unwrap();
        assert!(verify_binary_integrity_with_manifest(&identity, &manifest).is_ok());

        // Replace binary on disk AFTER qualification.
        fs::write(&fake_bin, b"replaced malicious bytes").unwrap();
        let err = verify_binary_integrity_with_manifest(&identity, &manifest).unwrap_err();
        assert!(matches!(err, QualificationError::Sha256Mismatch { .. }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn qualifies_real_pinned_binary_when_present() {
        let Some(path) = crate::pinned_binary_path() else {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        };
        let manifest = QualificationManifest::embedded().unwrap();
        let identity = qualify_binary(&path).unwrap();
        assert_eq!(identity.version(), crate::PINNED_RCLONE_VERSION);
        assert_eq!(identity.path(), path);
        assert_eq!(identity.platform(), host_platform_key().unwrap());
        assert_eq!(
            identity.sha256(),
            manifest
                .platform_pin(host_platform_key().unwrap())
                .unwrap()
                .binary_sha256
        );
    }
}
