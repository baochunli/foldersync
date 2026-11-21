# Google Drive accounts, encrypted config, and qualification steps

This guide explains how FolderSync stores account credentials and how an owner
runs manual qualification against a **disposable** Google Drive shared parent
folder using `sync-cli`.

> **Qualification status:** Automated tests in this repository verify encrypted
> config creation (`0600`, `RCLONE_ENCRYPT_V0:`), `config encryption check`,
> single-remote import (`local` backend), `lsjson` JSON parsing and duplicate
> rejection (fixtures), Drive argv construction, per-account mutation locking,
> and the in-memory `SecretStore` fake. **Real Google Drive OAuth sign-in, real
> Drive folder IDs, real macOS Keychain storage, two-account shared-parent
> permissions/quota, and token refresh remain owner-qualification items and are
> not verified by automated CI.**

## How account configs and secrets are stored

- **App-owned encrypted config:** Each account gets its own rclone config at
  `<state-dir>/accounts/<account-id>/rclone.conf` (`0600` permissions inside a
  `0700` directory). FolderSync never reads or modifies `~/.config/rclone`.
- **Encryption passphrase:** A random 256-bit passphrase is generated per
  account and stored in macOS Keychain as a generic password under service
  `dev.foldersync` and account `<account-id>`.
- **Credential helper:** Pinned rclone retrieves the passphrase exclusively via
  `--password-command "<path-to-sync-cli> credential-helper --account <id>"`
  with `--ask-password=false` and stdin closed. Neither the config passphrase
  nor the OAuth `client_secret` is ever placed in command-line arguments or
  ordinary logs.
- **Locked Keychain handling:** The macOS Keychain backend sets
  `kSecUseAuthenticationUI = kSecUseAuthenticationUIFail` so a locked Keychain
  fails immediately with `authentication_required` (exit code `3`) instead of
  blocking on a hidden prompt.
- **Serialized config mutations:** All config mutations for an account
  (creation, import, encryption, OAuth reconnect) hold an exclusive per-account
  lock at `<state-dir>/accounts/<account-id>/config.lock`.
- **Direct shared-parent root:** Drive remotes set `root_folder_id` directly to
  the actual shared parent folder ID and never enable `shared_with_me`.

## Step 1: Create your own Google OAuth client

Rclone's shared Google Drive OAuth client is being retired during 2026. Create
your own personal OAuth client before connecting a real Drive account:

1. Open [Google Cloud Console](https://console.cloud.google.com/) and create a
   dedicated project (for example, `foldersync-personal`).
2. Under **APIs & Services → Library**, enable the **Google Drive API**.
3. Under **APIs & Services → OAuth consent screen**:
   - Choose **External** (for personal Google accounts) and keep the app in
     **Testing** mode during household qualification.
   - Add each participating household Google account email under **Test users**.
   - Note: Google Testing-mode refresh tokens can expire after ~7 days; test
     reauthorization and review consent/publishing requirements before long-term
     use.
4. Under **APIs & Services → Credentials → Create Credentials → OAuth client ID**:
   - Application type: **Desktop app**.
   - Record the **Client ID** (`...apps.googleusercontent.com`) and **Client
     secret**.
5. Save the Client secret to a private temporary file (`0600`) so it stays out
   of shell history and process argument lists:

```sh
umask 077
printf '%s' 'YOUR_CLIENT_SECRET' > /tmp/foldersync-oauth-secret.txt
```

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

# 1. Create the encrypted Drive config and run browser OAuth authorization.
#    sync-cli prints the loopback authorization URL to open in your browser.
"$SYNC_CLI" account add-drive \
  --state-dir "$QUAL_STATE" \
  --id household-a \
  --display-name "Household Drive (Account A)" \
  --remote household \
  --root-folder-id "1AbCdEfGhIjKlMnOpQrStUvWxYz" \
  --client-id "1234567890-example.apps.googleusercontent.com" \
  --client-secret-file /tmp/foldersync-oauth-secret.txt

# Remove the temporary secret file once encrypted into accounts/household-a/rclone.conf.
rm -f /tmp/foldersync-oauth-secret.txt

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

### Explicit single-remote import (optional developer flow)

To copy **one** named remote from a user-chosen unencrypted rclone config file
without modifying the source file:

```sh
"$SYNC_CLI" account import \
  --state-dir "$QUAL_STATE" \
  --from /path/to/exported-rclone.conf \
  --remote household \
  --id imported-1
```

By default, `account import` refuses to read `~/.config/rclone/rclone.conf`,
`$XDG_CONFIG_HOME/rclone/rclone.conf`, or `~/.rclone.conf` unless
`--allow-global-config` is passed explicitly.

## Step 4: Owner qualification checks (real Google Drive)

Before enabling household use, verify on the disposable shared parent with both
Account A and Account B:

1. **Keychain roundtrip:** Run the manual `#[ignore]` Keychain test if desired:
   `cargo test -p sync-manager --lib keychain_generic_password_roundtrip_manual_only -- --ignored`.
2. **Duplicate-name guard:** Temporarily create two sibling folders named
   `Data/Research` in Drive and confirm `remote resolve Data/Research` refuses
   with an ambiguous-duplicate error listing both IDs.
3. **Storage & permissions:** Upload disposable files from Account A and Account
   B, confirm which account's quota is charged, and verify that overwriting or
   deleting a file owned by the other account successfully moves the recovery
   copy into `household:Recovery/<collection>/<device>/<run>`.
4. **Skip policies:** Confirm Google Docs/Sheets/Slides and Drive shortcuts are
   omitted when syncing with `--drive-skip-gdocs --drive-skip-shortcuts`.
5. **Token refresh & reauthorization:** Wait for access-token expiry (and check
   after several days in Testing mode) to confirm rclone persists refreshed
   tokens into the encrypted `accounts/<id>/rclone.conf` under `config.lock`,
   and that a revoked token cleanly enters `authentication_required`.
