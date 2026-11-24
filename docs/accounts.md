# Google Drive accounts, encrypted config, and qualification steps

This guide explains how FolderSync stores account credentials and how an owner
runs manual qualification against a **disposable** Google Drive shared parent
folder using `sync-cli`.

> **Qualification status:** Automated tests in this repository verify encrypted
> config creation in a random `0600` temp file before atomic `rename(2)`
> publication (`0600`, `RCLONE_ENCRYPT_V0:`), `config encryption check`,
> `config redacted` token verification (never `config dump`), strict unquoted
> allowlist Drive import with root listing verification, `lsjson --max-depth 1
> --dirs-only` parsing and duplicate rejection (fixtures), Drive argv
> construction, per-account mutation locking, per-state Keychain key isolation,
> recovery-needed preservation of `rclone.conf.old*` backups, and the in-memory
> `SecretStore` fake. **Real Google Drive OAuth sign-in, real Drive folder IDs,
> real macOS Keychain storage, two-account shared-parent permissions/quota, and
> token refresh remain owner-qualification items and are not verified by
> automated CI.**

## How account configs and secrets are stored

- **App-owned encrypted config:** Each account gets its own rclone config at
  `<state-dir>/accounts/<account-id>/rclone.conf` (`0600` permissions inside a
  `0700` directory). Setup writes the single-section INI to a unique random
  `0600` `O_EXCL` temp file (`.foldersync-setup-tmp-*`) inside that directory,
  encrypts and verifies it in place via pinned rclone, and publishes
  `rclone.conf` with an atomic `rename(2)`. Cleanup deletes only FolderSync's
  own `.foldersync-setup-tmp-*` temp files — never `rclone.conf`, never
  rclone's `rclone.conf.old*` backups, and never unrelated user files. If
  `rclone.conf` is missing while an `rclone.conf.old*` backup exists,
  FolderSync reports recovery needed and leaves all files untouched. FolderSync
  never modifies any user-global rclone config and refuses to read rclone's
  default locations (`<rclone-exe-dir>/rclone.conf`,
  `$XDG_CONFIG_HOME/rclone/rclone.conf`, `~/.config/rclone/rclone.conf`, or
  `~/.rclone.conf`) unless the user explicitly passes both `--from <path>` and
  `--allow-global-config` to `account import`.
- **Per-state scoped encryption passphrase:** A random 256-bit passphrase is
  generated per account and registered (create-only) in the macOS login
  Keychain as a generic password under service `dev.foldersync` and account
  `<install-id>/<account-id>`, where `<install-id>` is the random per-state ID
  stored in `state.sqlite`. Disposable qualification state directories therefore
  never overwrite or delete another state directory's Keychain credentials. No
  released FolderSync version ever stored bare-ID Keychain items, so no legacy
  bare-ID migration is performed. If registration crashes after credential
  creation and before the SQLite account row is written, startup reports the
  stale installation-scoped credential and retrying the same `--id` recovers by
  replacing it cleanly.
- **Credential helper:** Pinned rclone retrieves the passphrase exclusively via
  `--password-command "<path-to-sync-cli> credential-helper --account <install-id>/<account-id>"`
  with `--ask-password=false` and stdin closed. The CLI and app helper use the
  macOS account's home directory to open the login Keychain explicitly, even
  when rclone runs with an isolated `HOME`. Account management uses the same
  login Keychain for credential creation, updates, and deletion. Neither the
  config passphrase nor the OAuth `client_secret` is ever placed in
  command-line arguments or ordinary logs. Config inspection uses
  `rclone config redacted` (never `config dump`) and never includes child stdout
  in error messages.
- **Locked Keychain handling:** The macOS Keychain backend sets
  `kSecUseAuthenticationUI = kSecUseAuthenticationUIFail` so a locked Keychain
  fails immediately with `authentication_required` (exit code `3`) instead of
  blocking on a hidden prompt.
- **Serialized config mutations:** One exclusive per-account lock at
  `<state-dir>/accounts/<account-id>/config.lock` serializes add/import
  existence checks, credential registration, config publication, OAuth
  reconnect, post-auth root verification, and refresh-capable `lsjson` / `check`
  invocations.
- **Direct shared-parent root:** Drive remotes set `root_folder_id` directly to
  the actual shared parent folder ID; Drive import accepts only unquoted keys
  and values from the strict allowlist (`type`, `client_id`, `client_secret`,
  `scope`, `token`, `root_folder_id`) and rejects `starred_only`,
  `shared_with_me`, `team_drive`, quoted syntax, and unknown options.
- **Reserved fixture ID:** The account ID `local-fixture` is reserved for
  local-to-local CLI fixture mappings; `account add-drive` and `account import`
  refuse it.

## Drive folder selection in the manager

A ready Drive account can list immediate child folders of the shared
`Data` folder. Each candidate includes the folder ID, name, modification time,
and ancestor IDs. The listing reports skipped names as **duplicate** or
**unsupported name**. The remaining candidates can be selected and sized.
Creating a folder validates its requested name and checks the names already
used in `Data` before creating it, then verifies the new folder's ID under
the same parent. These operations use the account's config lock.
An operation reports the account as busy if another operation holds that lock.

Request the size of one selected folder when needed. The manager runs a
recursive size query with Google Docs and shortcuts skipped and the root
FolderSync marker excluded. The result reports bytes and file count for the
whole collection before mapping-specific exclusions. A result is advisory,
since files can change and the local filesystem may allocate a different
amount of space. If rclone cannot determine all file sizes, the calculation
reports a failure. Compare the result with the free space reported for the
selected local volume.

Only one size calculation runs at a time. The engine calls share a two-minute
deadline, including the listings used to verify folder identity. Cancellation
or timeout sends a graceful signal, then escalates by PID if needed. A
successful size is cached in memory for five minutes with a timestamp. The
manager publishes calculating, known, and failed states through its snapshot
stream.

The manager's cancellable authorization API supports initial sign-in and
reconnect. Successful sign-in clears the account's mapping sign-in holds.
**Check again** can also clear the hold after verifying the ready account
and the saved Drive folder. Canceling an active sign-in stops the rclone child
and leaves the account eligible for another attempt. Controlled manager shutdown requests
the same cancellation and allows up to one second for cleanup, subject to its
shutdown timeout. The desktop wizard's account controls are wired separately.

## Step 1: Create your own Google OAuth client

Rclone's shared Google Drive OAuth client is being retired during 2026. Create
your own personal OAuth client before connecting a real Drive account:

1. Open [Google Cloud Console](https://console.cloud.google.com/) and create a
   dedicated project (for example, `foldersync-personal`).
2. Under **APIs & Services → Library**, enable the **Google Drive API**.
3. Open **Google Auth Platform → Branding** and choose **Get started** if
   prompted. Enter an app name, a user support email, and developer contact
   information. Leave the logo blank. Displaying a logo for an external
   production app requires brand verification.
   [Google branding setup](https://support.google.com/cloud/answer/15544987)
4. Under **Audience**, choose **External**. Choose one publishing status:
   - **Testing** for qualification. Under **Test users**, add the Google
     accounts that will sign in. Google limits testing to 100 users and
     expires Drive refresh tokens after seven days.
   - **Publish app** for long-lived refresh tokens. Authorize again after
     publishing to obtain a new token. While your app is unverified for its
     Drive scopes, Google shows the **Google hasn’t verified this app** screen
     and caps access at 100 users over the life of the project. For the
     project you created, choose **Advanced → Go to [project name] (unsafe)**
     to continue.

   [Google audience and publishing settings](https://support.google.com/cloud/answer/15549945)
5. Under **Clients → Create client**, choose **Desktop app** and create the
   client. Copy the **Client ID** and **Client secret** immediately or
   download the client JSON. Google may only show the full secret at
   creation. [Google client secret visibility](https://support.google.com/cloud/answer/15549257)
6. Supply the Client secret without putting it as a literal in a shell command,
   echoing it to your terminal, or placing it at a predictable `/tmp` path —
   choose one of:
   - **No-echo shell read piped to `--client-secret-stdin`:** Note that
     `sync-cli account add-drive --client-secret-stdin` reads raw stdin directly
     and does **not** print its own prompt or disable terminal echo. Read the
     secret with echo disabled in your shell (`IFS= read -rs`) and pipe it to
     `sync-cli`, then `unset` the shell variable immediately (shown in Step 3).
   - **Private `0600` file (`--client-secret-file`):** Create a private `0600`
     file without echoing the secret to the terminal, then delete it right
     after setup:
     ```sh
     umask 077 && SECRET_FILE="$(mktemp)"
     printf 'Google OAuth Client Secret: ' && IFS= read -rs OAUTH_SECRET && printf '\n'
     printf '%s\n' "$OAUTH_SECRET" > "$SECRET_FILE"
     unset OAUTH_SECRET
     ```
     Pass `--client-secret-file "$SECRET_FILE"` to `account add-drive`, then run
     `rm -f "$SECRET_FILE"`.

FolderSync defaults to the `drive` scope because `drive.file` does not cover
preexisting arbitrary Drive files.

## Step 2: Create a disposable shared parent folder

Do not test against production data.

1. In Google Drive (Account A), create a disposable folder (for example,
   `FolderSync-Qual-Disposable`).
2. Inside it, create two disjoint top-level folders matching the cloud layout:
   - `Data` (and a disposable collection inside it, e.g. `Data/Research`)
   - `Recovery`
3. Share `FolderSync-Qual-Disposable` with Account B with **Editor** access.
4. Open `FolderSync-Qual-Disposable` in the browser and copy its folder ID from
   the URL (`https://drive.google.com/drive/folders/<SHARED_PARENT_ID>`).

## Step 3: Authorize and verify with `sync-cli`

Ensure the pinned rclone executable is installed (`scripts/fetch-rclone.sh`) and
build `sync-cli`:

```sh
scripts/fetch-rclone.sh
cargo build -p sync-cli
```

Use a disposable `--state-dir` for qualification (never point `sync-cli` at a
production state directory):

```sh
QUAL_STATE="$(mktemp -d /tmp/foldersync-qual-state.XXXXXX)"
SYNC_CLI="./target/debug/sync-cli"

# 1. Read the OAuth Client Secret with terminal echo disabled and pipe it to
#    `account add-drive --client-secret-stdin` (which reads raw stdin without
#    printing a prompt of its own). sync-cli prints the loopback URL to open
#    in your browser and marks the account ready only after verifying both
#    saved token presence (`config redacted`) and a lightweight root listing
#    (`lsjson --max-depth 1 --dirs-only`).
printf 'Google OAuth Client Secret: ' && IFS= read -rs OAUTH_SECRET && printf '\n'
printf '%s\n' "$OAUTH_SECRET" | "$SYNC_CLI" account add-drive \
  --state-dir "$QUAL_STATE" \
  --id household-a \
  --display-name "Household Drive (Account A)" \
  --remote household \
  --root-folder-id "1AbCdEfGhIjKlMnOpQrStUvWxYz" \
  --client-id "1234567890-example.apps.googleusercontent.com" \
  --client-secret-stdin
unset OAUTH_SECRET

# If authorization was canceled or exited without saving a token, resume with:
"$SYNC_CLI" account reconnect \
  --state-dir "$QUAL_STATE" \
  --account household-a

# 2. List configured accounts (never prints secrets).
"$SYNC_CLI" account list --state-dir "$QUAL_STATE"

# 3. Verify encrypted config decryption + list the shared parent via lsjson.
"$SYNC_CLI" account check --state-dir "$QUAL_STATE" --account household-a

# 4. Resolve each component ID under the shared parent (rejects duplicate names).
"$SYNC_CLI" remote resolve Data/Research \
  --state-dir "$QUAL_STATE" \
  --account household-a

# 5. Re-verify against the persisted folder ID (fails if renamed/replaced).
"$SYNC_CLI" remote resolve Data/Research \
  --state-dir "$QUAL_STATE" \
  --account household-a \
  --expected-id "<LEAF_FOLDER_ID_FROM_STEP_4>"
```

### Troubleshoot Keychain access and browser sign-in

- Run setup from Terminal in a logged-in macOS desktop session. An SSH
  session can fail with `errSecInteractionNotAllowed`. If the login Keychain
  is locked, open Keychain Access to unlock it or run
  `security unlock-keychain "$HOME/Library/Keychains/login.keychain-db"`.
  Retry from the desktop session if access is still denied.
- FolderSync looks for the passphrase in the login Keychain. If an earlier
  setup created it in a custom default Keychain, copy the item to the login
  Keychain in Keychain Access before retrying.
- The browser must be able to reach rclone's OAuth callback at
  `127.0.0.1:53682` on the Mac running setup. Open the sign-in URL in a
  browser on that Mac.

### Explicit single-remote import (optional developer flow)

To copy **one** named remote from a user-chosen unencrypted rclone config file
without modifying the source file:

```sh
"$SYNC_CLI" account import \
  --state-dir "$QUAL_STATE" \
  --from /path/to/exported-rclone.conf \
  --remote household \
  --id imported-1 \
  --root-folder-id "1AbCdEfGhIjKlMnOpQrStUvWxYz"
```

For Google Drive remotes, `account import` accepts only unquoted keys and values
from the strict allowlist (`type`, `client_id`, `client_secret`, `scope`,
`token`, `root_folder_id`) — rejecting `starred_only`, `shared_with_me`,
`team_drive`, quoted syntax, and unknown options — sets `root_folder_id` to the
requested shared-parent ID (or rejects if none is available), and verifies the
effective root with `lsjson --max-depth 1 --dirs-only` before recording the
account.

By default, `account import` refuses to read `<rclone-exe-dir>/rclone.conf`,
`$XDG_CONFIG_HOME/rclone/rclone.conf`, `~/.config/rclone/rclone.conf`, or
`~/.rclone.conf` unless `--allow-global-config` is passed explicitly.

### Select and sync a Drive collection

Use a ready account and an existing local directory. For manual qualification,
pause automatic scheduling before adding a mapping and leave it paused until
you finish the scripted runs.

```sh
"$SYNC_CLI" --state-dir "$QUAL_STATE" scheduling pause
"$SYNC_CLI" --state-dir "$QUAL_STATE" collections list --account household-a

# Enter the exact folder ID printed for the disposable collection.
IFS= read -r FOLDER_ID
"$SYNC_CLI" --state-dir "$QUAL_STATE" collections size \
  --account household-a --folder-id "$FOLDER_ID"

QUAL_FOLDERS="$(mktemp -d)"
mkdir "$QUAL_FOLDERS/local" "$QUAL_FOLDERS/recovery"
"$SYNC_CLI" --state-dir "$QUAL_STATE" add-mapping --id research \
  --account household-a --folder-id "$FOLDER_ID" \
  --local "$QUAL_FOLDERS/local" --local-recovery "$QUAL_FOLDERS/recovery"
"$SYNC_CLI" --state-dir "$QUAL_STATE" init research
"$SYNC_CLI" --state-dir "$QUAL_STATE" run-once research
"$SYNC_CLI" --state-dir "$QUAL_STATE" status --json
```

The manager rechecks the collection ID before saving the mapping and applies
its first-sync checks during `init`. Use an empty local folder to initialize
from an existing populated Drive collection. `collections list` and
`collections size` accept `--json`. If you omit `--id` when adding a Drive
mapping, use the assigned ID printed by the CLI.

`pause research` blocks manual and scheduled runs for that mapping.
`resume research` clears that hold. `scheduling pause` still permits manual
runs on ready, resumed mappings. `scheduling resume` restores automatic
scheduling for the state directory. The `scheduling` command requires either
`pause` or `resume`.

## Step 4: Owner qualification checks (real Google Drive)

Use the [Drive qualification runner](qualification.md#1-real-google-drive-qualification)
for scripted multi-client evidence.

Before enabling household use, verify on the disposable shared parent with both
Account A and Account B:

1. **Keychain roundtrip:** From a logged-in macOS Terminal, build a disposable
   debug helper and run the manual test with the separate test service:
   ```sh
   (
     set -e
     TEST_ROOT="$(mktemp -d)"
     mkdir "$TEST_ROOT/home" "$TEST_ROOT/state" "$TEST_ROOT/tmp"
     export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
     export HOME="$TEST_ROOT/home" FOLDERSYNC_STATE_DIR="$TEST_ROOT/state" TMPDIR="$TEST_ROOT/tmp"
     cargo build -p sync-cli --target-dir "$TEST_ROOT/target" --features sync-manager/keychain-test-helper
     FOLDERSYNC_TEST_HELPER="$TEST_ROOT/target/debug/sync-cli" \
       cargo test -p sync-manager --target-dir "$TEST_ROOT/target" --features keychain-test-helper \
       --lib credential_helper_reads_login_keychain_with_isolated_home_manual_only -- --ignored
   )
   ```
2. **Duplicate-name guard:** Temporarily create two sibling folders named
   `Data/Research` in Drive and confirm `remote resolve Data/Research` refuses
   with an error of the form:
   ``Error: Ambiguous duplicate cloud folder `Research` at `Data/Research` (2 matching folders; IDs: <id-1>, <id-2>).``
3. **Storage & permissions:** Upload disposable files from Account A and Account
   B, confirm which account's quota is charged, and verify that overwriting or
   deleting a file owned by the other account successfully moves the recovery
   copy into `household:Recovery/<collection>/<device>/<run>`.
4. **Skip policies:** Confirm Google Docs/Sheets/Slides and Drive shortcuts are
   omitted when syncing with `--drive-skip-gdocs --drive-skip-shortcuts`.
5. **Token refresh & reauthorization:** Wait for access-token expiry (and check
   after several days in Testing mode) to confirm rclone persists refreshed
   tokens into the encrypted `accounts/<id>/rclone.conf` under `config.lock`,
   and that a revoked token cleanly enters `authentication_required` and
   recovers via `account reconnect`.
