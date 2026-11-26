#!/usr/bin/env bash
set -euo pipefail

# Owner-run Google Drive qualification using a qualification home or disposable import.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "${SCRIPT_DIR}/run_drive_qualification.py" "$@"
