# FolderSync NSH app

The generated `nsh.json` selects the Rust root in `src/lib.rs`. Use the
pinned CLI from this directory.

Always run `nsh test` in demo mode with an isolated `HOME` and
`FOLDERSYNC_STATE_DIR` so tests never touch your real `~/Library/Application Support/FolderSync`
state directory or macOS services:

```sh
~/.local/bin/nsh build
TMP_DIR="$(mktemp -d)"
HOME="$TMP_DIR/home" FOLDERSYNC_STATE_DIR="$TMP_DIR/state" FOLDERSYNC_DEMO=1 ~/.local/bin/nsh test
rm -rf "$TMP_DIR"
```

To launch the interactive macOS app against an isolated state directory:

```sh
FOLDERSYNC_STATE_DIR=/tmp/foldersync-dev ~/.local/bin/nsh run --target macos
```
