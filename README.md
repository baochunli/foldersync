# FolderSync

FolderSync is a macOS desktop app that syncs selected Google Drive folders
in both directions. Each Drive folder can sync to a different local folder,
including one on an external disk. Finder shows badges on your mapped folders.

**FolderSync is a prototype. Use only files you have backed up elsewhere.
Start with one small folder.**

[Get started](docs/getting-started.md)

1. Build and install FolderSync on your Mac.
2. Set up a Google OAuth client and a Drive parent folder.
3. Connect your account and sync one small folder.

## Sign, notarize, and update

`packaging/build-app.sh` signs ad hoc by default. Use that build on the Mac
that built it. Each rebuild changes the signature, so expect macOS to ask for
Keychain access again.

1. For distribution, your team's **Account Holder** must create the local
   Developer ID Application certificate. In Xcode, open **Settings → Accounts**
   and select your account and team. Choose **Manage Certificates → + →
   Developer ID Application**. You can also create a certificate signing
   request in Keychain Access and upload it under **Certificates** on the
   Apple Developer site.
   [Apple's certificate instructions](https://developer.apple.com/help/account/certificates/create-developer-id-certificates)
2. Check the signing identity with `security find-identity -v -p codesigning`.
   If it appears only without `-v`, install Apple's
   [Developer ID G2 intermediate certificate](https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer)
   in your login Keychain and check again. Build from the repository root.

   ```sh
   packaging/build-app.sh --sign "Developer ID Application: <Name> (<TEAM_ID>)"
   ```

3. Create the notary profile once. Enter the app-specific password at the
   secure prompt. Never put it on the command line.

   ```sh
   xcrun notarytool store-credentials "foldersync-notary" \
     --apple-id "<apple-id>" --team-id "<TEAM_ID>"
   ```

4. Submit the signed build and staple its ticket.

   ```sh
   packaging/notarize.sh --app target/package/FolderSync.app \
     --keychain-profile foldersync-notary
   ```

| Where the rebuilt app will run | What to do after each rebuild |
| --- | --- |
| The build Mac only, copied with `ditto` | No notarization needed. |
| Another Mac, via AirDrop, download, or a shared drive | Sign with Developer ID and notarize each build. The ticket covers that exact signed build. |

With the same Developer ID signing identity and bundle ID, Keychain
**Always Allow** and the **FolderSync Finder** setting normally carry over.

Before replacing the app, choose **Pause scheduling**, let active runs
finish, then choose **Quit FolderSync**. Move the old app to the Trash and
copy the new bundle to Applications.

```sh
ditto target/package/FolderSync.app /Applications/FolderSync.app
```

See the [release guide](docs/release.md) for interactive credential setup and
the full release checklist.

## Desktop interface (`apps/sync-ui`)

The desktop app uses the live manager by default. It supports Drive account
setup, selective folder sync, automatic scheduling after first sync, run and
recovery inspection, and Finder status badges on mapped folder roots. Set
`FOLDERSYNC_DEMO=1` or pass `--demo` to open the in-memory demo. `sync-cli`
supports Drive account checks and local sync fixtures with an explicit
`--state-dir <dir>`.

Use disposable data during prototype qualification. Local fixtures exercise
sync behavior. The owner still needs to qualify real Google Drive OAuth and
transfers, two-account sharing, physical disk unplug behavior, Developer ID
signing and notarization, and the household pilot.
See the [qualification checklist](docs/qualification.md).

- [Household guide](docs/household-guide.md)
- [Pilot, rollback, and maintenance](docs/pilot-and-rollback.md)

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
Use **Change subfolders…** to review a replacement selection. On an established
mapping with files on both sides, you can add exclusions, including suggested
build and cache folders. After catch-up, FolderSync moves matching Drive files
to Recovery and keeps the copies on this Mac in place.

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
