#!/usr/bin/env bash
set -euo pipefail

# Build FolderSync's macOS Finder Sync extension (.appex) into <repo>/target/
# using Xcode Command Line Tools only (swiftc + codesign + plutil).
# Defaults to production bundle IDs (dev.foldersync.app / dev.foldersync.app.finder-extension)
# and the documented <state dir>/finder home-relative temporary file exception.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd -P)"

OUT_DIR="${REPO_ROOT}/target/finder-extension"
DEV_SHARED_DIR=0
HOST_BUNDLE_ID="dev.foldersync.app"
SIGN_IDENTITY="-"
ARCH="$(uname -m)"
MIN_MACOS="14.0"
APP_VERSION="1.0.0"
APP_BUILD="1"
RUN_TESTS=0
WITH_MOCK_HOST=0
SKIP_SIGN=0
ENTITLEMENTS_OUT=""

usage() {
  cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --out <dir>              Output directory for built artifacts; must resolve physically
                           inside <repo>/target/ with no symlinked components or control chars
                           (default: <repo>/target/finder-extension)
  --appex-only             Build FolderSyncFinder.appex only (default)
  --with-mock-host         Also wrap the extension in a throwaway FolderSyncHost.app
                           for standalone local verification (creates a fresh mktemp handoff
                           dir inside <repo>/target/)
  --dev-shared-dir         Create a fresh disposable state directory under <repo>/target/
                           with an absolute-path temporary exception for its finder/ child.
                           When omitted in production (.appex-only) mode, uses the checked-in
                           home-relative exception for ~/Library/Application Support/FolderSync/finder.
  --bundle-id <id>         Host app bundle identifier
                           (default: dev.foldersync.app;
                           extension uses <id>.finder-extension)
  --version <ver>          CFBundleShortVersionString (default: 1.0.0)
  --build <num>            CFBundleVersion (default: 1)
  --sign <identity>        codesign identity (default: "-" for ad hoc signing;
                           non-"-" adds --options runtime --timestamp)
  --no-sign                Skip codesign (used when packaging/build-app.sh signs inner -> outer)
  --entitlements-out <p>   Copy the effective extension entitlements plist to <p>
                           (must resolve physically inside <repo>/target/, non-symlink leaf)
  --arch <arch>            Target architecture (default: host arch, e.g. arm64)
  --test                   Compile and run SnapshotModelTests and SandboxProbe before building
  -h, --help               Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) OUT_DIR="$2"; shift 2 ;;
    --appex-only) WITH_MOCK_HOST=0; shift ;;
    --with-mock-host) WITH_MOCK_HOST=1; shift ;;
    --dev-shared-dir) DEV_SHARED_DIR=1; shift ;;
    --shared-dir)
      echo "Error: arbitrary --shared-dir paths are not permitted; use --dev-shared-dir to create a fresh directory under target/." >&2
      exit 2
      ;;
    --bundle-id) HOST_BUNDLE_ID="$2"; shift 2 ;;
    --version) APP_VERSION="$2"; shift 2 ;;
    --build) APP_BUILD="$2"; shift 2 ;;
    --sign) SIGN_IDENTITY="$2"; shift 2 ;;
    --no-sign) SKIP_SIGN=1; shift ;;
    --entitlements-out) ENTITLEMENTS_OUT="$2"; shift 2 ;;
    --arch) ARCH="$2"; shift 2 ;;
    --test) RUN_TESTS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for tool in swiftc codesign plutil xcrun python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Error: required Command Line Tools binary not found: $tool" >&2
    exit 1
  fi
done

# Validate a path in-place (NO command substitution, which strips trailing newlines):
# rejects newlines/control characters up front, verifies <repo>/target is not a symlink,
# verifies physical containment inside <repo>/target/, and rejects any symlinked
# component from target/ down before any write, mkdir, mktemp, or rm.
validate_contained_in_target() {
  python3 - "$REPO_ROOT" "$1" "$2" "$3" <<'PY'
import os
import stat
import sys

repo_root = os.path.realpath(sys.argv[1])
label = sys.argv[2]
raw_path = sys.argv[3]
leaf_kind = sys.argv[4]  # "any", "dir", or "file"

if not raw_path or any(ord(c) < 32 or ord(c) == 127 for c in raw_path):
    sys.stderr.write(f"Error: {label} must be non-empty and contain no newlines or control characters.\n")
    sys.exit(2)

target_root = os.path.join(repo_root, "target")
if os.path.lexists(target_root):
    st = os.lstat(target_root)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        sys.stderr.write(f"Error: refusing symlinked or non-directory target root: {target_root}\n")
        sys.exit(2)

abs_path = os.path.abspath(raw_path)
cur = abs_path
tail = []
while not os.path.lexists(cur):
    parent, name = os.path.split(cur)
    if parent == cur:
        sys.stderr.write(f"Error: cannot resolve ancestor for {label}\n")
        sys.exit(2)
    tail.append(name)
    cur = parent

real_ancestor = os.path.realpath(cur)
phys_path = os.path.join(real_ancestor, *reversed(tail)) if tail else real_ancestor

if phys_path != target_root and not phys_path.startswith(target_root + os.sep):
    sys.stderr.write(f"Error: refusing {label} outside {target_root}: {raw_path} (resolved: {phys_path})\n")
    sys.exit(2)

for candidate in (phys_path, abs_path):
    if candidate == target_root or candidate.startswith(target_root + os.sep):
        rel = os.path.relpath(candidate, target_root)
        if rel != ".":
            check = target_root
            for part in rel.split(os.sep):
                check = os.path.join(check, part)
                if os.path.lexists(check) and stat.S_ISLNK(os.lstat(check).st_mode):
                    sys.stderr.write(f"Error: refusing symlinked component inside target/: {check}\n")
                    sys.exit(2)

if os.path.lexists(abs_path):
    st = os.lstat(abs_path)
    if stat.S_ISLNK(st.st_mode):
        sys.stderr.write(f"Error: refusing symlinked leaf for {label}: {abs_path}\n")
        sys.exit(2)
    if leaf_kind == "file" and not stat.S_ISREG(st.st_mode):
        sys.stderr.write(f"Error: destination leaf for {label} must be absent or a regular file: {abs_path}\n")
        sys.exit(2)
    if leaf_kind == "dir" and not stat.S_ISDIR(st.st_mode):
        sys.stderr.write(f"Error: destination leaf for {label} must be absent or a directory: {abs_path}\n")
        sys.exit(2)
PY
}

TARGET_ROOT="${REPO_ROOT}/target"
validate_contained_in_target "target" "$TARGET_ROOT" "dir"
validate_contained_in_target "--out" "$OUT_DIR" "dir"
if [[ -n "$ENTITLEMENTS_OUT" ]]; then
  validate_contained_in_target "--entitlements-out" "$ENTITLEMENTS_OUT" "file"
fi

SDK_PATH="$(xcrun --show-sdk-path)"
TARGET="${ARCH}-apple-macos${MIN_MACOS}"
EXT_BUNDLE_ID="${HOST_BUNDLE_ID}.finder-extension"

mkdir -p "$TARGET_ROOT" "$OUT_DIR"
# Write all generated test binaries, entitlements, and staging bundles inside a
# fresh mktemp -d staging directory under target/ so fixed filenames in --out
# are never followed via shell redirection.
BUILD_STAGE="$(mktemp -d "${TARGET_ROOT}/.finder-build-stage.XXXXXXXX")"
trap 'rm -rf "$BUILD_STAGE"' EXIT

if [[ "$RUN_TESTS" -eq 1 ]]; then
  TEST_BIN="${BUILD_STAGE}/SnapshotModelTests"
  echo "==> Running SnapshotModelTests"
  swiftc -parse-as-library \
    -target "$TARGET" -sdk "$SDK_PATH" \
    "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
    "${SCRIPT_DIR}/Tests/SnapshotModelTests.swift" \
    -o "$TEST_BIN"
  "$TEST_BIN"

  echo "==> Running sandboxed handoff probe (ad hoc signed, parent dirs denied, no pluginkit)"
  PROBE_BIN="${BUILD_STAGE}/SandboxProbe"
  PROBE_ROOT="${BUILD_STAGE}/sandbox-probe-root"
  PROBE_ENTITLED="${PROBE_ROOT}/state/finder"
  PROBE_OUTSIDE="${PROBE_ROOT}/outside-state/finder"
  mkdir -p "$PROBE_ENTITLED" "$PROBE_OUTSIDE"
  ln -s "${PROBE_ROOT}/outside-state" "${PROBE_ROOT}/symlink-state"
  REAL_PROBE_ROOT="$(cd "$PROBE_ROOT" && pwd -P)"
  REAL_PROBE_ENTITLED="$(cd "$PROBE_ENTITLED" && pwd -P)"
  PROBE_SB="${BUILD_STAGE}/SandboxProbe.sb"
  cat > "$PROBE_SB" <<EOF
(version 1)
(allow default)
(deny file-read-data file-write*
  (require-all
    (subpath "${REAL_PROBE_ROOT}")
    (require-not (subpath "${REAL_PROBE_ENTITLED}"))))
EOF
  swiftc -parse-as-library \
    -target "$TARGET" -sdk "$SDK_PATH" \
    "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
    "${SCRIPT_DIR}/Tests/SandboxProbe.swift" \
    -o "$PROBE_BIN"
  codesign --force --sign - "$PROBE_BIN"
  sandbox-exec -f "$PROBE_SB" "$PROBE_BIN" "$REAL_PROBE_ENTITLED" "${REAL_PROBE_ROOT}/symlink-state/finder"
fi

REAL_SHARED_DIR=""
if [[ "$DEV_SHARED_DIR" -eq 1 || "$WITH_MOCK_HOST" -eq 1 ]]; then
  # Match the live manager's <state dir>/finder layout without granting access to the state root.
  DEV_STATE_DIR="$(mktemp -d "${TARGET_ROOT}/finder-dev-shared.XXXXXXXX")"
  mkdir -m 700 "${DEV_STATE_DIR}/finder"
  REAL_SHARED_DIR="$(cd "${DEV_STATE_DIR}/finder" && pwd -P)"
fi

if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  STAGED_APP="${BUILD_STAGE}/FolderSyncHost.app"
  STAGED_APPEX="${STAGED_APP}/Contents/PlugIns/FolderSyncFinder.appex"
  mkdir -p "${STAGED_APP}/Contents/MacOS" "${STAGED_APPEX}/Contents/MacOS"
else
  STAGED_APP=""
  STAGED_APPEX="${BUILD_STAGE}/FolderSyncFinder.appex"
  mkdir -p "${STAGED_APPEX}/Contents/MacOS"
fi

echo "==> Compiling Finder Sync extension (${TARGET}, ${EXT_BUNDLE_ID})"
swiftc -parse-as-library -application-extension \
  -target "$TARGET" -sdk "$SDK_PATH" \
  -framework Cocoa -framework FinderSync -framework Foundation \
  -Xlinker -e -Xlinker _NSExtensionMain \
  -o "${STAGED_APPEX}/Contents/MacOS/FolderSyncFinder" \
  "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
  "${SCRIPT_DIR}/Sources/BadgeRenderer.swift" \
  "${SCRIPT_DIR}/Sources/FinderSyncExtension.swift"

cp "${SCRIPT_DIR}/Info.plist" "${STAGED_APPEX}/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "$EXT_BUNDLE_ID" "${STAGED_APPEX}/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$APP_VERSION" "${STAGED_APPEX}/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$APP_BUILD" "${STAGED_APPEX}/Contents/Info.plist"
plutil -replace FolderSyncHostBundleIdentifier -string "$HOST_BUNDLE_ID" "${STAGED_APPEX}/Contents/Info.plist"

ENTITLEMENTS_PATH="${BUILD_STAGE}/FinderExtension.entitlements"
if [[ -n "$REAL_SHARED_DIR" ]]; then
  plutil -replace FolderSyncSharedDir -string "$REAL_SHARED_DIR" "${STAGED_APPEX}/Contents/Info.plist"
  cat > "$ENTITLEMENTS_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <true/>
    <key>com.apple.security.temporary-exception.files.absolute-path.read-write</key>
    <array>
        <string>${REAL_SHARED_DIR%/}/</string>
    </array>
</dict>
</plist>
EOF
else
  cp "${SCRIPT_DIR}/FinderExtension.entitlements" "$ENTITLEMENTS_PATH"
fi

if [[ -n "$ENTITLEMENTS_OUT" ]]; then
  validate_contained_in_target "--entitlements-out" "$ENTITLEMENTS_OUT" "file"
  mkdir -p "$(dirname "$ENTITLEMENTS_OUT")"
  rm -f "$ENTITLEMENTS_OUT"
  cp "$ENTITLEMENTS_PATH" "$ENTITLEMENTS_OUT"
fi

plutil -lint "${SCRIPT_DIR}/FinderExtension.entitlements" >/dev/null
plutil -lint "${STAGED_APPEX}/Contents/Info.plist" >/dev/null
plutil -lint "$ENTITLEMENTS_PATH" >/dev/null

if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  echo "==> Compiling throwaway mock host app (${TARGET}, ${HOST_BUNDLE_ID})"
  swiftc -parse-as-library \
    -target "$TARGET" -sdk "$SDK_PATH" \
    -framework Cocoa -framework FinderSync -framework Foundation \
    -o "${STAGED_APP}/Contents/MacOS/FolderSyncHost" \
    "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
    "${SCRIPT_DIR}/Sources/HostAppMain.swift"

  cp "${SCRIPT_DIR}/HostInfo.plist" "${STAGED_APP}/Contents/Info.plist"
  plutil -replace CFBundleIdentifier -string "$HOST_BUNDLE_ID" "${STAGED_APP}/Contents/Info.plist"
  plutil -replace CFBundleShortVersionString -string "$APP_VERSION" "${STAGED_APP}/Contents/Info.plist"
  plutil -replace CFBundleVersion -string "$APP_BUILD" "${STAGED_APP}/Contents/Info.plist"
  plutil -replace FolderSyncSharedDir -string "$REAL_SHARED_DIR" "${STAGED_APP}/Contents/Info.plist"
  plutil -lint "${STAGED_APP}/Contents/Info.plist" >/dev/null
fi

if [[ "$SKIP_SIGN" -eq 0 ]]; then
  SIGN_ARGS=(--force --sign "$SIGN_IDENTITY")
  if [[ "$SIGN_IDENTITY" != "-" ]]; then
    SIGN_ARGS+=(--options runtime --timestamp)
  fi

  echo "==> Signing extension (identity: ${SIGN_IDENTITY})"
  codesign "${SIGN_ARGS[@]}" --entitlements "$ENTITLEMENTS_PATH" "$STAGED_APPEX"

  if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
    echo "==> Signing throwaway mock host app (identity: ${SIGN_IDENTITY})"
    codesign "${SIGN_ARGS[@]}" "$STAGED_APP"
    codesign --verify --deep --strict "$STAGED_APP"
  else
    codesign --verify --strict "$STAGED_APPEX"
  fi
fi

if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  APP_BUNDLE="${OUT_DIR}/FolderSyncHost.app"
  APPEX_BUNDLE="${APP_BUNDLE}/Contents/PlugIns/FolderSyncFinder.appex"
  validate_contained_in_target "app bundle" "$APP_BUNDLE" "dir"
  rm -rf "$APP_BUNDLE"
  mv "$STAGED_APP" "$APP_BUNDLE"
else
  APP_BUNDLE=""
  APPEX_BUNDLE="${OUT_DIR}/FolderSyncFinder.appex"
  validate_contained_in_target "appex bundle" "$APPEX_BUNDLE" "dir"
  rm -rf "$APPEX_BUNDLE"
  mv "$STAGED_APPEX" "$APPEX_BUNDLE"
fi

echo ""
echo "Built Finder Sync extension successfully:"
echo "  Extension:     ${APPEX_BUNDLE}"
echo "  Bundle ID:     ${EXT_BUNDLE_ID}"
echo "  Host ID:       ${HOST_BUNDLE_ID}"
if [[ -n "$REAL_SHARED_DIR" ]]; then
  echo "  State dir:     ${REAL_SHARED_DIR%/finder}"
  echo "  Shared dir:    ${REAL_SHARED_DIR} (fresh mktemp absolute temporary exception)"
else
  echo "  Shared dir:    ~/Library/Application Support/FolderSync/finder (home-relative temporary exception)"
fi
if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  echo "  Mock host app: ${APP_BUNDLE}"
fi
