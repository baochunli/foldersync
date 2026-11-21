# Pilot, rollback, and maintenance

## Pilot on disposable files

1. Start with a small collection of replaceable ordinary files. Keep an
   independent copy outside both synced folders and their recovery locations.
   Prepare the [OAuth client and shared parent](accounts.md#step-1-create-your-own-google-oauth-client).
   With an empty destination, set up the first Mac using the
   [household guide](household-guide.md). Confirm the first sync and compare
   both sides with the independent copy.
2. Add a second Mac with its own setup and an empty local destination. Exercise
   the two Google accounts that will share the collection. Check ordinary edits,
   deletions, and a deliberate same-file conflict. Inspect the recovery copies.
3. Set up a third independent client, take it offline, and edit a disposable
   file there while the other two clients make changes. Reconnect it and compare
   all copies after follow-up runs. Account for each version and check that
   deleted files stay deleted.
4. Observe several ordinary work cycles and deliberate failures before adding
   irreplaceable files. Record the results of the
   [owner qualification checks](qualification.md#owner-run-qualification-scenarios-not-verified-in-automated-lanes),
   including removable-disk and installed-app tests.
5. Move from small active folders to representative archives. Measure the time
   for a check with no changes and the growth of recovery storage at a realistic
   file count before adding more data.

Keep a private record for each mapping. Include the initial source and sync
direction, local destination, disk identity and folder on that disk, Drive
folder IDs, account used, rclone version, local and cloud recovery paths, and
expected file types and exclusions. Record the checks performed, outcomes,
and any files or versions you could not account for.

## Retire an existing sync client

Pause the previous client. Review its pending changes and establish which
copy is authoritative before you disable its management of the folder.
Preserve both copies if they differ. Initialize FolderSync in the reviewed
direction with an empty destination. Give only one sync client control of
each local folder at a time.

## Roll back

1. Choose **Pause scheduling** and let any active run finish when safe.
   Choose **Quit FolderSync** and confirm any active sync has stopped before
   changing the folder setup or starting another client.
2. Keep the local files, Drive contents, FolderSync state, logs, and recovery
   copies intact. Check both ends against your pilot record and independent
   copy. Preserve any additional versions outside the sync folders.
3. Review the workflow you will return to against the current files on both
   sides. Re-enable a former client only after deciding how to reconcile any
   changes since it last ran. For an app downgrade, follow the
   [release rollback procedure](release.md#rollback-procedure).

## Maintain the setup

- Review upstream rclone changes and requalify updates before rolling them out.
  Install app and engine updates deliberately using the [release guide](release.md).
- After a macOS upgrade, check that Google Drive sign-in and Keychain access
  still work on a disposable mapping.
- Monitor space used by local and Drive recovery folders. Review what you still
  need before removing copies. Keep automatic purging off the maintenance plan.
- Test a restore from the independent backup periodically. Check the recovered
  file in a separate folder before trusting the backup.
