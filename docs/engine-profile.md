# Engine profile

## Pinned engine

FolderSync pins **rclone `1.76.0`** in
[`packaging/qualification-manifest.toml`](../packaging/qualification-manifest.toml),
recording both archive and executable SHA-256 digests for macOS arm64
(`osx-arm64`), macOS amd64 (`osx-amd64`), and Linux amd64 (`linux-amd64`).
[Upstream release](https://github.com/rclone/rclone/releases/tag/v1.76.0) ·
[Official checksums](https://downloads.rclone.org/v1.76.0/SHA256SUMS)

Run `scripts/fetch-rclone.sh` to verify and install the pinned executable in
`.tools/rclone/1.76.0/rclone`. `rclone-adapter` resolves that file to an
absolute path (`FOLDERSYNC_RCLONE` may override it during development) and
qualifies it against the **host platform's** manifest entry before launching any
sync:

1. Compute the executable's SHA-256 and compare it against the host platform
   entry in `packaging/qualification-manifest.toml` **before** spawning it
   (detects binary replacement).
2. Run `<binary> version --config <throwaway-temp-config> --ask-password=false`
   with stdin closed (`Stdio::null`) and an isolated environment (`env_clear` +
   minimal allowlist with `HOME` pointed at the throwaway temp directory) so
   startup never probes or creates `~/.config/rclone`, and verify the parsed
   version matches `1.76.0`.
3. Return an immutable `EngineIdentity` (private fields with read-only getters)
   bound to the host platform and verified digest.
4. Re-hash the executable on disk at the launch boundary of every sync run
   (`BisyncRequest::spawn`, with `RealProcessRunner::spawn` also re-hashing
   immediately before `exec`) against the checked-in manifest pin and refuse to
   spawn if the bytes changed after qualification. Custom-manifest and
   spec-builder hooks are restricted to crate-internal tests.

`rclone selfupdate` and `rclone` on `PATH` are never used.

## Process and credential isolation

- **Cleared environment (`env_clear`):** Every `RealProcessRunner` spawn clears
  the inherited environment and forwards only a minimal allowlist (`PATH`,
  `TMPDIR`, `LANG`, `LC_ALL`, `TZ`), plus `HOME` directed to a run-owned or
  throwaway directory (`workdir` for bisync runs, temp dir for version probes).
  Ambient `RCLONE_*` variables (including `RCLONE_FORCE` and `RCLONE_RESYNC`)
  and `XDG_*` variables cannot reach rclone.
- **Credential-helper contract (`PasswordCommand`):** `--password-command`
  accepts only an absolute path to a dedicated helper executable (shell
  interpreters such as `sh`, `bash`, `zsh`, `cmd`, and `powershell` are
  rejected) plus non-secret lookup arguments (e.g.
  `sync-cli credential-helper --account <id>`), encoded as an RFC4180 CSV
  `SpaceSepList`. The helper retrieves the account passphrase from the OS
  credential store (`SecretStore`, macOS Keychain generic password under service
  `dev.foldersync`), prints the passphrase solely to stdout on success, never
  prompts on stdin, and exits with code `3` (`authentication_required`) when the
  store is locked or unavailable. `rclone-adapter` additionally redacts
  `--password-command stderr:` payloads before capture storage or live observer
  delivery.
- **App-owned encrypted configs and per-account locking:** Each account uses
  `<state-dir>/accounts/<id>/rclone.conf` (`0600`) encrypted immediately via
  pinned `rclone config encryption set` and verified via
  `rclone config encryption check` (header `RCLONE_ENCRYPT_V0:`). All config
  mutations for an account (creation, single-remote import, encryption, OAuth
  `config reconnect`) are serialized through `<state-dir>/accounts/<id>/config.lock`
  (in-process mutex + Unix `flock(LOCK_EX)`). OAuth `client_secret` and config
  passphrases are never passed in argv or logs. See [`docs/accounts.md`](accounts.md).
- **Bounded capture and observer delivery:** Stdout/stderr lines are bounded by
  line length, line count, and total bytes. Live observer delivery uses a
  bounded, nonblocking `SyncSender::try_send` with drop accounting. Recognized
  diagnostic events (conflicts, critical errors, deletion-guard aborts, access
  check failures, auth/config errors, interrupts, and raw lines) are retained in
  a separate bounded store so high-volume `INFO` output cannot evict early
  conflicts; if diagnostic events are dropped or lines are truncated, outcome
  classification returns `Outcome::IncompleteDiagnosis`.

## Implemented argument profile (`BisyncRequest`)

`rclone-adapter` constructs arguments strictly through the validating builder
`BisyncRequest::builder()`, returning `Vec<OsString>` (never a shell string).
There is no free-form extra-arguments field; `--force`, `--inplace`,
`--copy-links`, `--download-hash`, `--track-renames`, and automatic resync
cannot be expressed.

### Normal bidirectional run (`RunMode::Normal`)

```text
bisync
  <absolute-path1-local-dir>
  <remote:collection-path | absolute-path2-local-dir>
  --config <absolute-app-owned-config-path>
  [--password-command <validated-helper-argv-as-SpaceSepList>]
  --ask-password=false
  --workdir <absolute-generation-workdir>
  --filters-file <absolute-frozen-filters-file>
  --check-access
  --check-filename <check-filename>        # default: RCLONE_TEST
  --max-delete <max_delete_percent>        # 1..=100; percent of tracked files, default: 10
  --compare size,modtime
  --conflict-resolve none
  --conflict-loser num
  --check-sync true
  --resilient
  --recover
  --backup-dir1 <absolute-local-recovery-dir>
  --backup-dir2 <same-remote:recovery-path | absolute-local-recovery-dir>
  --use-json-log
  --log-level INFO
  --stats 2s
  [--drive-skip-gdocs --drive-skip-shortcuts]   # emitted only for Path2::DriveRemote
```

### Initialization run (`RunMode::Initialize(InitPreference::Path1 | Path2)`)

Initialization uses the same validated base profile, except:

- `--resilient` and `--recover` are omitted.
- `--resync-mode path1` (local has data, remote is newly empty) or
  `--resync-mode path2` (remote has data, local is empty) is emitted instead.
- Ordinary runs never carry `--resync` or `--resync-mode`.

### Comparison policy and known `--compare size,modtime` limitation (§10)

The initial profile uses `--compare size,modtime` and retains normal
post-transfer verification without silently enabling full-library checksum
hashing or `--download-hash`. **Known limitation:** an in-place edit that
preserves **both** exact byte length and modification timestamp is not selected
for transfer by `--compare size,modtime`, and post-transfer verification cannot
detect a file never selected. This limitation is demonstrated explicitly by
`demonstrates_same_size_same_mtime_edit_is_missed_by_size_modtime_compare` in
`crates/rclone-adapter/tests/local_bisync.rs`. Workflows that require detecting
same-size, same-mtime mutations must wait for a qualified checksum comparison
policy or explicit content-audit command.

### Validation boundaries

`BisyncRequestBuilder::build()` rejects:

- Unqualified engine version, non-host platform, or SHA-256 not matching the
  host platform pin.
- Relative or empty local paths (`path1`, `config_path`, `workdir`,
  `filters_file`, `backup_dir1`, and local `path2`/`backup_dir2`).
- Identical or nested paths across `path1`, `path2`, `backup_dir1`,
  `backup_dir2`, and `workdir`, resolving the longest existing ancestor via
  `canonicalize` before appending absent tail components (catching symlink
  aliases to live trees and rejecting dangling/unresolvable ancestors).
- Mismatched `backup_dir2` kind or remote alias, or overlapping remote paths.
- `max_delete_percent` outside `1..=100`.
- Invalid remote names/paths or invalid `check_filename` values.
- Relative, shell-interpreter, non-UTF-8, or control-character-bearing
  `PasswordCommand` arguments.

- `--backup-dir2` for Google Drive uses `BackupDir2::drive_recovery` to build
  the same-remote cloud recovery path
  `<remote>:Recovery/<collection>/<device>/<run>` and rejects cross-remote or
  overlapping destinations.
- Drive remotes root directly at the shared parent folder ID (`root_folder_id`
  in the encrypted remote config) without enabling `shared_with_me`.
- Pre-run cloud folder identity checks use `rclone lsjson --dirs-only` (with
  `--drive-skip-gdocs --drive-skip-shortcuts` for Drive remotes) to resolve each
  path component's folder ID, reject ambiguous duplicate sibling names, and
  confirm the expected folder and ancestor IDs have not changed.

## Exit-code and diagnostic classification (`1.76.0`)

`rclone-adapter` keeps the pinned rclone `v1.76.0` exit-code table in a single
module (`crates/rclone-adapter/src/outcome.rs`), matching
[rclone `v1.76.0` `lib/exitcode/exitcode.go`](https://github.com/rclone/rclone/blob/v1.76.0/lib/exitcode/exitcode.go#L13-L18),
and always combines exit status with pinned-prefix `--use-json-log` diagnostics:

- `0` (`Success`) / `9` (`NoFilesTransferred`) → `Success`, or
  `SuccessWithConflicts { paths }` when conflict rename/detection markers were
  logged.
- Signal termination, exit `130`/`137`/`143`, or a non-zero exit after
  `request_stop()` / interrupt log markers → `Interrupted { graceful, needs_recovery, reason }`.
- Holds that never auto-retry take precedence over resilient-retry notices and
  coarse exit codes: `DeletionGuardAborted`, `AccessCheckFailed`, `AuthFailure`
  (including listing-time OAuth token expiry and `invalid_grant`),
  `InvalidConfig` (and exit `2`), `EndpointMismatch` (and exit `3`), and
  `Critical { requires_resync: true }` (unusable baseline: missing, unreadable,
  or out-of-sync prior listings, changed filters hash, or explicit
  `Must run --resync`).
- When none of those hold conditions are present (and exit code is not `6`),
  rclone's explicit `--resilient` retryable notice (e.g. transient listing
  failure exiting `7`) classifies as `Outcome::Retryable`.
- Exit `5` (`RetryError`) or `stats.retryError` (without holds or exit `6`) →
  `Retryable`.
- Exit `6` is `NoRetryError` in rclone `v1.76.0` and is **never** classified as
  `Retryable`, regardless of `stats.retryError`.
- When diagnostic events are dropped or lines are truncated,
  `classify_run_outcome` returns `Outcome::IncompleteDiagnosis` wrapping the
  preliminary outcome.

## Observed local qualification results (rclone `1.76.0`)

Verified locally on macOS arm64 (`osx-arm64`) using local-to-local fixtures in
disposable temp directories with throwaway empty `--config` files, cleared
environments, uniquely identifiable file contents, external truth sets, SHA-256
verification, and a second clean sync cycle
(`crates/rclone-adapter/tests/local_bisync.rs`):

| Scenario | Result | Observed details |
| --- | --- | --- |
| Binary SHA-256 + isolated `version` qualification + launch re-hash | Passed | Exact host manifest digest and `rclone v1.76.0` match using throwaway `--config` and isolated env; binary replacement detected both before qualification and between qualification and sync launch. |
| Parent `RCLONE_FORCE=true` & `RCLONE_RESYNC=true` isolation | Passed | `env_clear` + allowlist prevented parent `RCLONE_RESYNC=true` from initializing an uninitialized workdir (`Critical { requires_resync: true }`) and prevented `RCLONE_FORCE=true` from bypassing `--max-delete 10` (`DeletionGuardAborted`, zero deletes). |
| Initialization (`--resync-mode path1` and `--resync-mode path2`) | Passed | Propagated initial tree in the chosen direction and remained clean on the second normal cycle. |
| Bidirectional add / edit / delete + `--backup-dir1/2` | Passed | Changes propagated both ways; overwritten and deleted files were captured in `--backup-dir1` and `--backup-dir2` with exact pre-change SHA-256 hashes; second cycle preserved truth set without resurrection. |
| Same-file edit conflict (`--conflict-resolve none --conflict-loser num`) | Passed | Exercised `notes/draft - final.txt` (containing ` - `) alongside benign `notes/must run --resync.txt`; exited `0`, classified as `SuccessWithConflicts` with exact `.conflict1`/`.conflict2` paths, preserved both contents by SHA-256 across two cycles, and did not misclassify the benign filename. |
| Deletion guard (`--max-delete 10` as 10%) | Passed | Deleting 4 of 10 files aborted with `DeletionGuardAborted`; zero files were deleted on Path2; restoring files allowed a clean subsequent cycle. |
| Access check (`--check-access --check-filename RCLONE_TEST`) | Passed | Removing Path2's `RCLONE_TEST` aborted with `AccessCheckFailed` and left all synced files untouched. |
| Graceful `SIGINT` during larger batch (400 × 32 KiB) + next run | Passed | First `SIGINT` triggered graceful shutdown (`exit 130`, `Signal received: interrupt`, `Graceful shutdown completed successfully`), leaving `.lst-new` files in `workdir`; classified as `Interrupted { graceful: true, needs_recovery: true }`. The subsequent normal `--resilient --recover` run completed with `exit 0` (`Outcome::Success`), converged all 400 files to exact truth-set hashes, and stayed clean on the following cycle. |
| Same-size, same-mtime in-place edit (`--compare size,modtime`) | Known limitation verified | Overwriting a 9-byte file with different 9-byte contents while restoring its exact mtime exited `0` (`Outcome::Success`) without propagating the edit to Path2, confirming §10's documented limitation. |
| Encrypted app-owned config (`config encryption set` + `check` + `dump`) | Passed | Created `0600` encrypted `rclone.conf` (`RCLONE_ENCRYPT_V0:`) via `--password-command` without secrets in argv, logs, or plaintext disk; verified `AccountConfigLock` serializes concurrent mutations. |
| Single-remote import (`local` backend) + `lsjson --dirs-only` | Passed | Imported one named `local` remote into an encrypted config while leaving the multi-section source byte-identical, refusing the global default config path without explicit opt-in, and listing subdirectories via `lsjson --dirs-only`. Drive folder-ID and duplicate-name logic verified with recorded JSON fixtures. |

### Surprises and engine quirks observed in `1.76.0`

1. **ANSI escape codes inside `--use-json-log` messages**: rclone `1.76.0`
   embeds ANSI SGR escape sequences (such as `\u001b[31m`, `\u001b[36m`) inside
   JSON `msg` fields. `log_parser::strip_ansi_codes` strips CSI sequences
   before prefix matching while preserving the raw `msg` for diagnostics.
2. **`--max-delete` aborts with exit code `1` and `stats.retryError: true`**:
   In `1.76.0`, exceeding `--max-delete` exits `1` and sets `"retryError": true`
   in the final stats JSON. Matching the `"too many deletes"` / `"Safety abort"`
   diagnostic before retryable heuristics prevents misclassifying a
   deletion-guard abort as `Retryable`.
3. **`--resilient` emits a retryable notice on both transient errors and some
   holds**: On transient listing errors, rclone exits `7` with a generic
   `Bisync critical error:` plus `"Error is retryable without --resync due to
   --resilient mode."`, which is legitimately `Retryable`. It also emits that
   notice on missing `RCLONE_TEST` and missing `filters.txt.md5`, so explicit
   hold diagnostics (`AccessCheckFailed`, `MustResync`, `AuthFailure`,
   `EndpointMismatch`) must always win over the resilient notice.
4. **`--resync-mode path1|path2` implies resync without `--resync`**: Passing
   `--resync-mode path1` or `--resync-mode path2` alone performs initialization
   in `1.76.0` and honors the specified winner preference.
5. **`--password-command` uses RFC4180 CSV `SpaceSepList` quoting and logs
   helper stderr on failure**: Tokens containing spaces or quotes must use CSV
   double-quote escaping (`""`), and rclone logs failing helper stderr verbatim
   at error level, so `rclone-adapter` redacts `--password-command stderr:`
   payloads at the capture boundary.

## Remaining qualification TODO

See [`docs/qualification.md`](qualification.md) for the full pending matrix. In
particular, **Real Google Drive** qualification (`--drive-skip-gdocs`,
`--drive-skip-shortcuts`, shared-parent root IDs, same-remote `Recovery/...`
moves, two separately authenticated accounts, OAuth token refresh with encrypted
`--config` + Keychain `--password-command`, concurrent clients, offline return,
and removable-volume unplug races) remains **TODO**; local tests do not satisfy
real-Drive qualification.
