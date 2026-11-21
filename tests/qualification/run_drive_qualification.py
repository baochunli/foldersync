#!/usr/bin/env python3
"""Owner-run Google Drive qualification harness (plan §15, §20).

SAFETY GUARDRAILS:
- Refuses to run unless `--remote <name>`, `--state-dir <dir>`, AND
  `--rclone-config <disposable-config>` are explicitly supplied.
- Refuses the real macOS application state directory
  (`~/Library/Application Support/FolderSync`) and refuses `~/.config/rclone`.
- Requires a fresh, empty, non-symlinked `--state-dir` and creates every
  fixture subdirectory/file exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW` /
  `dir_fd`).
- Redacts `--password-command stderr:` payloads at the capture boundary before
  storing command output, raising exceptions, or writing evidence.
- Uses ONLY the pinned rclone binary verified against
  `packaging/qualification-manifest.toml` (never rclone on PATH).
- Until `sync-cli` supports Google Drive mappings (blocked on `drive-t4` /
  `connect-t4`), executes preflight checks ONLY (account connectivity + folder-ID
  resolution) and records all unexercised Drive scenarios as `NOT RUN`; overall
  evidence status is `INCOMPLETE` (never `PASS`) and exit status is non-zero.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import stat
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from common import (
    REPO_ROOT,
    ScenarioEvidence,
    compute_overall_status,
    get_tool_versions,
    is_forbidden_rclone_config,
    is_forbidden_real_state_dir,
    redact_credential_stderr,
    resolve_verified_pinned_rclone,
    utc_now,
    write_evidence_files,
)


REMOTE_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")

PENDING_SYNC_CLI_SCENARIOS: list[tuple[str, str]] = [
    (
        "drive_three_client_distinct_edits",
        "Three independent sync-cli clients on Drive converge distinct edits across multiple cycles with truth-set SHA-256 verification.",
    ),
    (
        "drive_three_client_same_file_edit_conflict",
        "Three independent sync-cli clients on Drive preserve concurrent same-file edits as conflict copies and verify recovery copies by SHA-256.",
    ),
    (
        "drive_three_client_edit_vs_delete",
        "Edit vs delete across three sync-cli clients on Drive preserves the edit and retains pre-delete recovery copies.",
    ),
    (
        "drive_three_client_offline_return",
        "Offline third sync-cli client returns with stale edits without resurrecting deletions or losing data.",
    ),
    (
        "drive_deletion_guard_abort",
        "Deletion guard (--max-delete 10) aborts bulk deletion on Drive with zero remote deletes and latches RecoveryRequired.",
    ),
    (
        "drive_interruption_and_recover",
        "Mid-transfer SIGINT interruption and subsequent --resilient --recover cycle on Drive converge 100% of truth-set hashes.",
    ),
]


def validate_fresh_non_symlink_state_dir(raw_state_dir: str) -> Path:
    """Requires a fresh, empty, non-symlinked --state-dir (never real app state dir)."""
    expanded = os.path.expanduser(raw_state_dir)
    if not expanded or "\x00" in expanded:
        raise ValueError("Invalid --state-dir path.")

    path = Path(expanded)
    if is_forbidden_real_state_dir(path):
        raise ValueError(
            f"Refused --state-dir `{path}`: never run qualification against the real FolderSync state directory."
        )

    abs_str = os.path.abspath(expanded)
    if os.path.lexists(abs_str):
        st = os.lstat(abs_str)
        if stat.S_ISLNK(st.st_mode):
            raise ValueError(f"Refused symlinked --state-dir `{path}`.")
        if not stat.S_ISDIR(st.st_mode):
            raise ValueError(f"Refused non-directory --state-dir `{path}`.")
        if os.listdir(abs_str):
            raise ValueError(
                f"Refused non-empty --state-dir `{path}`: pass a fresh, empty directory for qualification."
            )
    else:
        # Walk up to existing parent and refuse any symlinked immediate parent inside user dirs.
        parent = os.path.dirname(abs_str)
        if not parent or not os.path.isdir(parent):
            raise ValueError(f"Parent directory does not exist for --state-dir `{path}`.")
        if os.path.islink(parent):
            raise ValueError(f"Refused symlinked parent for --state-dir `{path}`.")

    return Path(abs_str)


def open_exclusive_state_dir_fd(state_dir: Path) -> int:
    """Creates (if absent) and opens a verified fresh, empty, non-symlinked state dir fd."""
    if not os.path.lexists(state_dir):
        os.mkdir(state_dir, 0o700)
    st = os.lstat(state_dir)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        raise ValueError(f"Refused symlinked or non-directory --state-dir `{state_dir}`.")
    os.chmod(state_dir, 0o700)
    fd = os.open(str(state_dir), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        if os.listdir(fd):
            raise ValueError(f"Refused non-empty --state-dir `{state_dir}`.")
    except Exception:
        os.close(fd)
        raise
    return fd


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

    state_dir = validate_fresh_non_symlink_state_dir(args.state_dir)

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

    return remote, state_dir, rclone_cfg.resolve()


def isolated_rclone_env(fake_home: Path) -> dict[str, str]:
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(fake_home),
        "TMPDIR": str(fake_home / "tmp"),
        "LANG": "en_US.UTF-8",
    }


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
    redacted_stdout = redact_credential_stderr(proc.stdout)
    redacted_stderr = redact_credential_stderr(proc.stderr)
    rec = {
        "args": rclone_args,
        "returncode": proc.returncode,
        "stdout": redacted_stdout.strip()[:400],
        "stderr": redacted_stderr.strip()[:400],
    }
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"rclone command failed ({rclone_args[0]}): rc={proc.returncode}\n"
            f"stderr={redacted_stderr[:600]}"
        )
    return rec


def sync_cli_supports_drive_mappings(sync_cli: Path) -> bool:
    """Checks whether the built sync-cli exposes Drive mapping support."""
    if not sync_cli.is_file():
        return False
    try:
        out = subprocess.check_output(
            [str(sync_cli), "--help"],
            stderr=subprocess.STDOUT,
            text=True,
            timeout=10,
        )
    except Exception:  # noqa: BLE001
        return False
    return "--drive-remote" in out or "add-drive-mapping" in out


def build_dry_run_plan(
    remote: str,
    remote_b: str | None,
    state_dir: Path,
    prefix: str,
    out_dir: Path,
) -> dict[str, Any]:
    not_run = [name for name, _ in PENDING_SYNC_CLI_SCENARIOS]
    if remote_b:
        not_run.append("drive_two_account_ownership_and_recovery_move")
    return {
        "mode": "dry-run",
        "execution_scope": "preflight-only until sync-cli supports Google Drive mappings (drive-t4/connect-t4)",
        "overall_expected_status": "INCOMPLETE",
        "remote": remote,
        "remote_b": remote_b,
        "disposable_collection": f"{remote}:{prefix}/Data/qual",
        "disposable_recovery": f"{remote}:{prefix}/Recovery/qual",
        "state_dir": str(state_dir),
        "evidence_dir": str(out_dir),
        "preflight_scenarios": ["drive_preflight_account_and_folder_ids"],
        "not_run_scenarios": not_run,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Owner-run real Google Drive qualification harness (requires explicit --remote, --state-dir, --rclone-config)."
    )
    parser.add_argument("--remote", default="", help="Required owner-configured disposable Drive remote name (e.g. qualdrive)")
    parser.add_argument("--remote-b", default="", help="Optional second-account Drive remote name pointing at the same shared parent")
    parser.add_argument("--state-dir", default="", help="Required fresh, empty, non-symlinked qualification state directory")
    parser.add_argument("--rclone-config", default="", help="Required explicit disposable rclone config path (never ~/.config/rclone)")
    parser.add_argument("--password-command", default="", help="Optional validated credential-helper command for encrypted config")
    parser.add_argument("--prefix", default=None, help="Disposable subfolder prefix under the remote root (default: FolderSyncQual-<timestamp>)")
    parser.add_argument("--out-dir", default=str(REPO_ROOT / "target" / "qualification"), help="Evidence output directory")
    parser.add_argument("--seed", type=int, default=20261120, help="Deterministic content seed")
    parser.add_argument("--sync-cli", default=None, help="Optional path to sync-cli executable")
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

    # Open validated fresh empty non-symlinked state directory and create private dirs exclusively.
    try:
        state_fd = open_exclusive_state_dir_fd(state_dir)
    except ValueError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 2

    try:
        os.mkdir("isolated-home", 0o700, dir_fd=state_fd)
        home_fd = os.open("isolated-home", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=state_fd)
        try:
            os.mkdir("tmp", 0o700, dir_fd=home_fd)
        finally:
            os.close(home_fd)
    finally:
        os.close(state_fd)

    fake_home = state_dir / "isolated-home"
    remote_data = f"{remote}:{prefix}/Data/qual"
    remote_rec = f"{remote}:{prefix}/Recovery/qual"
    scenarios: list[ScenarioEvidence] = []

    # 1. Preflight ONLY: account connectivity + unambiguous Drive folder-ID resolution.
    s_start = utc_now()
    cmds: list[dict[str, Any]] = []
    notes: list[str] = []
    try:
        about_rec = run_rclone(rclone_bin, rclone_cfg, fake_home, ["about", f"{remote}:", "--json"], pw_cmd, check=False)
        cmds.append(about_rec)
        account_reachable = about_rec["returncode"] == 0

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
        folder_id = qual_entries[0].get("ID") if len(qual_entries) == 1 else None
        passed = account_reachable and len(qual_entries) == 1 and bool(folder_id)

        if args.remote_b:
            rb = args.remote_b.rstrip(":")
            ls_b = run_rclone(
                rclone_bin,
                rclone_cfg,
                fake_home,
                ["lsjson", "--dirs-only", f"{rb}:{prefix}/Data"],
                pw_cmd,
                check=False,
            )
            cmds.append(ls_b)
            b_entries = json.loads(ls_b["stdout"] or "[]") if ls_b["returncode"] == 0 else []
            b_qual = [e for e in b_entries if e.get("Name") == "qual"]
            b_id = b_qual[0].get("ID") if len(b_qual) == 1 else None
            notes.append(f"Second-account remote `{rb}` preflight folder ID match: {b_id == folder_id}")
            passed = passed and (b_id == folder_id)

        scenarios.append(
            ScenarioEvidence(
                name="drive_preflight_account_and_folder_ids",
                description="Verify Google Drive account connectivity and unambiguous collection folder ID resolution via rclone lsjson --dirs-only.",
                started_at=s_start,
                finished_at=utc_now(),
                injected_failure="none (preflight only)",
                passed=passed,
                status="PASS" if passed else "FAIL",
                conclusion="preflight verified (account reachable, unambiguous folder ID)" if passed else "preflight failed",
                commands=cmds,
                outcomes={
                    "account_reachable": account_reachable,
                    "matched_count": len(qual_entries),
                    "folder_id": folder_id,
                },
                notes=notes,
            )
        )
    except Exception as exc:  # noqa: BLE001
        safe_err = redact_credential_stderr(str(exc))
        scenarios.append(
            ScenarioEvidence(
                name="drive_preflight_account_and_folder_ids",
                description="Verify Google Drive account connectivity and folder ID resolution.",
                started_at=s_start,
                finished_at=utc_now(),
                injected_failure="none (preflight only)",
                passed=False,
                status="FAIL",
                conclusion=f"preflight error: {safe_err}",
                commands=cmds,
                notes=notes,
            )
        )

    # 2. Record all sync-cli Drive scenarios honestly as NOT RUN until drive-t4/connect-t4 lands.
    has_cli_drive = sync_cli_supports_drive_mappings(sync_cli)
    not_run_reason = (
        "NOT RUN: sync-cli Drive mapping commands are not wired into this harness yet; full Drive scenarios must drive sync-cli."
        if has_cli_drive
        else "NOT RUN: sync-cli does not yet support Google Drive mappings (blocked on drive-t4 / connect-t4); Drive qualification requires driving sync-cli."
    )
    now_ts = utc_now()
    for name, desc in PENDING_SYNC_CLI_SCENARIOS:
        scenarios.append(
            ScenarioEvidence(
                name=name,
                description=desc,
                started_at=now_ts,
                finished_at=now_ts,
                injected_failure="not executed (preflight-only mode)",
                passed=False,
                status="NOT RUN",
                conclusion=not_run_reason,
            )
        )
    if args.remote_b:
        scenarios.append(
            ScenarioEvidence(
                name="drive_two_account_ownership_and_recovery_move",
                description="Verify two-account file ownership, overwrite, deletion, and same-remote Recovery move via sync-cli.",
                started_at=now_ts,
                finished_at=now_ts,
                injected_failure="not executed (preflight-only mode)",
                passed=False,
                status="NOT RUN",
                conclusion=not_run_reason,
            )
        )

    finished_at = utc_now()
    overall_status = compute_overall_status(scenarios)
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
            f"Google Drive qualification status is {overall_status}: only account/folder-ID preflight "
            "was executed; sync-cli Drive scenarios are recorded as NOT RUN until drive-t4/connect-t4 lands."
        ),
    )
    for s in scenarios:
        print(f"[{s.effective_status()}] {s.name}: {s.conclusion}")
    print(f"Overall status: {overall_status} (evidence: {out_dir / 'drive-evidence.json'})")
    return 0 if overall_status == "PASS" else 3


def datetime_slug() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


if __name__ == "__main__":
    sys.exit(main())
