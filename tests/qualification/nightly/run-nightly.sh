#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${FOLDERSYNC_QUAL_PYTHON:-python3}" "${SCRIPT_DIR}/../run_drive_qualification.py" --keep 14 "$@"
