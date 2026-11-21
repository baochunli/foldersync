#!/usr/bin/env bash
set -euo pipefail

# Submit a Developer-ID-signed FolderSync.app bundle to Apple's notarization
# service using `xcrun notarytool`, staple the ticket, and assess with `spctl`.
#
# Credentials are NEVER stored in this repository or passed on the command line:
# the owner creates a named keychain profile locally once via
# `xcrun notarytool store-credentials <profile-name> ...` and passes only the
# profile NAME to this script via `--keychain-profile <name>`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

APP_PATH="${REPO_ROOT}/target/package/FolderSync.app"
KEYCHAIN_PROFILE=""
OUT_ZIP=""
DRY_RUN=0

usage() {
  cat <<EOF
Usage: $(basename "$0") --keychain-profile <name> [options]

Options:
  --app <path>                Path to Developer-ID-signed FolderSync.app
                              (default: <repo>/target/package/FolderSync.app)
  --keychain-profile <name>   Required notarytool Keychain profile NAME created by the owner
                              (never pass raw Apple IDs or passwords)
  --out-zip <path>            Output path for the notarization archive
                              (default: <repo>/target/package/FolderSync-notarize.zip)
  --dry-run                   Validate bundle signature and print commands without calling
                              notarytool, stapler, or touching the Keychain
  -h, --help                  Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app) APP_PATH="$2"; shift 2 ;;
    --keychain-profile) KEYCHAIN_PROFILE="$2"; shift 2 ;;
    --out-zip) OUT_ZIP="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$KEYCHAIN_PROFILE" ]]; then
  echo "Error: --keychain-profile <name> is required." >&2
  usage >&2
  exit 2
fi

if [[ "$KEYCHAIN_PROFILE" =~ [[:space:]] ]]; then
  echo "Error: --keychain-profile must be a single profile name without whitespace." >&2
  exit 2
fi

if [[ ! -d "$APP_PATH" || "${APP_PATH##*.}" != "app" ]]; then
  echo "Error: app bundle not found or not a .app directory: $APP_PATH" >&2
  exit 1
fi

for tool in codesign ditto xcrun spctl; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Error: required macOS tool not found: $tool" >&2
    exit 1
  fi
done

echo "==> Verifying deep strict code signature on ${APP_PATH}"
codesign --verify --deep --strict --verbose=2 "$APP_PATH"

SIG_INFO="$(codesign -dv --verbose=4 "$APP_PATH" 2>&1)"
if grep -q "Signature=adhoc" <<< "$SIG_INFO"; then
  echo "Error: ${APP_PATH} is ad hoc signed; rebuild with packaging/build-app.sh --sign \"Developer ID Application: ...\" before notarizing." >&2
  exit 1
fi

if ! grep -q "runtime" <<< "$SIG_INFO"; then
  echo "Error: ${APP_PATH} does not have Hardened Runtime enabled; rebuild with --sign \"Developer ID Application: ...\"." >&2
  exit 1
fi

if [[ -z "$OUT_ZIP" ]]; then
  OUT_ZIP="${REPO_ROOT}/target/package/FolderSync-notarize.zip"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "==> Dry run OK (notarytool and Keychain were NOT invoked)"
  echo "Would run:"
  echo "  ditto -c -k --keepParent \"${APP_PATH}\" \"${OUT_ZIP}\""
  echo "  xcrun notarytool submit \"${OUT_ZIP}\" --keychain-profile \"${KEYCHAIN_PROFILE}\" --wait"
  echo "  xcrun stapler staple \"${APP_PATH}\""
  echo "  xcrun stapler validate \"${APP_PATH}\""
  echo "  spctl --assess --type execute --verbose=4 \"${APP_PATH}\""
  exit 0
fi

mkdir -p "$(dirname "$OUT_ZIP")"
rm -f "$OUT_ZIP"

echo "==> Creating notarization archive at ${OUT_ZIP}"
ditto -c -k --keepParent "$APP_PATH" "$OUT_ZIP"

echo "==> Submitting to Apple notary service (keychain profile: ${KEYCHAIN_PROFILE})"
xcrun notarytool submit "$OUT_ZIP" --keychain-profile "$KEYCHAIN_PROFILE" --wait

echo "==> Stapling notarization ticket to ${APP_PATH}"
xcrun stapler staple "$APP_PATH"
xcrun stapler validate "$APP_PATH"

echo "==> Assessing Gatekeeper acceptance"
spctl --assess --type execute --verbose=4 "$APP_PATH"

echo "Notarization and stapling completed for ${APP_PATH}"
