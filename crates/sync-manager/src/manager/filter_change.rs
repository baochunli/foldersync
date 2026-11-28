//! One worker-owned migration. A durable recovery case points to the immutable
//! move manifest in the first move run's private artifacts.

use rclone_adapter::{
    ControlledProcessRunner, LsJsonEntry, ProcessError, ProcessRunner, ProcessSpec,
    RealProcessRunner, RunHandle, check_filtered, qualify_pinned_binary, render_move_filter,
};

use super::*;

pub(super) const FILTER_MIGRATION_HOLD: &str = "filter_migration";
const MANIFEST: &str = "filter-migration.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterMigrationPhase {
    CatchUp,
    Checking,
    MovingToRecovery,
    Rebaseline,
    Verifying,
    Restoring,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusionMatch {
    pub pattern: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
pub(super) enum FilterJob {
    Apply {
        generation_id: GenerationId,
        exclusions: Vec<String>,
    },
    Finish,
    Restore,
}

impl FilterJob {
    pub(super) fn phase(&self) -> FilterMigrationPhase {
        match self {
            Self::Apply { .. } => FilterMigrationPhase::CatchUp,
            Self::Finish => FilterMigrationPhase::MovingToRecovery,
            Self::Restore => FilterMigrationPhase::Restoring,
        }
    }
}

pub(super) struct ActiveFilterChange {
    pub(super) mapping_id: MappingId,
    pub(super) control: ProcessControl,
    pub(super) phase: FilterMigrationPhase,
}

impl SharedState {
    pub(super) fn filter_change_running(&self, id: &MappingId) -> bool {
        self.active_filter_change
            .as_ref()
            .is_some_and(|job| &job.mapping_id == id)
    }

    pub(super) fn cancel_filter_change(&self, id: &MappingId) {
        if let Some(job) = self
            .active_filter_change
            .as_ref()
            .filter(|job| &job.mapping_id == id)
        {
            job.control.cancel();
        }
    }
}

impl SyncManager {
    pub fn finish_filter_change(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.queue_filter_recovery(mapping_id.into(), FilterJob::Finish)
    }

    pub fn restore_filter_change(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.queue_filter_recovery(mapping_id.into(), FilterJob::Restore)
    }

    fn queue_filter_recovery(
        &self,
        mapping_id: MappingId,
        job: FilterJob,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mapping = lookup_mapping(&guard.store, &mapping_id)?;
        if !matches!(
            mapping.state.execution,
            Execution::Idle | Execution::Interrupted
        ) || guard.filter_change_running(&mapping_id)
            || hold(&guard.store, &mapping_id)?.is_none()
        {
            return Err(rejected(
                "This mapping has no idle exclusion migration to recover.",
            ));
        }
        queue(&self.inner, &mut guard, mapping, job)
    }
}

pub(super) fn rejected(cause: impl Into<String>) -> ManagerError {
    ManagerError::PreflightRejected {
        cause: cause.into(),
        next_step: "Review the sync run and recovery details before trying again.".to_owned(),
    }
}

pub(super) fn hold(
    store: &SqliteStateStore,
    id: &MappingId,
) -> Result<Option<RecoveryCaseRecord>, ManagerError> {
    Ok(store
        .list_open_recovery_cases(id)?
        .into_iter()
        .find(|case| case.reason_kind == FILTER_MIGRATION_HOLD))
}

pub(super) fn queue(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mut mapping: MappingRecord,
    job: FilterJob,
) -> Result<CommandAcceptance, ManagerError> {
    if guard.shutdown
        || guard.storage_blocked_mappings.contains(&mapping.id)
        || find_blocking_global_orphan(&guard.store, inner.process_inspector.as_ref())?.is_some()
    {
        return Err(rejected(
            "Resolve the earlier sync or storage hold before changing exclusions.",
        ));
    }
    mapping.state.execution = Execution::Queued;
    mapping.updated_at = inner.clock.now_rfc3339();
    guard.store.update_mapping_runtime(&mapping)?;
    guard
        .job_queue
        .retain(|queued| queued.mapping_id != mapping.id);
    guard.job_queue.push_front(QueuedJob {
        automatic: false,
        mapping_id: mapping.id.clone(),
        kind: QueuedJobKind::FilterChange(job),
    });
    inner.worker_cv.notify_all();
    Ok(CommandAcceptance {
        outcome: CommandOutcome::InitializationPreflightQueued {
            mapping_id: mapping.id,
        },
        snapshot: publish_updated_snapshot(inner, guard)?,
    })
}

fn file_inventory(
    drive: &LockedDrive<'_>,
    root: &str,
    filters: Option<&Path>,
) -> Result<BTreeMap<String, LsJsonEntry>, ManagerError> {
    let mut inventory = BTreeMap::new();
    for entry in drive
        .request
        .list_files(root, filters, &drive.runner)
        .map_err(|err| drive_error(err.into()))?
    {
        let path = entry.path.clone();
        if entry.is_dir
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || path.chars().any(char::is_control)
            || inventory.insert(path.clone(), entry).is_some()
        {
            return Err(rejected(format!(
                "The Drive file path `{path}` is invalid or ambiguous."
            )));
        }
    }
    if let Some(finding) = sync_core::validate_names(&inventory.keys().collect::<Vec<_>>())
        .into_iter()
        .next()
    {
        return Err(rejected(finding.cause));
    }
    Ok(inventory)
}

/// Validate the selected files and the directories used to address them.
/// Keep unrelated entries only to detect aliases of paths in this scope.
fn move_inventory(
    entries: Vec<LsJsonEntry>,
    manifest: &MigrationManifest,
) -> Result<BTreeMap<String, LsJsonEntry>, ManagerError> {
    let files: BTreeSet<_> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let mut scope = files.clone();
    for path in &files {
        scope.extend(path.match_indices('/').map(|(index, _)| &path[..index]));
    }
    let mut names: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
    let present: BTreeSet<_> = names.iter().copied().collect();
    // Include planned paths missing on this side to catch case and Unicode
    // aliases of a destination that is about to be created.
    names.extend(files.difference(&present).copied());
    if let Some(finding) = sync_core::validate_names(&names)
        .into_iter()
        .find(|finding| {
            finding
                .paths
                .iter()
                .any(|path| scope.contains(path.as_str()))
        })
    {
        return Err(rejected(finding.cause));
    }
    let mut inventory = BTreeMap::new();
    for entry in entries
        .into_iter()
        .filter(|entry| scope.contains(entry.path.as_str()))
    {
        let is_file = files.contains(entry.path.as_str());
        if is_file == entry.is_dir {
            return Err(rejected(format!(
                "The Drive file or folder `{}` occupies a path required by the exclusion move.",
                entry.path
            )));
        }
        if is_file {
            inventory.insert(entry.path.clone(), entry);
        }
    }
    Ok(inventory)
}

fn temporary_filters(rules: &str) -> Result<tempfile::NamedTempFile, ManagerError> {
    let mut file = tempfile::NamedTempFile::new()
        .map_err(|err| rejected(format!("Could not stage the exclusion filter: {err}")))?;
    file.write_all(rules.as_bytes())
        .map_err(|err| rejected(format!("Could not stage the exclusion filter: {err}")))?;
    Ok(file)
}

pub(super) fn preview_move_counts(
    inner: &ManagerInner,
    cmd: &AddMappingCommand,
    old: &BTreeSet<String>,
    added: &[String],
) -> Result<Vec<ExclusionMatch>, ManagerError> {
    let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
    let drive = inner
        .accounts
        .locked_drive(&cmd.account_id, &control)
        .map_err(drive_error)?;
    drive
        .verify_collection(&cmd.collection)
        .map_err(drive_error)?;
    let root = &cmd.collection.display_path;
    let mut counts = Vec::with_capacity(added.len());
    for pattern in added {
        let rules = render_move_filter(old.iter().map(String::as_str), [pattern.as_str()])?;
        let filter = temporary_filters(&rules)?;
        let matched = file_inventory(&drive, root, Some(filter.path()))?;
        counts.push(ExclusionMatch {
            pattern: pattern.clone(),
            file_count: matched.len(),
            total_bytes: matched.values().fold(0u64, |total, entry| {
                total.saturating_add(entry.size.max(0) as u64)
            }),
        });
    }
    Ok(counts)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MoveFile {
    path: String,
    id: String,
    size: i64,
    hashes: BTreeMap<String, String>,
}

impl MoveFile {
    fn matches(&self, entry: &LsJsonEntry) -> bool {
        entry.id.as_deref() == Some(&self.id)
            && entry.size == self.size
            && entry.hashes == self.hashes
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct MigrationManifest {
    exclusions: Vec<String>,
    files: Vec<MoveFile>,
}

enum TransferAction<'a> {
    FirstMove(Vec<LsJsonEntry>),
    Finish {
        original: &'a RunRecord,
        filters: &'a Path,
    },
    Restore {
        original: &'a RunRecord,
    },
}

struct Migration<'a> {
    inner: &'a Arc<ManagerInner>,
    mapping_id: &'a MappingId,
    control: &'a ProcessControl,
}

impl Migration<'_> {
    fn check_cancelled(&self) -> Result<(), ManagerError> {
        self.control
            .check()
            .map_err(|err| rejected(format!("The exclusion change stopped: {err}")))
    }

    fn phase(&self, phase: FilterMigrationPhase) -> Result<(), ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        self.check_cancelled()?;
        if let Some(active) = guard.active_filter_change.as_mut() {
            active.phase = phase;
        }
        publish_updated_snapshot(self.inner, &mut guard)?;
        Ok(())
    }

    fn prepare(&self, id: &GenerationId, purpose: RunPurpose) -> Result<PreparedRun, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        self.check_cancelled()?;
        if guard.shutdown
            || guard.storage_blocked_mappings.contains(self.mapping_id)
            || find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
                .is_some()
        {
            return Err(rejected(
                "Resolve the earlier sync or storage hold before continuing.",
            ));
        }
        let mapping = lookup_mapping(&guard.store, self.mapping_id)?;
        if matches!(purpose, RunPurpose::Bisync(RunMode::Normal))
            && (mapping.state.readiness() != Readiness::Ready
                || mapping.state.execution != Execution::Queued
                || mapping.active_generation_id.as_ref() != Some(id)
                || !guard
                    .store
                    .list_open_recovery_cases(self.mapping_id)?
                    .is_empty()
                || mapping.unreviewed_conflict_run_id.is_some())
        {
            return Err(rejected(
                "The verified mapping is no longer ready for catch-up.",
            ));
        }
        prepare_run_locked(self.inner, &mut guard, mapping, id, purpose)
    }

    fn bisync(
        &self,
        prepared: PreparedRun,
        label: &str,
        drive: Option<&LockedDrive<'_>>,
    ) -> Result<(), ManagerError> {
        let id = prepared.run.id.clone();
        execute_prepared_job(self.inner, prepared, drive);
        self.check_cancelled()?;
        let guard = self.inner.state.lock().unwrap();
        let run = guard
            .store
            .get_run(&id)?
            .ok_or_else(|| rejected("The exclusion sync run could not be found."))?;
        if run.outcome_kind.as_deref() != Some("success") {
            let reason = run.outcome_detail.as_deref().unwrap_or(&run.status);
            return Err(rejected(format!("{label} stopped: {reason}")));
        }
        Ok(())
    }

    fn verify_base(
        &self,
        mapping: &MappingRecord,
        generation: &GenerationRecord,
    ) -> Result<(), ManagerError> {
        verify_endpoints_for_generation(
            self.inner,
            mapping,
            generation,
            &observe_binding(self.inner.volume_probe.as_ref(), &mapping.local_binding),
        )
        .map_err(|(_, err)| *err)
    }

    fn open_drive(
        &self,
        mapping: &MappingRecord,
        generation: &GenerationRecord,
    ) -> Result<Option<LockedDrive<'_>>, ManagerError> {
        self.check_cancelled()?;
        self.verify_base(mapping, generation)?;
        if mapping.remote_endpoint_kind != "drive_remote" {
            return Ok(None);
        }
        let drive = self
            .inner
            .accounts
            .locked_drive(&mapping.account_id, self.control)
            .map_err(drive_error)?;
        verify_drive_generation(&drive, mapping, generation).map_err(|(_, err)| *err)?;
        Ok(Some(drive))
    }

    fn check_trees(
        &self,
        mut plan: LaunchPlan,
        filters: &Path,
        drive: Option<&LockedDrive<'_>>,
    ) -> Result<(), ManagerError> {
        self.check_cancelled()?;
        let check = |plan: LaunchPlan, engine, runner: &dyn ProcessRunner| {
            let request = plan.bisync_request(engine)?;
            check_filtered(&request, filters, runner).map_err(|err| {
                rejected(format!(
                    "The folders did not pass the check under the new exclusions: {err}"
                ))
            })
        };
        if let Some(drive) = drive {
            plan.config_path = drive.account.config_path.clone();
            plan.password_command = drive.request.password_command().cloned();
            check(plan, drive.request.engine().clone(), &drive.runner)
        } else {
            let engine = qualify_pinned_binary()?
                .ok_or_else(|| rejected("The pinned sync engine is unavailable."))?;
            let runner = ControlledProcessRunner {
                runner: &RealProcessRunner,
                control: self.control,
            };
            check(plan, engine, &runner)
        }
    }

    fn apply(
        &self,
        generation_id: GenerationId,
        exclusions: Vec<String>,
    ) -> Result<(), ManagerError> {
        let prepared = self.prepare(&generation_id, RunPurpose::Bisync(RunMode::Normal))?;
        let mapping = prepared.mapping.clone();
        let old = prepared.generation.clone();
        let gate_plan = prepared.plan.clone();
        self.bisync(prepared, "Catch-up sync", None)?;

        self.phase(FilterMigrationPhase::Checking)?;
        let filters =
            temporary_filters(&render_filters_file(&exclusions.iter().cloned().collect()))?;
        let drive = self.open_drive(&mapping, &old)?;
        self.check_trees(gate_plan.clone(), filters.path(), drive.as_ref())?;
        if let Some(drive) = drive.as_ref() {
            let (manifest, inventory) =
                self.find_new_exclusions(drive, &mapping, &old, exclusions)?;
            if !manifest.files.is_empty() {
                let prepared = self.prepare(&old.id, RunPurpose::MoveExcluded)?;
                self.transfer(
                    prepared,
                    drive,
                    &manifest,
                    TransferAction::FirstMove(inventory),
                )?;
                self.check_trees(gate_plan, filters.path(), Some(drive))?;
            }
            self.rebaseline(&old, manifest.exclusions, Some(drive))
        } else {
            self.rebaseline(&old, exclusions, None)
        }
    }

    fn find_new_exclusions(
        &self,
        drive: &LockedDrive<'_>,
        mapping: &MappingRecord,
        old: &GenerationRecord,
        exclusions: Vec<String>,
    ) -> Result<(MigrationManifest, Vec<LsJsonEntry>), ManagerError> {
        let previous = &old.config.effective_filters;
        let added = exclusions
            .iter()
            .filter(|pattern| !previous.contains(*pattern))
            .map(String::as_str);
        let rules = render_move_filter(previous.iter().map(String::as_str), added)?;
        let filter = temporary_filters(&rules)?;
        let inventory = drive
            .request
            .list_inventory(
                &mapping.collection.display_path,
                Some(filter.path()),
                &drive.runner,
            )
            .map_err(|error| drive_error(error.into()))?;
        let mut files = Vec::new();
        for entry in inventory.iter().filter(|entry| !entry.is_dir) {
            if entry.path == ACCESS_MARKER_FILENAME {
                return Err(rejected(
                    "The new exclusions would hide the FolderSync access marker.",
                ));
            }
            let id = entry
                .id
                .as_ref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    rejected(format!("Drive did not report an ID for `{}`.", entry.path))
                })?;
            if entry.size < 0
                || entry.hashes.is_empty()
                || entry.hashes.values().any(String::is_empty)
            {
                return Err(rejected(format!(
                    "Drive could not verify the bytes of `{}`.",
                    entry.path
                )));
            }
            if entry
                .path
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
                || entry.path.chars().any(char::is_control)
            {
                return Err(rejected(format!(
                    "The Drive file path `{}` is invalid.",
                    entry.path
                )));
            }
            files.push(MoveFile {
                path: entry.path.clone(),
                id: id.clone(),
                size: entry.size,
                hashes: entry.hashes.clone(),
            });
        }
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok((MigrationManifest { exclusions, files }, inventory))
    }

    fn recover(&self, restore: bool) -> Result<(), ManagerError> {
        let (mapping, old, original) = {
            let guard = self.inner.state.lock().unwrap();
            let mapping = lookup_mapping(&guard.store, self.mapping_id)?;
            let original = hold(&guard.store, self.mapping_id)?
                .and_then(|case| case.run_id)
                .map(|id| guard.store.get_run(&id))
                .transpose()?
                .flatten()
                .ok_or_else(|| rejected("The saved exclusion move could not be found."))?;
            let old = active_generation(&guard.store, &mapping)?
                .filter(|generation| generation.id == original.generation_id)
                .ok_or_else(|| {
                    rejected("The active generation changed since the exclusion move started.")
                })?;
            (mapping, old, original)
        };
        let path = reject_symlinks_under_root(
            "exclusion move manifest",
            &self.inner.state_dir,
            &original.run_dir.join(MANIFEST),
        )?;
        let manifest_file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|err| {
                rejected(format!("Could not read the exclusion move manifest: {err}"))
            })?;
        if !manifest_file
            .metadata()
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(rejected(
                "The exclusion move manifest is not a regular file.",
            ));
        }
        let manifest: MigrationManifest =
            serde_json::from_reader(manifest_file).map_err(|err| {
                rejected(format!("Could not read the exclusion move manifest: {err}"))
            })?;
        let drive = self.open_drive(&mapping, &old)?.ok_or_else(|| {
            rejected("The held migration requires its original Google Drive account.")
        })?;
        let filters = if restore {
            None
        } else {
            Some(temporary_filters(&render_filters_file(
                &manifest.exclusions.iter().cloned().collect(),
            ))?)
        };
        let prepared = self.prepare(&old.id, RunPurpose::MoveExcluded)?;
        let plan = prepared.plan.clone();
        let action = match &filters {
            Some(filters) => TransferAction::Finish {
                original: &original,
                filters: filters.path(),
            },
            None => TransferAction::Restore {
                original: &original,
            },
        };
        self.transfer(prepared, &drive, &manifest, action)?;
        if let Some(filters) = filters {
            self.check_trees(plan, filters.path(), Some(&drive))?;
            self.rebaseline(&old, manifest.exclusions, Some(&drive))?;
        } else {
            self.release(None, "Restored all moved files to Data.")?;
        }
        Ok(())
    }

    fn persist_hold(&self, guard: &mut SharedState, run: &RunRecord) -> Result<(), ManagerError> {
        self.check_cancelled()?;
        let mut mapping = lookup_mapping(&guard.store, self.mapping_id)?;
        if mapping.state.readiness() != Readiness::Ready
            || mapping.active_generation_id.as_ref() != Some(&run.generation_id)
        {
            return Err(rejected(
                "The old sync setup changed before the cloud move.",
            ));
        }
        mapping.state = MappingState::new(
            Readiness::RecoveryRequired,
            Execution::Running,
            mapping.state.intent,
        );
        let now = self.inner.clock.now_rfc3339();
        mapping.updated_at = now.clone();
        let case = RecoveryCaseRecord {
            id: format!("rc-{}", Uuid::new_v4()),
            mapping_id: mapping.id.clone(),
            run_id: Some(run.id.clone()),
            status: "open".to_owned(),
            reason_kind: FILTER_MIGRATION_HOLD.to_owned(),
            cause: "Exclusion changes are moving files between Data and Recovery.".to_owned(),
            next_step: "Choose Finish excluding or Put the moved files back.".to_owned(),
            workdir_path: Some(run.workdir_path.clone()),
            backup_dir1: None,
            backup_dir2: Some(run.backup_dir2.clone()),
            log_path: Some(run.log_path.clone()),
            orphan_pid: None,
            created_at: now,
            resolved_at: None,
            resolution_note: None,
        };
        guard
            .store
            .commit_mapping_and_recovery(&mapping, Some(&case))?;
        Ok(())
    }

    fn transfer(
        &self,
        mut prepared: PreparedRun,
        drive: &LockedDrive<'_>,
        manifest: &MigrationManifest,
        action: TransferAction<'_>,
    ) -> Result<(), ManagerError> {
        let result = self.transfer_files(&mut prepared, drive, manifest, action);
        let now = self.inner.clock.now_rfc3339();
        let mut run = prepared.run;
        run.finished_at = Some(now);
        run.last_verified_phase = if result.is_ok() {
            "filter_move_verified"
        } else {
            "filter_move_stopped"
        }
        .to_owned();
        run.status = if result.is_ok() {
            "completed"
        } else {
            "failed"
        }
        .to_owned();
        run.outcome_kind = Some(
            if result.is_ok() {
                "success"
            } else {
                "filter_move_failed"
            }
            .to_owned(),
        );
        run.outcome_detail = result.as_ref().err().map(ManagerError::cause);
        let mut guard = self.inner.state.lock().unwrap();
        if guard
            .active_run
            .as_ref()
            .is_some_and(|active| active.timed_out_during_shutdown)
        {
            return Err(rejected(
                "The move was interrupted during shutdown. Review recovery before continuing.",
            ));
        }
        write_run_outcome_json(&self.inner.state_dir, &run);
        if let Err(err) = guard.store.commit_run_terminal(&run, None, None) {
            record_terminal_db_error_locked(&mut guard, self.mapping_id, err);
            return Err(rejected("Could not save the migration run outcome."));
        }
        guard.active_run = None;
        result
    }

    fn transfer_files(
        &self,
        prepared: &mut PreparedRun,
        drive: &LockedDrive<'_>,
        manifest: &MigrationManifest,
        action: TransferAction<'_>,
    ) -> Result<(), ManagerError> {
        let mapping = &prepared.mapping;
        let run = &prepared.run;
        let (original_run, restore, first_inventory) = match action {
            TransferAction::FirstMove(inventory) => (run, false, Some(inventory)),
            TransferAction::Finish { original, filters } => {
                self.check_trees(prepared.plan.clone(), filters, Some(drive))?;
                (original, false, None)
            }
            TransferAction::Restore { original } => (original, true, None),
        };
        validate_safe_path_id("migration run", original_run.id.as_str())?;
        let remote = format!("{}:", drive.request.remote_name());
        let recovery_root = mapping
            .cloud_recovery_path
            .strip_prefix(&remote)
            .ok_or_else(|| rejected("The saved cloud Recovery destination has changed."))?;
        let recovery = format!("{recovery_root}/{}/{}", mapping.device_id, original_run.id);
        if original_run.backup_dir2 != format!("{remote}{recovery}") {
            return Err(rejected(
                "The saved cloud Recovery destination has changed.",
            ));
        }
        self.phase(if restore {
            FilterMigrationPhase::Restoring
        } else {
            FilterMigrationPhase::MovingToRecovery
        })?;
        let data = &mapping.collection.display_path;
        write_move_log(&mut prepared.log_file, "Cloud Recovery folder", &recovery)?;
        drive
            .request
            .mkdir(&recovery, &drive.runner)
            .map_err(|err| drive_error(err.into()))?;
        let (source, target) = if restore {
            (recovery.as_str(), data.as_str())
        } else {
            (data.as_str(), recovery.as_str())
        };
        let list_side = |root| {
            drive
                .request
                .list_inventory(root, None, &drive.runner)
                .map_err(|error| drive_error(error.into()))
                .and_then(|entries| move_inventory(entries, manifest))
        };
        let first_move = first_inventory.is_some();
        let mut source_files = if let Some(entries) = first_inventory {
            move_inventory(entries, manifest)?
        } else {
            list_side(source)?
        };
        let mut target_files = list_side(target)?;
        let mut pending = String::new();
        for file in &manifest.files {
            match (source_files.get(&file.path), target_files.get(&file.path)) {
                (Some(entry), None) if file.matches(entry) => {
                    pending.push_str(&file.path);
                    pending.push('\n');
                }
                (None, Some(entry)) if file.matches(entry) => {}
                _ => {
                    return Err(rejected(format!(
                        "Could not locate an unchanged copy of `{}` on exactly one side of the move.",
                        file.path
                    )));
                }
            }
            write_move_log(&mut prepared.log_file, "Planned cloud path", &file.path)?;
        }
        sync_move_log(&prepared.log_file)?;
        self.check_cancelled()?;
        if !pending.is_empty() {
            let list = run.run_dir.join("move-files.txt");
            write_private_file_under(
                "exact move list",
                &self.inner.state_dir,
                &list,
                pending.as_bytes(),
            )?;
            if first_move {
                let bytes = serde_json::to_vec(manifest).map_err(|err| {
                    rejected(format!("Could not save the planned exclusion moves: {err}"))
                })?;
                write_private_file_under(
                    "exclusion move manifest",
                    &self.inner.state_dir,
                    &run.run_dir.join(MANIFEST),
                    &bytes,
                )?;
            }
            let runner = RecordedMoveRunner {
                migration: self,
                drive,
                run,
                first_move,
            };
            if let Err(err) = drive.request.move_files(source, target, &list, &runner) {
                return Err(self.incomplete_move(
                    &mut prepared.log_file,
                    drive,
                    manifest,
                    &recovery,
                    err,
                ));
            }
            (source_files, target_files) =
                (|| Ok::<_, ManagerError>((list_side(source)?, list_side(target)?)))().map_err(
                    |err| {
                        self.incomplete_move(
                            &mut prepared.log_file,
                            drive,
                            manifest,
                            &recovery,
                            err.cause(),
                        )
                    },
                )?;
        }
        let mut incomplete = Vec::new();
        for file in &manifest.files {
            if !source_files.contains_key(&file.path)
                && target_files
                    .get(&file.path)
                    .is_some_and(|entry| file.matches(entry))
            {
                write_move_log(
                    &mut prepared.log_file,
                    if restore {
                        "Verified returned to Data"
                    } else {
                        "Verified in cloud Recovery"
                    },
                    &file.path,
                )?;
            } else {
                incomplete.push(file.path.as_str());
            }
        }
        sync_move_log(&prepared.log_file)?;
        if !incomplete.is_empty() {
            return Err(rejected(format!(
                "The move could not be verified for {}. See the run log for verified files.",
                incomplete.join(", ")
            )));
        }
        Ok(())
    }

    fn incomplete_move(
        &self,
        log: &mut fs::File,
        drive: &LockedDrive<'_>,
        manifest: &MigrationManifest,
        recovery: &str,
        error: impl std::fmt::Display,
    ) -> ManagerError {
        let verification = ProcessControl::with_timeout(Duration::from_secs(10));
        let runner = ControlledProcessRunner {
            runner: drive.runner.runner,
            control: &verification,
        };
        match drive.request.list_files(recovery, None, &runner) {
            Ok(entries) => {
                let mut retained = Vec::new();
                for entry in &entries {
                    if manifest
                        .files
                        .binary_search_by(|file| file.path.cmp(&entry.path))
                        .is_ok_and(|index| manifest.files[index].matches(entry))
                    {
                        retained.push(entry.path.as_str());
                        let _ = write_move_log(log, "Verified in cloud Recovery", &entry.path);
                    }
                }
                let _ = sync_move_log(log);
                let paths = if retained.is_empty() {
                    "none".to_owned()
                } else {
                    retained.join(", ")
                };
                rejected(format!(
                    "Cloud move stopped: {error}. Verified in Recovery: {paths}."
                ))
            }
            Err(_) => {
                let candidates = manifest
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = write_move_log(log, "Unverified cloud move paths", &candidates);
                let _ = sync_move_log(log);
                rejected(format!(
                    "Cloud move stopped: {error}. Check these paths in {recovery}: {candidates}."
                ))
            }
        }
    }

    fn rebaseline(
        &self,
        old: &GenerationRecord,
        exclusions: Vec<String>,
        drive: Option<&LockedDrive<'_>>,
    ) -> Result<(), ManagerError> {
        self.phase(FilterMigrationPhase::Rebaseline)?;
        let generation = self.create_generation(old, exclusions)?;
        let prepared = self.prepare(
            &generation.id,
            RunPurpose::Bisync(RunMode::Initialize(InitPreference::Path1)),
        )?;
        let plan = prepared.plan.clone();
        let mapping = prepared.mapping.clone();
        self.bisync(prepared, "Re-baseline", drive)?;
        self.phase(FilterMigrationPhase::Verifying)?;
        self.verify_base(&mapping, &generation)?;
        self.check_trees(plan, &generation.filters_file_path, drive)?;
        self.release(Some(&generation), "Verified the narrower sync generation.")
    }

    fn create_generation(
        &self,
        old: &GenerationRecord,
        exclusions: Vec<String>,
    ) -> Result<GenerationRecord, ManagerError> {
        let guard = self.inner.state.lock().unwrap();
        self.check_cancelled()?;
        validate_safe_path_id("mapping_id", self.mapping_id.as_str())?;
        let parent = self
            .inner
            .state_dir
            .join("mappings")
            .join(self.mapping_id.as_str());
        let mut number = guard.store.next_generation_number(self.mapping_id)?;
        let dir = loop {
            let dir = parent.join(format!("generation-{number}"));
            reject_symlinks_under_root("new generation", &self.inner.state_dir, &dir)?;
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => {
                    number += 1
                }
                Err(err) => {
                    return Err(rejected(format!(
                        "Could not reserve the new sync generation: {err}"
                    )));
                }
            }
        };
        let mut config = old.config.clone();
        config.effective_filters = exclusions.into_iter().collect();
        let generation = GenerationRecord {
            id: GenerationId::new(format!("{}-gen-{number}", self.mapping_id)),
            mapping_id: self.mapping_id.clone(),
            generation_number: number,
            fingerprint: config.fingerprint().to_hex(),
            workdir_path: ensure_private_dir_under(
                "new workdir",
                &self.inner.state_dir,
                &dir.join("bisync"),
            )?,
            filters_file_path: dir.join("filters.txt"),
            initial_direction: Some("path1".into()),
            config,
            created_at: self.inner.clock.now_rfc3339(),
        };
        write_private_file_under(
            "generation filters",
            &self.inner.state_dir,
            &generation.filters_file_path,
            render_filters_file(&generation.config.effective_filters).as_bytes(),
        )?;
        write_binding_marker(&self.inner.state_dir, &generation)?;
        guard.store.insert_generation(&generation)?;
        Ok(generation)
    }

    fn release(
        &self,
        generation: Option<&GenerationRecord>,
        note: &str,
    ) -> Result<(), ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        self.check_cancelled()?;
        let mut mapping = lookup_mapping(&guard.store, self.mapping_id)?;
        if let Some(generation) = generation {
            mapping.active_generation_id = Some(generation.id.clone());
            mapping.exclusions = generation
                .config
                .effective_filters
                .iter()
                .cloned()
                .collect();
        }
        mapping.state = apply_event(
            mapping.state,
            ObservedEvent::RecoveryCompleted {
                next_readiness: sync_core::PostRecoveryReadiness::Ready,
            },
        )
        .state;
        mapping.state.execution = Execution::Idle;
        let now = self.inner.clock.now_rfc3339();
        mapping.updated_at = now.clone();
        guard
            .store
            .resolve_recovery_and_update_mapping(&mapping, &now, note)?;
        guard.schedules.remove(self.mapping_id);
        Ok(())
    }
}

fn write_move_log(log: &mut fs::File, label: &str, path: &str) -> Result<(), ManagerError> {
    let line = serde_json::json!({ "stream": "stdout", "raw": format!("{label}: {path}"), "truncated": false });
    writeln!(log, "{line}")
        .map_err(|err| rejected(format!("Could not record cloud move evidence: {err}")))
}

fn sync_move_log(log: &fs::File) -> Result<(), ManagerError> {
    log.sync_data()
        .map_err(|err| rejected(format!("Could not save cloud move evidence: {err}")))
}

struct RecordedMoveRunner<'a> {
    migration: &'a Migration<'a>,
    drive: &'a LockedDrive<'a>,
    run: &'a RunRecord,
    first_move: bool,
}

impl ProcessRunner for RecordedMoveRunner<'_> {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        let inner = self.migration.inner;
        let mut guard = inner.state.lock().unwrap();
        self.migration.control.check()?;
        if launch_cancellation(&guard, &self.run.id).is_some() {
            return Err(ProcessError::Canceled);
        }
        if self.first_move {
            self.migration
                .persist_hold(&mut guard, self.run)
                .map_err(|error| ProcessError::Spawn {
                    program: spec.program.clone(),
                    source: io::Error::other(error),
                })?;
        }
        let handle = self.drive.runner.spawn(spec, options)?;
        let engine = self.drive.request.engine().into();
        let saved = guard
            .store
            .update_run_child_identity(&self.run.id, handle.identity(), &engine)
            .and_then(|()| {
                if self.first_move {
                    publish_updated_snapshot(inner, &mut guard)?;
                }
                Ok(())
            });
        if let Err(error) = saved {
            drop(guard);
            let _ = handle.stop_handle().force_stop(Duration::ZERO);
            let _ = handle.wait();
            return Err(ProcessError::Spawn {
                program: spec.program.clone(),
                source: io::Error::other(error),
            });
        }
        Ok(handle)
    }
}

pub(super) fn run_job(
    inner: &Arc<ManagerInner>,
    mapping_id: MappingId,
    job: FilterJob,
    control: ProcessControl,
) {
    let migration = Migration {
        inner,
        mapping_id: &mapping_id,
        control: &control,
    };
    let result = match job {
        FilterJob::Apply {
            generation_id,
            exclusions,
        } => migration.apply(generation_id, exclusions),
        FilterJob::Finish => migration.recover(false),
        FilterJob::Restore => migration.recover(true),
    };
    let mut guard = inner.state.lock().unwrap();
    if let Err(err) = result {
        let now = inner.clock.now_rfc3339();
        let save = (|| {
            let mut mapping = lookup_mapping(&guard.store, &mapping_id)?;
            if matches!(
                mapping.state.execution,
                Execution::Queued | Execution::Running | Execution::Stopping
            ) {
                mapping.state.execution = Execution::Idle;
            }
            mapping.updated_at = now.clone();
            if mapping.last_run_outcome == Some(RunOutcome::Success) {
                mapping.last_run_outcome = Some(if control.check().is_err() {
                    RunOutcome::Interrupted
                } else {
                    RunOutcome::UnknownFailure
                });
                mapping.last_run_completed_at = Some(now.clone());
            }
            mapping.last_run_summary_text = Some(err.cause());
            guard.store.update_mapping_runtime(&mapping)
        })();
        if let Err(error) = save {
            record_terminal_db_error_locked(&mut guard, &mapping_id, error);
        }
        inner
            .event_bus
            .publish_event(ManagerEvent::PreflightFailed {
                mapping_id: mapping_id.clone(),
                cause: err.cause(),
                next_step: err.next_step(),
                timestamp: now,
            });
    }
    guard.active_filter_change = None;
    guard.active_run = None;
    guard.worker_busy = false;
    if let Err(error) = publish_updated_snapshot(inner, &mut guard) {
        record_terminal_db_error_locked(&mut guard, &mapping_id, error);
    }
    inner.idle_cv.notify_all();
}
