# FolderSync

FolderSync is a desktop prototype for syncing selected Google Drive folders
with local directories. Each folder can have a different local destination,
including an external disk.

**Status** — prototype, not for real data. The desktop NSH app previews the
approved screens against an in-memory demo source: system actions (Reveal,
Open cloud folder, diagnostics export, and Login Items) are simulated without
touching macOS, rclone, Keychain, or the filesystem. Real manager integration
and full onboarding execution are planned for the next phase. The CLI prints
its version and help.

## Desktop interface (`apps/sync-ui`)

The GUI uses NSH 0.6.1 with macOS system sans-serif typography and bundled
[Paper Mono](https://github.com/paper-design/paper-mono) (`OFL-1.1`, see
`NOTICE` and `apps/sync-ui/assets/fonts/OFL.txt`) for paths, IDs, and logs.
Use the sidebar demo switcher to preview mapping states, Welcome states, and
scheduling holds in memory.

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
