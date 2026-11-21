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
                           inside <repo>/target/ with no symlinked components
                           (default: <repo>/target/finder-extension)
  --appex-only             Build FolderSyncFinder.appex only (default)
  --with-mock-host         Also wrap the extension in a throwaway FolderSyncHost.app
                           for standalone local verification (creates a fresh mktemp handoff
                           dir inside <repo>/target/)
  --dev-shared-dir         Create a fresh disposable handoff directory via mktemp under
                           <repo>/target/ with a scoped absolute-path temporary file exception.
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
                           (must resolve physically inside <repo>/target/)
  --arch <arch>            Target architecture (default: host arch, e.g. arm64)
  --test                   Compile and run SnapshotModelTests before building
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

# Physically validate output paths inside <repo>/target/ with zero symlinked components
# BEFORE any mkdir, mktemp, copy, or rm.
resolve_contained_in_target() {
  python3 - "$REPO_ROOT" "$1" "$2" <<'PY'
import os
import stat
import sys

repo_root = os.path.realpath(sys.argv[1])
label = sys.argv[2]
raw_path = sys.argv[3]

if not raw_path or "\x00" in raw_path:
    sys.stderr.write(f"Error: {label} must be a non-empty valid path inside target/.\n")
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
        sys.stderr.write(f"Error: cannot resolve ancestor for {label} {raw_path}\n")
        sys.exit(2)
    tail.append(name)
    cur = parent

real_ancestor = os.path.realpath(cur)
phys_path = os.path.join(real_ancestor, *reversed(tail)) if tail else real_ancestor

if phys_path != target_root and not phys_path.startswith(target_root + os.sep):
    sys.stderr.write(f"Error: refusing {label} outside {target_root}: {raw_path} (resolved: {phys_path})\n")
    sys.exit(2)

rel = os.path.relpath(phys_path, target_root)
if rel != ".":
    check = target_root
    for part in rel.split(os.sep):
        check = os.path.join(check, part)
        if os.path.lexists(check) and stat.S_ISLNK(os.lstat(check).st_mode):
            sys.stderr.write(f"Error: refusing symlinked path component inside target/: {check}\n")
            sys.exit(2)

if abs_path.startswith(target_root + os.sep):
    rel_raw = os.path.relpath(abs_path, target_root)
    check = target_root
    for part in rel_raw.split(os.sep):
        check = os.path.join(check, part)
        if os.path.lexists(check) and stat.S_ISLNK(os.lstat(check).st_mode):
            sys.stderr.write(f"Error: refusing symlinked {label} component: {check}\n")
            sys.exit(2)

print(phys_path)
PY
}

TARGET_ROOT="$(resolve_contained_in_target "target" "${REPO_ROOT}/target")"
OUT_DIR="$(resolve_contained_in_target "--out" "$OUT_DIR")"
if [[ -n "$ENTITLEMENTS_OUT" ]]; then
  ENTITLEMENTS_OUT="$(resolve_contained_in_target "--entitlements-out" "$ENTITLEMENTS_OUT")"
fi

SDK_PATH="$(xcrun --show-sdk-path)"
TARGET="${ARCH}-apple-macos${MIN_MACOS}"
EXT_BUNDLE_ID="${HOST_BUNDLE_ID}.finder-extension"

mkdir -p "$TARGET_ROOT" "$OUT_DIR"

TEST_BIN="${OUT_DIR}/SnapshotModelTests"
if [[ "$RUN_TESTS" -eq 1 ]]; then
  echo "==> Running SnapshotModelTests"
  swiftc -parse-as-library \
    -target "$TARGET" -sdk "$SDK_PATH" \
    "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
    "${SCRIPT_DIR}/Tests/SnapshotModelTests.swift" \
    -o "$TEST_BIN"
  "$TEST_BIN"
fi

REAL_SHARED_DIR=""
if [[ "$DEV_SHARED_DIR" -eq 1 || "$WITH_MOCK_HOST" -eq 1 ]]; then
  # Always create a fresh disposable directory via mktemp directly under <repo>/target/.
  FRESH_SHARED="$(mktemp -d "${TARGET_ROOT}/finder-dev-shared.XXXXXXXX")"
  chmod 700 "$FRESH_SHARED"
  REAL_SHARED_DIR="$(cd "$FRESH_SHARED" && pwd -P)"
fi

if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  APP_BUNDLE="$(resolve_contained_in_target "app bundle" "${OUT_DIR}/FolderSyncHost.app")"
  APPEX_BUNDLE="${APP_BUNDLE}/Contents/PlugIns/FolderSyncFinder.appex"
  rm -rf "$APP_BUNDLE"
  mkdir -p "${APP_BUNDLE}/Contents/MacOS" "${APPEX_BUNDLE}/Contents/MacOS"
else
  APP_BUNDLE=""
  APPEX_BUNDLE="$(resolve_contained_in_target "appex bundle" "${OUT_DIR}/FolderSyncFinder.appex")"
  rm -rf "$APPEX_BUNDLE"
  mkdir -p "${APPEX_BUNDLE}/Contents/MacOS"
fi

echo "==> Compiling Finder Sync extension (${TARGET}, ${EXT_BUNDLE_ID})"
swiftc -parse-as-library -application-extension \
  -target "$TARGET" -sdk "$SDK_PATH" \
  -framework Cocoa -framework FinderSync -framework Foundation \
  -Xlinker -e -Xlinker _NSExtensionMain \
  -o "${APPEX_BUNDLE}/Contents/MacOS/FolderSyncFinder" \
  "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
  "${SCRIPT_DIR}/Sources/BadgeRenderer.swift" \
  "${SCRIPT_DIR}/Sources/FinderSyncExtension.swift"

cp "${SCRIPT_DIR}/Info.plist" "${APPEX_BUNDLE}/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "$EXT_BUNDLE_ID" "${APPEX_BUNDLE}/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$APP_VERSION" "${APPEX_BUNDLE}/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$APP_BUILD" "${APPEX_BUNDLE}/Contents/Info.plist"
plutil -replace FolderSyncHostBundleIdentifier -string "$HOST_BUNDLE_ID" "${APPEX_BUNDLE}/Contents/Info.plist"

if [[ -n "$REAL_SHARED_DIR" ]]; then
  plutil -replace FolderSyncSharedDir -string "$REAL_SHARED_DIR" "${APPEX_BUNDLE}/Contents/Info.plist"
  ENTITLEMENTS_PATH="${OUT_DIR}/FinderExtension.generated.entitlements"
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
  ENTITLEMENTS_PATH="${OUT_DIR}/FinderExtension.effective.entitlements"
  cp "${SCRIPT_DIR}/FinderExtension.entitlements" "$ENTITLEMENTS_PATH"
fi

if [[ -n "$ENTITLEMENTS_OUT" ]]; then
  mkdir -p "$(dirname "$ENTITLEMENTS_OUT")"
  cp "$ENTITLEMENTS_PATH" "$ENTITLEMENTS_OUT"
fi

plutil -lint "${SCRIPT_DIR}/FinderExtension.entitlements" >/dev/null
plutil -lint "${APPEX_BUNDLE}/Contents/Info.plist" >/dev/null
plutil -lint "$ENTITLEMENTS_PATH" >/dev/null

if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  echo "==> Compiling throwaway mock host app (${TARGET}, ${HOST_BUNDLE_ID})"
  swiftc -parse-as-library \
    -target "$TARGET" -sdk "$SDK_PATH" \
    -framework Cocoa -framework FinderSync -framework Foundation \
    -o "${APP_BUNDLE}/Contents/MacOS/FolderSyncHost" \
    "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
    "${SCRIPT_DIR}/Sources/HostAppMain.swift"

  cp "${SCRIPT_DIR}/HostInfo.plist" "${APP_BUNDLE}/Contents/Info.plist"
  plutil -replace CFBundleIdentifier -string "$HOST_BUNDLE_ID" "${APP_BUNDLE}/Contents/Info.plist"
  plutil -replace CFBundleShortVersionString -string "$APP_VERSION" "${APP_BUNDLE}/Contents/Info.plist"
  plutil -replace CFBundleVersion -string "$APP_BUILD" "${APP_BUNDLE}/Contents/Info.plist"
  plutil -replace FolderSyncSharedDir -string "$REAL_SHARED_DIR" "${APP_BUNDLE}/Contents/Info.plist"
  plutil -lint "${APP_BUNDLE}/Contents/Info.plist" >/dev/null
fi

if [[ "$SKIP_SIGN" -eq 0 ]]; then
  SIGN_ARGS=(--force --sign "$SIGN_IDENTITY")
  if [[ "$SIGN_IDENTITY" != "-" ]]; then
    SIGN_ARGS+=(--options runtime --timestamp)
  fi

  echo "==> Signing extension (identity: ${SIGN_IDENTITY})"
  codesign "${SIGN_ARGS[@]}" --entitlements "$ENTITLEMENTS_PATH" "$APPEX_BUNDLE"

  if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
    echo "==> Signing throwaway mock host app (identity: ${SIGN_IDENTITY})"
    codesign "${SIGN_ARGS[@]}" "$APP_BUNDLE"
    codesign --verify --deep --strict "$APP_BUNDLE"
  else
    codesign --verify --strict "$APPEX_BUNDLE"
  fi
fi

echo ""
echo "Built Finder Sync extension successfully:"
echo "  Extension:     ${APPEX_BUNDLE}"
echo "  Bundle ID:     ${EXT_BUNDLE_ID}"
echo "  Host ID:       ${HOST_BUNDLE_ID}"
if [[ -n "$REAL_SHARED_DIR" ]]; then
  echo "  Shared dir:    ${REAL_SHARED_DIR} (fresh mktemp absolute temporary exception)"
else
  echo "  Shared dir:    ~/Library/Application Support/FolderSync/finder (home-relative temporary exception)"
fi
if [[ "$WITH_MOCK_HOST" -eq 1 ]]; then
  echo "  Mock host app: ${APP_BUNDLE}"
fi
