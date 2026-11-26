# APFS disk-image qualification

The macOS integration test runs FolderSync against disposable APFS images and
a local-to-local rclone fixture. Each forced detach is labeled **simulated
unplug (disk image forced detach)**. The real USB spot check in owner task 4
remains open.

## Run on macOS

From the repository root, run the following in Bash. The pinned engine comes
from the qualification manifest. The test serializes its cases so each one
can detach and check its images before the next case starts.

```bash
scripts/fetch-rclone.sh
mkdir -p target/disk-images-evidence
(
  set -o pipefail
  export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  test_home="$(mktemp -d)"
  trap 'rmdir "$test_home"' EXIT
  HOME="$test_home" FOLDERSYNC_STATE_DIR="$test_home/state" \
    cargo test --locked -p sync-manager --test disk_images \
    -- --ignored --nocapture --test-threads=1 2>&1 \
    | tee target/disk-images-evidence/local.log
)
```

The test creates each image and all throwaway sync state in its own temporary
directory. It uses `hdiutil attach -nobrowse -mountpoint` and records the
image's device node. Before any detach it checks that `hdiutil info` still
associates that node with the exact image path. After each detach it polls
for up to 120 seconds for the image to disappear. On timeout, it retries
with one ordinary detach if the recorded node still belongs to that image.
Cleanup retains the scratch directory if any fixture image is still
registered or its mountpoint cannot be confirmed safe. The local-fixture
account uses its own empty config. No Google Drive account or Keychain
item is needed.

## Read the evidence

- Classification logs the APFS UUID, name, Disk Arbitration connection field
  and FolderSync's `VolumeKind`, plus the disk-image protocol and removable
  flag reported by `diskutil`. On macOS releases that omit `DeviceInternal`,
  the test uses the verified image's UUID and connection location only for
  that image. The log marks these results **NOT VERIFIED for real disks**.
- The missing-disk and same-name replacement cases check that interval
  scheduling and Sync now cannot start a run while the bound UUID is absent.
  The missing disk produces the Finder `disk_missing` status. The
  replacement has a matching access marker and must emerge unchanged.
  Reattaching the original UUID starts an interval run, followed by two
  more sync cycles.
- The upload, download and deletion cases force-detach after a completed
  operation in a still-running batch. The test disables local backend
  server-side copies and throttles only the injected run. The deletion
  case stores the fake remote's recovery copies on a second image so each
  deletion follows a copy across filesystems.
- After an interruption the test checks the retained run record, workdir
  and recovery hold, and records the outcome of rclone's graceful stop.
  In the download case it checks the unmounted path for host-side writes
  and archives any verified fixture data before reattach. During review
  it opens interrupted-run recovery copies and preserves identified
  transfer fragments outside both sync trees before resuming. Independent
  SHA-256 manifests cover both endpoints and both recovery roots after
  recovery and after two further cycles.

The `macos-disk-images` job runs these cases on `macos-15` in parallel
with the other CI jobs. Its log gives an explicit skip reason if attach
is unavailable on the runner or the pinned engine is absent.
