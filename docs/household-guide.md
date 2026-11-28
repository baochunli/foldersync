# Using FolderSync

Use disposable files until the owner completes the [pilot](pilot-and-rollback.md).
A mapping is one Google Drive folder paired with a folder on this Mac.

## Set up your first folder

1. Ask the owner for the desktop OAuth client details and shared parent folder
   ID. In **Accounts**, choose **Connect Google Drive…**. For **Account ID**,
   choose a name for this Mac, such as `personal`. Use 1–128 letters (A–Z or
   a–z), digits, hyphens, or underscores, starting with a letter or digit. Fill in
   the rest of the form and choose **Open browser to sign in**. Use your own
   Google account and wait for **Connected**. Let the owner set up the client
   and sharing first.
2. Choose **Add mapping…** and pick a folder under the shared `Data` folder.
   Compare its estimated size with the free space on the disk you want to use.
   Connect an external disk before selecting a folder on it. After choosing a
   local folder, wait for the disk and free-space checks before continuing.
3. Under **Choose what to include**, clear the subfolders you want to skip.
   **Suggested: build and cache folders** lists matching folders that
   FolderSync found and pre-excluded, with their folder count, file count,
   and size. Choose **Remove** or **Remove all suggested** to include
   those files. The size estimate updates as you make changes.
   Use **Pattern (optional)** and **Add** for a custom exclusion. Use
   **Remove** beside a pattern to take it out of the selection.
   Choose a local location for **Recovery copies**. Leave room for both synced
   files and earlier versions.
4. Review the direction of the first sync. Use an empty destination for an
   upload or download. If both folders contain files, ask the owner to compare
   and preserve both sets before proceeding. Choose **Start first sync** once
   the setup checks pass. The first-sync screen shows elapsed time,
   transferred bytes and files, and a progress bar. The bar fills as bytes
   transfer when the expected byte total is known. FolderSync can estimate
   time left after two minutes.

## Archive finished folders during setup

Under **Choose what to include**, FolderSync suggests **Upload as one
archive** for an immediate subfolder with at least 2,000 included files and
no included file modified in the past 12 months. The count applies your
exclusions, including any suggested build and cache folders you keep
excluded. Clear any archive suggestion you want to keep as ordinary files.
**Review** shows the selected archives and where the originals will go.

When you start first sync, FolderSync checks that the disk has room to make
and verify each ZIP while the original is still present. ZIPs contain the
included files, including Git repository data within included folders. The
complete original moves to **FolderSync Archived Originals** on the same
disk, outside the mapping. The ZIP takes its place in the mapping and syncs
as an ordinary file. First-sync progress shows ZIP creation before upload
and then counts each ZIP as one file at its on-disk size. Keep the source
folders quiet while the ZIPs are made.

If the holding folder is inaccessible or the ZIP name is already in use,
review the error before starting. If the app stops during a move, reopen
FolderSync with the same disk connected so it can recover the archive before
you resume first sync.

Open the mapping’s **Archived originals** section to see their sizes and
reveal them in Finder. FolderSync keeps the originals until you remove them
yourself. Open a ZIP to retrieve an included file.

## Everyday use

- After a successful first sync, FolderSync checks both folders every five
  minutes by default while the app is running. Edits and deletions can sync
  in either direction. A local edit can trigger an earlier check. Choose
  **Sync now** to request a run, or adjust **Check schedule** in the mapping’s
  **Schedule** section. Use **Sync all now** in the menu bar or Overview to
  run every ready mapping.
- **Pause scheduling** holds automatic runs on this Mac. An active run can
  finish, and **Sync now** still works for eligible mappings. To hold one
  mapping, use its **Pause** button. Use **Resume scheduling** in the sidebar
  for the whole Mac, or **Resume** for a paused mapping.
- In **Change subfolders…**, use **Remove** beside a custom pattern, then
  choose **Review changes**. Check **Removed exclusions** and the first-sync
  checks before confirming. Files you newly exclude stay on disk. Ask the
  owner to review the change if both sides already contain files.
- FolderSync appears in the Dock and app switcher while its window is open.
  Closing the window leaves it running in the menu bar. Use **Show window**
  to reopen it. **Quit FolderSync** stops new runs and exits the app. If a
  run is active, **Stop and quit** requests a clean stop. When you plan to
  eject a disk, pause scheduling and wait for the current run to finish first.

With **FolderSync Finder** enabled in System Settings, badges appear on the
mapped folder itself. **Synced** shows the last reported ready status.
**Syncing** means work is queued or in progress. **Needs attention** means
check **Attention**. **Paused** means sync is on hold. **Disk missing** means
check the expected disk. **FolderSync unavailable** means Finder lacks current
status. Open FolderSync to check.

## Routine fixes

Open **Attention** and read **Cause**, **What FolderSync did**, and **Next**
on the card for your folder.

- If you see **Waiting for** followed by your disk’s name, reconnect the disk
  you used during setup and wait for the automatic check. A manual pause or
  recovery hold stays in effect.
- If you see **A different disk named … is connected**, reconnect the original
  disk. If the right disk still shows a folder change or mounts at a new
  location, leave the mapping held and ask the owner.
- For **Can’t access** messages, check the disk and folder permissions, then
  choose **Check again**. Ask the owner if the check still fails.
- A **Low space** advisory names the mappings on the volume that needs space.
  **Reveal recovery folder** appears when one of those mappings keeps its
  recovery folder on that volume.
- For **Sign-in required**, choose **Reconnect…** in **Accounts** and sign in again
  with the same Google account. If macOS Keychain is locked, unlock your login
  keychain and choose **Check access**. Ask the owner if access still fails.
- On a **Conflicts** card, read the paths and choose **Reveal in
  Finder** or **Show run**. Look next to the original file for numbered copies
  such as `notes.txt.conflict1` and `notes.txt.conflict2`. Keep both copies in a
  separate folder while you compare them. Agree on which content to keep, save
  it under the intended name, then choose **Mark as reviewed**. This clears the
  notice. Ask the owner if you cannot account for both versions.

## Restore an earlier version

1. Use **Activity** to open the relevant run, or choose **Show recovery copies**
   on its recovery card in **Attention**.
2. Under **Recovery copies from this run**, select the file and choose **Copy to
   inspection folder…**. Pick an existing folder outside all synced folders
   and choose **Copy here**.
3. Open the inspected copy and confirm it is the version you want. Leave the
   live file intact. To bring the recovered version back into sync, use an
   unused filename. Ask the owner to look in the shared Drive `Recovery`
   folder if the version you need is on the cloud side.

## Diagnose a stop before resuming

The default **Deletion limit** is 10% of tracked files. Even one deletion can
trip it in a small folder. If a run shows **Stopped by deletion limit**, check
what was removed on both sides and ask the owner to review the changes and the
independent backup before resuming. Have the owner review planned bulk
deletions ahead of time.

1. For **Recovery required**, open **Attention** and choose **Start recovery…**
   on the affected mapping. Sync stays on hold during the review.
2. Check the disk and both folder identities. If an earlier sync is still
   running, wait for it to finish and choose **Check again**. If FolderSync
   cannot confirm the process status or the folders fail verification, get
   the owner’s help.
3. Choose **Show run log and recovery copies**. Compare both folders with what
   you expected and copy any needed versions to an inspection folder. Preserve
   the live folders, sync state, logs, and recovery copies while investigating.
4. Once you understand the cause and the checklist allows it, choose **Run
   normally and resume**. Choose **Keep paused** if you have finished the
   checklist and want to leave the mapping paused. Leave the recovery hold in
   place when you still need a diagnosis.

For an unexplained stop, repeat sign-in failure, unexpected deletion, or disk
or folder mismatch, use **Preferences** → **Diagnostics** → **Export diagnostics…**
and share the export privately with the owner. It can include folder paths.
