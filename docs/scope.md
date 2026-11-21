# Scope

A mapping pairs one Google Drive folder with one local directory on one
computer. Each computer selects its own nonoverlapping collections and stores
its own mapping state. A directory on an external disk must be bound to its
volume identity before it can sync.

Sync is selective: Drive can hold far more than one computer can store, so each
computer syncs only the folders it selects, and a mapping can exclude
subfolders. The app shows each selected folder's size next to the free space on
its target volume. Mappings on the internal disk and on external disks sync
independently to Google Drive.

The manager schedules verified mappings automatically. Active mappings use a
five-minute interval by default. The suggested archive preset is 15 minutes.
Active schedules, user pauses, and recovery holds are managed independently.
Changing an exclusion set requires reviewing and initializing a new generation.
A pair of populated folders requires reconciliation before that initialization.

The first intended workflow is asynchronous editing across computers. Each
computer runs independently, so editing a file during overlapping syncs can
cause conflicts. Two clients editing different files and a third returning
with offline changes are required qualification scenarios.

Finder integration is part of the first release: a Finder Sync extension shows
a status badge on each mapped folder's root (synced, syncing, needs attention,
paused, or disk missing) and offers Sync now, Pause, Open FolderSync, and Show
last run. The extension owns no sync state; it reads the app's status snapshot.
Per-file badges are not planned.

Online-only placeholders, a virtual drive, live document merging, and full
macOS metadata fidelity are outside the initial release.

**TODO** qualify the supported file types, volume configurations, and
multi-computer behavior.
