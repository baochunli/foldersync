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
qualifies it before launching any sync:

1. Compute the executable's SHA-256 and compare it against the host platform
   entry in `packaging/qualification-manifest.toml` **before** spawning it
   (detects binary replacement).
2. Run `<binary> version` with stdin closed (`Stdio::null`) and verify the
   parsed version matches `1.76.0`.
3. Record `EngineIdentity { version, sha256, path }` with the run.

`rclone selfupdate` and `rclone` on `PATH` are never used.

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

### Validation boundaries

`BisyncRequestBuilder::build()` rejects:

- Unqualified engine version or SHA-256 not present in the qualification
  manifest.
- Relative or empty local paths (`path1`, `config_path`, `workdir`,
  `filters_file`, `backup_dir1`, and local `path2`/`backup_dir2`).
- Identical or nested paths across `path1`, `path2`, `backup_dir1`,
  `backup_dir2`, and `workdir` (lexical normalization plus `canonicalize` when
  paths exist on disk; component-wise prefix checks for remote paths).
- Mismatched `backup_dir2` kind or remote alias.
- `max_delete_percent` outside `1..=100`.
- Invalid remote names/paths or invalid `check_filename` values.
- Relative, non-UTF-8, or control-character-bearing `PasswordCommand` tokens.

## Exit-code and diagnostic classification (`1.76.0`)

`rclone-adapter` keeps the rclone `1.76.0` exit-code table in a single module
(`crates/rclone-adapter/src/outcome.rs`) and always combines exit status with
parsed `--use-json-log` diagnostics (never trusting exit code alone):

- `0` / `9` → `Success`, or `SuccessWithConflicts { paths }` when conflict
  rename/detection markers were logged.
- Signal termination, exit `130`/`137`/`143`, or a non-zero exit after
  `request_stop()` / interrupt log markers → `Interrupted { graceful, needs_recovery, reason }`.
- Specific log diagnostics override coarse exit codes (`1` and `7`):
  `DeletionGuardAborted`, `AccessCheckFailed`, `AuthFailure`, `InvalidConfig`,
  `EndpointMismatch`, `Critical { requires_resync, reason }`, `Retryable`, and
  `Unknown`.

## Observed local qualification results (rclone `1.76.0`)

Verified locally on macOS arm64 (`osx-arm64`) using local-to-local fixtures in
disposable temp directories with throwaway empty `--config` files, uniquely
identifiable file contents, an external truth set, SHA-256 verification, and a
second clean sync cycle (`crates/rclone-adapter/tests/local_bisync.rs`):

| Scenario | Result | Observed details |
| --- | --- | --- |
| Binary SHA-256 + `version` qualification | Passed | Exact manifest digest and `rclone v1.76.0` match; replaced binary rejected before execution. |
| Initialization (`--resync-mode path1` and `--resync-mode path2`) | Passed | Propagated initial tree in the chosen direction and remained clean on the second normal cycle. |
| Bidirectional add / edit / delete + `--backup-dir1/2` | Passed | Changes propagated both ways; overwritten and deleted files were captured in `--backup-dir1` and `--backup-dir2` with exact pre-change SHA-256 hashes; second cycle preserved truth set without resurrection. |
| Same-file edit conflict (`--conflict-resolve none --conflict-loser num`) | Passed | Exited `0`, emitted conflict rename markers for `.conflict1` and `.conflict2`, classified as `SuccessWithConflicts`, and preserved both contents by SHA-256 on both sides across two cycles. |
| Deletion guard (`--max-delete 10` as 10%) | Passed | Deleting 4 of 10 files aborted with `DeletionGuardAborted`; zero files were deleted on Path2; restoring files allowed a clean subsequent cycle. |
| Access check (`--check-access --check-filename RCLONE_TEST`) | Passed | Removing Path2's `RCLONE_TEST` aborted with `AccessCheckFailed` and left all synced files untouched. |
| Graceful `SIGINT` during larger batch (400 × 32 KiB) + next run | Passed | First `SIGINT` triggered graceful shutdown (`exit 130`, `Signal received: interrupt`, `Graceful shutdown completed successfully`), leaving `.lst-new` files in `workdir`; classified as `Interrupted { graceful: true, needs_recovery: true }`. The subsequent normal `--resilient --recover` run completed with `exit 0` (`Outcome::Success`), converged all 400 files to exact truth-set hashes, and stayed clean on the following cycle. |

### Surprises and engine quirks observed in `1.76.0`

1. **ANSI escape codes inside `--use-json-log` messages**: rclone `1.76.0`
   embeds ANSI SGR escape sequences (such as `\u001b[31m`, `\u001b[36m`) inside
   JSON `msg` fields. `log_parser::strip_ansi_codes` strips CSI sequences
   before marker matching while preserving the raw `msg` for diagnostics.
2. **`--max-delete` aborts with exit code `1` and `stats.retryError: true`**:
   In `1.76.0`, exceeding `--max-delete` exits `1` (generic error) and sets
   `"retryError": true` in the final stats JSON. Matching the `"too many
   deletes"` / `"Safety abort"` diagnostic before generic retryable heuristics
   prevents misclassifying a deletion-guard abort as `Retryable`.
3. **`--resilient` emits a retryable notice even on `must run --resync` and
   access-check failures**: On a fresh workdir with missing `filters.txt.md5`
   or a missing `RCLONE_TEST` file, rclone exits `7` and logs both a critical
   error (`(must run --resync)` / `check file check failed`) and `"Error is
   retryable without --resync due to --resilient mode."`. Classifier priority
   ensures `AccessCheckFailed` and `Critical { requires_resync: true }` win
   over `--resilient` retry notices.
4. **`--resync-mode path1|path2` implies resync without `--resync`**: Passing
   `--resync-mode path1` or `--resync-mode path2` alone performs initialization
   in `1.76.0` and honors the specified winner preference (even backing up an
   overwritten file on the non-winning side).
5. **`--password-command` uses RFC4180 CSV `SpaceSepList` quoting**: Tokens
   containing spaces or quotes must use CSV double-quote escaping (`""`), not
   shell backslash escaping or single quotes.

## Remaining qualification TODO

- **TODO (Real Google Drive)**: Qualify the full Drive profile (including
  `--drive-skip-gdocs`, `--drive-skip-shortcuts`, shared-parent root IDs,
  same-remote `Recovery/...` moves, two separately authenticated accounts,
  OAuth token refresh with encrypted `--config` + `--password-command`,
  concurrent clients, offline return, and removable-volume unplug races). Local
  tests do not satisfy real-Drive qualification.
