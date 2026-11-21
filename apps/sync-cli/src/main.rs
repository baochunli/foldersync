use std::{
    env,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use sync_core::{MappingId, Readiness, RunId, RunOutcome};
use sync_manager::{
    AddMappingCommand, InitializeCommand, ManagerConfig, ManagerError, MappingStatusSnapshot,
    SyncManager,
};

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(600);
const LOCAL_FIXTURE_ACCOUNT_ID: &str = "local-fixture";

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    if raw_args.is_empty() {
        print_version();
        return ExitCode::SUCCESS;
    }

    // Allow leading `--state-dir <dir>` or `--finder-dir <dir>` before the subcommand.
    let mut prefix_flags = Vec::new();
    let mut idx = 0;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "--state-dir" | "--finder-dir" => {
                prefix_flags.push(raw_args[idx].clone());
                if let Some(val) = raw_args.get(idx + 1) {
                    prefix_flags.push(val.clone());
                    idx += 2;
                } else {
                    eprintln!("Missing value for {}.\nRun sync-cli --help.", raw_args[idx]);
                    return ExitCode::from(2);
                }
            }
            _ => break,
        }
    }
    let Some(cmd) = raw_args.get(idx).map(String::as_str) else {
        eprintln!("Invalid arguments. Run sync-cli --help.");
        return ExitCode::from(2);
    };
    let mut rest = prefix_flags;
    rest.extend_from_slice(&raw_args[idx + 1..]);

    match cmd {
        "version" | "--version" | "-V" if rest.is_empty() => {
            print_version();
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" if rest.is_empty() => {
            print_help();
            ExitCode::SUCCESS
        }
        "status" => run_cmd(cmd_status(&rest)),
        "add-mapping" => run_cmd(cmd_add_mapping(&rest)),
        "init" => run_cmd(cmd_init(&rest)),
        "run-once" => run_cmd(cmd_run_once(&rest)),
        "pause" => run_cmd(cmd_pause(&rest)),
        "resume" => run_cmd(cmd_resume(&rest)),
        _ => {
            eprintln!("Invalid arguments. Run sync-cli --help.");
            ExitCode::from(2)
        }
    }
}

fn run_cmd(res: Result<(), CliError>) -> ExitCode {
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            eprintln!("{msg}\nRun sync-cli --help.");
            ExitCode::from(2)
        }
        Err(CliError::Manager(err)) => {
            eprintln!("Error: {}\nNext step: {}", err.cause(), err.next_step());
            ExitCode::from(1)
        }
        Err(CliError::Failed(msg)) => {
            eprintln!("Error: {msg}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    Manager(ManagerError),
    Failed(String),
}

impl From<ManagerError> for CliError {
    fn from(value: ManagerError) -> Self {
        Self::Manager(value)
    }
}

fn print_version() {
    println!("FolderSync {}", env!("CARGO_PKG_VERSION"));
}

fn print_help() {
    println!(
        "\
Usage: sync-cli <command> [options]

Commands:
  version                                 Print FolderSync version
  status --state-dir <dir> [--json]       Show current mapping status snapshot
  add-mapping --state-dir <dir> [options] Add a local-to-local fixture mapping
    --id <mapping-id>
    --local-dir <path>
    --remote-dir <path>
    --recovery-dir <path>
    [--remote-recovery-dir <path>]
    [--collection <path>]
    [--max-delete-percent <1-100>]
  init <mapping> --state-dir <dir>        Initialize a mapping (local-to-local)
  run-once <mapping> --state-dir <dir>    Run one manual synchronization pass
  pause <mapping> --state-dir <dir>       Persist pause intent for a mapping
  resume <mapping> --state-dir <dir>      Clear pause intent for a mapping

Options:
  --state-dir <dir>                       Required state directory for fixture commands
  --finder-dir <dir>                      Override Finder status.json directory
  --help, -h                              Show this help message
  --version, -V                           Print version"
    );
}

fn open_manager(
    state_dir: Option<PathBuf>,
    finder_dir: Option<PathBuf>,
) -> Result<SyncManager, CliError> {
    let state_dir = match state_dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => {
            return Err(CliError::Usage(
                "Missing required non-empty --state-dir <dir> (fixture CLI never uses the default application state directory)."
                    .to_string(),
            ));
        }
    };
    let mut cfg = ManagerConfig::with_state_dir(state_dir);
    cfg.finder_status_dir = finder_dir;
    Ok(SyncManager::open(cfg)?)
}

fn take_value<'a>(args: &'a [String], idx: &mut usize, flag: &str) -> Result<&'a str, CliError> {
    *idx += 1;
    args.get(*idx)
        .map(String::as_str)
        .ok_or_else(|| CliError::Usage(format!("Missing value for {flag}.")))
}

fn cmd_status(args: &[String]) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut finder_dir = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--finder-dir" => {
                finder_dir = Some(PathBuf::from(take_value(args, &mut i, "--finder-dir")?))
            }
            "--json" => json = true,
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }

    let mgr = open_manager(state_dir, finder_dir)?;
    let snap = mgr.snapshot();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(snap.as_ref()).expect("StatusSnapshot serializes")
        );
    } else if snap.mappings.is_empty() {
        println!("No mappings configured.");
    } else {
        for m in &snap.mappings {
            println!(
                "{} | {:?} | {:?} | {:?} | {}",
                m.id, m.readiness, m.execution, m.intent, m.status_text
            );
        }
    }
    mgr.shutdown()?;
    Ok(())
}

fn cmd_add_mapping(args: &[String]) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut finder_dir = None;
    let mut id: Option<String> = None;
    let mut local_dir: Option<PathBuf> = None;
    let mut remote_dir: Option<PathBuf> = None;
    let mut recovery_dir: Option<PathBuf> = None;
    let mut remote_recovery_dir: Option<PathBuf> = None;
    let mut collection: Option<String> = None;
    let mut max_delete_percent: Option<u8> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--finder-dir" => {
                finder_dir = Some(PathBuf::from(take_value(args, &mut i, "--finder-dir")?))
            }
            "--id" => id = Some(take_value(args, &mut i, "--id")?.to_string()),
            "--local-dir" => {
                local_dir = Some(PathBuf::from(take_value(args, &mut i, "--local-dir")?))
            }
            "--remote-dir" => {
                remote_dir = Some(PathBuf::from(take_value(args, &mut i, "--remote-dir")?))
            }
            "--recovery-dir" => {
                recovery_dir = Some(PathBuf::from(take_value(args, &mut i, "--recovery-dir")?))
            }
            "--remote-recovery-dir" => {
                remote_recovery_dir = Some(PathBuf::from(take_value(
                    args,
                    &mut i,
                    "--remote-recovery-dir",
                )?))
            }
            "--collection" => {
                collection = Some(take_value(args, &mut i, "--collection")?.to_string())
            }
            "--max-delete-percent" => {
                let raw = take_value(args, &mut i, "--max-delete-percent")?;
                let pct = raw.parse::<u8>().map_err(|_| {
                    CliError::Usage(format!("Invalid --max-delete-percent value `{raw}`."))
                })?;
                max_delete_percent = Some(pct);
            }
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }

    let id =
        id.ok_or_else(|| CliError::Usage("Missing required --id <mapping-id>.".to_string()))?;
    let local_dir = local_dir
        .ok_or_else(|| CliError::Usage("Missing required --local-dir <path>.".to_string()))?;
    let remote_dir = remote_dir
        .ok_or_else(|| CliError::Usage("Missing required --remote-dir <path>.".to_string()))?;
    let recovery_dir = recovery_dir
        .ok_or_else(|| CliError::Usage("Missing required --recovery-dir <path>.".to_string()))?;
    let remote_recovery_dir = match remote_recovery_dir {
        Some(dir) => dir,
        None => default_remote_recovery_dir(&recovery_dir)?,
    };
    let collection_path = collection.unwrap_or_else(|| format!("Data/{id}"));

    let mgr = open_manager(state_dir, finder_dir)?;
    mgr.register_local_fixture_account(LOCAL_FIXTURE_ACCOUNT_ID, "Local Fixture")?;
    let mut cmd = AddMappingCommand::local_fixture(
        id.clone(),
        LOCAL_FIXTURE_ACCOUNT_ID,
        collection_path,
        local_dir,
        remote_dir,
        recovery_dir,
        remote_recovery_dir,
    )?;
    if let Some(pct) = max_delete_percent {
        cmd.max_delete_percent = pct;
    }
    let ack = mgr.add_mapping(cmd)?;
    if let Some(m) = ack.snapshot.mappings.iter().find(|m| m.id.as_str() == id) {
        println!("Added mapping `{}` ({}).", m.id, m.status_text);
    }
    mgr.shutdown()?;
    Ok(())
}

fn parse_mapping_and_flags(
    args: &[String],
) -> Result<(String, Option<PathBuf>, Option<PathBuf>), CliError> {
    let mut state_dir = None;
    let mut finder_dir = None;
    let mut mapping_id = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--finder-dir" => {
                finder_dir = Some(PathBuf::from(take_value(args, &mut i, "--finder-dir")?))
            }
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("Unknown argument `{other}`.")));
            }
            positional => {
                if mapping_id.is_some() {
                    return Err(CliError::Usage(format!(
                        "Unexpected extra argument `{positional}`."
                    )));
                }
                mapping_id = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let id =
        mapping_id.ok_or_else(|| CliError::Usage("Missing <mapping> argument.".to_string()))?;
    Ok((id, state_dir, finder_dir))
}

fn default_remote_recovery_dir(local_recovery_dir: &Path) -> Result<PathBuf, CliError> {
    let file_name = local_recovery_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            CliError::Usage(format!(
                "Cannot derive sibling remote recovery directory from `{}`; pass --remote-recovery-dir explicitly.",
                local_recovery_dir.display()
            ))
        })?;
    let sibling_name = format!("{file_name}-remote");
    match local_recovery_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent.join(sibling_name)),
        _ => Ok(PathBuf::from(sibling_name)),
    }
}

fn run_succeeded(readiness: Readiness, outcome: Option<RunOutcome>) -> bool {
    readiness == Readiness::Ready
        && matches!(
            outcome,
            Some(RunOutcome::Success | RunOutcome::SuccessWithConflicts)
        )
}

fn verify_new_run_succeeded(
    mgr: &SyncManager,
    mapping_id: &MappingId,
    prev_run_id: Option<&RunId>,
    mapping: &MappingStatusSnapshot,
) -> Result<(), String> {
    let Some(new_run_id) = mapping.last_run_id.as_ref() else {
        return Err(format!(
            "No new run was recorded for `{}` (readiness={:?}): {}",
            mapping.id, mapping.readiness, mapping.status_text
        ));
    };
    if prev_run_id == Some(new_run_id) {
        return Err(format!(
            "No new run completed for `{}` (still pointing at previous run `{}`; readiness={:?}): {}",
            mapping.id, new_run_id, mapping.readiness, mapping.status_text
        ));
    }
    let runs = mgr
        .list_runs(mapping_id)
        .map_err(|err| format!("Failed to read run record `{new_run_id}`: {}", err.cause()))?;
    let run = runs
        .iter()
        .find(|r| &r.id == new_run_id)
        .ok_or_else(|| format!("Recorded run `{new_run_id}` was not found in SQLite."))?;
    let ok_outcome = matches!(
        run.outcome_kind.as_deref(),
        Some("success" | "success_with_conflicts")
    );
    if mapping.readiness == Readiness::Ready
        && run.status == "completed"
        && ok_outcome
        && run_succeeded(mapping.readiness, mapping.last_run_outcome)
    {
        Ok(())
    } else {
        Err(format!(
            "Run `{}` for `{}` did not succeed (status={}, outcome={:?}, readiness={:?}): {}",
            run.id,
            mapping.id,
            run.status,
            run.outcome_kind,
            mapping.readiness,
            run.outcome_detail
                .as_deref()
                .unwrap_or(mapping.status_text.as_str())
        ))
    }
}

fn cmd_init(args: &[String]) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mapping_id = MappingId::new(id.clone());
    let mgr = open_manager(state_dir, finder_dir)?;
    let prev_run_id = mgr.last_run_id(&mapping_id);
    mgr.initialize(InitializeCommand {
        mapping_id: mapping_id.clone(),
        expected_direction: None,
    })?;
    let snap = mgr.wait_until_idle(DEFAULT_WAIT_TIMEOUT)?;
    let mapping = snap
        .mappings
        .iter()
        .find(|m| m.id == mapping_id)
        .ok_or_else(|| CliError::Failed(format!("Mapping `{id}` disappeared after init.")))?
        .clone();
    let verification = verify_new_run_succeeded(&mgr, &mapping_id, prev_run_id.as_ref(), &mapping);
    mgr.shutdown()?;

    match verification {
        Ok(()) => {
            println!("Initialized `{}`: {}", mapping.id, mapping.status_text);
            Ok(())
        }
        Err(msg) => Err(CliError::Failed(format!(
            "Initialization for `{}` failed: {msg}",
            mapping.id
        ))),
    }
}

fn cmd_run_once(args: &[String]) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mapping_id = MappingId::new(id.clone());
    let mgr = open_manager(state_dir, finder_dir)?;
    let prev_run_id = mgr.last_run_id(&mapping_id);
    mgr.run_now(mapping_id.clone())?;
    let snap = mgr.wait_until_idle(DEFAULT_WAIT_TIMEOUT)?;
    let mapping = snap
        .mappings
        .iter()
        .find(|m| m.id == mapping_id)
        .ok_or_else(|| CliError::Failed(format!("Mapping `{id}` disappeared after run.")))?
        .clone();
    let verification = verify_new_run_succeeded(&mgr, &mapping_id, prev_run_id.as_ref(), &mapping);
    mgr.shutdown()?;

    match verification {
        Ok(()) => {
            println!(
                "Run completed for `{}`: {}",
                mapping.id, mapping.status_text
            );
            Ok(())
        }
        Err(msg) => Err(CliError::Failed(msg)),
    }
}

fn cmd_pause(args: &[String]) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mgr = open_manager(state_dir, finder_dir)?;
    let ack = mgr.pause(id.clone())?;
    if let Some(m) = ack.snapshot.mappings.iter().find(|m| m.id.as_str() == id) {
        println!("Paused `{}`: {}", m.id, m.status_text);
    }
    mgr.shutdown()?;
    Ok(())
}

fn cmd_resume(args: &[String]) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mgr = open_manager(state_dir, finder_dir)?;
    let ack = mgr.resume(id.clone())?;
    if let Some(m) = ack.snapshot.mappings.iter().find(|m| m.id.as_str() == id) {
        println!("Resumed `{}`: {}", m.id, m.status_text);
    }
    mgr.shutdown()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_remote_recovery_dir_is_disjoint_sibling() {
        let local = Path::new("/tmp/work/local_recovery");
        let remote = default_remote_recovery_dir(local).unwrap();
        assert_eq!(remote, PathBuf::from("/tmp/work/local_recovery-remote"));
        assert!(!remote.starts_with(local));
        assert!(!local.starts_with(&remote));
    }

    #[test]
    fn run_succeeded_rejects_retryable_and_other_failure_outcomes() {
        assert!(run_succeeded(Readiness::Ready, Some(RunOutcome::Success)));
        assert!(run_succeeded(
            Readiness::Ready,
            Some(RunOutcome::SuccessWithConflicts)
        ));
        assert!(!run_succeeded(
            Readiness::Ready,
            Some(RunOutcome::RetryableFailure)
        ));
        assert!(!run_succeeded(Readiness::Ready, None));
        assert!(!run_succeeded(
            Readiness::RecoveryRequired,
            Some(RunOutcome::CriticalFailure)
        ));
    }
}
