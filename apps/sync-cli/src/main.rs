use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use rclone_adapter::{ProcessRunner, RealProcessRunner};
use sync_core::{AccountId, MappingId, Readiness, RunId, RunOutcome};
use sync_manager::{
    AccountError, AccountService, AccountServiceConfig, AddDriveAccountRequest, AddMappingCommand,
    CollectionSkipReason, FolderSizeStatus, ImportAccountRequest, InitializeCommand, ManagerConfig,
    ManagerError, MappingStatusSnapshot, PasswordCommandFactory, RESERVED_LOCAL_FIXTURE_ACCOUNT_ID,
    SecretStore, SecretStoreError, SecretString, SyncManager, SystemSecretStore,
    run_credential_helper,
};

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(600);
const LOCAL_FIXTURE_ACCOUNT_ID: &str = RESERVED_LOCAL_FIXTURE_ACCOUNT_ID;
const EXIT_AUTH_REQUIRED: u8 = 3;

struct CliRuntime {
    secret_store: Arc<dyn SecretStore>,
    process_runner: Arc<dyn ProcessRunner>,
    rclone_binary_override: Option<PathBuf>,
    password_command_factory: Option<PasswordCommandFactory>,
    echo_auth_url_to_process_stdout: bool,
}

impl CliRuntime {
    fn production() -> Self {
        Self {
            secret_store: Arc::new(SystemSecretStore),
            process_runner: Arc::new(RealProcessRunner),
            rclone_binary_override: None,
            password_command_factory: None,
            echo_auth_url_to_process_stdout: true,
        }
    }

    fn account_config(&self, state_dir: PathBuf) -> AccountServiceConfig {
        let mut cfg = AccountServiceConfig::with_state_dir(state_dir);
        cfg.secret_store = Arc::clone(&self.secret_store);
        cfg.process_runner = Arc::clone(&self.process_runner);
        cfg.rclone_binary_override = self.rclone_binary_override.clone();
        cfg.password_command_factory = self.password_command_factory.clone();
        cfg
    }

    fn account_service(&self, state_dir: Option<PathBuf>) -> Result<AccountService, CliError> {
        Ok(AccountService::new(
            self.account_config(require_state_dir(state_dir)?),
        )?)
    }

    fn open_manager(
        &self,
        state_dir: Option<PathBuf>,
        finder_dir: Option<PathBuf>,
    ) -> Result<SyncManager, CliError> {
        let state_dir = require_state_dir(state_dir)?;
        let mut cfg = ManagerConfig::with_state_dir(&state_dir);
        cfg.finder_status_dir = finder_dir;
        Ok(SyncManager::open_with_account_config(
            cfg,
            self.account_config(state_dir),
        )?)
    }
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let rt = CliRuntime::production();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    dispatch(&rt, &raw_args, &mut stdout, &mut stderr)
}

fn dispatch(
    rt: &CliRuntime,
    raw_args: &[String],
    out: &mut dyn Write,
    err_out: &mut dyn Write,
) -> ExitCode {
    if raw_args.is_empty() {
        let _ = print_version(out);
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
                    let _ = writeln!(
                        err_out,
                        "Missing value for {}.\nRun sync-cli --help.",
                        raw_args[idx]
                    );
                    return ExitCode::from(2);
                }
            }
            _ => break,
        }
    }
    let Some(cmd) = raw_args.get(idx).map(String::as_str) else {
        let _ = writeln!(err_out, "Invalid arguments. Run sync-cli --help.");
        return ExitCode::from(2);
    };
    let mut rest = prefix_flags;
    rest.extend_from_slice(&raw_args[idx + 1..]);

    match cmd {
        "version" | "--version" | "-V" if rest.is_empty() => {
            let _ = print_version(out);
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" if rest.is_empty() => {
            let _ = print_help(out);
            ExitCode::SUCCESS
        }
        "status" => run_cmd(cmd_status(rt, &rest, out), err_out),
        "collections" => run_cmd(cmd_collections(rt, &rest, out), err_out),
        "add-mapping" => run_cmd(cmd_add_mapping(rt, &rest, out), err_out),
        "init" => run_cmd(cmd_init(rt, &rest, out), err_out),
        "run-once" => run_cmd(cmd_run_once(rt, &rest, out), err_out),
        "pause" => run_cmd(cmd_pause(rt, &rest, out), err_out),
        "resume" => run_cmd(cmd_resume(rt, &rest, out), err_out),
        "scheduling" => run_cmd(cmd_scheduling(rt, &rest, out), err_out),
        "credential-helper" => run_cmd(cmd_credential_helper(rt, &rest, out), err_out),
        "account" => run_cmd(cmd_account(rt, &rest, out), err_out),
        "remote" => run_cmd(cmd_remote(rt, &rest, out), err_out),
        _ => {
            let _ = writeln!(err_out, "Invalid arguments. Run sync-cli --help.");
            ExitCode::from(2)
        }
    }
}

fn run_cmd(res: Result<(), CliError>, err_out: &mut dyn Write) -> ExitCode {
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            let _ = writeln!(err_out, "{msg}\nRun sync-cli --help.");
            ExitCode::from(2)
        }
        Err(CliError::AuthRequired(msg)) => {
            let _ = writeln!(err_out, "authentication_required: {msg}");
            ExitCode::from(EXIT_AUTH_REQUIRED)
        }
        Err(CliError::Manager(err)) => {
            let _ = writeln!(
                err_out,
                "Error: {}\nNext step: {}",
                err.cause(),
                err.next_step()
            );
            ExitCode::from(1)
        }
        Err(CliError::Account(err)) => {
            if matches!(*err, AccountError::AuthenticationRequired { .. }) {
                let _ = writeln!(
                    err_out,
                    "authentication_required: {}\nNext step: {}",
                    err.cause(),
                    err.next_step()
                );
                ExitCode::from(EXIT_AUTH_REQUIRED)
            } else {
                let _ = writeln!(
                    err_out,
                    "Error: {}\nNext step: {}",
                    err.cause(),
                    err.next_step()
                );
                ExitCode::from(1)
            }
        }
        Err(CliError::Failed(msg)) => {
            let _ = writeln!(err_out, "Error: {msg}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    AuthRequired(String),
    Manager(Box<ManagerError>),
    Account(Box<AccountError>),
    Failed(String),
}

impl From<ManagerError> for CliError {
    fn from(value: ManagerError) -> Self {
        Self::Manager(Box::new(value))
    }
}

impl From<AccountError> for CliError {
    fn from(value: AccountError) -> Self {
        Self::Account(Box::new(value))
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        Self::Failed(format!("I/O error: {value}"))
    }
}

fn print_version(out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "FolderSync {}", env!("CARGO_PKG_VERSION"))
}

fn print_help(out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "\
Usage: sync-cli <command> [options]

Commands:
  version                                      Print FolderSync version
  status --state-dir <dir> [--json]            Show current mapping status snapshot
  collections list --state-dir <dir> --account <id> [--json]
  collections size --state-dir <dir> --account <id> --folder-id <id> [--json]
  add-mapping --state-dir <dir> --account <id> --folder-id <id>
    --local <dir> --local-recovery <dir> [--id <mapping-id>] [--max-delete-percent <1-100>]
  add-mapping --state-dir <dir> [options]      Add a local-to-local fixture mapping
    --id <mapping-id>
    --local-dir <path>
    --remote-dir <path>
    --recovery-dir <path>
    [--remote-recovery-dir <path>]
    [--collection <path>]
    [--max-delete-percent <1-100>]
  init <mapping> --state-dir <dir>             Initialize a mapping
  run-once <mapping> --state-dir <dir>         Run one manual synchronization pass
  pause <mapping> --state-dir <dir>            Persist pause intent for a mapping
  resume <mapping> --state-dir <dir>           Clear pause intent for a mapping
  scheduling pause|resume --state-dir <dir>  Pause or resume automatic scheduling

  credential-helper --account <id>             Print account rclone config passphrase on stdout
  account add-drive --state-dir <dir> [opts]   Create encrypted Drive config and authorize
    --id <account-id>
    --root-folder-id <drive-folder-id>
    --client-id <oauth-client-id>
    (--client-secret-file <path> | --client-secret-stdin | env FOLDERSYNC_OAUTH_CLIENT_SECRET)
    [--display-name <name>]
    [--remote <name>]                          (default: household)
    [--scope <scope>]                          (default: drive)
  account import --state-dir <dir> [opts]      Import ONE remote into encrypted app config
    --from <rclone-config-file>
    --remote <source-remote-name>
    [--id <account-id>]
    [--as <target-remote-name>]
    [--display-name <name>]
    [--root-folder-id <id>]
    [--allow-global-config]
  account list --state-dir <dir> [--json]      List configured accounts
  account reconnect --state-dir <dir> [opts]   Resume or refresh Drive OAuth for an account
    --account <id> (or positional <id>)
  account check --state-dir <dir> [opts]       Verify encrypted config and lsjson parent root
    --account <id> (or positional <id>) [--json]
  remote resolve <path> --state-dir <dir>      Resolve cloud folder component IDs via lsjson
    --account <id> [--expected-id <folder-id>] [--json]

Options:
  --state-dir <dir>                            Required state directory for all stateful commands
  --finder-dir <dir>                           Override Finder status.json directory
  --help, -h                                   Show this help message
  --version, -V                                Print version"
    )
}

fn require_state_dir(state_dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
    match state_dir {
        Some(dir) if !dir.as_os_str().is_empty() => Ok(dir),
        _ => Err(CliError::Usage(
            "Missing required non-empty --state-dir <dir> (sync-cli never uses the default application state directory)."
                .to_string(),
        )),
    }
}

fn take_value<'a>(args: &'a [String], idx: &mut usize, flag: &str) -> Result<&'a str, CliError> {
    *idx += 1;
    args.get(*idx)
        .map(String::as_str)
        .ok_or_else(|| CliError::Usage(format!("Missing value for {flag}.")))
}

fn split_subcommand(args: &[String]) -> Result<(String, Vec<String>), CliError> {
    let mut leading = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" | "--finder-dir" => {
                let flag = args[i].as_str();
                leading.push(args[i].clone());
                let val = take_value(args, &mut i, flag)?;
                leading.push(val.to_string());
                i += 1;
            }
            _ => break,
        }
    }
    let Some(sub) = args.get(i) else {
        return Err(CliError::Usage("Missing subcommand.".to_string()));
    };
    let mut rest = leading;
    rest.extend_from_slice(&args[i + 1..]);
    Ok((sub.clone(), rest))
}

fn cmd_credential_helper(
    rt: &CliRuntime,
    args: &[String],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut account_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--account" => {
                account_id = Some(take_value(args, &mut i, "--account")?.to_string());
            }
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }
    let account_id = account_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CliError::Usage("Missing required --account <id>.".to_string()))?;

    run_credential_helper(rt.secret_store.as_ref(), &account_id, out).map_err(|err| match err {
        SecretStoreError::AuthenticationRequired { reason } => CliError::AuthRequired(reason),
        SecretStoreError::InvalidAccountId(id) => {
            CliError::Usage(format!("Invalid account identifier `{id}`."))
        }
        SecretStoreError::NotFound { account_id } => {
            CliError::Failed(format!("No credential stored for account `{account_id}`."))
        }
        SecretStoreError::AlreadyExists { account_id } => CliError::Failed(format!(
            "Credential already exists for account `{account_id}`."
        )),
        SecretStoreError::Backend { reason } => CliError::Failed(reason),
    })
}

fn cmd_account(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (sub, rest) = split_subcommand(args)?;
    match sub.as_str() {
        "add-drive" => cmd_account_add_drive(rt, &rest, out),
        "reconnect" => cmd_account_reconnect(rt, &rest, out),
        "import" => cmd_account_import(rt, &rest, out),
        "list" => cmd_account_list(rt, &rest, out),
        "check" => cmd_account_check(rt, &rest, out),
        other => Err(CliError::Usage(format!(
            "Unknown account subcommand `{other}` (expected add-drive, reconnect, import, list, or check)."
        ))),
    }
}

fn read_client_secret(
    secret_file: Option<&Path>,
    from_stdin: bool,
) -> Result<SecretString, CliError> {
    let raw = if let Some(path) = secret_file {
        fs::read_to_string(path).map_err(|err| {
            CliError::Failed(format!(
                "Failed to read OAuth client secret file `{}`: {err}",
                path.display()
            ))
        })?
    } else if from_stdin {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).map_err(|err| {
            CliError::Failed(format!(
                "Failed to read OAuth client secret from stdin: {err}"
            ))
        })?;
        buf
    } else if let Ok(val) = env::var("FOLDERSYNC_OAUTH_CLIENT_SECRET") {
        val
    } else {
        return Err(CliError::Usage(
            "Missing OAuth client secret: pass --client-secret-file <path>, --client-secret-stdin, or set FOLDERSYNC_OAUTH_CLIENT_SECRET."
                .to_string(),
        ));
    };

    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(CliError::Usage(
            "OAuth client secret must not be empty.".to_string(),
        ));
    }
    Ok(SecretString::new(trimmed))
}

fn cmd_account_add_drive(
    rt: &CliRuntime,
    args: &[String],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut id: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut remote_name: Option<String> = None;
    let mut root_folder_id: Option<String> = None;
    let mut client_id: Option<String> = None;
    let mut client_secret_file: Option<PathBuf> = None;
    let mut client_secret_stdin = false;
    let mut scope: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--id" => id = Some(take_value(args, &mut i, "--id")?.to_string()),
            "--display-name" => {
                display_name = Some(take_value(args, &mut i, "--display-name")?.to_string())
            }
            "--remote" => remote_name = Some(take_value(args, &mut i, "--remote")?.to_string()),
            "--root-folder-id" => {
                root_folder_id = Some(take_value(args, &mut i, "--root-folder-id")?.to_string())
            }
            "--client-id" => client_id = Some(take_value(args, &mut i, "--client-id")?.to_string()),
            "--client-secret-file" => {
                client_secret_file = Some(PathBuf::from(take_value(
                    args,
                    &mut i,
                    "--client-secret-file",
                )?))
            }
            "--client-secret-stdin" => client_secret_stdin = true,
            "--scope" => scope = Some(take_value(args, &mut i, "--scope")?.to_string()),
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }

    let svc = rt.account_service(state_dir)?;
    let id =
        id.ok_or_else(|| CliError::Usage("Missing required --id <account-id>.".to_string()))?;
    let root_folder_id = root_folder_id.ok_or_else(|| {
        CliError::Usage("Missing required --root-folder-id <drive-folder-id>.".to_string())
    })?;
    let client_id = client_id.ok_or_else(|| {
        CliError::Usage("Missing required --client-id <oauth-client-id>.".to_string())
    })?;
    let client_secret = read_client_secret(client_secret_file.as_deref(), client_secret_stdin)?;

    let captured_urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured_cb = Arc::clone(&captured_urls);
    let echo_stdout = rt.echo_auth_url_to_process_stdout;
    let result = svc.add_drive_account(
        AddDriveAccountRequest {
            id: AccountId::new(id.clone()),
            display_name: display_name.unwrap_or_else(|| id.clone()),
            remote_name: remote_name.unwrap_or_else(|| "household".to_string()),
            root_folder_id,
            client_id,
            client_secret,
            scope: scope.unwrap_or_else(|| "drive".to_string()),
            run_oauth_flow: true,
        },
        Some(Box::new(move |url| {
            if echo_stdout {
                let _ = writeln!(
                    io::stdout(),
                    "Open this URL in your browser to authorize Google Drive:\n{url}"
                );
                let _ = io::stdout().flush();
            }
            captured_cb.lock().unwrap().push(url.to_string());
        })),
    )?;

    let surfaced_url = result
        .auth_url
        .clone()
        .or_else(|| captured_urls.lock().ok().and_then(|v| v.first().cloned()));
    if let Some(url) = surfaced_url {
        writeln!(out, "Authorization URL: {url}")?;
    }
    writeln!(
        out,
        "Added Drive account `{}` (remote `{}:`, root `{}`): auth_status={}.",
        result.account.id,
        result.account.remote_name,
        result.account.remote_root_folder_id,
        result.account.auth_status
    )?;
    Ok(())
}

fn cmd_account_reconnect(
    rt: &CliRuntime,
    args: &[String],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut account_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--account" => {
                account_id = Some(take_value(args, &mut i, "--account")?.to_string());
            }
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("Unknown argument `{other}`.")));
            }
            positional => {
                if account_id.is_some() {
                    return Err(CliError::Usage(format!(
                        "Unexpected extra argument `{positional}`."
                    )));
                }
                account_id = Some(positional.to_string());
            }
        }
        i += 1;
    }

    let svc = rt.account_service(state_dir)?;
    let account_id = account_id
        .ok_or_else(|| CliError::Usage("Missing required --account <id>.".to_string()))?;

    let captured_urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured_cb = Arc::clone(&captured_urls);
    let echo_stdout = rt.echo_auth_url_to_process_stdout;
    let result = svc.reconnect_drive_account(
        &AccountId::new(account_id),
        Some(Box::new(move |url| {
            if echo_stdout {
                let _ = writeln!(
                    io::stdout(),
                    "Open this URL in your browser to authorize Google Drive:\n{url}"
                );
                let _ = io::stdout().flush();
            }
            captured_cb.lock().unwrap().push(url.to_string());
        })),
    )?;

    let surfaced_url = result
        .auth_url
        .clone()
        .or_else(|| captured_urls.lock().ok().and_then(|v| v.first().cloned()));
    if let Some(url) = surfaced_url {
        writeln!(out, "Authorization URL: {url}")?;
    }
    writeln!(
        out,
        "Reconnected Drive account `{}` (remote `{}:`, root `{}`): auth_status={}.",
        result.account.id,
        result.account.remote_name,
        result.account.remote_root_folder_id,
        result.account.auth_status
    )?;
    Ok(())
}

fn cmd_account_import(
    rt: &CliRuntime,
    args: &[String],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut from_path: Option<PathBuf> = None;
    let mut remote: Option<String> = None;
    let mut id: Option<String> = None;
    let mut target_remote: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut root_folder_id: Option<String> = None;
    let mut allow_global = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--from" => from_path = Some(PathBuf::from(take_value(args, &mut i, "--from")?)),
            "--remote" => remote = Some(take_value(args, &mut i, "--remote")?.to_string()),
            "--id" => id = Some(take_value(args, &mut i, "--id")?.to_string()),
            "--as" => target_remote = Some(take_value(args, &mut i, "--as")?.to_string()),
            "--display-name" => {
                display_name = Some(take_value(args, &mut i, "--display-name")?.to_string())
            }
            "--root-folder-id" => {
                root_folder_id = Some(take_value(args, &mut i, "--root-folder-id")?.to_string())
            }
            "--allow-global-config" => allow_global = true,
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }

    let svc = rt.account_service(state_dir)?;
    let source_config_path =
        from_path.ok_or_else(|| CliError::Usage("Missing required --from <file>.".to_string()))?;
    let source_remote_name =
        remote.ok_or_else(|| CliError::Usage("Missing required --remote <name>.".to_string()))?;
    let account_id = id.unwrap_or_else(|| source_remote_name.clone());
    let target_remote_name = target_remote.unwrap_or_else(|| source_remote_name.clone());

    let account = svc.import_account(ImportAccountRequest {
        id: AccountId::new(account_id.clone()),
        display_name: display_name.unwrap_or_else(|| account_id.clone()),
        source_config_path,
        source_remote_name,
        target_remote_name,
        root_folder_id,
        allow_global_default_source: allow_global,
    })?;

    writeln!(
        out,
        "Imported account `{}` (type={}, remote `{}:`, encrypted config `{}`).",
        account.id,
        account.kind,
        account.remote_name,
        account.config_path.display()
    )?;
    Ok(())
}

fn cmd_account_list(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--json" => json = true,
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }

    let svc = rt.account_service(state_dir)?;
    let accounts = svc.list_accounts()?;
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&accounts).expect("AccountRecord serializes")
        )?;
    } else if accounts.is_empty() {
        writeln!(out, "No accounts configured.")?;
    } else {
        for acct in &accounts {
            writeln!(
                out,
                "{} | {} | {}: | root={} | {}",
                acct.id, acct.kind, acct.remote_name, acct.remote_root_folder_id, acct.auth_status
            )?;
        }
    }
    Ok(())
}

fn cmd_account_check(
    rt: &CliRuntime,
    args: &[String],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut account_id: Option<String> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--account" => {
                account_id = Some(take_value(args, &mut i, "--account")?.to_string());
            }
            "--json" => json = true,
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("Unknown argument `{other}`.")));
            }
            positional => {
                if account_id.is_some() {
                    return Err(CliError::Usage(format!(
                        "Unexpected extra argument `{positional}`."
                    )));
                }
                account_id = Some(positional.to_string());
            }
        }
        i += 1;
    }

    let svc = rt.account_service(state_dir)?;
    let account_id = account_id
        .ok_or_else(|| CliError::Usage("Missing required --account <id>.".to_string()))?;
    let report = svc.check_account(&AccountId::new(account_id))?;

    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report).expect("AccountCheckReport serializes")
        )?;
    } else {
        writeln!(
            out,
            "Account `{}` ({}, `{}:`) verified: {} parent folder(s).",
            report.account.id,
            report.account.kind,
            report.account.remote_name,
            report.parent_directories.len()
        )?;
        for dir in &report.parent_directories {
            let id_str = dir.id.as_deref().unwrap_or("(no backend ID)");
            writeln!(out, "  {} | id={}", dir.name, id_str)?;
        }
        for dup in &report.duplicate_directories {
            writeln!(
                out,
                "  WARNING: duplicate parent folder `{}` (IDs: {:?})",
                dup.name, dup.matching_ids
            )?;
        }
    }
    Ok(())
}

fn cmd_remote(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (sub, rest) = split_subcommand(args)?;
    match sub.as_str() {
        "resolve" => cmd_remote_resolve(rt, &rest, out),
        other => Err(CliError::Usage(format!(
            "Unknown remote subcommand `{other}` (expected resolve)."
        ))),
    }
}

fn cmd_remote_resolve(
    rt: &CliRuntime,
    args: &[String],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut account_id: Option<String> = None;
    let mut path_arg: Option<String> = None;
    let mut expected_id: Option<String> = None;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(args, &mut i, "--state-dir")?))
            }
            "--account" => {
                account_id = Some(take_value(args, &mut i, "--account")?.to_string());
            }
            "--expected-id" => {
                expected_id = Some(take_value(args, &mut i, "--expected-id")?.to_string());
            }
            "--json" => json = true,
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("Unknown argument `{other}`.")));
            }
            positional => {
                if path_arg.is_some() {
                    return Err(CliError::Usage(format!(
                        "Unexpected extra argument `{positional}`."
                    )));
                }
                path_arg = Some(positional.to_string());
            }
        }
        i += 1;
    }

    let svc = rt.account_service(state_dir)?;
    let account_id = account_id
        .ok_or_else(|| CliError::Usage("Missing required --account <id>.".to_string()))?;
    let path_arg =
        path_arg.ok_or_else(|| CliError::Usage("Missing required <path> argument.".to_string()))?;

    let resolved = if let Some(exp_id) = expected_id {
        let collection = sync_core::CloudCollection {
            display_path: path_arg,
            folder_id: exp_id,
            ancestor_ids: Vec::new(),
        };
        svc.verify_collection_identity(&AccountId::new(account_id), &collection)?
    } else {
        svc.resolve_remote_path(&AccountId::new(account_id), &path_arg)?
    };

    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&resolved).expect("ResolvedRemotePath serializes")
        )?;
    } else {
        writeln!(out, "{}:{}", resolved.remote, resolved.display_path)?;
        for comp in &resolved.components {
            let id_str = comp.folder_id.as_deref().unwrap_or("(no backend ID)");
            writeln!(out, "  {} -> {}", comp.path, id_str)?;
        }
    }
    Ok(())
}

fn cmd_status(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
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

    let mgr = rt.open_manager(state_dir, finder_dir)?;
    let snap = mgr.snapshot();
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(snap.as_ref()).expect("StatusSnapshot serializes")
        )?;
    } else if snap.mappings.is_empty() {
        writeln!(out, "No mappings configured.")?;
    } else {
        for m in &snap.mappings {
            writeln!(
                out,
                "{} | {:?} | {:?} | {:?} | {}",
                m.id, m.readiness, m.execution, m.intent, m.status_text
            )?;
        }
    }
    mgr.shutdown()?;
    Ok(())
}

fn cmd_collections(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (sub, args) = split_subcommand(args)?;
    if !matches!(sub.as_str(), "list" | "size") {
        return Err(CliError::Usage(format!(
            "Unknown collections action `{sub}`."
        )));
    }
    let mut state_dir = None;
    let mut finder_dir = None;
    let mut account_id = None;
    let mut folder_id = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(take_value(&args, &mut i, "--state-dir")?))
            }
            "--finder-dir" => {
                finder_dir = Some(PathBuf::from(take_value(&args, &mut i, "--finder-dir")?))
            }
            "--account" => account_id = Some(take_value(&args, &mut i, "--account")?),
            "--folder-id" if sub == "size" => {
                folder_id = Some(take_value(&args, &mut i, "--folder-id")?)
            }
            "--json" => json = true,
            other => return Err(CliError::Usage(format!("Unknown argument `{other}`."))),
        }
        i += 1;
    }
    let account_id = AccountId::new(required_text(account_id, "--account <id>")?);
    let folder_id = if sub == "size" {
        Some(required_text(folder_id, "--folder-id <id>")?)
    } else {
        None
    };
    let mgr = rt.open_manager(state_dir, finder_dir)?;
    let result = (|| {
        if let Some(folder_id) = folder_id {
            let size = mgr.request_folder_size(&account_id, folder_id)?.wait()?;
            let FolderSizeStatus::Known { bytes, files } = size.status else {
                return Err(CliError::Failed(
                    "Folder size could not be verified.".into(),
                ));
            };
            if json {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&size).expect("Folder size serializes")
                )?;
            } else {
                writeln!(out, "{folder_id} | {bytes} bytes | {files} files")?;
            }
        } else {
            let listing = mgr.list_collection_candidates(&account_id)?;
            if json {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&listing).expect("Collection listing serializes")
                )?;
            } else if listing.candidates.is_empty() && listing.skipped.is_empty() {
                writeln!(out, "No collections found under Data.")?;
            } else {
                for candidate in listing.candidates {
                    writeln!(
                        out,
                        "{} | {}",
                        candidate.collection.folder_id, candidate.collection.display_path
                    )?;
                }
                for skipped in listing.skipped {
                    let reason = match skipped.reason {
                        CollectionSkipReason::Duplicate => "duplicate",
                        CollectionSkipReason::UnsupportedName => "unsupported name",
                    };
                    writeln!(out, "Skipped {} ({reason}).", skipped.name)?;
                }
            }
        }
        Ok(())
    })();
    mgr.shutdown()?;
    result
}

fn required_text<'a>(value: Option<&'a str>, flag: &str) -> Result<&'a str, CliError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CliError::Usage(format!("Missing required {flag}.")))
}

fn cmd_add_mapping(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let mut state_dir = None;
    let mut finder_dir = None;
    let mut id: Option<String> = None;
    let mut account_id = None;
    let mut folder_id = None;
    let mut drive_local = None;
    let mut drive_recovery = None;
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
            "--account" => account_id = Some(take_value(args, &mut i, "--account")?),
            "--folder-id" => folder_id = Some(take_value(args, &mut i, "--folder-id")?),
            "--local" => drive_local = Some(take_value(args, &mut i, "--local")?),
            "--local-recovery" => {
                drive_recovery = Some(take_value(args, &mut i, "--local-recovery")?)
            }
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

    let state_dir = require_state_dir(state_dir)?;
    if account_id.is_some()
        || folder_id.is_some()
        || drive_local.is_some()
        || drive_recovery.is_some()
    {
        if local_dir.is_some()
            || remote_dir.is_some()
            || recovery_dir.is_some()
            || remote_recovery_dir.is_some()
            || collection.is_some()
        {
            return Err(CliError::Usage(
                "Drive and local fixture options cannot be combined.".into(),
            ));
        }
        let account_id = AccountId::new(required_text(account_id, "--account <id>")?);
        let folder_id = required_text(folder_id, "--folder-id <id>")?;
        let local = Path::new(required_text(drive_local, "--local <dir>")?);
        let recovery = Path::new(required_text(drive_recovery, "--local-recovery <dir>")?);
        let mgr = rt.open_manager(Some(state_dir), finder_dir)?;
        let result = (|| {
            let mut candidates = mgr
                .list_collection_candidates(&account_id)?
                .candidates
                .into_iter()
                .filter(|candidate| candidate.collection.folder_id == folder_id);
            let candidate = candidates.next()
                .filter(|_| candidates.next().is_none())
                .ok_or_else(|| CliError::Failed(format!(
                    "Folder ID `{folder_id}` is not selectable for `{account_id}`. List the account's collections and choose a current folder ID."
                )))?;
            let id = match id {
                Some(id) => id,
                None => {
                    let snapshot = mgr.snapshot();
                    (1..)
                        .map(|n| format!("drive-{n}"))
                        .find(|id| snapshot.mappings.iter().all(|m| m.id.as_str() != id))
                        .expect("A finite set of mappings cannot exhaust mapping IDs")
                }
            };
            let cmd =
                AddMappingCommand::drive(id, account_id, candidate.collection, local, recovery)?;
            save_mapping(&mgr, cmd, max_delete_percent, out)
        })();
        mgr.shutdown()?;
        return result;
    }

    let id = required_text(id.as_deref(), "--id <mapping-id>")?;
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

    let mgr = rt.open_manager(Some(state_dir), finder_dir)?;
    // Finding 8: Refuse to overwrite an existing non-fixture account with the
    // reserved `local-fixture` ID.
    let acct_svc =
        AccountService::for_locked_manager(AccountServiceConfig::with_state_dir(mgr.state_dir()))?;
    if let Some(existing) = acct_svc.get_account(&AccountId::new(LOCAL_FIXTURE_ACCOUNT_ID))?
        && existing.kind != "local_fixture"
    {
        mgr.shutdown()?;
        return Err(CliError::Failed(format!(
            "Refused to overwrite existing non-fixture account `{LOCAL_FIXTURE_ACCOUNT_ID}` (kind={}).",
            existing.kind
        )));
    }
    mgr.register_local_fixture_account(LOCAL_FIXTURE_ACCOUNT_ID, "Local Fixture")?;
    let cmd = AddMappingCommand::local_fixture(
        id,
        LOCAL_FIXTURE_ACCOUNT_ID,
        collection_path,
        local_dir,
        remote_dir,
        recovery_dir,
        remote_recovery_dir,
    )?;
    save_mapping(&mgr, cmd, max_delete_percent, out)?;
    mgr.shutdown()?;
    Ok(())
}

fn save_mapping(
    mgr: &SyncManager,
    mut cmd: AddMappingCommand,
    max_delete_percent: Option<u8>,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if let Some(pct) = max_delete_percent {
        cmd.max_delete_percent = pct;
    }
    let id = cmd.id.clone();
    let ack = mgr.add_mapping(cmd)?;
    if let Some(m) = ack.snapshot.mappings.iter().find(|m| m.id == id) {
        writeln!(out, "Added mapping `{}` ({}).", m.id, m.status_text)?;
    }
    Ok(())
}

fn cmd_scheduling(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    const EXPECTED_ACTION: &str = "Expected scheduling pause or resume.";
    let (action, state_dir, finder_dir) = parse_positional_and_flags(args, EXPECTED_ACTION)?;
    let paused = match action.as_str() {
        "pause" => true,
        "resume" => false,
        _ => return Err(CliError::Usage(EXPECTED_ACTION.into())),
    };
    let mgr = rt.open_manager(state_dir, finder_dir)?;
    mgr.set_scheduling_paused(paused)?;
    mgr.shutdown()?;
    writeln!(
        out,
        "Automatic scheduling {}.",
        if paused { "paused" } else { "resumed" }
    )?;
    Ok(())
}

fn parse_mapping_and_flags(
    args: &[String],
) -> Result<(String, Option<PathBuf>, Option<PathBuf>), CliError> {
    parse_positional_and_flags(args, "Missing <mapping> argument.")
}

fn parse_positional_and_flags(
    args: &[String],
    missing_positional: &str,
) -> Result<(String, Option<PathBuf>, Option<PathBuf>), CliError> {
    let mut state_dir = None;
    let mut finder_dir = None;
    let mut positional_arg = None;
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
                if positional_arg.is_some() {
                    return Err(CliError::Usage(format!(
                        "Unexpected extra argument `{positional}`."
                    )));
                }
                positional_arg = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let positional_arg =
        positional_arg.ok_or_else(|| CliError::Usage(missing_positional.into()))?;
    Ok((positional_arg, state_dir, finder_dir))
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
        .list_recent_runs(mapping_id, 2)
        .map_err(|err| format!("Failed to read run record `{new_run_id}`: {}", err.cause()))?;
    let run = runs
        .first()
        .filter(|r| &r.id == new_run_id && runs.get(1).map(|r| &r.id) == prev_run_id)
        .ok_or_else(|| {
            format!("Could not identify run `{new_run_id}` as the only new run in SQLite.")
        })?;
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

fn cmd_init(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mapping_id = MappingId::new(id.clone());
    let mgr = rt.open_manager(state_dir, finder_dir)?;
    let ack = mgr.initialize(InitializeCommand {
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
    let prev_run_id = ack
        .snapshot
        .mappings
        .iter()
        .find(|m| m.id == mapping_id)
        .and_then(|m| m.last_run_id.as_ref());
    let verification = verify_new_run_succeeded(&mgr, &mapping_id, prev_run_id, &mapping);
    mgr.shutdown()?;

    match verification {
        Ok(()) => {
            writeln!(out, "Initialized `{}`: {}", mapping.id, mapping.status_text)?;
            Ok(())
        }
        Err(msg) => Err(CliError::Failed(format!(
            "Initialization for `{}` failed: {msg}",
            mapping.id
        ))),
    }
}

fn cmd_run_once(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mapping_id = MappingId::new(id.clone());
    let mgr = rt.open_manager(state_dir, finder_dir)?;
    let ack = mgr.run_now(mapping_id.clone())?;
    let snap = mgr.wait_until_idle(DEFAULT_WAIT_TIMEOUT)?;
    let mapping = snap
        .mappings
        .iter()
        .find(|m| m.id == mapping_id)
        .ok_or_else(|| CliError::Failed(format!("Mapping `{id}` disappeared after run.")))?
        .clone();
    let prev_run_id = ack
        .snapshot
        .mappings
        .iter()
        .find(|m| m.id == mapping_id)
        .and_then(|m| m.last_run_id.as_ref());
    let verification = verify_new_run_succeeded(&mgr, &mapping_id, prev_run_id, &mapping);
    mgr.shutdown()?;

    match verification {
        Ok(()) => {
            writeln!(
                out,
                "Run completed for `{}`: {}",
                mapping.id, mapping.status_text
            )?;
            Ok(())
        }
        Err(msg) => Err(CliError::Failed(msg)),
    }
}

fn cmd_pause(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mgr = rt.open_manager(state_dir, finder_dir)?;
    let ack = mgr.pause(id.clone())?;
    if let Some(m) = ack.snapshot.mappings.iter().find(|m| m.id.as_str() == id) {
        writeln!(out, "Paused `{}`: {}", m.id, m.status_text)?;
    }
    mgr.shutdown()?;
    Ok(())
}

fn cmd_resume(rt: &CliRuntime, args: &[String], out: &mut dyn Write) -> Result<(), CliError> {
    let (id, state_dir, finder_dir) = parse_mapping_and_flags(args)?;
    let mgr = rt.open_manager(state_dir, finder_dir)?;
    let ack = mgr.resume(id.clone())?;
    if let Some(m) = ack.snapshot.mappings.iter().find(|m| m.id.as_str() == id) {
        writeln!(out, "Resumed `{}`: {}", m.id, m.status_text)?;
    }
    mgr.shutdown()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use rclone_adapter::{
        FakeProcessRunner, FakeRunScript, OutputLine, PasswordCommand, StreamKind,
        pinned_binary_path,
    };
    use sync_manager::InMemorySecretStore;
    use tempfile::TempDir;

    fn run_args(rt: &CliRuntime, args: &[&str]) -> (ExitCode, String, String) {
        let vec_args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(rt, &vec_args, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    #[test]
    fn drive_collections_size_and_mapping_use_the_selected_account_and_id() {
        use sync_manager::{AccountRecord, SqliteStateStore, scoped_credential_key};

        let Some(binary) = pinned_binary_path() else {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        };
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let state = root.join("state");
        let local = root.join("local");
        let recovery = root.join("recovery");
        let config = state.join("accounts/drive/rclone.conf");
        fs::create_dir(&local).unwrap();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, b"fixture config").unwrap();
        let runner = Arc::new(FakeProcessRunner::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let rt = CliRuntime {
            secret_store: secrets.clone(),
            process_runner: runner.clone(),
            rclone_binary_override: Some(binary),
            password_command_factory: None,
            echo_auth_url_to_process_stdout: false,
        };
        let state_arg = state.to_str().unwrap();
        assert_eq!(
            run_args(&rt, &["--state-dir", state_arg, "scheduling", "pause"]).0,
            ExitCode::SUCCESS
        );
        let store = SqliteStateStore::open(&state.join("state.sqlite")).unwrap();
        let device = store.get_default_device().unwrap().unwrap();
        secrets
            .create_secret(
                &scoped_credential_key(device.id.as_str(), "drive").unwrap(),
                &SecretString::new("fixture"),
            )
            .unwrap();
        store
            .upsert_account(&AccountRecord {
                id: AccountId::new("drive"),
                display_name: "Fixture".into(),
                kind: "drive".into(),
                remote_name: "household".into(),
                remote_root_folder_id: "parent-id".into(),
                config_path: config,
                auth_status: "ready".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
        drop(store);

        let cli = |args: &[&str]| {
            let mut flags = vec!["--state-dir", state_arg];
            flags.extend_from_slice(args);
            let (status, out, err) = run_args(&rt, &flags);
            assert_eq!(status, ExitCode::SUCCESS, "{err}");
            out
        };
        let output = |json: &str| {
            runner.push_run(FakeRunScript {
                lines: vec![OutputLine {
                    stream: StreamKind::Stdout,
                    text: json.into(),
                    truncated: false,
                }],
                ..Default::default()
            });
        };
        let listing = || {
            output(r#"[{"Name":"Data","Path":"Data","IsDir":true,"ID":"data-id"}]"#);
            output(r#"[{"Name":"Research","Path":"Research","IsDir":true,"ID":"chosen-id"}]"#);
        };

        listing();
        let listed = cli(&["collections", "list", "--account", "drive", "--json"]);
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(
            listed["candidates"][0]["collection"]["folder_id"],
            "chosen-id"
        );
        assert_eq!(
            listed["candidates"][0]["collection"]["display_path"],
            "Data/Research"
        );

        listing();
        output(r#"{"count":2,"bytes":42,"sizeless":0}"#);
        listing();
        let size = cli(&[
            "collections",
            "size",
            "--account",
            "drive",
            "--folder-id",
            "chosen-id",
            "--json",
        ]);
        let size: serde_json::Value = serde_json::from_str(&size).unwrap();
        assert_eq!(
            size["status"]["Known"],
            serde_json::json!({"bytes": 42, "files": 2})
        );

        listing();
        listing();
        let added = cli(&[
            "add-mapping",
            "--account",
            "drive",
            "--folder-id",
            "chosen-id",
            "--local",
            local.to_str().unwrap(),
            "--local-recovery",
            recovery.to_str().unwrap(),
        ]);
        assert!(added.contains("Added mapping `drive-1`"));
        let snapshot: serde_json::Value =
            serde_json::from_str(&cli(&["status", "--json"])).unwrap();
        let mapping = &snapshot["mappings"][0];
        assert_eq!(mapping["id"], "drive-1");
        assert_eq!(mapping["account_id"], "drive");
        assert_eq!(mapping["collection_folder_id"], "chosen-id");
        assert_eq!(
            mapping["cloud_recovery_path"],
            "household:Recovery/chosen-id"
        );
        assert_eq!(mapping["readiness"], "NeedsInitialization");
        assert_eq!(snapshot["scheduling_paused"], true);
    }

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

    #[test]
    fn credential_helper_uses_fake_store_and_exits_3_when_locked() {
        let store = Arc::new(InMemorySecretStore::new());
        store
            .create_secret("inst-1/drive-1", &SecretString::new("fake-secret-42"))
            .unwrap();
        let rt = CliRuntime {
            secret_store: Arc::clone(&store) as Arc<dyn SecretStore>,
            process_runner: Arc::new(FakeProcessRunner::new()),
            rclone_binary_override: None,
            password_command_factory: None,
            echo_auth_url_to_process_stdout: false,
        };

        let (code, stdout, stderr) =
            run_args(&rt, &["credential-helper", "--account", "inst-1/drive-1"]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(stdout, "fake-secret-42\n");
        assert!(stderr.is_empty());

        store.set_locked(true);
        let (code2, stdout2, stderr2) =
            run_args(&rt, &["credential-helper", "--account", "inst-1/drive-1"]);
        assert_eq!(code2, ExitCode::from(EXIT_AUTH_REQUIRED));
        assert!(stdout2.is_empty());
        assert!(stderr2.contains("authentication_required"));
        assert!(!stderr2.contains("fake-secret-42"));
    }

    #[cfg(unix)]
    #[test]
    fn cli_account_import_list_check_and_resolve_work_with_fake_store_and_pinned_rclone() {
        if pinned_binary_path().is_none() {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let tree = tmp.path().join("parent-tree");
        fs::create_dir_all(tree.join("Data/Research")).unwrap();

        let src_cfg = tmp.path().join("chosen/source.conf");
        fs::create_dir_all(src_cfg.parent().unwrap()).unwrap();
        let src_bytes = b"[src_local]\ntype = local\n\n[other_secret]\ntype = drive\nclient_secret = do-not-copy\n";
        fs::write(&src_cfg, src_bytes).unwrap();

        let store = Arc::new(InMemorySecretStore::new());
        let helper_dir = tmp.path().join("helpers");
        fs::create_dir_all(&helper_dir).unwrap();
        let store_for_cb = Arc::clone(&store);
        let factory: PasswordCommandFactory = Arc::new(move |cred_key: &str| {
            let secret = store_for_cb
                .get_secret(cred_key)
                .map_err(AccountError::from)?;
            let safe = cred_key.replace('/', "__");
            let sec_file = helper_dir.join(format!("{safe}.sec"));
            fs::write(&sec_file, secret.expose().as_bytes()).unwrap();
            fs::set_permissions(&sec_file, fs::Permissions::from_mode(0o600)).unwrap();
            let bin = helper_dir.join(format!("helper-{safe}"));
            fs::write(&bin, format!("#!/bin/sh\ncat \"{}\"\n", sec_file.display())).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
            Ok(
                PasswordCommand::new(bin, vec!["--account".to_string(), cred_key.to_string()])
                    .unwrap(),
            )
        });

        let rt = CliRuntime {
            secret_store: Arc::clone(&store) as Arc<dyn SecretStore>,
            process_runner: Arc::new(RealProcessRunner),
            rclone_binary_override: None,
            password_command_factory: Some(factory),
            echo_auth_url_to_process_stdout: false,
        };

        let sd = state_dir.to_str().unwrap();
        let (code, stdout, stderr) = run_args(
            &rt,
            &[
                "account",
                "import",
                "--state-dir",
                sd,
                "--from",
                src_cfg.to_str().unwrap(),
                "--remote",
                "src_local",
                "--id",
                "local-1",
                "--as",
                "household",
                "--root-folder-id",
                tree.to_str().unwrap(),
            ],
        );
        assert_eq!(code, ExitCode::SUCCESS, "stderr={stderr}");
        assert!(stdout.contains("Imported account `local-1`"));
        assert_eq!(fs::read(&src_cfg).unwrap(), src_bytes);

        let (code, list_json, stderr) =
            run_args(&rt, &["account", "list", "--state-dir", sd, "--json"]);
        assert_eq!(code, ExitCode::SUCCESS, "stderr={stderr}");
        let parsed: serde_json::Value = serde_json::from_str(&list_json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["id"], "local-1");

        let (code, check_out, stderr) = run_args(
            &rt,
            &[
                "account",
                "check",
                "--state-dir",
                sd,
                "--account",
                "local-1",
            ],
        );
        assert_eq!(code, ExitCode::SUCCESS, "stderr={stderr}");
        assert!(check_out.contains("verified: 1 parent folder(s)"));
        assert!(check_out.contains("Data"));

        let (code, res_out, stderr) = run_args(
            &rt,
            &[
                "remote",
                "resolve",
                "Data/Research",
                "--state-dir",
                sd,
                "--account",
                "local-1",
                "--json",
            ],
        );
        assert_eq!(code, ExitCode::SUCCESS, "stderr={stderr}");
        let resolved: serde_json::Value = serde_json::from_str(&res_out).unwrap();
        assert_eq!(resolved["components"].as_array().unwrap().len(), 2);
        assert_eq!(resolved["components"][1]["name"], "Research");
    }

    #[cfg(unix)]
    #[test]
    fn cli_account_add_drive_requires_saved_token_and_reconnect_resumes_without_leaking_secrets() {
        use rclone_adapter::{ProcessError, ProcessSpec, RunHandle, SpawnOptions};
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Runs real pinned rclone for local config encryption/check commands,
        /// serves recorded fixtures for `config reconnect`, `config redacted`,
        /// and `lsjson` so no Google network call or browser launch ever occurs.
        struct HybridDriveAuthRunner {
            fake_reconnect: FakeProcessRunner,
            simulate_saved_token: AtomicBool,
            recorded_argvs: std::sync::Mutex<Vec<Vec<String>>>,
        }

        impl ProcessRunner for HybridDriveAuthRunner {
            fn spawn(
                &self,
                spec: &ProcessSpec,
                options: SpawnOptions,
            ) -> Result<RunHandle, ProcessError> {
                let argv: Vec<String> = spec
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let is_reconnect = argv.first().map(String::as_str) == Some("config")
                    && argv.get(1).map(String::as_str) == Some("reconnect");
                let is_redacted = argv.first().map(String::as_str) == Some("config")
                    && argv.get(1).map(String::as_str) == Some("redacted");
                let is_lsjson = argv.first().map(String::as_str) == Some("lsjson");
                assert!(
                    !(argv.first().map(String::as_str) == Some("config")
                        && argv.get(1).map(String::as_str) == Some("dump")),
                    "CLI must never invoke `config dump`: {argv:?}"
                );
                self.recorded_argvs.lock().unwrap().push(argv);
                if is_reconnect {
                    self.fake_reconnect.spawn(spec, options)
                } else if is_redacted && self.simulate_saved_token.load(Ordering::SeqCst) {
                    let fake_redacted = FakeProcessRunner::new();
                    fake_redacted.push_run(FakeRunScript {
                        lines: vec![OutputLine {
                            stream: StreamKind::Stdout,
                            text: "[household]\ntype = drive\nclient_id = XXX\nclient_secret = XXX\nscope = drive\nroot_folder_id = XXX\ntoken = XXX\n".to_string(),
                            truncated: false,
                        }],
                        ..Default::default()
                    });
                    fake_redacted.spawn(spec, options)
                } else if is_lsjson && self.simulate_saved_token.load(Ordering::SeqCst) {
                    let fake_ls = FakeProcessRunner::new();
                    fake_ls.push_run(FakeRunScript {
                        lines: vec![OutputLine {
                            stream: StreamKind::Stdout,
                            text: r#"[{"Path":"Data","Name":"Data","ID":"1Data","IsDir":true}]"#
                                .to_string(),
                            truncated: false,
                        }],
                        ..Default::default()
                    });
                    fake_ls.spawn(spec, options)
                } else {
                    RealProcessRunner.spawn(spec, options)
                }
            }
        }

        if pinned_binary_path().is_none() {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let secret_file = tmp.path().join("oauth-secret.txt");
        let oauth_secret = "GOCSPX-super-secret-oauth-value";
        fs::write(&secret_file, oauth_secret).unwrap();
        fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).unwrap();

        let store = Arc::new(InMemorySecretStore::new());
        let helper_dir = tmp.path().join("helpers");
        fs::create_dir_all(&helper_dir).unwrap();
        let store_for_cb = Arc::clone(&store);
        let captured_key = Arc::new(std::sync::Mutex::new(None::<String>));
        let captured_key_cb = Arc::clone(&captured_key);
        let factory: PasswordCommandFactory = Arc::new(move |cred_key: &str| {
            *captured_key_cb.lock().unwrap() = Some(cred_key.to_string());
            let secret = store_for_cb
                .get_secret(cred_key)
                .map_err(AccountError::from)?;
            let safe = cred_key.replace('/', "__");
            let sec_file = helper_dir.join(format!("{safe}.sec"));
            fs::write(&sec_file, secret.expose().as_bytes()).unwrap();
            fs::set_permissions(&sec_file, fs::Permissions::from_mode(0o600)).unwrap();
            let bin = helper_dir.join(format!("helper-{safe}"));
            fs::write(&bin, format!("#!/bin/sh\ncat \"{}\"\n", sec_file.display())).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
            Ok(
                PasswordCommand::new(bin, vec!["--account".to_string(), cred_key.to_string()])
                    .unwrap(),
            )
        });

        let fake_reconnect = FakeProcessRunner::new();
        // Attempt 1: `config reconnect` exits 0 without writing any token.
        fake_reconnect.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stderr,
                text: "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=cliAttempt1".to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        // Attempt 2 (`account reconnect`): succeeds and token is present in dump.
        fake_reconnect.push_run(FakeRunScript {
            lines: vec![
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=cliAddDriveTest".to_string(),
                    truncated: false,
                },
                OutputLine {
                    stream: StreamKind::Stderr,
                    text: "NOTICE: Got code".to_string(),
                    truncated: false,
                },
            ],
            ..Default::default()
        });
        let runner = Arc::new(HybridDriveAuthRunner {
            fake_reconnect,
            simulate_saved_token: AtomicBool::new(false),
            recorded_argvs: std::sync::Mutex::new(Vec::new()),
        });

        let rt = CliRuntime {
            secret_store: Arc::clone(&store) as Arc<dyn SecretStore>,
            process_runner: Arc::clone(&runner) as Arc<dyn ProcessRunner>,
            rclone_binary_override: None,
            password_command_factory: Some(factory),
            echo_auth_url_to_process_stdout: false,
        };

        let sd = state_dir.to_str().unwrap();
        let (code1, stdout1, stderr1) = run_args(
            &rt,
            &[
                "account",
                "add-drive",
                "--state-dir",
                sd,
                "--id",
                "drive-1",
                "--root-folder-id",
                "0AParentId",
                "--client-id",
                "123.apps.googleusercontent.com",
                "--client-secret-file",
                secret_file.to_str().unwrap(),
            ],
        );
        assert_eq!(
            code1,
            ExitCode::from(EXIT_AUTH_REQUIRED),
            "CLI must not mark account ready when reconnect exits 0 without a saved token; stdout={stdout1} stderr={stderr1}"
        );
        assert!(stderr1.contains("authentication_required"));

        let (list_code, list_json, list_err) =
            run_args(&rt, &["account", "list", "--state-dir", sd, "--json"]);
        assert_eq!(list_code, ExitCode::SUCCESS, "stderr={list_err}");
        let parsed: serde_json::Value = serde_json::from_str(&list_json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["id"], "drive-1");
        assert_eq!(parsed[0]["auth_status"], "authentication_required");

        // Now complete authorization via `account reconnect` with a saved token.
        runner.simulate_saved_token.store(true, Ordering::SeqCst);
        let (code, stdout, stderr) = run_args(
            &rt,
            &[
                "account",
                "reconnect",
                "--state-dir",
                sd,
                "--account",
                "drive-1",
            ],
        );
        assert_eq!(code, ExitCode::SUCCESS, "stderr={stderr}");
        assert!(
            stdout.contains("Authorization URL: http://127.0.0.1:53682/auth?state=cliAddDriveTest"),
            "expected auth URL in stdout, got: {stdout}"
        );
        assert!(stdout.contains("auth_status=ready"));

        let cred_key = captured_key
            .lock()
            .unwrap()
            .clone()
            .expect("scoped credential key recorded");
        assert!(
            cred_key.contains("/drive-1"),
            "expected per-state install-id scoped key, got {cred_key}"
        );
        let stored_passphrase = store.get_secret(&cred_key).unwrap();
        // Neither OAuth secret, config passphrase, nor OAuth token may appear in stdout, stderr, argv, or plaintext config.
        for secret in [
            oauth_secret,
            stored_passphrase.expose(),
            "ya29.fake-access-token",
            "1//fake-refresh-token",
        ] {
            assert!(!stdout1.contains(secret));
            assert!(!stderr1.contains(secret));
            assert!(!stdout.contains(secret));
            assert!(!stderr.contains(secret));
            for argv in runner.recorded_argvs.lock().unwrap().iter() {
                assert!(
                    !argv.iter().any(|a| a.contains(secret)),
                    "secret leaked in argv: {argv:?}"
                );
            }
        }
        let cfg_path = state_dir.join("accounts/drive-1/rclone.conf");
        let on_disk = fs::read_to_string(&cfg_path).unwrap();
        assert!(rclone_adapter::is_encrypted_config_content(&on_disk));
        assert!(!on_disk.contains(oauth_secret));
        assert!(!on_disk.contains(stored_passphrase.expose()));
        let mode = fs::symlink_metadata(&cfg_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn cli_never_leaks_config_stdout_canaries_on_nonzero_or_signaled_child() {
        use rclone_adapter::{ExitStatusInfo, ProcessError, ProcessSpec, RunHandle, SpawnOptions};
        use sync_core::{AccountId, DeviceId};
        use sync_manager::{AccountRecord, DeviceRecord, SqliteStateStore, scoped_credential_key};

        if pinned_binary_path().is_none() {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        }

        struct CanaryFailRunner {
            exit_status: ExitStatusInfo,
            canary: String,
        }

        impl ProcessRunner for CanaryFailRunner {
            fn spawn(
                &self,
                spec: &ProcessSpec,
                options: SpawnOptions,
            ) -> Result<RunHandle, ProcessError> {
                let argv: Vec<String> = spec
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                assert!(
                    !(argv.first().map(String::as_str) == Some("config")
                        && argv.get(1).map(String::as_str) == Some("dump")),
                    "CLI must never run config dump: {argv:?}"
                );
                let fake = FakeProcessRunner::new();
                if argv.first().map(String::as_str) == Some("config")
                    && argv.get(1).map(String::as_str) == Some("redacted")
                {
                    fake.push_run(FakeRunScript {
                        lines: vec![OutputLine {
                            stream: StreamKind::Stdout,
                            text: format!("[household]\ntoken = {}\n", self.canary),
                            truncated: false,
                        }],
                        exit_status: self.exit_status,
                        ..Default::default()
                    });
                } else {
                    // `config reconnect` and `config encryption check` succeed.
                    fake.push_run(FakeRunScript::default());
                }
                fake.spawn(spec, options)
            }
        }

        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let acct_dir = state_dir.join("accounts/canary-1");
        fs::create_dir_all(&acct_dir).unwrap();
        let cfg_path = acct_dir.join("rclone.conf");
        fs::write(&cfg_path, "RCLONE_ENCRYPT_V0:\nFakeCipher==\n").unwrap();
        fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o600)).unwrap();

        let db = SqliteStateStore::open(&state_dir.join("state.sqlite")).unwrap();
        let dev = db
            .ensure_device(&DeviceRecord {
                id: DeviceId::new("device-canary-cli"),
                name: "cli".to_string(),
                created_at: "2026-11-20T00:00:00Z".to_string(),
            })
            .unwrap();
        db.upsert_account(&AccountRecord {
            id: AccountId::new("canary-1"),
            display_name: "Canary".to_string(),
            kind: "drive".to_string(),
            remote_name: "household".to_string(),
            remote_root_folder_id: "0ARoot".to_string(),
            config_path: cfg_path,
            auth_status: "authentication_required".to_string(),
            created_at: "2026-11-20T00:00:00Z".to_string(),
            updated_at: "2026-11-20T00:00:00Z".to_string(),
        })
        .unwrap();
        drop(db);

        let store = Arc::new(InMemorySecretStore::new());
        let cred_key = scoped_credential_key(dev.id.as_str(), "canary-1").unwrap();
        store
            .create_secret(&cred_key, &SecretString::new("canary-pw"))
            .unwrap();

        let helper_bin = tmp.path().join("fake-helper");
        fs::write(&helper_bin, "#!/bin/sh\necho canary-pw\n").unwrap();
        fs::set_permissions(&helper_bin, fs::Permissions::from_mode(0o700)).unwrap();

        let canary = "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K";
        for exit_status in [
            ExitStatusInfo {
                code: Some(1),
                signal: None,
            },
            ExitStatusInfo {
                code: None,
                signal: Some(9),
            },
        ] {
            let helper_c = helper_bin.clone();
            let factory: PasswordCommandFactory =
                Arc::new(move |_| Ok(PasswordCommand::new(helper_c.clone(), vec![]).unwrap()));
            let rt = CliRuntime {
                secret_store: Arc::clone(&store) as Arc<dyn SecretStore>,
                process_runner: Arc::new(CanaryFailRunner {
                    exit_status,
                    canary: canary.to_string(),
                }),
                rclone_binary_override: None,
                password_command_factory: Some(factory),
                echo_auth_url_to_process_stdout: false,
            };
            let (code, stdout, stderr) = run_args(
                &rt,
                &[
                    "account",
                    "reconnect",
                    "--state-dir",
                    state_dir.to_str().unwrap(),
                    "--account",
                    "canary-1",
                ],
            );
            assert_ne!(code, ExitCode::SUCCESS);
            assert!(
                !stdout.contains(canary) && !stderr.contains(canary),
                "CLI leaked child stdout canary! stdout={stdout} stderr={stderr}"
            );
        }
    }
}
