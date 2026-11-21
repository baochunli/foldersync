//! Folder identity resolution and pre-run ID verification built on
//! `rclone lsjson --dirs-only` (§7).
//!
//! Google Drive allows duplicate sibling folder names with distinct object IDs.
//! This module resolves each path component step-by-step, rejects ambiguous
//! duplicates, and verifies persisted expected folder IDs before a run.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    config::stderr_indicates_auth_failure,
    process::{ProcessError, ProcessRunner, ProcessSpec, SpawnOptions, minimal_isolated_env},
    qualification::{
        EngineIdentity, QualificationError, QualificationManifest,
        verify_binary_integrity_with_manifest,
    },
    request::{PasswordCommand, RequestError, validate_remote_name_str, validate_remote_path_str},
};

/// Single item emitted by `rclone lsjson` (Google Drive and local backends).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LsJsonEntry {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub mod_time: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default, rename = "ID")]
    pub id: Option<String>,
    #[serde(default, rename = "OrigID")]
    pub orig_id: Option<String>,
    #[serde(default)]
    pub is_bucket: Option<bool>,
}

/// Resolved single component along a cloud collection path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedComponent {
    pub name: String,
    pub path: String,
    pub folder_id: Option<String>,
}

/// Complete step-by-step folder identity resolution for a remote path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRemotePath {
    pub remote: String,
    pub display_path: String,
    pub components: Vec<ResolvedComponent>,
}

impl ResolvedRemotePath {
    /// Returns the Drive object ID of the leaf folder, if the backend supplies IDs.
    pub fn leaf_folder_id(&self) -> Option<&str> {
        self.components.last().and_then(|c| c.folder_id.as_deref())
    }

    /// Returns ancestor folder IDs above the leaf folder, optionally prefixed by
    /// the account's shared parent `root_folder_id`.
    pub fn ancestor_ids(&self, root_folder_id: Option<&str>) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(root) = root_folder_id.map(str::trim).filter(|s| !s.is_empty()) {
            ids.push(root.to_string());
        }
        if self.components.len() > 1 {
            for comp in &self.components[..self.components.len() - 1] {
                if let Some(id) = comp.folder_id.as_deref() {
                    ids.push(id.to_string());
                }
            }
        }
        ids
    }
}

/// Duplicate sibling directory name observed in a single `lsjson --dirs-only` listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateDirFinding {
    pub name: String,
    pub matching_ids: Vec<Option<String>>,
}

/// Validated `rclone lsjson --dirs-only` request targeting an account remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsJsonRequest {
    engine: EngineIdentity,
    config_path: PathBuf,
    remote_name: String,
    is_drive: bool,
    password_command: Option<PasswordCommand>,
}

/// Errors returned while listing or resolving remote folder identities.
#[derive(Debug, Error)]
pub enum LsJsonError {
    #[error("invalid request parameter: {0}")]
    Request(#[from] RequestError),
    #[error("engine integrity check failed: {0}")]
    Qualification(#[from] QualificationError),
    #[error("authentication required while querying remote: {reason}")]
    AuthenticationRequired { reason: String },
    #[error("folder `{component}` was not found at `{path}`")]
    NotFound { path: String, component: String },
    #[error(
        "ambiguous duplicate folder name `{component}` at `{path}` ({count} matching directories; IDs: {matching_ids:?})"
    )]
    AmbiguousDuplicate {
        path: String,
        component: String,
        count: usize,
        matching_ids: Vec<Option<String>>,
    },
    #[error("backend did not return a folder ID for `{component}` at `{path}`")]
    MissingFolderId { path: String, component: String },
    #[error(
        "folder identity changed at `{path}`: expected ID `{expected_id}`, observed `{actual_id}`"
    )]
    FolderIdMismatch {
        path: String,
        expected_id: String,
        actual_id: String,
    },
    #[error(
        "ancestor folder IDs changed for `{path}`: expected {expected_ancestors:?}, observed {actual_ancestors:?}"
    )]
    AncestorIdMismatch {
        path: String,
        expected_ancestors: Vec<String>,
        actual_ancestors: Vec<String>,
    },
    #[error("failed to parse rclone lsjson output: {reason}")]
    ParseJson { reason: String },
    #[error("rclone lsjson failed (code={code:?}, signal={signal:?}): {detail}")]
    CommandFailed {
        code: Option<i32>,
        signal: Option<i32>,
        detail: String,
    },
    #[error("process error while running lsjson: {0}")]
    Process(#[from] ProcessError),
}

impl LsJsonRequest {
    pub fn new(
        engine: EngineIdentity,
        config_path: PathBuf,
        remote_name: impl Into<String>,
        is_drive: bool,
        password_command: Option<PasswordCommand>,
    ) -> Result<Self, LsJsonError> {
        let remote_name = remote_name.into();
        validate_remote_name_str(&remote_name)?;
        if !config_path.is_absolute() {
            return Err(LsJsonError::Request(RequestError::RelativePath {
                field: "config_path",
                path: config_path,
            }));
        }
        Ok(Self {
            engine,
            config_path,
            remote_name,
            is_drive,
            password_command,
        })
    }

    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    pub fn is_drive(&self) -> bool {
        self.is_drive
    }

    pub fn password_command(&self) -> Option<&PasswordCommand> {
        self.password_command.as_ref()
    }

    /// Builds the `rclone lsjson --dirs-only` argv for `subpath` (`""` for remote root).
    pub fn to_args(&self, subpath: &str) -> Result<Vec<OsString>, LsJsonError> {
        if !subpath.is_empty() {
            // Allow absolute paths only for local-backend test remotes (e.g. `mylocal:/tmp/tree`).
            if self.is_drive {
                validate_remote_path_str(subpath)?;
            } else if subpath.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
                return Err(LsJsonError::Request(RequestError::InvalidRemotePath {
                    path: subpath.to_string(),
                    reason: "must not contain NUL, CR, or LF",
                }));
            }
        }
        let target = format!("{}:{subpath}", self.remote_name);
        let mut args = vec![
            OsString::from("lsjson"),
            OsString::from("--dirs-only"),
            OsString::from(target),
            OsString::from("--config"),
            self.config_path.as_os_str().to_os_string(),
        ];
        if let Some(pw) = &self.password_command {
            args.push(OsString::from("--password-command"));
            args.push(OsString::from(pw.to_rclone_space_sep_list()));
        }
        args.push(OsString::from("--ask-password=false"));
        if self.is_drive {
            args.push(OsString::from("--drive-skip-gdocs"));
            args.push(OsString::from("--drive-skip-shortcuts"));
        }
        Ok(args)
    }

    fn build_verified_spec(
        &self,
        subpath: &str,
        manifest: &QualificationManifest,
    ) -> Result<ProcessSpec, LsJsonError> {
        verify_binary_integrity_with_manifest(&self.engine, manifest)?;
        let account_dir = self.config_path.parent();
        Ok(ProcessSpec {
            program: self.engine.path().to_path_buf(),
            args: self.to_args(subpath)?,
            current_dir: None,
            env: minimal_isolated_env(account_dir),
            expected_binary_sha256: Some(self.engine.sha256().to_string()),
        })
    }
}

/// Parses raw `rclone lsjson` stdout JSON into typed [`LsJsonEntry`] records.
pub fn parse_lsjson_entries(stdout: &str) -> Result<Vec<LsJsonEntry>, LsJsonError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<LsJsonEntry>>(trimmed).map_err(|err| LsJsonError::ParseJson {
        reason: err.to_string(),
    })
}

/// Detects duplicate sibling directory names within a single `lsjson --dirs-only` listing.
pub fn find_duplicate_dir_names(entries: &[LsJsonEntry]) -> Vec<DuplicateDirFinding> {
    let mut by_name: BTreeMap<&str, Vec<Option<String>>> = BTreeMap::new();
    for entry in entries {
        if entry.is_dir {
            by_name
                .entry(entry.name.as_str())
                .or_default()
                .push(entry.id.clone().filter(|s| !s.is_empty()));
        }
    }
    by_name
        .into_iter()
        .filter(|(_, ids)| ids.len() >= 2)
        .map(|(name, matching_ids)| DuplicateDirFinding {
            name: name.to_string(),
            matching_ids,
        })
        .collect()
}

/// Lists immediate child directories at `subpath` (`""` for the configured remote root).
pub fn list_dirs_with_runner(
    req: &LsJsonRequest,
    subpath: &str,
    runner: &dyn ProcessRunner,
) -> Result<Vec<LsJsonEntry>, LsJsonError> {
    let manifest = QualificationManifest::embedded()?;
    list_dirs_with_manifest(req, subpath, &manifest, runner)
}

pub(crate) fn list_dirs_with_manifest(
    req: &LsJsonRequest,
    subpath: &str,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
) -> Result<Vec<LsJsonEntry>, LsJsonError> {
    let spec = req.build_verified_spec(subpath, manifest)?;
    let handle = runner.spawn(&spec, SpawnOptions::default())?;
    let run_out = handle.wait()?;
    if !run_out.exit_status.is_success() {
        let stderr = run_out.output.stderr_text();
        let stdout = run_out.output.stdout_text();
        let combined = if stderr.is_empty() { stdout } else { stderr };
        if stderr_indicates_auth_failure(&combined) {
            return Err(LsJsonError::AuthenticationRequired {
                reason: combined.trim().to_string(),
            });
        }
        return Err(LsJsonError::CommandFailed {
            code: run_out.exit_status.code,
            signal: run_out.exit_status.signal,
            detail: combined.trim().to_string(),
        });
    }
    parse_lsjson_entries(&run_out.output.stdout_text())
}

/// Resolves every component of `relative_path` (e.g. `Data/Research`) using
/// `rclone lsjson --dirs-only`, rejecting missing components, ambiguous
/// duplicate folder names, and (when `require_ids` is `true`) missing object IDs.
pub fn resolve_folder_path_with_runner(
    req: &LsJsonRequest,
    relative_path: &str,
    require_ids: bool,
    runner: &dyn ProcessRunner,
) -> Result<ResolvedRemotePath, LsJsonError> {
    let manifest = QualificationManifest::embedded()?;
    resolve_folder_path_with_manifest(req, relative_path, require_ids, &manifest, runner)
}

pub(crate) fn resolve_folder_path_with_manifest(
    req: &LsJsonRequest,
    relative_path: &str,
    require_ids: bool,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
) -> Result<ResolvedRemotePath, LsJsonError> {
    validate_remote_path_str(relative_path)?;
    let segments: Vec<&str> = relative_path.split('/').collect();
    let mut prefix = String::new();
    let mut resolved_components = Vec::with_capacity(segments.len());

    for segment in segments {
        let entries = list_dirs_with_manifest(req, &prefix, manifest, runner)?;
        let matches: Vec<&LsJsonEntry> = entries
            .iter()
            .filter(|e| e.is_dir && e.name == segment)
            .collect();

        let current_path = if prefix.is_empty() {
            segment.to_string()
        } else {
            format!("{prefix}/{segment}")
        };

        match matches.as_slice() {
            [] => {
                return Err(LsJsonError::NotFound {
                    path: current_path,
                    component: segment.to_string(),
                });
            }
            [single] => {
                let folder_id = single
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned);
                if require_ids && folder_id.is_none() {
                    return Err(LsJsonError::MissingFolderId {
                        path: current_path,
                        component: segment.to_string(),
                    });
                }
                resolved_components.push(ResolvedComponent {
                    name: segment.to_string(),
                    path: current_path.clone(),
                    folder_id,
                });
            }
            many => {
                let matching_ids = many
                    .iter()
                    .map(|e| e.id.clone().filter(|s| !s.is_empty()))
                    .collect();
                return Err(LsJsonError::AmbiguousDuplicate {
                    path: current_path,
                    component: segment.to_string(),
                    count: many.len(),
                    matching_ids,
                });
            }
        }

        prefix = current_path;
    }

    Ok(ResolvedRemotePath {
        remote: req.remote_name.clone(),
        display_path: relative_path.to_string(),
        components: resolved_components,
    })
}

/// Pre-run check (§7): resolves `expected_path` via `lsjson --dirs-only`,
/// rejects duplicates or missing components, and confirms that the leaf folder ID
/// (and optional ancestor IDs) still match what was persisted for the mapping.
pub fn verify_expected_folder_identity_with_runner(
    req: &LsJsonRequest,
    expected_path: &str,
    expected_folder_id: &str,
    root_folder_id: Option<&str>,
    expected_ancestor_ids: Option<&[String]>,
    runner: &dyn ProcessRunner,
) -> Result<ResolvedRemotePath, LsJsonError> {
    let manifest = QualificationManifest::embedded()?;
    verify_expected_folder_identity_with_manifest(
        req,
        expected_path,
        expected_folder_id,
        root_folder_id,
        expected_ancestor_ids,
        &manifest,
        runner,
    )
}

pub(crate) fn verify_expected_folder_identity_with_manifest(
    req: &LsJsonRequest,
    expected_path: &str,
    expected_folder_id: &str,
    root_folder_id: Option<&str>,
    expected_ancestor_ids: Option<&[String]>,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
) -> Result<ResolvedRemotePath, LsJsonError> {
    let resolved = resolve_folder_path_with_manifest(req, expected_path, true, manifest, runner)?;
    let actual_id = resolved
        .leaf_folder_id()
        .ok_or_else(|| LsJsonError::MissingFolderId {
            path: expected_path.to_string(),
            component: expected_path
                .rsplit('/')
                .next()
                .unwrap_or(expected_path)
                .to_string(),
        })?;
    if actual_id != expected_folder_id {
        return Err(LsJsonError::FolderIdMismatch {
            path: expected_path.to_string(),
            expected_id: expected_folder_id.to_string(),
            actual_id: actual_id.to_string(),
        });
    }
    if let Some(expected_ancestors) = expected_ancestor_ids {
        let actual_ancestors = resolved.ancestor_ids(root_folder_id);
        if actual_ancestors != expected_ancestors {
            return Err(LsJsonError::AncestorIdMismatch {
                path: expected_path.to_string(),
                expected_ancestors: expected_ancestors.to_vec(),
                actual_ancestors,
            });
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        process::{ExitStatusInfo, FakeProcessRunner, FakeRunScript, OutputLine, StreamKind},
        qualification::{host_platform_key, qualify_binary_with_runner, sha256_file},
    };
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn unique_tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "foldersync-lsjson-unit-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn make_fake_engine(tmp: &Path) -> (EngineIdentity, QualificationManifest) {
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
        (engine, manifest)
    }

    #[test]
    fn parses_real_drive_and_local_lsjson_shapes() {
        let drive_json = r#"[
            {"Path":"Data","Name":"Data","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T12:00:00.000Z","IsDir":true,"ID":"1DataFolderId"},
            {"Path":"Recovery","Name":"Recovery","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T12:01:00.000Z","IsDir":true,"ID":"1RecoveryId"}
        ]"#;
        let parsed = parse_lsjson_entries(drive_json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Data");
        assert_eq!(parsed[0].size, -1);
        assert!(parsed[0].is_dir);
        assert_eq!(parsed[0].id.as_deref(), Some("1DataFolderId"));

        // Local backend shape has no ID field.
        let local_json = r#"[
            {"Path":"Family","Name":"Family","Size":64,"MimeType":"inode/directory","ModTime":"2026-11-20T22:37:04.591421756-05:00","IsDir":true}
        ]"#;
        let local = parse_lsjson_entries(local_json).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].id, None);
    }

    #[cfg(unix)]
    #[test]
    fn resolves_drive_component_ids_and_verifies_expected_id_and_ancestors() {
        let tmp = unique_tmp("resolve-ok");
        let (engine, manifest) = make_fake_engine(&tmp);
        let pw = PasswordCommand::new(
            tmp.join("bin/sync-cli"),
            vec![
                "credential-helper".to_string(),
                "--account".to_string(),
                "a1".to_string(),
            ],
        )
        .unwrap();
        let req = LsJsonRequest::new(
            engine,
            tmp.join("accounts/a1/rclone.conf"),
            "household",
            true,
            Some(pw),
        )
        .unwrap();

        let runner = FakeProcessRunner::new();
        // Level 1: `household:`
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Data","Name":"Data","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T12:00:00Z","IsDir":true,"ID":"1DataId"},{"Path":"Recovery","Name":"Recovery","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T12:00:00Z","IsDir":true,"ID":"1RecId"}]"#.to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        // Level 2: `household:Data`
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Research","Name":"Research","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T12:05:00Z","IsDir":true,"ID":"1ResearchId"}]"#.to_string(),
                truncated: false,
            }],
            ..Default::default()
        });

        let expected_ancestors = vec!["0AParentRoot".to_string(), "1DataId".to_string()];
        let resolved = verify_expected_folder_identity_with_manifest(
            &req,
            "Data/Research",
            "1ResearchId",
            Some("0AParentRoot"),
            Some(&expected_ancestors),
            &manifest,
            &runner,
        )
        .unwrap();

        assert_eq!(resolved.leaf_folder_id(), Some("1ResearchId"));
        assert_eq!(resolved.components.len(), 2);
        assert_eq!(resolved.components[0].folder_id.as_deref(), Some("1DataId"));
        assert_eq!(
            resolved.components[1].folder_id.as_deref(),
            Some("1ResearchId")
        );
        assert_eq!(
            resolved.ancestor_ids(Some("0AParentRoot")),
            expected_ancestors
        );

        let specs = runner.spawned_specs();
        assert_eq!(specs.len(), 2);
        let argv0: Vec<String> = specs[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv0[0], "lsjson");
        assert_eq!(argv0[1], "--dirs-only");
        assert_eq!(argv0[2], "household:");
        assert!(argv0.contains(&"--drive-skip-gdocs".to_string()));
        assert!(argv0.contains(&"--drive-skip-shortcuts".to_string()));
        assert!(argv0.contains(&"--ask-password=false".to_string()));

        let argv1: Vec<String> = specs[1]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv1[2], "household:Data");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_ambiguous_duplicate_names_and_folder_id_mismatch() {
        let tmp = unique_tmp("resolve-dup");
        let (engine, manifest) = make_fake_engine(&tmp);
        let req = LsJsonRequest::new(
            engine,
            tmp.join("accounts/a1/rclone.conf"),
            "household",
            true,
            None,
        )
        .unwrap();

        // Case 1: duplicate sibling folder named `Research`.
        let runner_dup = FakeProcessRunner::new();
        runner_dup.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Data","Name":"Data","Size":-1,"IsDir":true,"ID":"1DataId"}]"#
                    .to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        runner_dup.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[
                    {"Path":"Research","Name":"Research","Size":-1,"IsDir":true,"ID":"1ResA"},
                    {"Path":"Research","Name":"Research","Size":-1,"IsDir":true,"ID":"1ResB"}
                ]"#
                .to_string(),
                truncated: false,
            }],
            ..Default::default()
        });

        let err =
            resolve_folder_path_with_manifest(&req, "Data/Research", true, &manifest, &runner_dup)
                .unwrap_err();
        assert!(matches!(
            err,
            LsJsonError::AmbiguousDuplicate {
                ref component,
                count: 2,
                ref matching_ids,
                ..
            } if component == "Research"
                && matching_ids == &[Some("1ResA".to_string()), Some("1ResB".to_string())]
        ));

        // Case 2: folder replaced at same path with a different ID -> FolderIdMismatch.
        let runner_mismatch = FakeProcessRunner::new();
        runner_mismatch.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Data","Name":"Data","Size":-1,"IsDir":true,"ID":"1DataId"}]"#
                    .to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        runner_mismatch.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Research","Name":"Research","Size":-1,"IsDir":true,"ID":"1ReplacementId"}]"#
                    .to_string(),
                truncated: false,
            }],
            ..Default::default()
        });

        let err2 = verify_expected_folder_identity_with_manifest(
            &req,
            "Data/Research",
            "1OriginalId",
            Some("0AParentRoot"),
            None,
            &manifest,
            &runner_mismatch,
        )
        .unwrap_err();
        assert!(matches!(
            err2,
            LsJsonError::FolderIdMismatch {
                ref expected_id,
                ref actual_id,
                ..
            } if expected_id == "1OriginalId" && actual_id == "1ReplacementId"
        ));

        // Case 3: locked credential store -> AuthenticationRequired.
        let runner_auth = FakeProcessRunner::new();
        runner_auth.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stderr,
                text:
                    "CRITICAL: Failed to load config file: password command failed: exit status 3"
                        .to_string(),
                truncated: false,
            }],
            exit_status: ExitStatusInfo {
                code: Some(1),
                signal: None,
            },
            ..Default::default()
        });
        let err3 = list_dirs_with_manifest(&req, "", &manifest, &runner_auth).unwrap_err();
        assert!(matches!(err3, LsJsonError::AuthenticationRequired { .. }));

        let _ = fs::remove_dir_all(&tmp);
    }
}
