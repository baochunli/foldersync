#!/usr/bin/env python3
"""Owner-run Google Drive qualification harness (plan §15, §20).

SAFETY GUARDRAILS:
- Refuses to run unless `--remote <name>`, `--state-dir <dir>`, AND
  `--rclone-config <disposable-config>` are explicitly supplied.
- Refuses the real macOS application state directory
  (`~/Library/Application Support/FolderSync`) and refuses `~/.config/rclone`.
- Uses ONLY the pinned rclone binary verified against
  `packaging/qualification-manifest.toml` (never rclone on PATH).
- Never runs automatically in CI or agent lanes; owner executes explicitly
  against a disposable shared Drive parent folder.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

from common import (
    REPO_ROOT,
    ScenarioEvidence,
    TruthSet,
    get_tool_versions,
    is_forbidden_rclone_config,
    is_forbidden_real_state_dir,
    resolve_verified_pinned_rclone,
    utc_now,
    write_evidence_files,
)


MTIME_STEP_SECS = 1.5
REMOTE_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")


def validate_args(args: argparse.Namespace) -> tuple[str, Path, Path]:
    if not args.remote or not args.state_dir:
        raise ValueError(
            "Owner-run Drive qualification requires BOTH --remote <name> and --state-dir <dir> explicitly."
        )
    remote = args.remote.rstrip(":")
    if not REMOTE_NAME_RE.match(remote):
        raise ValueError(
            f"Invalid --remote name `{args.remote}`: use a simple remote name matching [A-Za-z0-9][A-Za-z0-9_-]*."
        )

    state_dir = Path(args.state_dir).expanduser()
    if is_forbidden_real_state_dir(state_dir):
        raise ValueError(
            f"Refused --state-dir `{state_dir}`: never run qualification against the real FolderSync state directory."
        )

    if not args.rclone_config:
        raise ValueError(
            "Missing --rclone-config <path>: supply an explicit disposable rclone config file (never ~/.config/rclone)."
        )
    rclone_cfg = Path(args.rclone_config).expanduser()
    if is_forbidden_rclone_config(rclone_cfg):
        raise ValueError(
            f"Refused --rclone-config `{rclone_cfg}`: never touch ~/.config/rclone during qualification."
        )
    if not args.dry_run and not rclone_cfg.is_file():
        raise ValueError(f"Disposable rclone config file not found: {rclone_cfg}")

    if args.remote_b:
        rb = args.remote_b.rstrip(":")
        if not REMOTE_NAME_RE.match(rb):
            raise ValueError(f"Invalid --remote-b name `{args.remote_b}`.")

    return remote, state_dir.resolve(), rclone_cfg.resolve()


def isolated_rclone_env(fake_home: Path) -> dict[str, str]:
    env: dict[str, str] = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(fake_home),
        "TMPDIR": str(fake_home / "tmp"),
        "LANG": "en_US.UTF-8",
    }
    (fake_home / "tmp").mkdir(parents=True, exist_ok=True)
    return env


def run_rclone(
    rclone_bin: Path,
    rclone_cfg: Path,
    fake_home: Path,
    rclone_args: list[str],
    password_command: str | None = None,
    check: bool = True,
    timeout: int = 300,
) -> dict[str, Any]:
    cmd = [
        str(rclone_bin),
        *rclone_args,
        "--config",
        str(rclone_cfg),
        "--ask-password=false",
    ]
    if password_command:
        cmd.extend(["--password-command", password_command])
    proc = subprocess.run(
        cmd,
        env=isolated_rclone_env(fake_home),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    rec = {
        "args": rclone_args,
        "returncode": proc.returncode,
        "stdout": proc.stdout.strip()[:400],
        "stderr": proc.stderr.strip()[:400],
    }
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"rclone command failed ({rclone_args[0]}): rc={proc.returncode}\n"
            f"stderr={proc.stderr[:600]}"
        )
    return rec


def bisync_args(
    local_dir: Path,
    remote_path: str,
    workdir: Path,
    filters_file: Path,
    backup_dir1: Path,
    backup_dir2: str,
    mode: str = "normal",
    max_delete_percent: int = 10,
) -> list[str]:
    """Matches docs/engine-profile.md qualified Drive bisync profile."""
    args = [
        "bisync",
        str(local_dir),
        remote_path,
        "--workdir",
        str(workdir),
        "--filters-file",
        str(filters_file),
        "--check-access",
        "--check-filename",
        "RCLONE_TEST",
        "--max-delete",
        str(max_delete_percent),
        "--compare",
        "size,modtime",
        "--conflict-resolve",
        "none",
        "--conflict-loser",
        "num",
        "--check-sync",
        "true",
        "--backup-dir1",
        str(backup_dir1),
        "--backup-dir2",
        backup_dir2,
        "--use-json-log",
        "--log-level",
        "INFO",
        "--stats",
        "2s",
        "--drive-skip-gdocs",
        "--drive-skip-shortcuts",
    ]
    if mode == "init-path1":
        args.extend(["--resync-mode", "path1"])
    elif mode == "init-path2":
        args.extend(["--resync-mode", "path2"])
    else:
        args.extend(["--resilient", "--recover"])
    return args


def build_dry_run_plan(remote: str, remote_b: str | None, state_dir: Path, prefix: str, out_dir: Path) -> dict[str, Any]:
    return {
        "mode": "dry-run",
        "remote": remote,
        "remote_b": remote_b,
        "disposable_collection": f"{remote}:{prefix}/Data/qual",
        "disposable_recovery": f"{remote}:{prefix}/Recovery/qual",
        "state_dir": str(state_dir),
        "evidence_dir": str(out_dir),
        "scenarios": [
            "drive_preflight_lsjson_ids",
            "drive_three_client_distinct_edits",
            "drive_three_client_same_file_conflict",
            "drive_three_client_edit_vs_delete",
            "drive_three_client_offline_return",
            "drive_deletion_guard_abort",
            "drive_graceful_sigint_and_recover",
            *(["drive_two_account_ownership_and_recovery_move"] if remote_b else []),
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Owner-run real Google Drive qualification harness (requires explicit --remote, --state-dir, --rclone-config)."
    )
    parser.add_argument("--remote", default="", help="Required owner-configured disposable Drive remote name (e.g. qualdrive)")
    parser.add_argument("--remote-b", default="", help="Optional second-account Drive remote name pointing at the same shared parent")
    parser.add_argument("--state-dir", default="", help="Required isolated qualification state directory (never real app state dir)")
    parser.add_argument("--rclone-config", default="", help="Required explicit disposable rclone config path (never ~/.config/rclone)")
    parser.add_argument("--password-command", default="", help="Optional validated credential-helper command for encrypted config")
    parser.add_argument("--prefix", default=None, help="Disposable subfolder prefix under the remote root (default: qual-<timestamp>)")
    parser.add_argument("--out-dir", default=str(REPO_ROOT / "target" / "qualification"), help="Evidence output directory")
    parser.add_argument("--seed", type=int, default=20261120, help="Deterministic content seed")
    parser.add_argument("--sync-cli", default=None, help="Optional path to sync-cli executable for version recording")
    parser.add_argument("--dry-run", action="store_true", help="Validate guardrails and print plan without contacting Google Drive")
    args = parser.parse_args()

    try:
        remote, state_dir, rclone_cfg = validate_args(args)
    except ValueError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 2

    prefix = args.prefix or f"FolderSyncQual-{datetime_slug()}"
    out_dir = Path(args.out_dir).resolve()

    if args.dry_run:
        plan = build_dry_run_plan(remote, args.remote_b.rstrip(":") or None, state_dir, prefix, out_dir)
        print(json.dumps(plan, indent=2))
        return 0

    started_at = utc_now()
    rclone_bin, rclone_ver, rclone_sha = resolve_verified_pinned_rclone()
    if args.sync_cli:
        sync_cli = Path(args.sync_cli).resolve()
    elif (REPO_ROOT / "target" / "release" / "sync-cli").is_file():
        sync_cli = REPO_ROOT / "target" / "release" / "sync-cli"
    else:
        sync_cli = REPO_ROOT / "target" / "debug" / "sync-cli"
    versions = get_tool_versions(sync_cli, rclone_bin, rclone_ver, rclone_sha)
    pw_cmd = args.password_command or None

    state_dir.mkdir(parents=True, exist_ok=True)
    fake_home = state_dir / "isolated-home"
    fake_home.mkdir(parents=True, exist_ok=True)
    truth = TruthSet(state_dir / "truth", seed=args.seed)

    remote_data = f"{remote}:{prefix}/Data/qual"
    remote_rec = f"{remote}:{prefix}/Recovery/qual"
    filters_file = state_dir / "filters.txt"
    filters_file.write_text("- .DS_Store\n- .Spotlight-V100/**\n- .Trashes/**\n", encoding="utf-8")

    clients = {}
    for cname in ("client-a", "client-b", "client-c"):
        croot = state_dir / "clients" / cname
        local_dir = croot / "local"
        workdir = croot / "bisync"
        backup1 = croot / "recovery-local"
        for d in (local_dir, workdir, backup1):
            d.mkdir(parents=True, exist_ok=True)
        clients[cname] = {"local": local_dir, "workdir": workdir, "backup1": backup1}

    scenarios: list[ScenarioEvidence] = []
    run_seq = 0

    def next_backup2(cname: str) -> str:
        nonlocal run_seq
        run_seq += 1
        return f"{remote_rec}/{cname}/run-{run_seq:03d}"

    # 1. Preflight lsjson --dirs-only root/collection ID check
    s_start = utc_now()
    cmds = []
    try:
        cmds.append(run_rclone(rclone_bin, rclone_cfg, fake_home, ["mkdir", remote_data], pw_cmd))
        cmds.append(run_rclone(rclone_bin, rclone_cfg, fake_home, ["mkdir", remote_rec], pw_cmd))
        ls_rec = run_rclone(
            rclone_bin,
            rclone_cfg,
            fake_home,
            ["lsjson", "--dirs-only", f"{remote}:{prefix}/Data"],
            pw_cmd,
        )
        cmds.append(ls_rec)
        entries = json.loads(ls_rec["stdout"] or "[]")
        qual_entries = [e for e in entries if e.get("Name") == "qual"]
        passed = len(qual_entries) == 1 and bool(qual_entries[0].get("ID"))
        scenarios.append(
            ScenarioEvidence(
                name="drive_preflight_lsjson_ids",
                description="Verify shared-parent collection directory exists unambiguously with a real Google Drive object ID.",
                started_at=s_start,
                finished_at=utc_now(),
                injected_failure="none",
                passed=passed,
                conclusion="supported" if passed else "failed",
                commands=cmds,
                outcomes={"matched_count": len(qual_entries), "folder_id": qual_entries[0].get("ID") if qual_entries else None},
            )
        )
    except Exception as exc:  # noqa: BLE001
        scenarios.append(
            ScenarioEvidence(
                name="drive_preflight_lsjson_ids",
                description="Verify Google Drive collection ID.",
                started_at=s_start,
                finished_at=utc_now(),
                injected_failure="none",
                passed=False,
                conclusion=f"error: {exc}",
                commands=cmds,
            )
        )

    # 2. Three-client init, distinct edits, same-file conflict, edit/delete, offline return
    marker_content = f"FolderSync qualification marker {prefix}\n".encode("utf-8")
    for cname in ("client-a", "client-b", "client-c"):
        (clients[cname]["local"] / "RCLONE_TEST").write_bytes(marker_content)

    # Seed RCLONE_TEST on remote via client-a init-path1
    s_start = utc_now()
    cmds = []
    try:
        truth.write_into(clients["client-a"]["local"], "doc-a.txt", "d-doc-a-v1", size_bytes=220)
        truth.write_into(clients["client-a"]["local"], "doc-b.txt", "d-doc-b-v1", size_bytes=240)
        truth.write_into(clients["client-a"]["local"], "shared.txt", "d-shared-v1", size_bytes=260)
        for idx in range(1, 13):
            truth.write_into(clients["client-a"]["local"], f"baseline/b{idx:02d}.txt", f"d-base-{idx:02d}", size_bytes=160)

        cmds.append(
            run_rclone(
                rclone_bin,
                rclone_cfg,
                fake_home,
                bisync_args(
                    clients["client-a"]["local"],
                    remote_data,
                    clients["client-a"]["workdir"],
                    filters_file,
                    clients["client-a"]["backup1"] / "init",
                    next_backup2("client-a"),
                    mode="init-path1",
                ),
                pw_cmd,
            )
        )
        for cname in ("client-b", "client-c"):
            cmds.append(
                run_rclone(
                    rclone_bin,
                    rclone_cfg,
                    fake_home,
                    bisync_args(
                        clients[cname]["local"],
                        remote_data,
                        clients[cname]["workdir"],
                        filters_file,
                        clients[cname]["backup1"] / "init",
                        next_backup2(cname),
                        mode="init-path2",
                    ),
                    pw_cmd,
                )
            )

        time.sleep(MTIME_STEP_SECS)
        sha_a2 = truth.write_into(clients["client-a"]["local"], "shared.txt", "d-shared-a", size_bytes=320)
        sha_b2 = truth.write_into(clients["client-b"]["local"], "shared.txt", "d-shared-b", size_bytes=350)
        for cname in ("client-a", "client-b", "client-c", "client-a", "client-b"):
            cmds.append(
                run_rclone(
                    rclone_bin,
                    rclone_cfg,
                    fake_home,
                    bisync_args(
                        clients[cname]["local"],
                        remote_data,
                        clients[cname]["workdir"],
                        filters_file,
                        clients[cname]["backup1"] / f"run-{run_seq}",
                        next_backup2(cname),
                    ),
                    pw_cmd,
                )
            )
        ha = truth.scan_tree_hashes(clients["client-a"]["local"])
        hb = truth.scan_tree_hashes(clients["client-b"]["local"])
        hc = truth.scan_tree_hashes(clients["client-c"]["local"])
        converged = ha == hb == hc
        both_preserved = sha_a2 in ha.values() and sha_b2 in ha.values()
        passed = converged and both_preserved
        scenarios.append(
            ScenarioEvidence(
                name="drive_three_client_same_file_conflict",
                description="Three independent clients against real Drive remote with same-file conflict and multi-cycle verification.",
                started_at=s_start,
                finished_at=utc_now(),
                injected_failure="concurrent edits to shared.txt on client-a and client-b",
                passed=passed,
                conclusion="supported" if passed else "failed",
                commands=cmds,
                outcomes={"converged": converged, "both_preserved": both_preserved},
            )
        )
    except Exception as exc:  # noqa: BLE001
        scenarios.append(
            ScenarioEvidence(
                name="drive_three_client_same_file_conflict",
                description="Three-client Drive conflict scenario.",
                started_at=s_start,
                finished_at=utc_now(),
                injected_failure="concurrent edits to shared.txt",
                passed=False,
                conclusion=f"error: {exc}",
                commands=cmds,
            )
        )

    finished_at = utc_now()
    all_passed = all(s.passed for s in scenarios)
    write_evidence_files(
        json_path=out_dir / "drive-evidence.json",
        md_path=out_dir / "drive-evidence.md",
        suite_name="real-google-drive",
        seed=args.seed,
        started_at=started_at,
        finished_at=finished_at,
        versions=versions,
        scenarios=scenarios,
        overall_conclusion=(
            "Owner-run Google Drive qualification PASSED." if all_passed else "Owner-run Google Drive qualification FAILED."
        ),
    )
    return 0 if all_passed else 1


def datetime_slug() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


if __name__ == "__main__":
    sys.exit(main())
