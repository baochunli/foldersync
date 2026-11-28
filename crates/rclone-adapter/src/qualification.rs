//! Version and binary-integrity qualification for workspace and sealed bundled engines.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::process::{
    ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, SpawnOptions, minimal_isolated_env,
};

const EMBEDDED_MANIFEST_TOML: &str = include_str!("../../../packaging/qualification-manifest.toml");
const BUNDLE_HELPER: &str = "Contents/Helpers/rclone";
const BUNDLE_MANIFEST: &str = "Contents/Resources/rclone-manifest.json";
static PROBE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static EXECUTABLE_DIGESTS: Mutex<BTreeMap<PathBuf, ExecutableDigest>> = Mutex::new(BTreeMap::new());

/// Verified identity of the pinned rclone executable, stored with each run.
///
/// Fields are private so callers cannot forge or mutate an identity after
/// qualification; launch boundaries also check the SHA-256 of the current binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineIdentity {
    version: String,
    sha256: String,
    path: PathBuf,
    platform: String,
    bundled: bool,
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

    pub(crate) fn has_qualified_digest(&self, pin: &PlatformPin) -> bool {
        // Only bundle qualification can establish an identity from a sealed digest.
        self.bundled || self.sha256 == pin.binary_sha256
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
            bundled: false,
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
    #[error("failed to locate the current executable: {0}")]
    CurrentExecutable(#[source] std::io::Error),
    #[error("missing, invalid, or symlinked app bundle path: {0}")]
    InvalidBundlePath(PathBuf),
    #[error("code signature verification failed for app bundle {path}: {reason}")]
    BundleSignature { path: PathBuf, reason: String },
    #[error("failed to read qualification manifest at {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid qualification manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid bundled rclone manifest at {path}: {reason}")]
    InvalidBundleManifest { path: PathBuf, reason: String },
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

#[derive(Deserialize)]
struct BundledManifest {
    version: String,
    platform: String,
    bundle_relative_path: String,
    upstream_manifest_sha256: String,
    upstream_archive_sha256: String,
    bundled_sha256: String,
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
    let file = File::open(path).map_err(|source| hash_io(path, source))?;
    sha256_open_file(path, &file)
}

fn hash_io(path: &Path, source: io::Error) -> QualificationError {
    QualificationError::HashIo {
        path: path.to_path_buf(),
        source,
    }
}

fn sha256_open_file(path: &Path, mut file: &File) -> Result<String, QualificationError> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|source| hash_io(path, source))?;
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

#[derive(PartialEq, Eq)]
struct ExecutableStamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64),
    #[cfg(unix)]
    changed: (i64, i64),
}

impl ExecutableStamp {
    fn from_metadata(meta: Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a regular file",
            ));
        }
        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino()),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

struct ExecutableDigest {
    stamp: ExecutableStamp,
    sha256: Arc<str>,
}

pub(crate) fn executable_sha256(path: &Path) -> Result<Arc<str>, QualificationError> {
    let io_error = |source| hash_io(path, source);
    // Keep metadata observation and cache fill under the same lock. Concurrent
    // launches of one executable share a single full read for unchanged bytes.
    let mut cache = EXECUTABLE_DIGESTS.lock().unwrap();
    let canonical = path.canonicalize().map_err(io_error)?;
    let path_stamp = || fs::metadata(path).and_then(ExecutableStamp::from_metadata);
    let stamp = path_stamp().map_err(io_error)?;
    if let Some(entry) = cache.get(&canonical).filter(|entry| entry.stamp == stamp) {
        return Ok(Arc::clone(&entry.sha256));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    let file_stamp = || file.metadata().and_then(ExecutableStamp::from_metadata);
    let changed = || {
        io_error(io::Error::other(
            "binary changed during SHA-256 verification",
        ))
    };
    if file_stamp().map_err(io_error)? != stamp {
        return Err(changed());
    }
    let sha256 = Arc::<str>::from(sha256_open_file(path, &file)?);
    if file_stamp().map_err(io_error)? != stamp || path_stamp().map_err(io_error)? != stamp {
        return Err(changed());
    }
    cache.insert(
        canonical,
        ExecutableDigest {
            stamp,
            sha256: Arc::clone(&sha256),
        },
    );
    Ok(sha256)
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
                    if let Err(err) = File::create_new(&config_path) {
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

/// Confirms that the binary referenced by `identity` is still executable and
/// matches its qualified SHA-256. Hashes are reused for an unchanged file.
/// Also checks the identity against the host version and platform pins.
pub fn verify_binary_integrity(identity: &EngineIdentity) -> Result<(), QualificationError> {
    let manifest = QualificationManifest::embedded()?;
    verify_binary_integrity_with_manifest(identity, &manifest)
}

pub(crate) fn verify_binary_integrity_with_manifest(
    identity: &EngineIdentity,
    manifest: &QualificationManifest,
) -> Result<(), QualificationError> {
    verify_binary_identity_with_manifest(identity, manifest)?;
    verify_executable_digest(identity)
}

pub(crate) fn verify_binary_identity_with_manifest(
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
    if !identity.has_qualified_digest(pin) {
        return Err(QualificationError::Sha256Mismatch {
            path: identity.path.clone(),
            expected: pin.binary_sha256.clone(),
            actual: identity.sha256.clone(),
        });
    }
    Ok(())
}

pub(crate) fn verify_executable_metadata(
    identity: &EngineIdentity,
) -> Result<(), QualificationError> {
    let meta = if identity.bundled {
        bundle_file_metadata(&identity.path)?
    } else {
        fs::metadata(&identity.path)
            .map_err(|_| QualificationError::BinaryNotFound(identity.path.clone()))?
    };
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
    Ok(())
}

fn verify_executable_digest(identity: &EngineIdentity) -> Result<(), QualificationError> {
    verify_executable_metadata(identity)?;
    let actual_sha256 = executable_sha256(&identity.path)?;
    if actual_sha256.as_ref() != identity.sha256 {
        return Err(QualificationError::Sha256Mismatch {
            path: identity.path.clone(),
            expected: identity.sha256.clone(),
            actual: actual_sha256.to_string(),
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
    let (platform, pin) = manifest.host_platform_pin()?;
    let identity = EngineIdentity {
        version: manifest.rclone_version.clone(),
        sha256: pin.binary_sha256.clone(),
        path: canonical,
        platform: platform.to_owned(),
        bundled: false,
    };
    probe_version(identity, runner)
}

fn probe_version(
    identity: EngineIdentity,
    runner: &dyn ProcessRunner,
) -> Result<EngineIdentity, QualificationError> {
    // Compute and verify SHA-256 BEFORE executing the binary so a replaced binary is never run.
    verify_executable_digest(&identity)?;

    // Always isolate startup and pass an explicit throwaway --config so rclone never probes
    // or creates ~/.config/rclone during the version probe.
    let probe_tmp = ProbeTempDir::create()?;
    let spec = ProcessSpec {
        program: identity.path.clone(),
        args: vec![
            OsString::from("version"),
            OsString::from("--config"),
            probe_tmp.config_path.as_os_str().to_os_string(),
            OsString::from("--ask-password=false"),
        ],
        current_dir: None,
        env: minimal_isolated_env(Some(&probe_tmp.dir)),
        expected_binary_sha256: Some(identity.sha256.clone()),
    };
    let handle = runner
        .spawn(&spec, SpawnOptions::default())
        .map_err(|source| QualificationError::SpawnVersion {
            path: identity.path.clone(),
            source,
        })?;
    let run_out = handle
        .wait()
        .map_err(|source| QualificationError::SpawnVersion {
            path: identity.path.clone(),
            source,
        })?;
    if !run_out.exit_status.is_success() {
        return Err(QualificationError::VersionCommandFailed {
            path: identity.path,
            code: run_out.exit_status.code,
            signal: run_out.exit_status.signal,
        });
    }

    let actual_version = parse_rclone_version(&run_out.output.stdout_text())?;
    if actual_version != identity.version {
        return Err(QualificationError::VersionMismatch {
            expected: identity.version,
            actual: actual_version,
        });
    }

    Ok(identity)
}

/// Qualifies the running macOS app bundle's helper, or resolves and qualifies
/// the workspace's pinned rclone when running outside a bundle.
///
/// Call at startup and retain the returned identity for subsequent launches.
/// Each launch checks the helper against the digest established here. A
/// bundle error is final. `Ok(None)` means the workspace binary is absent.
pub fn qualify_pinned_binary() -> Result<Option<EngineIdentity>, QualificationError> {
    #[cfg(target_os = "macos")]
    let executable = Some(std::env::current_exe().map_err(QualificationError::CurrentExecutable)?);
    #[cfg(not(target_os = "macos"))]
    let executable = None::<PathBuf>;

    qualify_engine_at(
        executable.as_deref(),
        crate::pinned_binary_path,
        verify_bundle_signature,
        &QualificationManifest::embedded()?,
        &RealProcessRunner,
    )
}

fn qualify_engine_at(
    executable: Option<&Path>,
    development_binary: impl FnOnce() -> Option<PathBuf>,
    verify_bundle: impl FnOnce(&Path) -> Result<(), QualificationError>,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
) -> Result<Option<EngineIdentity>, QualificationError> {
    if let Some(executable) = executable
        && let Some(bundle) = app_bundle(executable)?
    {
        let helper = bundle.join(BUNDLE_HELPER);
        let manifest_path = bundle.join(BUNDLE_MANIFEST);
        bundle_file_metadata(&helper)?;
        bundle_file_metadata(&manifest_path)?;
        verify_bundle(bundle)?;
        let identity = bundled_identity(helper, &manifest_path, manifest)?;
        return probe_version(identity, runner).map(Some);
    }
    development_binary()
        .map(|path| qualify_binary_with_runner(&path, manifest, runner))
        .transpose()
}

fn app_bundle(executable: &Path) -> Result<Option<&Path>, QualificationError> {
    fn bundle_root(executable: &Path) -> Option<&Path> {
        let macos = executable.parent()?;
        let contents = macos.parent()?;
        let bundle = contents.parent()?;
        (macos.file_name()?.eq_ignore_ascii_case("MacOS")
            && contents.file_name()?.eq_ignore_ascii_case("Contents")
            && bundle.extension()?.eq_ignore_ascii_case("app"))
        .then_some(bundle)
    }

    if let Some(bundle) = bundle_root(executable) {
        bundle_file_metadata(executable)?;
        return Ok(Some(bundle));
    }
    // macOS can return a launch path containing symlinks. An alias into an
    // app must fail closed rather than appear to be a workspace executable.
    let physical_path = executable
        .canonicalize()
        .map_err(QualificationError::CurrentExecutable)?;
    if bundle_root(&physical_path).is_some() {
        return Err(QualificationError::InvalidBundlePath(
            executable.to_path_buf(),
        ));
    }
    Ok(None)
}

fn bundle_file_metadata(path: &Path) -> Result<fs::Metadata, QualificationError> {
    let invalid = |path: &Path| QualificationError::InvalidBundlePath(path.to_path_buf());
    if !path.is_absolute() {
        return Err(invalid(path));
    }
    let ancestors: Vec<_> = path.ancestors().skip(1).collect();
    for parent in ancestors.into_iter().rev() {
        // symlink_metadata does not follow the component being inspected.
        if !fs::symlink_metadata(parent).is_ok_and(|meta| meta.is_dir()) {
            return Err(invalid(parent));
        }
    }
    fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.is_file())
        .ok_or_else(|| invalid(path))
}

fn verify_bundle_signature(bundle: &Path) -> Result<(), QualificationError> {
    let error = |reason: String| QualificationError::BundleSignature {
        path: bundle.to_path_buf(),
        reason,
    };
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(bundle)
        .env_clear()
        .output()
        .map_err(|err| error(err.to_string()))?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        return Err(error(format!("{}: {}", output.status, diagnostic.trim())));
    }
    Ok(())
}

fn bundled_identity(
    helper: PathBuf,
    manifest_path: &Path,
    manifest: &QualificationManifest,
) -> Result<EngineIdentity, QualificationError> {
    let (platform, pin) = manifest.host_platform_pin()?;
    let text =
        fs::read_to_string(manifest_path).map_err(|source| QualificationError::ReadManifest {
            path: manifest_path.to_path_buf(),
            source,
        })?;
    let invalid = |reason: String| QualificationError::InvalidBundleManifest {
        path: manifest_path.to_path_buf(),
        reason,
    };
    let bundled: BundledManifest =
        serde_json::from_str(&text).map_err(|err| invalid(err.to_string()))?;
    for (field, actual, expected) in [
        (
            "version",
            bundled.version.as_str(),
            manifest.rclone_version(),
        ),
        ("platform", bundled.platform.as_str(), platform),
        (
            "bundle_relative_path",
            bundled.bundle_relative_path.as_str(),
            BUNDLE_HELPER,
        ),
        (
            "upstream_manifest_sha256",
            bundled.upstream_manifest_sha256.as_str(),
            pin.binary_sha256.as_str(),
        ),
        (
            "upstream_archive_sha256",
            bundled.upstream_archive_sha256.as_str(),
            pin.archive_sha256.as_str(),
        ),
    ] {
        if actual != expected {
            return Err(invalid(format!("{field} must match {expected}")));
        }
    }
    if !is_valid_sha256_hex(&bundled.bundled_sha256) {
        return Err(invalid(
            "bundled_sha256 must be 64 lowercase hex characters".into(),
        ));
    }

    Ok(EngineIdentity {
        version: bundled.version,
        sha256: bundled.bundled_sha256,
        path: helper,
        platform: bundled.platform,
        bundled: true,
    })
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

    #[cfg(unix)]
    struct BundleFixture {
        dir: PathBuf,
        app: PathBuf,
        executable: PathBuf,
        helper: PathBuf,
        manifest_path: PathBuf,
        manifest: QualificationManifest,
    }

    #[cfg(unix)]
    impl BundleFixture {
        fn new(label: &str) -> Self {
            let dir = unique_temp_dir(label).canonicalize().unwrap();
            let app = dir.join("FolderSync.app");
            let executable = app.join("Contents/MacOS/FolderSync");
            let helper = app.join("Contents/Helpers/rclone");
            let manifest_path = app.join("Contents/Resources/rclone-manifest.json");
            for file in [&executable, &helper, &manifest_path] {
                fs::create_dir_all(file.parent().unwrap()).unwrap();
            }
            fs::write(&executable, b"app executable").unwrap();
            fs::write(&helper, b"signed helper bytes").unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
            let fixture = Self {
                dir,
                app,
                executable,
                helper,
                manifest_path,
                manifest: QualificationManifest::embedded().unwrap(),
            };
            fixture.write_manifest(&fixture.bundle_manifest());
            fixture
        }

        fn bundle_manifest(&self) -> serde_json::Value {
            let (platform, pin) = self.manifest.host_platform_pin().unwrap();
            serde_json::json!({
                "version": self.manifest.rclone_version(),
                "platform": platform,
                "bundle_relative_path": "Contents/Helpers/rclone",
                "upstream_manifest_sha256": pin.binary_sha256,
                "upstream_archive_sha256": pin.archive_sha256,
                "bundled_sha256": sha256_file(&self.helper).unwrap(),
            })
        }

        fn write_manifest(&self, manifest: &serde_json::Value) {
            fs::write(&self.manifest_path, manifest.to_string()).unwrap();
        }

        fn qualify(
            &self,
            runner: &dyn ProcessRunner,
        ) -> Result<Option<EngineIdentity>, QualificationError> {
            qualify_engine_at(
                Some(&self.executable),
                || None,
                |bundle| {
                    assert_eq!(bundle, self.app);
                    Ok(())
                },
                &self.manifest,
                runner,
            )
        }
    }

    #[cfg(unix)]
    impl Drop for BundleFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn version_runner() -> FakeProcessRunner {
        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: format!("rclone v{}", crate::PINNED_RCLONE_VERSION),
                truncated: false,
            }],
            ..Default::default()
        });
        runner
    }

    #[cfg(unix)]
    #[test]
    fn bundle_wins_over_development_and_uses_its_signed_digest() {
        let fixture = BundleFixture::new("bundle-priority");
        let runner = version_runner();
        let verified = std::cell::Cell::new(false);
        let identity = qualify_engine_at(
            Some(&fixture.executable),
            || panic!("a bundle must not consult development resolution"),
            |bundle| {
                assert_eq!(bundle, fixture.app);
                assert!(!verified.replace(true));
                assert!(runner.spawned_specs().is_empty());
                Ok(())
            },
            &fixture.manifest,
            &runner,
        )
        .unwrap()
        .unwrap();
        assert!(verified.get());
        assert_eq!(identity.path(), fixture.helper);
        assert_eq!(identity.sha256(), sha256_file(&fixture.helper).unwrap());
        let specs = runner.spawned_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, fixture.helper);
        assert_eq!(
            specs[0].expected_binary_sha256.as_deref(),
            Some(identity.sha256())
        );
        verify_binary_integrity(&identity).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_failed_bundle_signature_before_probing_helper() {
        let fixture = BundleFixture::new("bundle-signature");
        let runner = FakeProcessRunner::new();
        let result = qualify_engine_at(
            Some(&fixture.executable),
            || None,
            |path| {
                Err(QualificationError::BundleSignature {
                    path: path.to_path_buf(),
                    reason: "signature failed".into(),
                })
            },
            &fixture.manifest,
            &runner,
        );
        assert!(matches!(
            result,
            Err(QualificationError::BundleSignature { path, .. }) if path == fixture.app
        ));
        assert!(runner.spawned_specs().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bundle_manifest_mismatches_before_probing_helper() {
        let fixture = BundleFixture::new("bundle-manifest");
        let runner = FakeProcessRunner::new();
        for (field, wrong) in [
            ("version", "0.0.1"),
            ("platform", "unknown"),
            ("bundle_relative_path", "Contents/MacOS/FolderSync"),
            ("upstream_manifest_sha256", &"0".repeat(64)),
            ("upstream_archive_sha256", &"0".repeat(64)),
            ("bundled_sha256", "malformed"),
        ] {
            let mut manifest = fixture.bundle_manifest();
            manifest[field] = wrong.into();
            fixture.write_manifest(&manifest);
            assert!(fixture.qualify(&runner).is_err(), "accepted {field}");
        }
        assert!(runner.spawned_specs().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bundle_helper_replacement_at_startup_and_launch() {
        let fixture = BundleFixture::new("bundle-replacement");
        let identity = fixture.qualify(&version_runner()).unwrap().unwrap();
        let at = |name: &str| fixture.dir.join(name);
        let request = crate::BisyncRequest::builder()
            .engine(identity)
            .mode(crate::RunMode::Normal)
            .path1(at("path1"))
            .path2(crate::Path2::LocalDir(at("path2")))
            .config_path(at("rclone.conf"))
            .workdir(at("workdir"))
            .filters_file(at("filters.txt"))
            .backup_dir1(at("backup1"))
            .backup_dir2(crate::BackupDir2::LocalDir(at("backup2")))
            .build()
            .unwrap();

        fs::write(&fixture.helper, b"replacement helper bytes").unwrap();
        let runner = FakeProcessRunner::new();
        assert!(matches!(
            fixture.qualify(&runner),
            Err(QualificationError::Sha256Mismatch { .. })
        ));
        assert!(runner.spawned_specs().is_empty());

        fixture.write_manifest(&fixture.bundle_manifest());
        assert!(matches!(
            request.spawn(&RealProcessRunner, SpawnOptions::default()),
            Err(crate::LaunchError::Process(
                ProcessError::BinaryIntegrityMismatch { .. }
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_missing_bundle_helper_without_development_fallback() {
        let fixture = BundleFixture::new("bundle-missing");
        fs::remove_file(&fixture.helper).unwrap();
        let runner = FakeProcessRunner::new();
        assert!(fixture.qualify(&runner).is_err());
        assert!(runner.spawned_specs().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_to_the_bundle_or_its_engine_files() {
        use std::os::unix::fs::symlink;

        let mut fixture = BundleFixture::new("bundle-symlink");
        let runner = FakeProcessRunner::new();
        for file in [&fixture.executable, &fixture.helper, &fixture.manifest_path] {
            let moved = file.with_extension("moved");
            fs::rename(file, &moved).unwrap();
            symlink(&moved, file).unwrap();
            assert!(fixture.qualify(&runner).is_err(), "followed {file:?}");
            fs::remove_file(file).unwrap();
            fs::rename(moved, file).unwrap();
        }
        let alias = fixture.dir.join("alias");
        symlink(&fixture.app, &alias).unwrap();
        fixture.executable = alias.join("Contents/MacOS/FolderSync");
        assert!(fixture.qualify(&runner).is_err());
        assert!(runner.spawned_specs().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a local ad hoc package built by packaging/build-app.sh"]
    fn qualifies_ad_hoc_packaged_helper() {
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/package/FolderSync.app")
            .canonicalize()
            .expect("build the ad hoc package as described in docs/release.md");
        let manifest = QualificationManifest::embedded().unwrap();
        let identity = qualify_engine_at(
            Some(&bundle.join("Contents/MacOS/FolderSync")),
            || panic!("a packaged app must not use the workspace binary"),
            verify_bundle_signature,
            &manifest,
            &RealProcessRunner,
        )
        .unwrap()
        .unwrap();
        assert_eq!(identity.path(), bundle.join(BUNDLE_HELPER));
        assert_eq!(identity.version(), manifest.rclone_version());
        assert_ne!(
            identity.sha256(),
            manifest.host_platform_pin().unwrap().1.binary_sha256
        );
        verify_binary_integrity(&identity).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn executable_sha256_shares_digest_and_invalidates_timestamp_preserving_replacement() {
        let dir = unique_temp_dir("cached-digest");
        let binary = dir.join("rclone");
        let alias = dir.join("alias");
        fs::write(&binary, b"before").unwrap();
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        let modified = fs::metadata(&binary).unwrap().modified().unwrap();

        let [original, repeated] = std::thread::scope(|scope| {
            [
                scope.spawn(|| executable_sha256(&binary).unwrap()),
                scope.spawn(|| executable_sha256(&alias).unwrap()),
            ]
            .map(|call| call.join().unwrap())
        });
        assert!(Arc::ptr_eq(&original, &repeated));
        assert_eq!(original.as_ref(), sha256_file(&binary).unwrap());

        // A same-length rewrite must invalidate the cache even if its mtime is restored.
        fs::write(&binary, b"edited").unwrap();
        File::open(&binary).unwrap().set_modified(modified).unwrap();
        assert_eq!(fs::metadata(&binary).unwrap().modified().unwrap(), modified);
        let edited = executable_sha256(&binary).unwrap();
        assert_ne!(original, edited);
        assert_eq!(edited.as_ref(), sha256_file(&binary).unwrap());

        // Atomic replacement under the same path and timestamp must also rehash.
        let replacement = dir.join("replacement");
        fs::write(&replacement, b"after!").unwrap();
        File::open(&replacement)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        fs::rename(&replacement, &binary).unwrap();
        assert_eq!(fs::metadata(&binary).unwrap().modified().unwrap(), modified);
        let replaced = executable_sha256(&alias).unwrap();
        assert_ne!(edited, replaced);
        assert_eq!(replaced.as_ref(), sha256_file(&binary).unwrap());
        let _ = fs::remove_dir_all(&dir);
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
        let identity = qualify_pinned_binary().unwrap().unwrap();
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
