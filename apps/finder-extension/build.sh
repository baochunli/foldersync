#!/usr/bin/env bash
set -euo pipefail

# Build FolderSync's macOS Finder Sync extension (.appex) and a minimal throwaway
# host app bundle using Xcode Command Line Tools only (swiftc + codesign + plutil).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

OUT_DIR="${REPO_ROOT}/target/finder-extension"
SHARED_DIR=""
HOST_BUNDLE_ID="dev.foldersync.app"
SIGN_IDENTITY="-"
ARCH="$(uname -m)"
MIN_MACOS="14.0"
RUN_TESTS=0

usage() {
  cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --out <dir>         Output directory for the built .app and .appex
                      (default: <repo>/target/finder-extension)
  --shared-dir <dir>  Override the shared snapshot/action directory for local
                      development or temp-dir verification (never touches
                      ~/Library/Application Support/FolderSync).
  --bundle-id <id>    Host bundle identifier (default: dev.foldersync.app;
                      extension uses <id>.finder-extension)
  --sign <identity>   codesign identity (default: "-" for ad hoc signing)
  --arch <arch>       Target architecture (default: host arch, e.g. arm64)
  --test              Compile and run SnapshotModel unit tests before building
  -h, --help          Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) OUT_DIR="$2"; shift 2 ;;
    --shared-dir) SHARED_DIR="$2"; shift 2 ;;
    --bundle-id) HOST_BUNDLE_ID="$2"; shift 2 ;;
    --sign) SIGN_IDENTITY="$2"; shift 2 ;;
    --arch) ARCH="$2"; shift 2 ;;
    --test) RUN_TESTS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for tool in swiftc codesign plutil xcrun; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Error: required Command Line Tools binary not found: $tool" >&2
    exit 1
  fi
done

SDK_PATH="$(xcrun --show-sdk-path)"
TARGET="${ARCH}-apple-macos${MIN_MACOS}"
EXT_BUNDLE_ID="${HOST_BUNDLE_ID}.finder-extension"

mkdir -p "$OUT_DIR"

if [[ "$RUN_TESTS" -eq 1 ]]; then
  echo "==> Running SnapshotModelTests"
  TEST_BIN="${OUT_DIR}/SnapshotModelTests"
  swiftc -parse-as-library \
    -target "$TARGET" -sdk "$SDK_PATH" \
    "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
    "${SCRIPT_DIR}/Tests/SnapshotModelTests.swift" \
    -o "$TEST_BIN"
  "$TEST_BIN"
fi

APP_BUNDLE="${OUT_DIR}/FolderSyncHost.app"
APPEX_BUNDLE="${APP_BUNDLE}/Contents/PlugIns/FolderSyncFinder.appex"

rm -rf "$APP_BUNDLE"
mkdir -p "${APP_BUNDLE}/Contents/MacOS" "${APPEX_BUNDLE}/Contents/MacOS"

echo "==> Compiling Finder Sync extension (${TARGET})"
swiftc -parse-as-library -application-extension \
  -target "$TARGET" -sdk "$SDK_PATH" \
  -framework Cocoa -framework FinderSync -framework Foundation \
  -Xlinker -e -Xlinker _NSExtensionMain \
  -o "${APPEX_BUNDLE}/Contents/MacOS/FolderSyncFinder" \
  "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
  "${SCRIPT_DIR}/Sources/BadgeRenderer.swift" \
  "${SCRIPT_DIR}/Sources/FinderSyncExtension.swift"

echo "==> Compiling throwaway host app (${TARGET})"
swiftc -parse-as-library \
  -target "$TARGET" -sdk "$SDK_PATH" \
  -framework Cocoa -framework FinderSync -framework Foundation \
  -o "${APP_BUNDLE}/Contents/MacOS/FolderSyncHost" \
  "${SCRIPT_DIR}/Sources/SnapshotModel.swift" \
  "${SCRIPT_DIR}/Sources/HostAppMain.swift"

cp "${SCRIPT_DIR}/Info.plist" "${APPEX_BUNDLE}/Contents/Info.plist"
cp "${SCRIPT_DIR}/HostInfo.plist" "${APP_BUNDLE}/Contents/Info.plist"

plutil -replace CFBundleIdentifier -string "$HOST_BUNDLE_ID" "${APP_BUNDLE}/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "$EXT_BUNDLE_ID" "${APPEX_BUNDLE}/Contents/Info.plist"
plutil -replace FolderSyncHostBundleIdentifier -string "$HOST_BUNDLE_ID" "${APPEX_BUNDLE}/Contents/Info.plist"

ENTITLEMENTS_PATH="${OUT_DIR}/FinderExtension.generated.entitlements"
if [[ -n "$SHARED_DIR" ]]; then
  # Resolve symlinks (e.g. /var -> /private/var) so the sandbox exception matches both forms.
  mkdir -p "$SHARED_DIR"
  REAL_SHARED_DIR="$(cd "$SHARED_DIR" && pwd -P)"
  plutil -replace FolderSyncSharedDir -string "$REAL_SHARED_DIR" "${APPEX_BUNDLE}/Contents/Info.plist"
  plutil -replace FolderSyncSharedDir -string "$REAL_SHARED_DIR" "${APP_BUNDLE}/Contents/Info.plist"

  cat > "$ENTITLEMENTS_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <true/>
    <key>com.apple.security.temporary-exception.files.absolute-path.read-write</key>
    <array>
        <string>${SHARED_DIR%/}/</string>
        <string>${REAL_SHARED_DIR%/}/</string>
    </array>
</dict>
</plist>
EOF
else
  cp "${SCRIPT_DIR}/FinderExtension.entitlements" "$ENTITLEMENTS_PATH"
fi

plutil -lint "${APPEX_BUNDLE}/Contents/Info.plist" >/dev/null
plutil -lint "${APP_BUNDLE}/Contents/Info.plist" >/dev/null
plutil -lint "$ENTITLEMENTS_PATH" >/dev/null

echo "==> Signing inner .appex then outer .app (identity: ${SIGN_IDENTITY})"
codesign --force --sign "$SIGN_IDENTITY" --entitlements "$ENTITLEMENTS_PATH" "$APPEX_BUNDLE"
codesign --force --sign "$SIGN_IDENTITY" "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"

echo ""
echo "Built and signed successfully:"
echo "  Host app:  ${APP_BUNDLE}"
echo "  Extension: ${APPEX_BUNDLE}"
echo "  Bundle ID: ${EXT_BUNDLE_ID}"
echo ""
echo "Local dev registration (temp folder verification only):"
echo "  pluginkit -a \"${APPEX_BUNDLE}\""
echo "  pluginkit -e use -i \"${EXT_BUNDLE_ID}\""
echo ""
echo "Cleanup when done:"
echo "  pluginkit -e ignore -i \"${EXT_BUNDLE_ID}\""
echo "  pluginkit -r \"${APPEX_BUNDLE}\""
echo "  rm -rf \"${OUT_DIR}\""
