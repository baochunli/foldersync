#!/usr/bin/env bash
set -euo pipefail

# Package FolderSync.app for macOS into target/ using NSH 0.6.1, embedding:
#   (a) the pinned rclone binary (from .tools, verified against packaging/qualification-manifest.toml)
#       at Contents/Helpers/rclone, plus its SHA-256 manifest at Contents/Resources/rclone-manifest.json
#   (b) the Finder Sync extension at Contents/PlugIns/FolderSyncFinder.appex
#   (c) third-party license/NOTICE files at Contents/Resources/Notices/
# Then sign strictly inner -> outer (default ad hoc "-"; --sign <identity> for Developer ID)
# and verify with `codesign --verify --deep --strict`.
#
# Never installs to /Applications, never registers with pluginkit, and never touches
# real application state or the Keychain.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MANIFEST="${REPO_ROOT}/packaging/qualification-manifest.toml"

OUT_DIR="${REPO_ROOT}/target/package"
BUNDLE_ID="dev.foldersync.app"
SIGN_IDENTITY="-"
ARCH="$(uname -m)"
NSH_BIN="${NSH_BIN:-$HOME/.local/bin/nsh}"
SHARED_DIR=""

usage() {
  cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --out <dir>         Output directory for FolderSync.app (default: <repo>/target/package)
  --bundle-id <id>    Host app bundle identifier (default: dev.foldersync.app;
                      extension uses <id>.finder-extension)
  --sign <identity>   codesign identity (default: "-" for ad hoc signing;
                      non-"-" enables --options runtime --timestamp)
  --arch <arch>       Target architecture: arm64 or x86_64 (default: host arch)
  --nsh-bin <path>    Path to pinned NSH 0.6.1 CLI (default: \$HOME/.local/bin/nsh)
  --shared-dir <dir>  Optional dev override for Finder extension shared handoff dir
                      (default: ~/Library/Application Support/FolderSync/finder via
                      home-relative temporary file exception)
  -h, --help          Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) OUT_DIR="$2"; shift 2 ;;
    --bundle-id) BUNDLE_ID="$2"; shift 2 ;;
    --sign) SIGN_IDENTITY="$2"; shift 2 ;;
    --arch) ARCH="$2"; shift 2 ;;
    --nsh-bin) NSH_BIN="$2"; shift 2 ;;
    --shared-dir) SHARED_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$OUT_DIR" in
  /Applications|/Applications/*|/System|/System/*|/Library|/Library/*|"$HOME/Applications"|"$HOME/Applications"/*)
    echo "Error: refusing to package directly into a system/Applications directory: $OUT_DIR" >&2
    exit 2
    ;;
esac

case "$ARCH" in
  arm64) RCLONE_PLATFORM="osx-arm64" ;;
  x86_64) RCLONE_PLATFORM="osx-amd64" ;;
  *) echo "Error: unsupported macOS --arch '$ARCH' (expected arm64 or x86_64)" >&2; exit 2 ;;
esac

for tool in codesign plutil shasum swiftc xcrun python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Error: required macOS tool not found: $tool" >&2
    exit 1
  fi
done

if [[ ! -x "$NSH_BIN" ]]; then
  echo "Error: pinned NSH binary not found or not executable at $NSH_BIN" >&2
  echo "Install NSH 0.6.1 via: curl -fsSL https://nsh.dev/install.sh | NSH_VERSION=v0.6.1 sh" >&2
  exit 1
fi

read_pin() {
  local value
  value=$(awk -F '"' -v section="[$1]" -v key="$2 = " '
      /^\[/ { selected = ($0 == section) }
      selected && $1 == key { print $2; exit }
  ' "$MANIFEST")
  if [[ -z "$value" ]]; then
    echo "Error: missing $1.$2 in $MANIFEST" >&2
    return 1
  fi
  printf '%s\n' "$value"
}

EXPECTED_NSH_VERSION="$(read_pin nsh version)"
ACTUAL_NSH_VERSION="$("$NSH_BIN" --version)"
if [[ "$ACTUAL_NSH_VERSION" != "nsh ${EXPECTED_NSH_VERSION}"* ]]; then
  echo "Error: expected NSH ${EXPECTED_NSH_VERSION} at ${NSH_BIN}, got '${ACTUAL_NSH_VERSION}'" >&2
  exit 1
fi

RCLONE_VERSION="$(read_pin rclone version)"
EXPECTED_RCLONE_SHA="$(read_pin "rclone.${RCLONE_PLATFORM}" binary_sha256)"
RCLONE_SRC="${REPO_ROOT}/.tools/rclone/${RCLONE_VERSION}/rclone"

if [[ -L "$RCLONE_SRC" ]]; then
  echo "Error: refusing symlinked rclone binary at $RCLONE_SRC" >&2
  exit 1
fi
if [[ ! -f "$RCLONE_SRC" || ! -x "$RCLONE_SRC" ]]; then
  echo "Error: pinned rclone ${RCLONE_VERSION} not found at ${RCLONE_SRC}; run scripts/fetch-rclone.sh first." >&2
  exit 1
fi

ACTUAL_RCLONE_SHA="$(shasum -a 256 "$RCLONE_SRC" | awk '{print $1}')"
if [[ "$ACTUAL_RCLONE_SHA" != "$EXPECTED_RCLONE_SHA" ]]; then
  echo "Error: rclone SHA-256 mismatch for ${RCLONE_SRC} (${RCLONE_PLATFORM}):" >&2
  echo "  expected: ${EXPECTED_RCLONE_SHA}" >&2
  echo "  actual:   ${ACTUAL_RCLONE_SHA}" >&2
  exit 1
fi
echo "==> Verified pinned rclone ${RCLONE_VERSION} (${RCLONE_PLATFORM}) SHA-256: ${ACTUAL_RCLONE_SHA}"

mkdir -p "$OUT_DIR"
WORK_STAGE="$(mktemp -d "${OUT_DIR}/.stage.XXXXXXXX")"
trap 'rm -rf "$WORK_STAGE"' EXIT

echo "==> Packaging NSH macOS app (${BUNDLE_ID}, arch=${ARCH})"
"$NSH_BIN" package \
  --target macos \
  --id "$BUNDLE_ID" \
  --arch "$ARCH" \
  --out "${WORK_STAGE}/nsh-out" \
  "${REPO_ROOT}/apps/sync-ui"

STAGED_APP="${WORK_STAGE}/nsh-out/FolderSync.app"
if [[ ! -d "$STAGED_APP" ]]; then
  echo "Error: expected packaged bundle at ${STAGED_APP}" >&2
  exit 1
fi

APP_VERSION="$(plutil -extract CFBundleShortVersionString raw "${STAGED_APP}/Contents/Info.plist")"
APP_BUILD="$(plutil -extract CFBundleVersion raw "${STAGED_APP}/Contents/Info.plist")"

echo "==> Embedding pinned rclone at Contents/Helpers/rclone"
mkdir -p "${STAGED_APP}/Contents/Helpers"
cp "$RCLONE_SRC" "${STAGED_APP}/Contents/Helpers/rclone"
chmod 755 "${STAGED_APP}/Contents/Helpers/rclone"

echo "==> Building and embedding Finder Sync extension into Contents/PlugIns/"
EXT_STAGE="${WORK_STAGE}/finder-ext"
EXT_ENTITLEMENTS="${WORK_STAGE}/FinderExtension.entitlements"
EXT_ARGS=(
  --appex-only
  --no-sign
  --bundle-id "$BUNDLE_ID"
  --version "$APP_VERSION"
  --build "$APP_BUILD"
  --arch "$ARCH"
  --out "$EXT_STAGE"
  --entitlements-out "$EXT_ENTITLEMENTS"
)
if [[ -n "$SHARED_DIR" ]]; then
  EXT_ARGS+=(--shared-dir "$SHARED_DIR")
fi
bash "${REPO_ROOT}/apps/finder-extension/build.sh" "${EXT_ARGS[@]}"

mkdir -p "${STAGED_APP}/Contents/PlugIns"
cp -R "${EXT_STAGE}/FolderSyncFinder.appex" "${STAGED_APP}/Contents/PlugIns/FolderSyncFinder.appex"

echo "==> Embedding third-party NOTICE and license files in Contents/Resources/Notices/"
NOTICES_DIR="${STAGED_APP}/Contents/Resources/Notices"
mkdir -p "$NOTICES_DIR"
cp "${REPO_ROOT}/NOTICE" "${NOTICES_DIR}/NOTICE.txt"
cp "${REPO_ROOT}/LICENSE" "${NOTICES_DIR}/FOLDERSYNC-LICENSE.txt"
cp "${REPO_ROOT}/packaging/notices/NSH-LICENSE-APACHE-2.0.txt" "${NOTICES_DIR}/NSH-LICENSE-APACHE-2.0.txt"
cp "${REPO_ROOT}/packaging/notices/RCLONE-LICENSE-MIT.txt" "${NOTICES_DIR}/RCLONE-LICENSE-MIT.txt"
cp "${REPO_ROOT}/apps/sync-ui/assets/fonts/OFL.txt" "${NOTICES_DIR}/PAPER-MONO-OFL.txt"

for notice_file in \
  NOTICE.txt \
  FOLDERSYNC-LICENSE.txt \
  NSH-LICENSE-APACHE-2.0.txt \
  RCLONE-LICENSE-MIT.txt \
  PAPER-MONO-OFL.txt; do
  if [[ ! -s "${NOTICES_DIR}/${notice_file}" ]]; then
    echo "Error: missing or empty notice file: ${NOTICES_DIR}/${notice_file}" >&2
    exit 1
  fi
done

SIGN_ARGS=(--force --sign "$SIGN_IDENTITY")
SIGN_MODE="adhoc"
if [[ "$SIGN_IDENTITY" != "-" ]]; then
  SIGN_ARGS+=(--options runtime --timestamp)
  SIGN_MODE="developer-id"
fi

echo "==> Signing inner -> outer (identity: ${SIGN_IDENTITY}, mode: ${SIGN_MODE})"
# 1. Sign nested rclone helper executable first.
codesign "${SIGN_ARGS[@]}" "${STAGED_APP}/Contents/Helpers/rclone"

# Record both the verified upstream manifest SHA-256 and the post-signing bundled
# SHA-256 inside Contents/Resources/rclone-manifest.json before sealing the outer app.
BUNDLED_RCLONE_SHA="$(shasum -a 256 "${STAGED_APP}/Contents/Helpers/rclone" | awk '{print $1}')"
python3 - "$STAGED_APP" "$RCLONE_VERSION" "$RCLONE_PLATFORM" "$EXPECTED_RCLONE_SHA" "$BUNDLED_RCLONE_SHA" "$SIGN_MODE" <<'PY'
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

staged_app, ver, plat, upstream_sha, bundled_sha, sign_mode = sys.argv[1:7]
manifest_out = Path(staged_app) / "Contents" / "Resources" / "rclone-manifest.json"
payload = {
    "version": ver,
    "platform": plat,
    "bundle_relative_path": "Contents/Helpers/rclone",
    "upstream_manifest_sha256": upstream_sha,
    "bundled_sha256": bundled_sha,
    "signing_mode": sign_mode,
    "packaged_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
}
manifest_out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY

# 2. Sign embedded Finder Sync extension with its sandbox/handoff entitlements.
codesign "${SIGN_ARGS[@]}" \
  --entitlements "$EXT_ENTITLEMENTS" \
  "${STAGED_APP}/Contents/PlugIns/FolderSyncFinder.appex"

# 3. Sign outer FolderSync.app bundle last (without --deep so inner entitlements stay intact).
codesign "${SIGN_ARGS[@]}" "$STAGED_APP"

FINAL_APP="${OUT_DIR}/FolderSync.app"
rm -rf "$FINAL_APP"
mv "$STAGED_APP" "$FINAL_APP"

echo "==> Verifying code signature (codesign --verify --deep --strict)"
codesign --verify --deep --strict --verbose=2 "$FINAL_APP"

echo "==> Bundle contents:"
find "$FINAL_APP" -maxdepth 4 | sort

echo ""
echo "Packaged and verified successfully:"
echo "  App bundle:    ${FINAL_APP}"
echo "  Bundle ID:     ${BUNDLE_ID} (${APP_VERSION} / build ${APP_BUILD})"
echo "  Extension ID:  ${BUNDLE_ID}.finder-extension"
echo "  Signing mode:  ${SIGN_MODE} (${SIGN_IDENTITY})"
echo "  rclone path:   Contents/Helpers/rclone (v${RCLONE_VERSION})"
echo "  rclone sha256: upstream=${EXPECTED_RCLONE_SHA}"
echo "                 bundled=${BUNDLED_RCLONE_SHA}"
echo "  Notices:       Contents/Resources/Notices/ (5 files)"
