#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for arg in "$@"; do
    case "$arg" in
        --only|--only=*)
            printf '%s\n' 'The nightly wrapper runs the full suite. Use run-drive.sh for --only.' >&2
            exit 2
            ;;
    esac
done
exec "${FOLDERSYNC_QUAL_PYTHON:-python3}" "${SCRIPT_DIR}/../run_drive_qualification.py" --keep 14 "$@"
