# Release

Ship the app with its own verified rclone executable and retain the third-party
license notices. Use platform credential storage and an encrypted app-owned
rclone configuration.

Before distributing a macOS build, sign the app and nested executables with
Developer ID, notarize and staple the app, and test the installed package.
Validate folder and removable-disk access from the installed app.

Before an upgrade, drain active jobs, checkpoint app state, and qualify
database and engine compatibility on disposable mappings. Record the exact
rollback requirements for each release.

**TODO** verify packaging, credential storage, signing, notarization, upgrade,
and restore procedures before distribution.
