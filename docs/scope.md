# Scope

A mapping pairs one Google Drive folder with one local directory on one
computer. Each computer selects its own nonoverlapping collections and stores
its own mapping state. A directory on an external disk must be bound to its
volume identity before it can sync.

Sync is selective: Drive can hold far more than one computer can store, so each
computer syncs only the folders it selects, and a mapping can exclude
subfolders. The app compares the selected folder's estimated size with free
space on the target volume. Each mapping has its own setup and schedule. On
each computer, the manager runs one sync job at a time across all disks.

The manager schedules verified mappings automatically. Active mappings use a
five-minute interval by default. Choose 15 minutes for archives if a longer
interval suits the collection.
Active schedules, user pauses, and recovery holds are managed independently.
Changing an exclusion set requires reviewing and initializing a new generation.
A pair of populated folders requires reconciliation before that initialization.

The first intended workflow is asynchronous editing across computers. Each
computer runs independently, so editing a file during overlapping syncs can
cause conflicts. Two clients editing different files and a third returning
with offline changes are required qualification scenarios.

The Finder Sync extension shows badges on mapped folder roots and offers
**Sync now**, **Pause**, **Open FolderSync**, and **Show last run**. It reads
the app's status snapshot. The installed extension still needs owner
qualification.

Online-only placeholders, a virtual drive, live document merging, and full
macOS metadata fidelity are outside the initial release.

See [qualification](qualification.md) for the pending file type, volume, and
multi-computer checks.
