#!/usr/bin/env python3
"""Run owner-authorized Google Drive scenarios in isolated FolderSync client states."""

from __future__ import annotations

import argparse
import configparser
import json
import os
import re
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from collections import Counter
from contextlib import closing
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from common import (
    REPO_ROOT,
    ScenarioEvidence,
    TruthSet,
    compute_overall_status,
    get_tool_versions,
    is_forbidden_rclone_config,
    is_forbidden_real_state_dir,
    redact_credential_stderr,
    resolve_verified_pinned_rclone,
    stop_owned_process,
    utc_now,
    write_evidence_files,
)

REMOTE_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}")
DRIVE_ID_RE = re.compile(r"[A-Za-z0-9_-]{1,255}")
CLI_TIMEOUT = 750  # Manager allows up to 600 s for a sync plus Drive preflight.
DRIVE_TIMEOUT = 180
COLLECTION_VISIBILITY_TIMEOUT = 60
BATCH_COUNT = 24

SCENARIOS = {
    "preflight": "drive_preflight_account_and_folder_ids",
    "distinct": "drive_three_client_distinct_edits",
    "conflict": "drive_three_client_same_file_edit_conflict",
    "edit-delete": "drive_three_client_edit_vs_delete",
    "offline": "drive_three_client_offline_return",
    "guard": "drive_deletion_guard_abort",
    "interrupt": "drive_interruption",
    "two-account": "drive_two_account_ownership_and_recovery_move",
}


def _normalize_os_root_alias(abs_path: str) -> str:
    """Normalizes only macOS root OS aliases (/var -> /private/var, /tmp -> /private/tmp)."""
    for alias, target in (("/var", "/private/var"), ("/tmp", "/private/tmp")):
        if abs_path == alias or abs_path.startswith(alias + "/"):
            try:
                link_dest = os.readlink(alias)
            except OSError:
                continue
            if link_dest in (target.lstrip("/"), target):
                return target + abs_path[len(alias) :]
    return abs_path


def _open_parent_fd_no_symlinks_from_root(abs_path: str) -> tuple[int, str, str]:
    """Opens every ancestor component from `/` with `O_DIRECTORY | O_NOFOLLOW`.

    Returns `(parent_fd, leaf_name, normalized_path)`. Refuses any symlinked
    ancestor component anywhere from `/` down.
    """
    normalized = _normalize_os_root_alias(abs_path)
    parts = [p for p in normalized.split("/") if p]
    if not parts:
        raise ValueError(f"Refused filesystem root as --state-dir `{abs_path}`.")
    for p in parts:
        if p in (".", ".."):
            raise ValueError(f"Refused traversal component `{p}` in --state-dir `{abs_path}`.")

    cur_fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for comp in parts[:-1]:
            try:
                next_fd = os.open(comp, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=cur_fd)
            except OSError as exc:
                import errno as errnos

                if exc.errno in (errnos.ELOOP, errnos.ENOTDIR):
                    try:
                        if stat.S_ISLNK(os.lstat(comp, dir_fd=cur_fd).st_mode):
                            raise ValueError(
                                f"Refused symlinked ancestor `{comp}` in --state-dir `{abs_path}`."
                            ) from exc
                    except OSError:
                        pass
                    if exc.errno == errnos.ELOOP:
                        raise ValueError(
                            f"Refused symlinked ancestor `{comp}` in --state-dir `{abs_path}`."
                        ) from exc
                    raise ValueError(
                        f"Non-directory ancestor `{comp}` in --state-dir `{abs_path}`."
                    ) from exc
                if exc.errno == errnos.ENOENT:
                    raise ValueError(
                        f"Missing ancestor directory `{comp}` for --state-dir `{abs_path}`."
                    ) from exc
                raise ValueError(
                    f"Cannot validate ancestor `{comp}` in --state-dir `{abs_path}`: {exc}"
                ) from exc
            os.close(cur_fd)
            cur_fd = next_fd
    except Exception:
        os.close(cur_fd)
        raise
    return cur_fd, parts[-1], "/" + "/".join(parts)


def validate_fresh_non_symlink_state_dir(raw_state_dir: str) -> Path:
    """Requires a fresh, empty, non-symlinked --state-dir validated from `/` via O_NOFOLLOW."""
    expanded = os.path.expanduser(raw_state_dir)
    if not expanded or any(ord(c) < 32 or ord(c) == 127 for c in expanded):
        raise ValueError("Invalid --state-dir path (empty or contains control characters).")

    path = Path(expanded)
    if is_forbidden_real_state_dir(path):
        raise ValueError(
            f"Refused --state-dir `{path}`: never run qualification against the real FolderSync state directory."
        )

    abs_str = os.path.abspath(expanded)
    parent_fd, leaf, normalized = _open_parent_fd_no_symlinks_from_root(abs_str)
    try:
        try:
            st = os.lstat(leaf, dir_fd=parent_fd)
        except FileNotFoundError:
            return Path(normalized)
        if stat.S_ISLNK(st.st_mode):
            raise ValueError(f"Refused symlinked --state-dir `{path}`.")
        if not stat.S_ISDIR(st.st_mode):
            raise ValueError(f"Refused non-directory --state-dir `{path}`.")
        leaf_fd = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent_fd)
        try:
            if os.listdir(leaf_fd):
                raise ValueError(
                    f"Refused non-empty --state-dir `{path}`: pass a fresh, empty directory for qualification."
                )
        finally:
            os.close(leaf_fd)
    finally:
        os.close(parent_fd)

    return Path(normalized)


def open_exclusive_state_dir_fd(state_dir: Path) -> int:
    """Validates every ancestor from `/` with O_NOFOLLOW, creates/opens leaf via dir_fd,
    applies `fchmod(0o700)` on the checked fd, and verifies the directory is empty."""
    abs_str = os.path.abspath(os.path.expanduser(str(state_dir)))
    parent_fd, leaf, _ = _open_parent_fd_no_symlinks_from_root(abs_str)
    try:
        try:
            os.mkdir(leaf, 0o700, dir_fd=parent_fd)
        except FileExistsError:
            pass
        try:
            fd = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent_fd)
        except OSError as exc:
            raise ValueError(f"Refused symlinked or non-directory --state-dir `{state_dir}`: {exc}") from exc
    finally:
        os.close(parent_fd)

    try:
        st = os.fstat(fd)
        if not stat.S_ISDIR(st.st_mode):
            raise ValueError(f"Refused non-directory --state-dir `{state_dir}`.")
        os.fchmod(fd, 0o700)
        if os.listdir(fd):
            raise ValueError(f"Refused non-empty --state-dir `{state_dir}`.")
    except Exception:
        os.close(fd)
        raise
    return fd


def validate_args(args: argparse.Namespace) -> tuple[str, str | None, Path, Path, Path]:
    if not args.remote or not args.state_dir:
        raise ValueError("Pass both --remote <name> and --state-dir <dir> explicitly.")
    remote = args.remote.removesuffix(":")
    account_b = args.account_b.removesuffix(":") if args.account_b else None
    if not REMOTE_NAME_RE.fullmatch(remote):
        raise ValueError("Invalid --remote name.")
    if account_b is not None and (not REMOTE_NAME_RE.fullmatch(account_b) or account_b == remote):
        raise ValueError("--account-b must name a different Drive remote in the export.")
    if len(args.prefix) > 32 or not REMOTE_NAME_RE.fullmatch(args.prefix):
        raise ValueError("--prefix must be a simple name of at most 32 characters.")

    state_dir = validate_fresh_non_symlink_state_dir(args.state_dir)
    if not args.rclone_config:
        raise ValueError("Pass --rclone-config with an explicit disposable rclone export.")
    cfg = Path(args.rclone_config).expanduser().absolute()
    xdg = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config").resolve()
    resolved_cfg = cfg.resolve()
    if (
        is_forbidden_rclone_config(cfg)
        or resolved_cfg == (Path.home() / ".rclone.conf").resolve()
        or resolved_cfg.is_relative_to(xdg / "rclone")
        or is_forbidden_real_state_dir(cfg)
    ):
        raise ValueError("Never use the live FolderSync state or ~/.config/rclone for --rclone-config.")
    if resolved_cfg.is_relative_to(state_dir):
        raise ValueError("Keep the rclone export outside the fresh --state-dir.")
    out = Path(args.out_dir).expanduser().resolve()
    if is_forbidden_real_state_dir(out) or is_forbidden_rclone_config(out) or out.is_relative_to(xdg / "rclone"):
        raise ValueError("Choose an evidence directory outside live FolderSync and rclone directories.")
    for name in ("drive-evidence.json", "drive-evidence.md"):
        target = out / name
        if target.is_symlink() or (target.exists() and not target.is_file()):
            raise ValueError(f"Refused non-regular evidence file {target}.")
        if target.exists() and target.samefile(resolved_cfg):
            raise ValueError("The rclone export cannot also be an evidence file.")
    return remote, account_b, state_dir, cfg, out


def read_export(path: Path, remote: str, account_b: str | None) -> tuple[bytes, str]:
    """Validate the selected Drive sections before importing into the fresh states."""
    parent_fd, leaf, _ = _open_parent_fd_no_symlinks_from_root(str(path))
    try:
        fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent_fd)
    finally:
        os.close(parent_fd)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077:
            raise ValueError("Use a regular disposable rclone export with permissions 0600.")
        content = source.read()
    parser = configparser.ConfigParser(interpolation=None)
    try:
        parser.read_string(content.decode("utf-8"))
    except (UnicodeError, configparser.Error):
        raise ValueError("Use the unencrypted single-remote export format supported by account import.") from None
    roots = []
    for name in (remote, account_b) if account_b else (remote,):
        if not parser.has_section(name) or parser[name].get("type") != "drive":
            raise ValueError(f"The export must include a Drive section named {name}.")
        root = parser[name].get("root_folder_id", "")
        if not DRIVE_ID_RE.fullmatch(root):
            raise ValueError(f"Set root_folder_id for {name} to the disposable shared parent ID.")
        roots.append(root)
    if len(set(roots)) != 1:
        raise ValueError("Both remotes must use the same disposable shared parent ID.")
    return content, roots[0]


@dataclass(frozen=True)
class Client:
    name: str
    root: Path
    remote: str

    @property
    def state(self) -> Path:
        return self.root / "state"

    @property
    def home(self) -> Path:
        return self.root / "home"

    def data(self, key: str) -> Path:
        return self.root / "data" / key

    def recovery(self, key: str) -> Path:
        return self.root / "recovery" / key

    def env(self, rclone: Path) -> dict[str, str]:
        return {
            "PATH": os.defpath,
            "HOME": str(self.home),
            "TMPDIR": str(self.home / "tmp"),
            "FOLDERSYNC_STATE_DIR": str(self.state),
            "FOLDERSYNC_RCLONE": str(rclone),
            "LANG": "en_US.UTF-8",
        }


def private_dir(path: Path) -> Path:
    path.mkdir(mode=0o700)
    return path


def capture_record(
    args: list[str], returncode: int, stdout: str, stderr: str, client: str, tool: str,
) -> tuple[dict[str, Any], str]:
    safe_out = redact_credential_stderr(stdout)
    safe_err = redact_credential_stderr(stderr)
    return {
        "client": client,
        "tool": tool,
        "args": args,
        "returncode": returncode,
        "stdout": safe_out.strip()[:400],
        "stderr": safe_err.strip()[:600],
    }, safe_out


def run_rows(client: Client, query: str, params: tuple = ()) -> list[sqlite3.Row]:
    uri = client.state.joinpath("state.sqlite").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        return conn.execute(query, params).fetchall()


def transfer_progress(log_path: Path, expected_batch: set[str]) -> int:
    completed = set()
    if log_path.is_file():
        for line in log_path.read_text(encoding="utf-8").splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue  # A writer may still be appending the final line.
            if isinstance(event, dict) and event.get("object") in expected_batch and event.get("msg", "").startswith("Copied ("):
                completed.add(event["object"])
    return len(completed)


class DriveRunner:
    """CLI and rclone invocations with isolated state and sanitized evidence."""

    def __init__(self, sync_cli: Path, rclone: Path) -> None:
        self.sync_cli = sync_cli
        self.rclone = rclone
        self.commands: list[dict[str, Any]] = []
        self.device_ids: dict[str, str] = {}

    def record(self, client: Client, tool: str, args: list[str], proc: Any, check: bool) -> str:
        entry, output = capture_record(args, proc.returncode, proc.stdout, proc.stderr, client.name, tool)
        self.commands.append(entry)
        if check and proc.returncode != 0:
            raise RuntimeError(f"{client.name} {tool} {' '.join(args[:2])} exited {proc.returncode}: {entry['stderr']}")
        return output

    def cli(self, client: Client, *args: str, check: bool = True, timeout: float = CLI_TIMEOUT) -> tuple[int, str]:
        command = [str(self.sync_cli), "--state-dir", str(client.state), *args]
        proc = subprocess.Popen(
            command, env=client.env(self.rclone), start_new_session=True, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace",
        )
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
        except BaseException:
            stop_owned_process(proc)
            raise
        result = subprocess.CompletedProcess(command, proc.returncode, stdout, stderr)
        return result.returncode, self.record(client, "sync-cli", list(args), result, check)

    def device_id(self, client: Client) -> str:
        if client.name not in self.device_ids:
            rows = run_rows(client, "SELECT id FROM devices")
            if len(rows) != 1 or not re.fullmatch(r"device-[a-f0-9-]+", rows[0]["id"]):
                raise RuntimeError("Could not verify the private client's device ID.")
            self.device_ids[client.name] = rows[0]["id"]
        return self.device_ids[client.name]

    def cloud(self, client: Client, *args: str) -> str:
        config = client.state / "accounts" / "qual" / "rclone.conf"
        # rclone's SpaceSepList uses CSV quoting, including doubled quotes.
        helper = '"' + str(self.sync_cli).replace('"', '""') + '"'
        helper += f" credential-helper --account {self.device_id(client)}/qual"
        command = [
            str(self.rclone), *args, "--use-json-log", "--config", str(config),
            "--ask-password=false", "--password-command", helper,
            "--drive-skip-gdocs", "--drive-skip-shortcuts",
        ]
        proc = subprocess.run(
            command, env=client.env(self.rclone), stdin=subprocess.DEVNULL, capture_output=True,
            text=True, errors="replace", timeout=DRIVE_TIMEOUT,
        )
        return self.record(client, "rclone", list(args), proc, True)

    def interrupt(self, client: Client, key: str, batch: set[str]) -> dict[str, Any]:
        args = ["run-once", key]
        command = [str(self.sync_cli), "--state-dir", str(client.state), *args]
        proc = subprocess.Popen(
            command, env=client.env(self.rclone), start_new_session=True, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace",
        )
        signaled_run = None
        signaled_at = None
        transferred = 0
        after_signal = len(batch)
        try:
            deadline = time.monotonic() + DRIVE_TIMEOUT
            while time.monotonic() < deadline and proc.poll() is None:
                rows = run_rows(
                    client,
                    """SELECT id, log_path, child_pid, child_executable FROM runs
                       WHERE mapping_id=? AND status='running' AND child_pid IS NOT NULL""",
                    (key,),
                )
                if len(rows) == 1:
                    row = rows[0]
                    log = Path(row["log_path"])
                    pid = row["child_pid"]
                    if Path(row["child_executable"] or "") == self.rclone and log.is_relative_to(client.state):
                        transferred = transfer_progress(log, batch)
                        if 1 <= transferred < len(batch):
                            try:
                                if pid != proc.pid and os.getpgid(pid) == proc.pid:
                                    os.kill(pid, signal.SIGINT)
                                    signaled_run = row["id"]
                                    signaled_at = utc_now()
                                    after_signal = transfer_progress(log, batch)
                                    break
                            except ProcessLookupError:
                                pass
                time.sleep(0.05)
            stdout, stderr = proc.communicate(timeout=CLI_TIMEOUT)
        except BaseException:
            stop_owned_process(proc)
            raise
        result = subprocess.CompletedProcess(command, proc.returncode, stdout, stderr)
        self.record(client, "sync-cli", args, result, False)
        return {
            "signaled_run": signaled_run,
            "signaled_at": signaled_at,
            "transferred_before_signal": transferred,
            "transferred_after_signal": after_signal,
            "returncode": proc.returncode,
        }


def verify_checkpoint(
    truth: TruthSet,
    trees: dict[str, dict[str, str]],
    expected_live: dict[str, set[str]],
    conflicts: set[str],
    recovered: dict[str, str | None],
) -> tuple[list[dict[str, Any]], list[str]]:
    """Require exact live inventories and locate each version by hash, including backups."""
    records = truth.records
    locations: dict[str, list[str]] = {label: [] for label in records}
    labels_by_hash = {record["sha256"]: label for label, record in records.items()}
    errors = [f"Missing live inventory for {tree}" for tree in sorted(expected_live.keys() - trees.keys())]
    for tree, hashes in trees.items():
        seen: Counter[str] = Counter()
        for path, digest in hashes.items():
            label = labels_by_hash.get(digest)
            if tree not in expected_live:
                if label:
                    locations[label].append(f"{tree}:recovery:{path}")
                continue
            original = records[label]["rel_path"] if label else None
            conflict = label in conflicts and re.fullmatch(re.escape(original) + r"\.conflict\d+", path)
            current = label not in conflicts and path == original
            if label and (current or conflict):
                locations[label].append(f"{tree}:{'conflict' if conflict else 'current'}:{path}")
            if label in expected_live[tree] and (current or conflict):
                seen[label] += 1
            else:
                errors.append(f"{tree} unexpected {path} ({label or digest})")
        if tree in expected_live:
            for label in sorted(expected_live[tree]):
                if seen[label] != 1:
                    errors.append(f"{tree} expected one copy of {label}, found {seen[label]}")

    accounting = []
    for label, record in records.items():
        if label in recovered:
            prefix = recovered[label]
            verified = any(
                location.startswith(prefix) if prefix else ":recovery:" in location
                for location in locations[label]
            )
            if not verified:
                errors.append(f"Missing required recovery copy of {label}")
        else:
            required = [tree for tree, labels in expected_live.items() if label in labels]
            verified = bool(required) and all(
                sum(location.startswith(f"{tree}:") for location in locations[label]) == 1
                for tree in required
            )
            if not verified:
                errors.append(f"Missing expected live copy of {label}")
        accounting.append({**record, "locations": locations[label], "verified": verified})
    return accounting, errors


class DriveQualification:
    def __init__(
        self, runner: DriveRunner, clients: list[Client], root: Path, prefix: str, seed: int, account_b: str | None,
    ) -> None:
        self.runner = runner
        self.clients = clients
        self.root = root
        self.prefix = prefix
        self.seed = seed
        self.account_b = account_b
        self.folder_ids: dict[str, str] = {}
        self.last_runs: dict[tuple[str, str], str] = {}
        self.cloud_checks = private_dir(root / "cloud-checks")
        self.truth_dir = private_dir(root / "truth")
        self.evidence: ScenarioEvidence

    def cli_json(self, client: Client, *args: str, timeout: float = CLI_TIMEOUT) -> Any:
        return json.loads(self.runner.cli(client, *args, "--json", timeout=timeout)[1])

    def preflight(self, export: Path, root_id: str) -> None:
        identities = {}
        for client in self.clients:
            self.runner.cli(client, "scheduling", "pause")
            self.runner.cli(
                client, "account", "import", "--from", str(export), "--remote", client.remote,
                "--id", "qual", "--root-folder-id", root_id,
            )
            accounts = self.cli_json(client, "account", "list")
            status = self.cli_json(client, "status")
            if (
                len(accounts) != 1 or accounts[0]["kind"] != "drive"
                or accounts[0]["auth_status"] != "ready"
                or accounts[0]["remote_root_folder_id"] != root_id
                or accounts[0]["remote_name"] != client.remote
                or not status["scheduling_paused"]
            ):
                raise RuntimeError(f"Drive account or paused scheduling did not verify for {client.name}.")
            identities[client.name] = self.runner.device_id(client)
            self.cli_json(client, "collections", "list", "--account", "qual")
        if len(set(identities.values())) != len(self.clients):
            raise RuntimeError("The clients do not have independent device IDs.")
        self.evidence.outcomes.update({
            "devices": identities, "root_folder_id": root_id, "account_b": self.account_b,
            "sources": {client.name: client.remote for client in self.clients},
        })

    def collection_matches(self, client: Client, name: str, *, timeout: float = CLI_TIMEOUT) -> list[dict[str, Any]]:
        listing = self.cli_json(client, "collections", "list", "--account", "qual", timeout=timeout)
        return [row["collection"] for row in listing["candidates"] if row["name"] == name]

    def wait_for_collection(self, client: Client, name: str, path: str) -> list[dict[str, Any]]:
        started = time.monotonic()
        deadline = started + COLLECTION_VISIBILITY_TIMEOUT
        delay = 1.0
        attempts = 0
        try:
            while (remaining := deadline - time.monotonic()) > 0:
                attempts += 1
                matches = self.collection_matches(client, name, timeout=remaining)
                if matches:
                    return matches
                remaining = deadline - time.monotonic()
                if remaining > 0:
                    time.sleep(min(delay, remaining))
                    delay = min(delay * 2, 8.0)
        except subprocess.TimeoutExpired:
            pass  # The listing used the remaining visibility budget.
        finally:
            elapsed = time.monotonic() - started
            self.evidence.outcomes["collection_visibility"] = {
                "client": client.name, "collection": path,
                "attempts": attempts, "elapsed_seconds": round(elapsed, 3),
            }
        raise RuntimeError(f"{client.name} could not see {path} after {attempts} listings in {elapsed:.1f} s.")

    def make_collection(self, key: str, clients: list[Client]) -> None:
        first = clients[0]
        name = f"{self.prefix}-{key}"
        path = f"Data/{name}"
        entries = json.loads(self.runner.cloud(first, "lsjson", f"{first.remote}:Data"))
        if any(entry["Name"].casefold() == name.casefold() for entry in entries):
            raise RuntimeError(f"Refused to reuse existing Drive fixture {path}.")
        self.runner.cloud(first, "mkdir", f"{first.remote}:{path}")
        for client in clients:
            if first.name == "client-a" and client.name == "client-b":
                matches = self.wait_for_collection(client, name, path)
            else:
                matches = self.collection_matches(client, name)
            if len(matches) != 1 or matches[0]["display_path"] != path:
                raise RuntimeError(f"{client.name} could not unambiguously see {path}.")
            folder_id = matches[0]["folder_id"]
            if not DRIVE_ID_RE.fullmatch(folder_id) or self.folder_ids.setdefault(key, folder_id) != folder_id:
                raise RuntimeError(f"{client.name} resolved a different Drive folder ID for {path}.")
            private_dir(client.data(key))
            private_dir(client.recovery(key))
            self.runner.cli(
                client, "add-mapping", "--id", key, "--account", "qual", "--folder-id", folder_id,
                "--local", str(client.data(key)), "--local-recovery", str(client.recovery(key)),
            )
        self.evidence.outcomes.update({"folder_id": folder_id, "collection": path})

    def status(self, client: Client, key: str) -> dict[str, Any]:
        snapshot = self.cli_json(client, "status")
        if not snapshot["scheduling_paused"]:
            raise RuntimeError(f"Automatic scheduling is enabled in {client.name}.")
        mapping = next(mapping for mapping in snapshot["mappings"] if mapping["id"] == key)
        if mapping["next_poll_at"] is not None:
            raise RuntimeError(f"An automatic run is scheduled for {client.name} {key}.")
        return mapping

    def sync(self, client: Client, key: str, command: str = "run-once") -> dict[str, Any]:
        self.runner.cli(client, command, key)
        status = self.status(client, key)
        run_id = status["last_run_id"]
        if (
            not run_id or self.last_runs.get((client.name, key)) == run_id
            or status["last_run_outcome"] not in {"Success", "SuccessWithConflicts"}
        ):
            raise RuntimeError(f"{client.name} {key} did not record a fresh successful run.")
        self.last_runs[client.name, key] = run_id
        self.evidence.outcomes.setdefault("runs", []).append({
            "client": client.name, "id": run_id, "command": command, "outcome": status["last_run_outcome"],
        })
        return status

    def remote_tree(self, client: Client, remote: str, destination: Path, *options: str) -> dict[str, str]:
        entries = json.loads(self.runner.cloud(client, "lsjson", remote, "--recursive", "--files-only", *options))
        paths = [entry["Path"] for entry in entries if entry["Path"] != "RCLONE_TEST"]
        if len(set(paths)) != len(paths):
            raise RuntimeError(f"Duplicate paths in Drive snapshot {remote}.")
        private_dir(destination)
        self.runner.cloud(client, "copy", remote, str(destination), *options)
        hashes = self.truth.scan_tree_hashes(destination)
        if set(hashes) != set(paths):
            raise RuntimeError(f"Downloaded inventory does not match listing for {remote}.")
        return hashes

    def snapshot(self, key: str, checkpoint: str, reader: Client, backups: bool) -> dict[str, dict[str, str]]:
        folder_id = self.folder_ids[key]
        save = private_dir(self.cloud_checks / f"{key}-{checkpoint}")
        trees = {
            "drive": self.remote_tree(reader, f"{reader.remote}:", save / "live", "--drive-root-folder-id", folder_id),
        }
        if backups:
            trees["drive-recovery"] = self.remote_tree(
                reader, f"{reader.remote}:Recovery/{folder_id}", save / "recovery",
            )
        return trees

    def checkpoint(
        self, key: str, name: str, clients: list[Client], live: set[str],
        recovered: dict[str, str | None], conflicts: set[str], reader: Client | None = None,
        offline: tuple[Client, set[str]] | None = None,
    ) -> None:
        trees = self.snapshot(key, name, reader or clients[0], bool(recovered))
        for client in clients:
            trees[client.name] = self.truth.scan_tree_hashes(client.data(key))
            trees[f"{client.name}-recovery"] = self.truth.scan_tree_hashes(client.recovery(key))
        expected = {tree: live for tree in ["drive", *(c.name for c in clients)]}
        mismatches = [
            f"{client.name} live paths or hashes differ from Drive"
            for client in clients if trees[client.name] != trees["drive"]
        ]
        if offline:
            client, labels = offline
            trees[client.name] = self.truth.scan_tree_hashes(client.data(key))
            expected[client.name] = labels
        self.check(name, trees, expected, conflicts, recovered, mismatches)

    def check(
        self, name: str, trees: dict[str, dict[str, str]], expected: dict[str, set[str]],
        conflicts: set[str], recovered: dict[str, str | None],
        mismatches: list[str] | None = None,
    ) -> None:
        accounting, errors = verify_checkpoint(self.truth, trees, expected, conflicts, recovered)
        errors.extend(mismatches or [])
        self.evidence.hash_accounting = accounting
        self.evidence.outcomes.setdefault("checkpoints", []).append({
            "name": name, "verified": not errors, "file_counts": {tree: len(hashes) for tree, hashes in trees.items()},
            "hash_accounting": accounting, "errors": errors,
        })
        if errors:
            raise RuntimeError(f"{name}: {errors[0]} ({len(errors)} inventory or hash discrepancies)")

    def put(self, client: Client, key: str, path: str, label: str, size: int = 256) -> None:
        self.truth.write_into(client.data(key), path, label, size_bytes=size, binary=True)

    def start(self, key: str, clients: list[Client], files: list[str], filler: int = 16) -> set[str]:
        self.make_collection(key, clients)
        first = clients[0]
        labels = set()
        for idx, path in enumerate(files + [f"seed-{i:02d}.txt" for i in range(filler)]):
            label = f"baseline-{idx:02d}"
            self.put(first, key, path, label)
            labels.add(label)
        for client in clients:
            self.sync(client, key, "init")
        self.checkpoint(key, "baseline", clients, labels, {}, set())
        return labels

    def repeat(
        self, key: str, clients: list[Client], live: set[str],
        recovered: dict[str, str | None], conflicts: set[str], reader: Client | None = None,
    ) -> None:
        for cycle in range(1, 3):
            for client in clients:
                self.sync(client, key)
            self.checkpoint(key, f"cycle-{cycle}", clients, live, recovered, conflicts, reader)

    def distinct(self) -> None:
        key = "distinct"
        a, b, _ = self.clients
        baseline = self.start(key, self.clients, ["a.txt", "b.txt"])
        self.put(a, key, "a.txt", "edit-a", 501)
        self.put(b, key, "b.txt", "edit-b", 502)
        self.sync(a, key)
        self.sync(b, key)
        old = {"baseline-00": None, "baseline-01": None}
        self.repeat(key, self.clients, baseline - old.keys() | {"edit-a", "edit-b"}, old, set())

    def conflict(self) -> None:
        key = "conflict"
        a, b, _ = self.clients
        baseline = self.start(key, self.clients, ["same.txt"])
        self.put(a, key, "same.txt", "edit-a", 501)
        self.put(b, key, "same.txt", "edit-b", 502)
        self.sync(a, key)
        conflict_outcome = self.sync(b, key)["last_run_outcome"]
        edits = {"edit-a", "edit-b"}
        self.repeat(key, self.clients, baseline - {"baseline-00"} | edits, {"baseline-00": None}, edits)
        if conflict_outcome != "SuccessWithConflicts":
            raise RuntimeError("The same-file edit did not produce a recorded conflict.")

    def edit_delete(self) -> None:
        key = "edit-delete"
        a, b, _ = self.clients
        baseline = self.start(key, self.clients, ["keep-edit.txt"])
        a.data(key).joinpath("keep-edit.txt").unlink()
        self.put(b, key, "keep-edit.txt", "edited", 501)
        self.sync(a, key)
        self.sync(b, key)
        self.repeat(key, self.clients, baseline - {"baseline-00"} | {"edited"}, {"baseline-00": None}, set())

    def offline(self) -> None:
        key = "offline"
        a, b, c = self.clients
        baseline = self.start(key, self.clients, ["shared.txt", "delete.txt"])
        self.runner.cli(c, "pause", key)
        paused = self.status(c, key)
        blocked_rc, _ = self.runner.cli(c, "run-once", key, check=False)
        if paused["intent"] != "Paused" or blocked_rc == 0:
            raise RuntimeError("The offline client's pause hold did not verify.")
        self.put(c, key, "shared.txt", "offline-edit", 502)
        self.put(c, key, "offline-new.txt", "offline-new", 503)
        self.put(a, key, "shared.txt", "online-edit", 501)
        a.data(key).joinpath("delete.txt").unlink()
        for cycle in range(1, 3):
            self.sync(a, key)
            self.sync(b, key)
            self.checkpoint(
                key, f"offline-{cycle}", [a, b], baseline - {"baseline-00", "baseline-01"} | {"online-edit"},
                {"baseline-00": None, "baseline-01": None}, set(),
                offline=(c, baseline - {"baseline-00"} | {"offline-edit", "offline-new"}),
            )
        unchanged = self.status(c, key)
        if unchanged["intent"] != "Paused" or unchanged["last_run_id"] != paused["last_run_id"]:
            raise RuntimeError("The paused client ran while it was offline.")
        self.runner.cli(c, "resume", key)
        conflict_outcome = self.sync(c, key)["last_run_outcome"]
        edits = {"offline-edit", "online-edit"}
        self.repeat(
            key, self.clients, baseline - {"baseline-00", "baseline-01"} | edits | {"offline-new"},
            {"baseline-00": None, "baseline-01": None}, edits,
        )
        if conflict_outcome != "SuccessWithConflicts":
            raise RuntimeError("The stale edit did not produce a recorded conflict.")

    def guard(self) -> None:
        key = "guard"
        a = self.clients[0]
        baseline = self.start(key, [a], [], filler=10)
        deleted = set(sorted(baseline)[:4])
        for label in deleted:
            a.data(key).joinpath(self.truth.records[label]["rel_path"]).unlink()
        aborted_run = None
        for attempt in range(1, 3):
            rc, _ = self.runner.cli(a, "run-once", key, check=False)
            status = self.status(a, key)
            rows = run_rows(
                a, "SELECT outcome_kind FROM runs WHERE id=?", (status["last_run_id"],),
            )
            if (
                rc == 0 or status["readiness"] != "RecoveryRequired" or len(rows) != 1
                or rows[0]["outcome_kind"] != "deletion_guard_aborted"
                or status["last_run_id"] == self.last_runs[a.name, key]
                or (aborted_run is not None and status["last_run_id"] != aborted_run)
            ):
                raise RuntimeError("Bulk deletion did not enter a fresh deletion guard hold.")
            aborted_run = status["last_run_id"]
            self.evidence.outcomes["aborted_run"] = aborted_run
            trees = self.snapshot(key, f"hold-{attempt}", a, False)
            trees[a.name] = self.truth.scan_tree_hashes(a.data(key))
            self.check(f"hold-{attempt}", trees, {"drive": baseline, a.name: baseline - deleted}, set(), {})

    def interrupt(self) -> None:
        key = "interrupt"
        a = self.clients[0]
        baseline = self.start(key, [a], [], filler=1)
        batch = {f"batch/part-{i:02d}.bin": f"batch-{i:02d}" for i in range(BATCH_COUNT)}
        for path, label in batch.items():
            self.put(a, key, path, label, 2 * 1024 * 1024)
        injected = self.runner.interrupt(a, key, set(batch))
        status = self.status(a, key)
        trees = self.snapshot(key, "after-signal", a, False)
        downloaded = set(trees["drive"]) & batch.keys()
        self.evidence.outcomes.update(injected)
        self.evidence.outcomes.update({
            "readiness": status["readiness"],
            "batch_files": len(batch),
            "downloaded_after_exit": len(downloaded),
        })
        expected = {"drive": baseline | {batch[path] for path in downloaded}, a.name: baseline | set(batch.values())}
        trees[a.name] = self.truth.scan_tree_hashes(a.data(key))
        self.check("after-signal", trees, expected, set(), {})
        if (
            injected["signaled_run"] is None or not 0 < injected["transferred_before_signal"] < len(batch)
            or injected["transferred_after_signal"] >= len(batch) or not 0 < len(downloaded) < len(batch)
        ):
            self.evidence.status = "NOT VERIFIED"
            self.evidence.conclusion = "Could not establish a mid-transfer interruption from logged and downloaded progress."
            return
        rows = run_rows(a, "SELECT workdir_path FROM runs WHERE id=?", (status["last_run_id"],))
        if (
            injected["returncode"] == 0 or status["last_run_id"] != injected["signaled_run"]
            or status["last_run_outcome"] != "Interrupted" or status["readiness"] != "RecoveryRequired"
            or not rows or not any(Path(rows[0]["workdir_path"]).iterdir())
        ):
            raise RuntimeError("The interrupted run did not persist its outcome, workdir, and recovery hold.")
        for attempt in range(1, 3):
            rc, _ = self.runner.cli(a, "run-once", key, check=False)
            if rc == 0 or self.status(a, key)["last_run_id"] != injected["signaled_run"]:
                raise RuntimeError("The interrupted client ran again while held for review.")
            followup = self.snapshot(key, f"hold-{attempt}", a, False)
            followup[a.name] = self.truth.scan_tree_hashes(a.data(key))
            self.check(f"hold-{attempt}", followup, expected, set(), {})

    def two_account(self) -> None:
        key = "two-account"
        a, b, _ = self.clients
        baseline = self.start(key, self.clients, ["overwrite.txt", "remove.txt"])
        self.put(b, key, "overwrite.txt", "edit-b", 501)
        b.data(key).joinpath("remove.txt").unlink()
        self.sync(b, key)
        device = self.runner.device_id(b)
        recover = {label: f"drive-recovery:recovery:{device}/" for label in ["baseline-00", "baseline-01"]}
        self.repeat(key, self.clients, baseline - recover.keys() | {"edit-b"}, recover, set(), reader=b)
        self.evidence.notes.append("Verify Drive ownership and quota attribution separately in the owner checklist.")

    def run(self, export: Path, root_id: str) -> list[ScenarioEvidence]:
        cases = [
            ("preflight", "Verify the imported accounts, three device IDs, and paused scheduling.",
             "none", lambda: self.preflight(export, root_id)),
            ("distinct", "Converge independent edits after both clients have synced.",
             "A and B edit different files", self.distinct),
            ("conflict", "Preserve competing same-file versions as conflict copies.",
             "A and B edit the same baseline", self.conflict),
            ("edit-delete", "Retain a competing edit and the pre-delete recovery bytes.",
             "A deletes a file B edited", self.edit_delete),
            ("offline", "Reconcile stale edits without resurrecting unrelated deletions.",
             "C edits while paused across two cycles", self.offline),
            ("guard", "Verify the deletion guard hold and unchanged Drive inventory.",
             "delete four of ten tracked files", self.guard),
            ("interrupt", "Verify preserved bytes, the workdir, and the recovery hold after interruption.",
             "SIGINT to the active bisync child during a batch", self.interrupt),
        ]
        if self.account_b:
            cases.append((
                "two-account", "Download A's original bytes from B's Drive recovery directory.",
                "B overwrites and deletes files uploaded by A", self.two_account,
            ))
        results = []
        preflight_ok = True
        for key, description, injected, run in cases:
            self.runner.commands = []
            self.evidence = evidence = ScenarioEvidence(
                SCENARIOS[key], description, utc_now(), "", injected, False, "",
                commands=self.runner.commands,
            )
            if not preflight_ok:
                evidence.status, evidence.conclusion = "NOT RUN", "Drive preflight did not pass."
            else:
                self.truth = TruthSet(private_dir(self.truth_dir / key), self.seed)
                try:
                    run()
                    if not evidence.status:
                        evidence.status, evidence.conclusion, evidence.passed = "PASS", "Verified.", True
                except (OSError, RuntimeError, ValueError, KeyError, StopIteration, sqlite3.Error, subprocess.SubprocessError) as exc:
                    evidence.status = "FAIL"
                    evidence.conclusion = redact_credential_stderr(str(exc))
            evidence.finished_at = utc_now()
            results.append(evidence)
            if key == "preflight":
                preflight_ok = evidence.passed
                export.unlink()
        return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--remote", help="Required primary Drive remote in the disposable export")
    parser.add_argument("--account-b", "--remote-b", help="Second Google account's remote in the same export")
    parser.add_argument("--state-dir", help="Required fresh, empty, non-symlinked state directory")
    parser.add_argument("--rclone-config", help="Required disposable unencrypted Drive config export")
    parser.add_argument("--prefix", default="FolderSyncQual", help="Prefix for newly named collections under Data")
    parser.add_argument("--out-dir", default=str(REPO_ROOT / "target" / "qualification"))
    parser.add_argument("--seed", type=int, default=20261120)
    parser.add_argument("--sync-cli", type=Path, default=REPO_ROOT / "target" / "release" / "sync-cli")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without launching any tools")
    args = parser.parse_args()

    try:
        remote, account_b, state_dir, config, out = validate_args(args)
        prefix = f"{args.prefix}-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{uuid.uuid4().hex[:12]}"
        if args.dry_run:
            print(json.dumps({
                "mode": "dry-run", "overall_expected_status": "INCOMPLETE",
                "remote": remote, "account_b": account_b,
                "collections": [f"Data/{prefix}-{key}" for key in SCENARIOS if key != "preflight" and (key != "two-account" or account_b)],
                "scenarios": [name for key, name in SCENARIOS.items() if key != "two-account" or account_b],
                "state_dir": str(state_dir), "evidence_dir": str(out),
            }, indent=2))
            return 0

        export_bytes, root_id = read_export(config, remote, account_b)
        rclone, version, digest = resolve_verified_pinned_rclone()
        sync_cli = args.sync_cli.resolve(strict=True)
        if not sync_cli.is_file() or not os.access(sync_cli, os.X_OK):
            raise ValueError("Build an executable sync-cli and pass its location with --sync-cli.")
        if any(char in str(sync_cli) for char in "\r\n"):
            raise ValueError("The sync-cli path contains a line break.")
        started = utc_now()
        state_fd = open_exclusive_state_dir_fd(state_dir)
        try:
            os.mkdir("qualification", 0o700, dir_fd=state_fd)
        finally:
            os.close(state_fd)
        root = state_dir / "qualification"
        clients = []
        for name, source in (("client-a", remote), ("client-b", account_b or remote), ("client-c", remote)):
            client = Client(name, private_dir(root / name), source)
            private_dir(client.home)
            private_dir(client.home / "tmp")
            private_dir(client.root / "data")
            private_dir(client.root / "recovery")
            clients.append(client)
        versions = get_tool_versions(sync_cli, rclone, version, digest, cli_env=clients[0].env(rclone))

        with tempfile.TemporaryDirectory(prefix="import-", dir=root) as staging:
            export = Path(staging) / "selection.conf"
            with os.fdopen(os.open(export, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as file:
                file.write(export_bytes)
            del export_bytes
            suite = DriveQualification(DriveRunner(sync_cli, rclone), clients, root, prefix, args.seed, account_b)
            results = suite.run(export, root_id)
        status = compute_overall_status(results)
        write_evidence_files(
            out / "drive-evidence.json", out / "drive-evidence.md", "real-google-drive",
            args.seed, started, utc_now(), versions, results,
            f"{status} for the scripted {'two-account' if account_b else 'single-account'} Drive scenarios. "
            "Use the owner decision checklist for workload and release acceptance.",
        )
        for scenario in results:
            print(f"[{scenario.effective_status()}] {scenario.name}: {scenario.conclusion}")
        print(f"Overall status: {status} (evidence: {out / 'drive-evidence.json'})")
        return 0 if status == "PASS" else 3
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"Error: {redact_credential_stderr(str(exc))}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
