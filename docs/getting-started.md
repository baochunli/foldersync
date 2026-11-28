# Get started with FolderSync

FolderSync syncs the Google Drive folders you choose with folders on your Mac.
After the first sync, edits and deletions can travel in either direction.
Each mapping can use a different disk, including an external disk. Finder
shows status badges on mapped folders.

**FolderSync is a prototype. Use only files you have backed up elsewhere.
Start with one small folder.**

## Requirements

- macOS 14 or later on Apple silicon (`arm64`) or Intel (`x86_64`). The
  rclone fetch script also supports Linux `x86_64` for development.
- Xcode command-line tools. Install them with `xcode-select --install` if
  needed. These include Git and the platform build tools.
- [rustup](https://rustup.rs/) on your PATH. It selects the pinned Rust
  toolchain when you build in the repository.
- Python 3 on your PATH for the packaging script.
- [NSH 0.6.1](https://nsh.dev/docs/installation/). Use the installer to pin
  that version.

  ```sh
  curl -fsSL https://nsh.dev/install.sh | NSH_VERSION=v0.6.1 sh
  ```

- Free disk space for the Rust toolchain, build files, and app bundle. Plan
  separately for your synced files and recovery copies.

## Build and install

1. Clone the repository from the directory where you want to keep it.

   ```sh
   git clone https://github.com/baochunli/foldersync.git
   cd foldersync
   ```

2. From the checkout, download and verify the pinned sync engine, then
   build the app.

   ```sh
   scripts/fetch-rclone.sh
   packaging/build-app.sh
   ```

   The build prints `Packaged and verified successfully:` followed by the
   app path, bundle and extension IDs, signing mode, rclone path and
   checksums, and notices. The default app path is
   `target/package/FolderSync.app`.

   The default ad hoc signature works on the Mac that built the app. You
   can build and install it without an Apple account. Each rebuild changes
   the ad hoc signature, so expect macOS to ask for Keychain access again.

3. Install the bundle in Applications and open it from there.

   ```sh
   ditto target/package/FolderSync.app /Applications/FolderSync.app
   open /Applications/FolderSync.app
   ```

## Google sign-in

You need a desktop OAuth client ID and secret before you can sign in.

1. **Create your own client.** Use the client ID and secret in the app. The
   later CLI qualification steps are optional developer work.
   [Step 1 in the accounts guide](accounts.md#step-1-create-your-own-google-oauth-client)
2. **Use a client shared privately by the repository owner.** Sign in with
   your own Google account. If the client is published and unverified, Google
   shows an unverified-app warning. If you recognize the client, choose
   **Advanced → Go to FolderSync**. The link uses the app name set by the
   owner and may end in `(unsafe)`.
   Google caps unverified access at 100 users over the life of the project.
   The owner can revoke the client, which can interrupt your sync.
   [Google's unverified app limits](https://support.google.com/cloud/answer/7454865)

Keep the client ID and secret private. Never post them publicly or commit
them to Git.

## Prepare your Google Drive folders

1. In Drive, create a parent folder for your own sync. Keep it separate from
   any disposable folder created for qualification.
2. In the parent, create two folders named `Data` and `Recovery`. Put the
   folders you want to sync directly inside `Data`, such as `Data/Research`.
   FolderSync uses `Recovery` for earlier cloud copies.
3. Open the parent folder. In the browser address bar, copy the value after
   `/folders/` and before any `?` in the URL. For example,
   `https://drive.google.com/drive/folders/<PARENT_ID>` contains
   `<PARENT_ID>`. Use the ID of the parent that contains both folders.

## Connect your account and sync your first folder

1. In **Accounts**, choose **Connect Google Drive…**. Enter an **Account ID**
   such as `personal-drive` and an **Account display name** you'll recognize.
   Fill in **Shared parent folder ID**, **OAuth client ID**, and
   **OAuth client secret**.
2. Choose **Open browser to sign in** and approve access using your Google
   account. Return to FolderSync and wait for **Connected**. If macOS asks
   for access to the FolderSync item in your login Keychain, choose
   **Always Allow**.
3. In **Overview**, choose **Choose a folder…** to start your first mapping.
   Pick one folder from `Data` and a local folder. Connect your external disk
   first if you're using one. Compare the download size with the disk's free
   space. Under **Choose what to include**, clear any subfolders you want to skip.
   Don't map a local folder that another sync client, such as Google Drive
   for Desktop, already manages.

   Once you have a mapping, **Add mapping…** appears in **Overview**. You can
   also choose **Mappings** → **Add mapping…** from the menu bar.
4. Start with an empty destination. FolderSync determines the first-sync
   direction based on which folder is empty.

   | Where your files are | Destination | Message in Review |
   | --- | --- | --- |
   | On this Mac | An empty folder inside Drive's `Data` | **First sync: upload to the cloud** |
   | In Drive | An empty local folder | **First sync: download from the cloud** |

   If both folders contain files, use a new empty destination and keep both
   existing copies while you decide which files to sync.

   In **Choose what to include**, check **Suggested: build and cache folders**.
   FolderSync pre-excludes matching directories it finds at any depth, such
   as `build/` or `.venv/`. Choose **Remove** to include a listed folder type
   or **Remove all suggested** to include them all. **Review** shows the
   count and size after exclusions.
5. In **Recovery copies**, review both paths. The default local path is
   `FolderSyncRecovery/<local-folder-name>` in the mapped folder's parent.
   You can choose a different local recovery folder outside every synced
   folder. Cloud copies go under `Recovery/<Drive-folder-ID>` in your Drive
   parent.
6. In **Review**, check the direction shown and confirm that no other sync
   app manages the local folder. Choose **Start first sync** when the
   checks pass. The first-sync screen shows elapsed time and transferred
   bytes and files. When the expected total is known, the bar shows progress.
   Time remaining appears once there is enough transfer history.
   FolderSync starts checking the mapping automatically after it
   verifies the first sync. The default deletion limit stops a run that
   would delete more than 10% of tracked files. Check both folders and your
   backup if a run stops.
7. In macOS **System Settings → General → Login Items & Extensions**, scroll
   to **Extensions** and turn on FolderSync's Finder extension. Recent macOS
   versions list it under **File Providers** as **FolderSync**, not under
   **Finder**. On macOS 14, look under **Privacy & Security → Extensions**.
   You can also enable it in Terminal with
   `pluginkit -e use -i dev.foldersync.app.finder-extension`.
   Then relaunch Finder (hold Option, right-click Finder in the Dock, and
   choose **Relaunch**) and open the parent folder. The badge appears on the
   mapped folder itself.

## Update FolderSync

1. Choose **Pause scheduling** and wait for active runs to finish. Choose
   **Quit FolderSync** from the menu bar.
2. Update your checkout. From the repository root, run
   `scripts/fetch-rclone.sh` and then `packaging/build-app.sh` again.
3. After the build succeeds, move the old `/Applications/FolderSync.app` to
   the Trash. Repeat the `ditto` and `open` commands above to install the
   rebuilt app.
4. If macOS asks for Keychain access again, choose **Always Allow**. Check
   your mappings, then choose **Resume scheduling** when you're ready.

## Uninstall FolderSync

1. Choose **Quit FolderSync**. Turn off **FolderSync Finder** in the same
   System Settings location where you enabled it.
2. Move `/Applications/FolderSync.app` to the Trash. Keep your synced files
   on both sides.
3. To preserve your settings and run history, keep
   `~/Library/Application Support/FolderSync`. If you want to remove the
   state from that location, archive it while the app is closed.
4. If you want to remove the app's credentials, use Keychain Access to locate
   the generic password items in your login Keychain with service
   `dev.foldersync` and account `<install-id>/<account-id>`. Remove only items
   belonging to the installation you're uninstalling.
5. Keep any versions you may need from your local recovery folders and the
   Drive parent's `Recovery` folder. They are separate from the app bundle.

## Where to go next

- [Household guide](household-guide.md#everyday-use) for day-to-day use and
  common fixes.
- [State and recovery](state-and-recovery.md) for logs, recovery copies, and
  interrupted runs.
- [Qualification checklist](qualification.md) for optional developer checks.
