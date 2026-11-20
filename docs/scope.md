# Scope

A mapping pairs one Google Drive folder with one local directory on one
computer. Each computer selects its own nonoverlapping collections and stores
its own mapping state. A directory on an external disk must be bound to its
volume identity before it can sync.

The first intended workflow is asynchronous editing across computers. Each
computer runs independently, so editing a file during overlapping syncs can
cause conflicts. Two clients editing different files and a third returning
with offline changes are required qualification scenarios.

Online-only placeholders, a virtual drive, live document merging, and full
macOS metadata fidelity are outside the initial release. Finder badges and
actions are planned for a later milestone.

**TODO** qualify the supported file types, volume configurations, and
multi-computer behavior.
