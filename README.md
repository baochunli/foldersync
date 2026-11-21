# FolderSync

FolderSync is a macOS desktop app for syncing selected Google Drive folders
with local directories. Each folder can have a different local destination,
including an external disk.

**Use disposable data during prototype qualification.** The desktop app uses the live
manager by default. It supports Drive account setup, selective folder sync,
automatic scheduling after first sync, run and recovery inspection, and Finder
status badges on mapped folder roots. Set `FOLDERSYNC_DEMO=1` or pass `--demo`
to open the in-memory demo. `sync-cli` supports Drive account checks and local
sync fixtures with an explicit `--state-dir <dir>`.

Local fixtures exercise sync behavior. The owner still needs to qualify real
Google Drive OAuth and transfers, two-account sharing, physical disk unplug
behavior, Developer ID signing and notarization, and the household pilot.
See the [qualification checklist](docs/qualification.md).

- [Household guide](docs/household-guide.md)
- [Pilot, rollback, and maintenance](docs/pilot-and-rollback.md)

## Desktop interface (`apps/sync-ui`)

The GUI uses NSH 0.6.1 with macOS system sans-serif typography and bundles
Paper Mono for paths, IDs, and logs. Set `FOLDERSYNC_STATE_DIR` for isolated
testing with the live manager. See [Paper Mono](https://github.com/paper-design/paper-mono)
and its [OFL-1.1 license](apps/sync-ui/assets/fonts/OFL.txt).

Overview paths use a single line with a middle ellipsis when space is tight.
The full path remains available in the accessibility label and mapping detail.
Automatic scheduling starts after FolderSync verifies a mapping’s first sync.
Choose its polling interval in the mapping’s **Schedule** section. Pause
scheduling in the sidebar or menu bar. **Sync now** requests an on-demand run
while scheduling is paused.
Activity shows the pending run order. Overview and Mapping Detail show the
estimated next check or retry when one is scheduled. Local changes can prompt
an earlier check.

Overview and Mapping Detail show reported disk capacity and filesystem.
Use **Change subfolders…** to review a replacement selection and its first-sync
checks. Applying the change keeps the previous sync setup and leaves newly
excluded local files in place.

Accounts supports browser sign-in, import from an explicitly chosen exported
configuration, reconnect, and checking access to the shared parent folder.
Choose **Cancel** in Accounts to stop an in-progress sign-in. Quit also cancels
sign-in as part of controlled shutdown. Both native launchers include the
credential helper.

The wizard lists folders directly inside the selected Drive account's shared
`Data` folder. It explains duplicate and unsupported folder names and lets you
create a new folder with an unused name. Select a folder to calculate its size
and file count before choosing subfolder exclusions. FolderSync compares that
estimate with free space on known volumes and checks the chosen destination
again during setup. Review the first sync before starting it.

## Development

macOS 14 or later is the first desktop target. The non-UI crates are built in
CI on Linux and macOS. Linux desktop qualification is pending.

Install rustup and the Xcode command-line tools on macOS. Rustup selects the
toolchain in `rust-toolchain.toml` when you run Cargo in this repository.

```sh
scripts/fetch-rclone.sh
cargo test -p sync-core -p sync-manager -p rclone-adapter -p sync-cli
cargo run -p sync-cli -- --help
```

The fetch script verifies a pinned rclone archive and installs its executable
under `.tools/rclone/`. It needs Bash, curl, shasum, and unzip.

Install [NSH 0.6.1](https://nsh.dev/docs/installation/) and use its explicit
path for the desktop app.

```sh
curl -fsSL https://nsh.dev/install.sh | NSH_VERSION=v0.6.1 sh
cd apps/sync-ui
~/.local/bin/nsh build
~/.local/bin/nsh run --target macos
```

See the [scope](docs/scope.md), [engine profile](docs/engine-profile.md),
[state and recovery](docs/state-and-recovery.md),
[qualification](docs/qualification.md), and [release](docs/release.md) notes.
