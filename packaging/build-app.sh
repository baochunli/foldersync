#!/usr/bin/env bash
set -euo pipefail

# Package FolderSync.app for macOS into <repo>/target/ using NSH 0.6.1, embedding:
#   (a) the pinned rclone binary (from .tools, verified against packaging/qualification-manifest.toml)
#       at Contents/Helpers/rclone, plus its SHA-256 manifest at Contents/Resources/rclone-manifest.json
#   (b) the Finder Sync extension at Contents/PlugIns/FolderSyncFinder.appex
#   (c) verbatim third-party license/NOTICE files at Contents/Resources/Notices/
# Then sign strictly inner -> outer (default ad hoc "-"; --sign <identity> for Developer ID)
# and verify with `codesign --verify --deep --strict`.
#
# Never writes or deletes outside <repo>/target/, never installs to /Applications,
# never registers with pluginkit, and never touches real application state or Keychain.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
MANIFEST="${REPO_ROOT}/packaging/qualification-manifest.toml"

OUT_DIR="${REPO_ROOT}/target/package"
BUNDLE_ID="dev.foldersync.app"
SIGN_IDENTITY="-"
ARCH="$(uname -m)"
NSH_BIN="${NSH_BIN:-$HOME/.local/bin/nsh}"
DEV_SHARED_DIR=0

usage() {
  cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --out <dir>         Output directory for FolderSync.app; must resolve physically inside
                      <repo>/target/ with no symlinked components or control characters
                      (default: <repo>/target/package)
  --bundle-id <id>    Host app bundle identifier (default: dev.foldersync.app;
                      extension uses <id>.finder-extension)
  --sign <identity>   codesign identity (default: "-" for ad hoc signing;
                      non-"-" enables --options runtime --timestamp)
  --arch <arch>       Target architecture: arm64 or x86_64 (default: host arch)
  --nsh-bin <path>    Path to pinned NSH 0.6.1 CLI (default: \$HOME/.local/bin/nsh)
  --dev-shared-dir    Create a fresh disposable Finder handoff directory via mktemp under
                      <repo>/target/ instead of using the production home-relative exception
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
    --dev-shared-dir) DEV_SHARED_DIR=1; shift ;;
    --shared-dir)
      echo "Error: arbitrary --shared-dir paths are not permitted; use --dev-shared-dir to create a fresh directory under target/." >&2
      exit 2
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

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

# Validate a path in-place (NO command substitution, which strips trailing newlines):
# rejects newlines/control characters up front, verifies <repo>/target is not a symlink,
# verifies physical containment inside <repo>/target/, and rejects any symlinked
# component from target/ down before any write, mkdir, mktemp, or rm.
validate_contained_in_target() {
  python3 - "$REPO_ROOT" "$1" "$2" <<'PY'
import os
import stat
import sys

repo_root = os.path.realpath(sys.argv[1])
label = sys.argv[2]
raw_path = sys.argv[3]

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
PY
}

validate_contained_in_target "target" "${REPO_ROOT}/target"
validate_contained_in_target "--out" "$OUT_DIR"

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
RCLONE_ARCHIVE_SHA="$(read_pin "rclone.${RCLONE_PLATFORM}" archive_sha256)"
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
if [[ "$DEV_SHARED_DIR" -eq 1 ]]; then
  EXT_ARGS+=(--dev-shared-dir)
fi
bash "${REPO_ROOT}/apps/finder-extension/build.sh" "${EXT_ARGS[@]}"

mkdir -p "${STAGED_APP}/Contents/PlugIns"
cp -R "${EXT_STAGE}/FolderSyncFinder.appex" "${STAGED_APP}/Contents/PlugIns/FolderSyncFinder.appex"

echo "==> Embedding verbatim third-party NOTICE and license files in Contents/Resources/Notices/"
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
# 1. Sign the verified staged helper with hardened runtime in both modes.
HELPER="${STAGED_APP}/Contents/Helpers/rclone"
if [[ "$(shasum -a 256 "$HELPER" | awk '{print $1}')" != "$EXPECTED_RCLONE_SHA" ]]; then
  echo "Error: staged rclone no longer matches the pinned upstream binary." >&2
  exit 1
fi
HELPER_SIGN_ARGS=("${SIGN_ARGS[@]}")
if [[ "$SIGN_MODE" == "adhoc" ]]; then
  HELPER_SIGN_ARGS+=(--options runtime)
fi
codesign "${HELPER_SIGN_ARGS[@]}" "$HELPER"

# Seal the upstream archive and binary pins and the post-signing helper digest.
BUNDLED_RCLONE_SHA="$(shasum -a 256 "$HELPER" | awk '{print $1}')"
python3 - "$STAGED_APP" "$RCLONE_VERSION" "$RCLONE_PLATFORM" "$RCLONE_ARCHIVE_SHA" "$EXPECTED_RCLONE_SHA" "$BUNDLED_RCLONE_SHA" "$SIGN_MODE" <<'PY'
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

staged_app, ver, plat, archive_sha, upstream_sha, bundled_sha, sign_mode = sys.argv[1:]
manifest_out = Path(staged_app) / "Contents" / "Resources" / "rclone-manifest.json"
payload = {
    "version": ver,
    "platform": plat,
    "bundle_relative_path": "Contents/Helpers/rclone",
    "upstream_archive_sha256": archive_sha,
    "upstream_manifest_sha256": upstream_sha,
    "bundled_sha256": bundled_sha,
    "signing_mode": sign_mode,
    "packaged_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
}
with manifest_out.open("x", encoding="utf-8") as output:
    output.write(json.dumps(payload, indent=2) + "\n")
PY

# 2. Sign embedded Finder Sync extension with its sandbox/handoff entitlements.
codesign "${SIGN_ARGS[@]}" \
  --entitlements "$EXT_ENTITLEMENTS" \
  "${STAGED_APP}/Contents/PlugIns/FolderSyncFinder.appex"

# 3. Sign outer FolderSync.app bundle last (without --deep so inner entitlements stay intact).
codesign "${SIGN_ARGS[@]}" "$STAGED_APP"

echo "==> Verifying staged bundle signature (codesign --verify --deep --strict)"
codesign --verify --deep --strict --verbose=2 "$STAGED_APP"

FINAL_APP="${OUT_DIR}/FolderSync.app"
validate_contained_in_target "final app bundle" "$FINAL_APP"
rm -rf "$FINAL_APP"
mv "$STAGED_APP" "$FINAL_APP"

codesign --verify --deep --strict "$FINAL_APP"

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
