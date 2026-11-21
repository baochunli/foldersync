use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, Mutex},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use rclone_adapter::{
    AccountConfigLock, ConfigError, DriveAuthRequest, DriveRemoteConfigSpec, ImportRemoteSpec,
    LsJsonRequest, PasswordCommand, RealProcessRunner, create_drive_remote_config,
    extract_auth_url, import_single_remote_into_encrypted_config, is_encrypted_config_content,
    list_dirs_with_runner, list_encrypted_config_remotes, pinned_binary_path, qualify_binary,
    verify_encrypted_config,
};

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "foldersync-rclone-acct-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { root: dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn make_test_password_helper(dir: &Path, secret: &str) -> PasswordCommand {
    let secret_file = dir.join("test-secret.txt");
    fs::write(&secret_file, secret.as_bytes()).unwrap();
    fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).unwrap();

    let helper = dir.join("foldersync-test-cred-helper");
    let script = format!("#!/bin/sh\ncat \"{}\"\n", secret_file.display());
    fs::write(&helper, script.as_bytes()).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();

    PasswordCommand::new(helper, vec!["--account".to_string(), "acc-1".to_string()]).unwrap()
}

#[test]
fn pinned_rclone_creates_encrypted_drive_config_with_0600_perms_and_no_secret_in_file() {
    let Some(rclone_path) = pinned_binary_path() else {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    };
    let engine = qualify_binary(&rclone_path).unwrap();
    let ws = TempWorkspace::new("enc-drive");
    let account_dir = ws.path("state/accounts/drive1");
    let config_path = account_dir.join("rclone.conf");
    let lock_path = account_dir.join("config.lock");
    let helper_dir = ws.path("helper");
    fs::create_dir_all(&helper_dir).unwrap();

    let config_secret = "cfg-passphrase-never-in-logs-9a8b7c";
    let oauth_secret = "oauth-client-secret-never-in-argv-xyz";
    let pw_cmd = make_test_password_helper(&helper_dir, config_secret);

    let spec = DriveRemoteConfigSpec {
        remote_name: "household".to_string(),
        client_id: "12345.apps.googleusercontent.com".to_string(),
        client_secret: oauth_secret.to_string(),
        scope: "drive".to_string(),
        root_folder_id: "0ASharedParentFolderId".to_string(),
    };

    create_drive_remote_config(
        &engine,
        &RealProcessRunner,
        &config_path,
        &lock_path,
        &pw_cmd,
        &spec,
    )
    .unwrap();

    // Verify 0600 permissions and encrypted header.
    #[cfg(unix)]
    {
        let mode = fs::symlink_metadata(&config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "rclone.conf must have 0600 permissions");
    }

    let on_disk = fs::read_to_string(&config_path).unwrap();
    assert!(
        is_encrypted_config_content(&on_disk),
        "rclone.conf must carry RCLONE_ENCRYPT_V0 header"
    );
    assert!(
        !on_disk.contains(oauth_secret),
        "encrypted config on disk must never contain plaintext OAuth secret"
    );
    assert!(
        !on_disk.contains(config_secret),
        "encrypted config on disk must never contain config passphrase"
    );
    assert!(
        !on_disk.contains("shared_with_me"),
        "shared_with_me must not be written"
    );

    // Verify rclone config encryption check succeeds with --password-command.
    verify_encrypted_config(&engine, &RealProcessRunner, &config_path, &pw_cmd).unwrap();

    // Verify DriveAuthRequest argv never carries either secret.
    let auth_req = DriveAuthRequest::new(
        engine.clone(),
        config_path.clone(),
        lock_path.clone(),
        "household",
        pw_cmd.clone(),
    )
    .unwrap();
    let argv: Vec<String> = auth_req
        .to_args()
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert!(
        !argv.iter().any(|a| a.contains(config_secret)),
        "config passphrase must never appear in auth argv: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a.contains(oauth_secret)),
        "OAuth client secret must never appear in auth argv: {argv:?}"
    );
    assert_eq!(
        extract_auth_url(
            "NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=test123"
        )
        .as_deref(),
        Some("http://127.0.0.1:53682/auth?state=test123")
    );
}

#[test]
fn pinned_rclone_imports_single_local_remote_leaving_source_byte_identical_and_lists_via_lsjson() {
    let Some(rclone_path) = pinned_binary_path() else {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    };
    let engine = qualify_binary(&rclone_path).unwrap();
    let ws = TempWorkspace::new("import-local");
    let source_path = ws.path("user-chosen/source-rclone.conf");
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();

    let source_bytes = b"\
[keep_me]
type = local
nounc = true

[do_not_copy]
type = drive
client_secret = super-secret-do-not-import
";
    fs::write(&source_path, source_bytes).unwrap();

    let account_dir = ws.path("state/accounts/imported1");
    let target_config = account_dir.join("rclone.conf");
    let lock_path = account_dir.join("config.lock");
    let helper_dir = ws.path("helper");
    fs::create_dir_all(&helper_dir).unwrap();
    let pw_cmd = make_test_password_helper(&helper_dir, "import-passphrase-12345");

    let imported = import_single_remote_into_encrypted_config(
        &engine,
        &RealProcessRunner,
        &ImportRemoteSpec {
            source_config_path: source_path.clone(),
            source_remote_name: "keep_me".to_string(),
            target_remote_name: "household".to_string(),
            target_config_path: target_config.clone(),
            lock_path,
            password_command: pw_cmd.clone(),
            allow_global_default_source: false,
        },
    )
    .unwrap();
    assert_eq!(imported.remote_type, "local");
    assert_eq!(imported.target_remote_name, "household");

    // Source config must be byte-identical.
    assert_eq!(fs::read(&source_path).unwrap(), source_bytes);

    // Target config must be 0600 and encrypted.
    #[cfg(unix)]
    {
        let mode = fs::symlink_metadata(&target_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    let encrypted_raw = fs::read_to_string(&target_config).unwrap();
    assert!(is_encrypted_config_content(&encrypted_raw));
    assert!(!encrypted_raw.contains("do_not_copy"));
    assert!(!encrypted_raw.contains("super-secret-do-not-import"));

    // Decrypt via pinned rclone `config dump` (non-secret summary) and confirm ONLY `household` exists.
    let remotes =
        list_encrypted_config_remotes(&engine, &RealProcessRunner, &target_config, &pw_cmd)
            .unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].remote_name, "household");
    assert_eq!(remotes[0].remote_type, "local");
    assert!(!remotes[0].shared_with_me);

    // Exercise lsjson --dirs-only against the imported local remote using the encrypted config!
    let tree = ws.path("local-tree");
    fs::create_dir_all(tree.join("Data/Research")).unwrap();
    fs::create_dir_all(tree.join("Data/Family")).unwrap();
    fs::write(tree.join("Data/file.txt"), b"hello").unwrap();

    let ls_req = LsJsonRequest::new(
        engine.clone(),
        target_config.clone(),
        "household",
        false,
        Some(pw_cmd.clone()),
    )
    .unwrap();
    let subdirs = list_dirs_with_runner(
        &ls_req,
        &tree.join("Data").to_string_lossy(),
        &RealProcessRunner,
    )
    .unwrap();
    let names: BTreeSet<String> = subdirs.into_iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        BTreeSet::from(["Family".to_string(), "Research".to_string()])
    );
}

#[test]
fn refuses_global_default_config_unless_explicitly_allowed() {
    let Some(rclone_path) = pinned_binary_path() else {
        eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
        return;
    };
    let engine = qualify_binary(&rclone_path).unwrap();
    let ws = TempWorkspace::new("refuse-global");
    let fake_home = ws.path("home");
    let global_cfg = fake_home.join(".config/rclone/rclone.conf");
    fs::create_dir_all(global_cfg.parent().unwrap()).unwrap();
    fs::write(&global_cfg, "[demo]\ntype = local\n").unwrap();

    let helper_dir = ws.path("helper");
    fs::create_dir_all(&helper_dir).unwrap();
    let pw_cmd = make_test_password_helper(&helper_dir, "pw-123");

    let err = rclone_adapter::config::import_single_remote_with_home_override(
        &engine,
        &RealProcessRunner,
        &ImportRemoteSpec {
            source_config_path: global_cfg.clone(),
            source_remote_name: "demo".to_string(),
            target_remote_name: "demo".to_string(),
            target_config_path: ws.path("state/accounts/a1/rclone.conf"),
            lock_path: ws.path("state/accounts/a1/config.lock"),
            password_command: pw_cmd,
            allow_global_default_source: false,
        },
        Some(&fake_home),
        None,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ConfigError::GlobalDefaultConfigRefused { .. }
    ));
}

#[test]
fn account_config_lock_serializes_concurrent_mutations() {
    let ws = TempWorkspace::new("lock-serial");
    let lock_path = ws.path("accounts/a1/config.lock");
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let barrier = Arc::new(Barrier::new(2));

    let mut handles = Vec::new();
    for worker in ["w1", "w2"] {
        let lp = lock_path.clone();
        let ev = Arc::clone(&events);
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            let _guard = AccountConfigLock::acquire(&lp).unwrap();
            ev.lock().unwrap().push(format!("{worker}:enter"));
            thread::sleep(Duration::from_millis(60));
            ev.lock().unwrap().push(format!("{worker}:exit"));
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let log = events.lock().unwrap().clone();
    assert_eq!(log.len(), 4);
    // Mutual exclusion: whoever entered first must exit BEFORE the second enters.
    assert!(
        (log[0] == "w1:enter"
            && log[1] == "w1:exit"
            && log[2] == "w2:enter"
            && log[3] == "w2:exit")
            || (log[0] == "w2:enter"
                && log[1] == "w2:exit"
                && log[2] == "w1:enter"
                && log[3] == "w1:exit"),
        "concurrent config mutations overlapped: {log:?}"
    );
}
