#!/usr/bin/env bash
set -euo pipefail

# Owner-run Google Drive qualification harness (plan §15, §20).
# Refuses to run unless --remote <name>, --state-dir <dir>, and
# --rclone-config <disposable-config> are explicitly provided.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "${SCRIPT_DIR}/run_drive_qualification.py" "$@"
