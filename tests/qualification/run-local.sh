#!/usr/bin/env bash
set -euo pipefail

# Run FolderSync's local-to-local qualification harness driving `sync-cli`
# against disposable temp directories (never touches real state or remotes).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "${SCRIPT_DIR}/run_local_qualification.py" "$@"
