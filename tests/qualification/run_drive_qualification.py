#!/usr/bin/env python3
"""Run owner-authorized Google Drive scenarios in isolated FolderSync client states."""

from __future__ import annotations

import argparse
import configparser
import csv
import fcntl
import http.client
import io
import json
import os
import re
import shutil
import signal
import sqlite3
import ssl
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from collections import Counter
from contextlib import closing, contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator

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
    sha256_file,
    stop_owned_process,
    utc_now,
    write_evidence_files,
)

REMOTE_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}")
DRIVE_ID_RE = re.compile(r"[A-Za-z0-9_-]{1,255}")
ACCESS_TOKEN_RE = re.compile(r"[A-Za-z0-9_.~+/-]+=*")
CLI_TIMEOUT = 750  # Manager allows up to 600 s for a sync plus Drive preflight.
DRIVE_TIMEOUT = 180
COLLECTION_VISIBILITY_TIMEOUT = 60
BATCH_COUNT = 24
SENTINEL = "FOLDERSYNC-QUALIFICATION-DISPOSABLE"
DRIVE_FOLDER = "application/vnd.google-apps.folder"
DRIVE_SHORTCUT = "application/vnd.google-apps.shortcut"
DRIVE_SHEET = "application/vnd.google-apps.spreadsheet"
RUN_ID_RE = re.compile(r"[0-9]{8}T[0-9]{6}Z-[a-f0-9]{12}")
QUAL_HOME = "~/Library/Application Support/FolderSync-Qual"
QUAL_CONFIG_RE = re.compile(r'parent_id = "([A-Za-z0-9_-]{1,255})"\naccount_b = (true|false)\nsync_cli_sha256 = "([a-f0-9]{64})"\n')
SCENARIO_ERRORS = (OSError, RuntimeError, ValueError, KeyError, StopIteration, sqlite3.Error, subprocess.SubprocessError)

SCENARIOS = {
    "preflight": "drive_preflight_account_and_folder_ids",
    "distinct": "drive_three_client_distinct_edits",
    "conflict": "drive_three_client_same_file_edit_conflict",
    "edit-delete": "drive_three_client_edit_vs_delete",
    "offline": "drive_three_client_offline_return",
    "guard": "drive_deletion_guard_abort",
    "interrupt": "drive_interruption",
    "rename-edit": "drive_rename_vs_edit",
    "outside-edit": "drive_outside_client_edit",
    "delayed-upload": "drive_delayed_upload_overlap",
    "duplicate": "drive_duplicate_names",
    "native": "drive_native_document_and_shortcut",
    "replacement": "drive_folder_identity_hold",
    "two-account": "drive_two_account_ownership_and_recovery_move",
    "cleanup": "drive_cleanup",
}
REQUIRED_SCENARIOS = {"preflight", "cleanup"}


def parse_only(value: str) -> set[str]:
    valid = {name: key for key, name in SCENARIOS.items() if key not in REQUIRED_SCENARIOS}
    requested = {name.strip() for name in value.split(",")}
    if unknown := requested - valid.keys():
        raise ValueError(f"Unknown scenario names for --only: {', '.join(map(repr, sorted(unknown)))}. "
                         f"Valid names: {', '.join(valid)}.")
    return {valid[name] for name in requested}


def select_scenarios(account_b: str | bool | None, only: set[str] | None) -> dict[str, str]:
    if only is not None and "two-account" in only and not account_b:
        raise ValueError(f"{SCENARIOS['two-account']} requires a second account.")
    return {
        key: name for key, name in SCENARIOS.items()
        if (key != "two-account" or account_b) and (only is None or key in REQUIRED_SCENARIOS or key in only)
    }


def skipped_scenarios(selected: dict[str, str], only: set[str] | None) -> list[str] | None:
    if only is None:
        return None
    return [name for key, name in SCENARIOS.items() if key not in selected]


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


def qual_path(raw: str) -> Path:
    expanded = os.path.expanduser(raw)
    if not expanded or ".." in Path(expanded).parts or any(ord(c) < 32 or ord(c) == 127 for c in expanded):
        raise ValueError("Invalid qualification home path.")
    path = Path(_normalize_os_root_alias(os.path.abspath(expanded)))
    if path.is_relative_to("/Volumes") or is_forbidden_real_state_dir(path) or is_forbidden_rclone_config(path):
        raise ValueError("Choose a dedicated qualification home outside live state, rclone, and mounted volumes.")
    return path


def checked_private_directory(path: Path) -> Path:
    parent_fd, leaf, _ = _open_parent_fd_no_symlinks_from_root(str(path))
    try:
        fd = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent_fd)
        try:
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                raise ValueError(f"Expected an owner-only 0700 directory at {path}.")
        finally:
            os.close(fd)
    finally:
        os.close(parent_fd)
    return path


def open_private_file(path: Path, flags: int) -> int:
    parent_fd, leaf, _ = _open_parent_fd_no_symlinks_from_root(str(path))
    try:
        fd = os.open(leaf, flags | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600, dir_fd=parent_fd)
    finally:
        os.close(parent_fd)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_nlink != 1:
        os.close(fd)
        raise ValueError(f"Expected a private regular file at {path}.")
    return fd


def read_qual_home(raw: str) -> tuple[Path, str, bool, str]:
    home = checked_private_directory(qual_path(raw))
    with os.fdopen(open_private_file(home / "qual.toml", os.O_RDONLY), encoding="utf-8") as file:
        match = QUAL_CONFIG_RE.fullmatch(file.read(1024))
    if not match:
        raise ValueError("Invalid qual.toml. Complete drive-setup.sh in a fresh qualification home.")
    return home, match[1], match[2] == "true", match[3]


@contextmanager
def lock_qual_home(home: Path):
    try:
        fd = open_private_file(home / "run.lock", os.O_RDWR | os.O_CREAT | os.O_EXCL)
    except FileExistsError:
        fd = open_private_file(home / "run.lock", os.O_RDWR)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("Drive qualification is already running for this home.") from None
        yield
    finally:
        os.close(fd)


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
    run_root: Path | None = None

    @property
    def state(self) -> Path:
        return self.root / "state"

    @property
    def home(self) -> Path:
        return self.root / "home"

    @property
    def config(self) -> Path:
        return self.state / "accounts" / "qual" / "rclone.conf"

    def data(self, key: str) -> Path:
        return (self.run_root or self.root) / "data" / key

    def recovery(self, key: str) -> Path:
        return (self.run_root or self.root) / "recovery" / key

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

    def add_account(self, client: Client, parent_id: str, client_id: str, secret: str) -> None:
        command = [
            str(self.sync_cli), "--state-dir", str(client.state), "account", "add-drive",
            "--id", "qual", "--remote", client.remote, "--root-folder-id", parent_id,
            "--client-id", client_id, "--client-secret-stdin",
        ]
        proc = subprocess.Popen(
            command, env=client.env(self.rclone), start_new_session=True, stdin=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, errors="replace",
        )
        try:
            _, stderr = proc.communicate(input=secret + "\n", timeout=CLI_TIMEOUT)
        except BaseException:
            stop_owned_process(proc)
            raise
        if proc.returncode:
            raise RuntimeError(f"{client.name} authorization failed: {redact_credential_stderr(stderr)}")

    def device_id(self, client: Client) -> str:
        if client.name not in self.device_ids:
            rows = run_rows(client, "SELECT id FROM devices")
            if len(rows) != 1 or not re.fullmatch(r"device-[a-f0-9-]+", rows[0]["id"]):
                raise RuntimeError("Could not verify the private client's device ID.")
            self.device_ids[client.name] = rows[0]["id"]
        return self.device_ids[client.name]

    def cloud_command(self, client: Client, *args: str) -> list[str]:
        # rclone's SpaceSepList uses CSV quoting, including doubled quotes.
        helper = '"' + str(self.sync_cli).replace('"', '""') + '"'
        helper += f" credential-helper --account {self.device_id(client)}/qual"
        return [
            str(self.rclone), "--use-json-log", "--config", str(client.config),
            "--ask-password=false", "--password-command", helper,
            "--drive-skip-gdocs", "--drive-skip-shortcuts",
            *args,
        ]

    def cloud(self, client: Client, *args: str) -> str:
        proc = subprocess.run(
            self.cloud_command(client, *args), env=client.env(self.rclone),
            stdin=subprocess.DEVNULL, capture_output=True,
            text=True, errors="replace", timeout=DRIVE_TIMEOUT,
        )
        if args[:2] == ("backend", "query") and "search result INCOMPLETE" in proc.stderr:
            raise RuntimeError("Drive reported an incomplete query. Refused to use its results.")
        return self.record(client, "rclone", list(args), proc, True)

    def _qualification_access_token(self, client: Client, parent_id: str) -> str:
        with os.fdopen(open_private_file(client.config, os.O_RDONLY), "rb") as file:
            header = next((line.strip() for line in file if line.strip() and not line.lstrip().startswith(b"#")), b"")
        if header != b"RCLONE_ENCRYPT_V0:":
            raise RuntimeError("Drive metadata updates require the encrypted qualification account config.")

        # Deliberately bypass command capture and its evidence pipeline. The pinned
        # config show prints the decrypted remote into a private in-memory pipe.
        try:
            proc = subprocess.run(
                self.cloud_command(client, "config", "show", client.remote), env=client.env(self.rclone),
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                text=True, timeout=DRIVE_TIMEOUT,
            )
            proc.check_returncode()
            parser = configparser.ConfigParser(interpolation=None)
            parser.read_string(proc.stdout)
            remote = parser[client.remote]
            if remote["type"] == "drive" and remote["root_folder_id"] == parent_id:
                token = json.loads(remote["token"])["access_token"]
                if isinstance(token, str) and ACCESS_TOKEN_RE.fullmatch(token):
                    return token
        except (OSError, RuntimeError, ValueError, KeyError, TypeError, configparser.Error, subprocess.SubprocessError):
            pass  # These exceptions can contain credential bytes.
        raise RuntimeError("Could not read an access token from the encrypted qualification account.")

    def trash_recorded_folders(
        self, client: Client, parent_id: str, expected_data_id: str, scratch: Path, folder_ids: dict[str, str],
    ) -> Iterator[dict[str, str]]:
        """Trash only recorded fixtures that are still direct children of the original Data folder."""
        if not folder_ids:
            return
        for fixture, folder_id in folder_ids.items():
            result = {"fixture": fixture, "folder_id": folder_id, "status": "REFUSED"}
            try:
                if not DRIVE_ID_RE.fullmatch(folder_id) or folder_id in {parent_id, expected_data_id}:
                    raise RuntimeError("Invalid or reserved Drive fixture folder ID.")
                data_id = require_sentinel(self, client, parent_id, scratch)
                if data_id != expected_data_id:
                    raise RuntimeError("The disposable Data folder ID changed during the run.")
                token = self._qualification_access_token(client, parent_id)
                with closing(qualification_drive_connection()) as conn:
                    before = qualification_drive_file(conn, folder_id, token)
                    if not matches_fixture(before, folder_id, data_id):
                        raise RuntimeError("The recorded ID is no longer a folder directly under the verified Data folder.")
                    if before.get("trashed") is True and before.get("explicitlyTrashed") is True:
                        result["status"] = "ALREADY TRASHED"
                    elif before.get("trashed") is False and before.get("explicitlyTrashed") is False:
                        result["status"] = "FAILED"
                        after = qualification_drive_file(conn, folder_id, token, update={"trashed": True})
                        if not matches_fixture(after, folder_id, data_id) or (
                            after.get("trashed") is not True or after.get("explicitlyTrashed") is not True
                        ):
                            raise RuntimeError("Drive did not confirm recoverable trash for this exact folder ID and parent.")
                        result["status"] = "TRASHED"
                    else:
                        raise RuntimeError("Drive did not confirm an independently trashable fixture.")
            except SCENARIO_ERRORS as exc:
                result["detail"] = redact_credential_stderr(str(exc))
            yield result

    def rename_duplicate(
        self, client: Client, parent_id: str, data_id: str, scratch: Path, folder_id: str, file_id: str,
    ) -> None:
        if not DRIVE_ID_RE.fullmatch(file_id) or file_id in {parent_id, data_id, folder_id}:
            raise RuntimeError("Invalid or reserved Drive duplicate fixture ID.")
        if require_sentinel(self, client, parent_id, scratch) != data_id:
            raise RuntimeError("The disposable Data folder ID changed during the run.")
        token = self._qualification_access_token(client, parent_id)
        identity = {"id": file_id, "parents": [folder_id], "trashed": False}
        with closing(qualification_drive_connection()) as conn:
            before = qualification_drive_file(conn, file_id, token)
            mime = before.get("mimeType")
            if (any(before.get(key) != value for key, value in identity.items()) or before.get("name") != "second.txt"
                    or not isinstance(mime, str) or not mime or mime.startswith("application/vnd.google-apps.")):
                raise RuntimeError("The recorded ID is no longer the ordinary second.txt directly under the scenario folder.")
            after = qualification_drive_file(conn, file_id, token, update={"name": "duplicate.txt"})
            expected = {**identity, "name": "duplicate.txt", "mimeType": mime}
            if any(after.get(key) != value for key, value in expected.items()):
                raise RuntimeError("Drive did not confirm the rename for this exact file ID and parent.")

    def overlap_upload(self, client: Client, source: Path, folder_id: str, during) -> dict[str, Any]:
        args = [
            "copyto", str(source), f"{client.remote}:shared.txt", "--drive-root-folder-id", folder_id,
            "--bwlimit", "16k", "--stats", "200ms", "--stats-log-level", "NOTICE",
        ]
        with tempfile.TemporaryFile(mode="w+t", encoding="utf-8") as log:
            proc = subprocess.Popen(
                self.cloud_command(client, *args), env=client.env(self.rclone), start_new_session=True,
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log,
            )
            try:
                deadline = time.monotonic() + DRIVE_TIMEOUT
                progress = 0
                while proc.poll() is None and time.monotonic() < deadline:
                    # pread does not move the shared write offset used by the child.
                    content = os.pread(log.fileno(), os.fstat(log.fileno()).st_size, 0)
                    if (progress := upload_progress(content, source.name)):
                        break
                    time.sleep(0.2)
                observed = utc_now()
                overlap = bool(progress and proc.poll() is None)
                if overlap:
                    during()
                active_after_peer = overlap and proc.poll() is None
                proc.wait(timeout=max(1, deadline - time.monotonic()))
                log.seek(0)
                self.record(client, "rclone", args, subprocess.CompletedProcess(
                    args, proc.returncode, "", log.read(),
                ), True)
                return {
                    "upload_observed_at": observed, "uploaded_bytes_before_peer": progress,
                    "peer_started_during_upload": overlap,
                    "upload_active_after_peer": active_after_peer,
                }
            except BaseException:
                if proc.poll() is None:
                    stop_owned_process(proc)
                raise

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


def matches_fixture(metadata: dict[str, Any], folder_id: str, data_id: str) -> bool:
    return (metadata.get("id") == folder_id and metadata.get("mimeType") == DRIVE_FOLDER
            and metadata.get("parents") == [data_id])


def qualification_drive_connection() -> http.client.HTTPSConnection:
    # Use verified TLS without proxy settings, redirects, or SSL key logging.
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    tls.load_default_certs()
    return http.client.HTTPSConnection("www.googleapis.com", timeout=30, context=tls)


def qualification_drive_file(
    conn: http.client.HTTPSConnection, file_id: str, token: str, *, update: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Read or update one exact Drive ID without retaining HTTP diagnostics."""
    target = f"/drive/v3/files/{file_id}?supportsAllDrives=true&fields=id,name,mimeType,parents,trashed,explicitlyTrashed"
    headers = {"Authorization": f"Bearer {token}", "Accept": "application/json"}
    if update is not None:
        headers["Content-Type"] = "application/json"
    method = "PATCH" if update is not None else "GET"
    try:
        conn.request(method, target, body=json.dumps(update).encode() if update is not None else None, headers=headers)
        with conn.getresponse() as response:
            if response.status != 200:
                raise RuntimeError(f"Drive {method} returned HTTP {response.status}.")
            body = response.read(16_385)
        if len(body) > 16_384:
            raise ValueError("Oversized metadata.")
        metadata = json.loads(body)
        if not isinstance(metadata, dict):
            raise ValueError("Invalid metadata.")
        return metadata
    except (http.client.HTTPException, OSError, ValueError):
        raise RuntimeError(f"Drive {method} did not return valid file metadata.") from None


def upload_progress(log: bytes, name: str) -> int:
    for line in reversed(log.splitlines()):
        try:
            event = json.loads(line)
        except (ValueError, UnicodeError):
            continue
        for row in (event.get("stats") or {}).get("transferring") or []:
            if row["name"] == name and 0 < row["bytes"] < row["size"]:
                return row["bytes"]
    return 0


def drive_children(runner: DriveRunner, client: Client, parent_id: str) -> list[dict[str, Any]]:
    if not DRIVE_ID_RE.fullmatch(parent_id):
        raise ValueError("Invalid Drive parent ID.")
    output = runner.cloud(client, "backend", "query", f"{client.remote}:", f"'{parent_id}' in parents and trashed = false")
    rows = json.loads(output)
    if rows is None:
        return []
    if not isinstance(rows, list) or any(
        not isinstance(row, dict) or not isinstance(row.get("id"), str) or not DRIVE_ID_RE.fullmatch(row["id"])
        or parent_id not in row.get("parents", []) or not isinstance(row.get("name"), str)
        or not isinstance(row.get("mimeType"), str)
        for row in rows
    ):
        raise RuntimeError("Drive returned an invalid child listing.")
    return rows


def drive_directory(runner: DriveRunner, client: Client, parent_id: str, name: str) -> str:
    matches = [row for row in drive_children(runner, client, parent_id) if row["name"].casefold() == name.casefold()]
    if len(matches) != 1 or matches[0]["name"] != name or matches[0]["mimeType"] != DRIVE_FOLDER:
        raise RuntimeError(f"Expected exactly one actual Drive directory named {name}.")
    return matches[0]["id"]


def sentinel_bytes(parent_id: str) -> bytes:
    return f"{SENTINEL}\n{parent_id}\n".encode()


def require_sentinel(runner: DriveRunner, client: Client, parent_id: str, scratch: Path) -> str:
    try:
        data_id = drive_directory(runner, client, parent_id, "Data")
        rows = [row for row in drive_children(runner, client, data_id) if row["name"].casefold() == SENTINEL.casefold()]
        expected = sentinel_bytes(parent_id)
        if (
            len(rows) != 1 or rows[0]["name"] != SENTINEL
            or rows[0]["mimeType"].startswith("application/vnd.google-apps.")
            or int(rows[0].get("size", -1)) != len(expected)
        ):
            raise RuntimeError("Expected exactly one ordinary sentinel with the configured parent ID.")
        with tempfile.TemporaryDirectory(prefix="sentinel-", dir=scratch) as temp:
            destination = Path(temp) / "sentinel"
            runner.cloud(client, "backend", "copyid", f"{client.remote}:", rows[0]["id"], str(destination))
            if destination.read_bytes() != expected:
                raise RuntimeError("The sentinel's contents do not match the configured parent ID.")
        return data_id
    except SCENARIO_ERRORS as exc:
        raise RuntimeError(f"Disposable Drive sentinel could not be verified: {exc}") from None


def verify_checkpoint(
    truth: TruthSet,
    trees: dict[str, dict[str, str]],
    expected_live: dict[str, set[str]],
    conflicts: set[str],
    recovered: dict[str, str | None],
    labels: set[str] | None = None,
    *, reject_unknown_recovery: bool = False,
) -> tuple[list[dict[str, Any]], list[str]]:
    """Require exact live inventories and locate each version by hash, including backups."""
    records = truth.records if labels is None else {label: truth.records[label] for label in sorted(labels)}
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
                elif reject_unknown_recovery:
                    errors.append(f"{tree} unexpected {path} ({digest})")
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
        self.mapping_ids: dict[tuple[str, str], str] = {}
        self.last_runs: dict[tuple[str, str], str] = {}
        self.cloud_checks = private_dir(root / "cloud-checks")
        self.truth_dir = private_dir(root / "truth")
        self.evidence: ScenarioEvidence

    def cli_json(self, client: Client, *args: str, timeout: float = CLI_TIMEOUT) -> Any:
        return json.loads(self.runner.cli(client, *args, "--json", timeout=timeout)[1])

    def preflight(self, export: Path | None, root_id: str) -> None:
        identities = {}
        for client in self.clients:
            self.runner.cli(client, "scheduling", "pause")
            if export is not None:
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

    def require_sentinel(self) -> str:
        return require_sentinel(self.runner, self.clients[0], self.parent_id, self.root)

    def cleanup(self) -> None:
        errors = []
        self.evidence.outcomes["recorded_fixture_ids"] = self.folder_ids.copy()
        trash = self.evidence.outcomes["drive_trash"] = []
        removed = []
        for client in self.clients:
            planned = {key: mapping_id for (name, key), mapping_id in self.mapping_ids.items() if name == client.name}
            if not planned:
                continue
            try:
                mappings = {row["id"]: row for row in self.cli_json(client, "status")["mappings"]}
            except SCENARIO_ERRORS as exc:
                errors.append(redact_credential_stderr(str(exc)))
                continue
            for key, mapping_id in planned.items():
                if (row := mappings.get(mapping_id)) is None:
                    continue
                if row["local_root"] != str(client.data(key)) or row["collection_folder_id"] != self.folder_ids[key]:
                    errors.append(f"Refused to remove {client.name} mapping {mapping_id}: its endpoints differ.")
                    continue
                try:
                    self.runner.cli(client, "remove-mapping", mapping_id)
                    removed.append({"client": client.name, "mapping_id": mapping_id})
                except SCENARIO_ERRORS as exc:
                    errors.append(redact_credential_stderr(str(exc)))
        self.evidence.outcomes.update({"removed_mappings": removed, "errors": errors})
        if errors:
            raise RuntimeError(f"Could not remove {len(errors)} run mappings. See the evidence.")
        if not self.folder_ids:
            return
        self.evidence.outcomes["data_folder_id"] = self.data_id
        for result in self.runner.trash_recorded_folders(
            self.clients[0], self.parent_id, self.data_id, self.root, self.folder_ids,
        ):
            trash.append(result)
        confirmed = {row["fixture"]: row["folder_id"] for row in trash if row["status"] in {"TRASHED", "ALREADY TRASHED"}}
        if confirmed != self.folder_ids:
            raise RuntimeError(f"Could not confirm trash for all {len(self.folder_ids)} Drive fixtures. See the evidence.")

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
        entries = drive_children(self.runner, first, self.data_id)
        if any(entry["name"].casefold() == name.casefold() for entry in entries):
            raise RuntimeError(f"Refused to reuse existing Drive fixture {path}.")
        self.runner.cloud(first, "mkdir", f"{first.remote}:{name}", "--drive-root-folder-id", self.data_id)
        folder_id = drive_directory(self.runner, first, self.data_id, name)
        self.folder_ids[key] = folder_id
        for client in clients:
            if first.name == "client-a" and client.name == "client-b":
                matches = self.wait_for_collection(client, name, path)
            else:
                matches = self.collection_matches(client, name)
            if len(matches) != 1 or matches[0]["display_path"] != path:
                raise RuntimeError(f"{client.name} could not unambiguously see {path}.")
            if matches[0]["folder_id"] != folder_id:
                raise RuntimeError(f"{client.name} resolved a different Drive folder ID for {path}.")
            private_dir(client.data(key))
            private_dir(client.recovery(key))
            mapping_id = f"{self.prefix}-{key}"
            self.mapping_ids[client.name, key] = mapping_id
            self.runner.cli(
                client, "add-mapping", "--id", mapping_id, "--account", "qual", "--folder-id", folder_id,
                "--local", str(client.data(key)), "--local-recovery", str(client.recovery(key)),
            )
        self.evidence.outcomes.update({"folder_id": folder_id, "collection": path})

    def status(self, client: Client, key: str) -> dict[str, Any]:
        snapshot = self.cli_json(client, "status")
        if not snapshot["scheduling_paused"]:
            raise RuntimeError(f"Automatic scheduling is enabled in {client.name}.")
        mapping = next(mapping for mapping in snapshot["mappings"] if mapping["id"] == self.mapping_ids[client.name, key])
        if mapping["next_poll_at"] is not None:
            raise RuntimeError(f"An automatic run is scheduled for {client.name} {key}.")
        return mapping

    def sync(self, client: Client, key: str, command: str = "run-once") -> dict[str, Any]:
        rc, _ = self.runner.cli(client, command, self.mapping_ids[client.name, key], check=False)
        status = self.status(client, key)
        run_id = status["last_run_id"]
        fresh = bool(run_id and self.last_runs.get((client.name, key)) != run_id)
        if fresh:
            self.last_runs[client.name, key] = run_id
            self.evidence.outcomes.setdefault("runs", []).append({
                "client": client.name, "id": run_id, "command": command, "outcome": status["last_run_outcome"],
            })
        if rc or not fresh or status["last_run_outcome"] not in {"Success", "SuccessWithConflicts"}:
            raise RuntimeError(f"{client.name} {key} did not record a fresh successful run.")
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
            recovery_id = drive_directory(self.runner, reader, self.parent_id, "Recovery")
            matches = [row for row in drive_children(self.runner, reader, recovery_id)
                       if row["name"].casefold() == folder_id.casefold()]
            if matches:
                if len(matches) != 1 or matches[0]["name"] != folder_id or matches[0]["mimeType"] != DRIVE_FOLDER:
                    raise RuntimeError(f"Unexpected Drive recovery directory for {folder_id}.")
                trees["drive-recovery"] = self.remote_tree(
                    reader, f"{reader.remote}:", save / "recovery", "--drive-root-folder-id", matches[0]["id"],
                )
        return trees

    def checkpoint_trees(
        self, key: str, name: str, clients: list[Client], reader: Client, backups: bool,
    ) -> dict[str, dict[str, str]]:
        trees = self.snapshot(key, name, reader, backups)
        for client in clients:
            trees[client.name] = self.truth.scan_tree_hashes(client.data(key))
            trees[f"{client.name}-recovery"] = self.truth.scan_tree_hashes(client.recovery(key))
        return trees

    def checkpoint(
        self, key: str, name: str, clients: list[Client], live: set[str],
        recovered: dict[str, str | None], conflicts: set[str], reader: Client | None = None,
        offline: tuple[Client, set[str]] | None = None,
    ) -> None:
        trees = self.checkpoint_trees(key, name, clients, reader or clients[0], bool(recovered))
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
        *, labels: set[str] | None = None, reject_unknown_recovery: bool = False,
    ) -> None:
        accounting, errors = verify_checkpoint(
            self.truth, trees, expected, conflicts, recovered, labels, reject_unknown_recovery=reject_unknown_recovery,
        )
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
        mapping_id = self.mapping_ids[c.name, key]
        self.runner.cli(c, "pause", mapping_id)
        paused = self.status(c, key)
        blocked_rc, _ = self.runner.cli(c, "run-once", mapping_id, check=False)
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
        self.runner.cli(c, "resume", mapping_id)
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
            rc, _ = self.runner.cli(a, "run-once", self.mapping_ids[a.name, key], check=False)
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
        injected = self.runner.interrupt(a, self.mapping_ids[a.name, key], set(batch))
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
            rc, _ = self.runner.cli(a, "run-once", self.mapping_ids[a.name, key], check=False)
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

    def upload(self, client: Client, key: str, path: str, source: Path, *options: str) -> None:
        self.runner.cloud(
            client, "copyto", str(source), f"{client.remote}:{path}",
            "--drive-root-folder-id", self.folder_ids[key], *options,
        )

    def rename_edit(self) -> None:
        key = "rename-edit"
        a, b = clients = self.clients[:2]
        baseline = self.start(key, clients, ["original.txt"])
        a.data(key).joinpath("original.txt").rename(a.data(key) / "renamed.txt")
        self.truth.records["baseline-00"]["rel_path"] = "renamed.txt"
        self.put(b, key, "original.txt", "concurrent-edit", 501)
        self.sync(a, key)
        self.sync(b, key)
        self.repeat(key, clients, baseline | {"concurrent-edit"}, {}, set())

    def outside_edit(self) -> None:
        key = "outside-edit"
        a, b = clients = self.clients[:2]
        baseline = self.start(key, clients, ["shared.txt"])
        self.truth.generate_version("outside-edit", "shared.txt", size_bytes=501, binary=True)
        self.upload(a, key, "shared.txt", Path(self.truth.records["outside-edit"]["truth_file"]))
        for client in clients:
            self.sync(client, key)
        self.repeat(key, clients, baseline - {"baseline-00"} | {"outside-edit"}, {"baseline-00": None}, set())

    def delayed_upload(self) -> None:
        key = "delayed-upload"
        a, b = clients = self.clients[:2]
        baseline = self.start(key, clients, ["shared.txt"])
        self.truth.generate_version("delayed", "shared.txt", size_bytes=768 * 1024, binary=True)
        self.put(b, key, "shared.txt", "peer-edit", 501)
        timing = self.runner.overlap_upload(
            a, Path(self.truth.records["delayed"]["truth_file"]), self.folder_ids[key], lambda: self.sync(b, key),
        )
        self.evidence.outcomes["overlap"] = timing
        if not timing["peer_started_during_upload"]:
            self.evidence.status = "NOT VERIFIED"
            self.evidence.conclusion = "Did not observe an in-progress upload when the peer run started."
            return
        self.sync(b, key)
        self.sync(a, key)
        competing = {self.truth.records[label]["sha256"]: label for label in ("delayed", "peer-edit")}
        winner = None
        for cycle in range(1, 3):
            for client in clients:
                self.sync(client, key)
            name = f"cycle-{cycle}"
            trees = self.checkpoint_trees(key, name, clients, a, backups=True)
            if winner is None:
                winner = competing.get(trees["drive"].get("shared.txt"))
                if winner is None:
                    raise RuntimeError(f"{name}: Drive shared.txt is missing or has an unknown version.")
            loser, = set(competing.values()) - {winner}
            live = baseline - {"baseline-00"} | {winner}
            self.check(
                name, trees, {tree: live for tree in ("drive", a.name, b.name)}, set(),
                {"baseline-00": None, loser: None}, reject_unknown_recovery=True,
            )
        loser_location = next(
            location for row in self.evidence.hash_accounting if row["label"] == loser
            for location in row["locations"] if ":recovery:" in location
        )
        timing.update(winner=winner, loser_location=loser_location)
        self.evidence.conclusion = f"{winner} stayed live on Drive and both clients. {loser} was verified at {loser_location}."

    def download_by_id(self, client: Client, file_id: str, filename: Path, *options: str) -> str:
        if not DRIVE_ID_RE.fullmatch(file_id):
            raise ValueError("Invalid Drive file ID.")
        self.runner.cloud(client, "backend", "copyid", f"{client.remote}:", file_id, str(filename), *options)
        return sha256_file(filename)

    def duplicate_names(self) -> None:
        key = "duplicate"
        a, b = clients = self.clients[:2]
        baseline = self.start(key, clients, [])
        folder_id = self.folder_ids[key]
        paths = {"duplicate.txt": "duplicate-one", "second.txt": "duplicate-two"}
        for path, label in paths.items():
            self.truth.generate_version(label, path, size_bytes=501, binary=True)
            self.upload(a, key, path, Path(self.truth.records[label]["truth_file"]))
        rows = drive_children(self.runner, a, folder_id)
        ids = {}
        for path, label in paths.items():
            matches = [row["id"] for row in rows if row["name"] == path]
            if len(matches) != 1:
                raise RuntimeError(f"Could not identify the uploaded duplicate fixture {path}.")
            ids[matches[0]] = label
        second_id = next(file_id for file_id, label in ids.items() if label == "duplicate-two")
        # Rename by ID so Drive keeps both objects under the same name.
        self.runner.rename_duplicate(a, self.parent_id, self.data_id, self.root, folder_id, second_id)
        for cycle in range(3):
            if cycle:
                for client in clients:
                    self.sync(client, key)
            rows = drive_children(self.runner, a, folder_id)
            duplicate_ids = {row["id"] for row in rows if row["name"] == "duplicate.txt"}
            observed = {"cycle": cycle, "objects": [], "local_versions": {}}
            self.evidence.outcomes.setdefault("duplicates", []).append(observed)
            if duplicate_ids != ids.keys():
                observed["actual_ids"] = sorted(duplicate_ids)
                raise RuntimeError("Drive duplicate IDs changed. See duplicates in the evidence.")
            save = private_dir(self.cloud_checks / f"duplicate-{cycle}")
            hashes = set()
            for file_id, label in ids.items():
                expected = self.truth.records[label]
                digest = self.download_by_id(a, file_id, save / file_id)
                observed["objects"].append({"id": file_id, "expected": expected, "sha256": digest})
                if digest != expected["sha256"]:
                    raise RuntimeError(f"Drive duplicate {file_id} lost its original bytes.")
                hashes.add(digest)
            trees = {"drive": self.remote_tree(
                a, f"{a.remote}:", save / "ordinary", "--drive-root-folder-id", folder_id, "--exclude", "/duplicate.txt",
            )}
            for client in clients:
                files = trees[client.name] = self.truth.scan_tree_hashes(client.data(key))
                digest = files.pop("duplicate.txt", None)
                observed["local_versions"][client.name] = digest
                if digest is not None and digest not in hashes:
                    raise RuntimeError(f"{client.name} has an unrecognized version of the duplicate.")
            self.check(f"duplicates-{cycle}", trees, {tree: baseline for tree in trees}, set(), {}, labels=baseline)

    def native_and_shortcut(self) -> None:
        key = "native"
        a, b = clients = self.clients[:2]
        baseline = self.start(key, clients, ["source.bin"])
        folder_id = self.folder_ids[key]
        source = self.truth.truth_dir / "fixture.csv"
        source.write_bytes(b"name,value\nfixture,retained\n")
        self.upload(a, key, "fixture.csv", source, "--drive-skip-gdocs=false",
                    "--drive-import-formats", "csv", "--drive-export-formats", "csv")
        self.runner.cloud(a, "backend", "shortcut", f"{a.remote}:", "source.bin", "source-link.bin",
                          "--drive-root-folder-id", folder_id, "--drive-skip-shortcuts=false")
        first_ids = None
        first_sha = None
        for cycle in range(3):
            if cycle:
                for client in clients:
                    self.sync(client, key)
            rows = drive_children(self.runner, a, folder_id)
            names = {"fixture": DRIVE_SHEET, "source-link.bin": DRIVE_SHORTCUT}
            actual = {}
            for name, mime in names.items():
                matches = [row for row in rows if row["name"] == name]
                if len(matches) != 1 or matches[0]["mimeType"] != mime:
                    raise RuntimeError(f"Expected exactly one {mime} named {name}.")
                actual[name] = matches[0]
            target_id = actual["source-link.bin"]["shortcutDetails"]["targetId"]
            if [row["id"] for row in rows if row["name"] == "source.bin"] != [target_id]:
                raise RuntimeError("Drive shortcut no longer points to the original file.")
            export = self.truth.truth_dir / "fixture-export.csv" if not cycle else self.cloud_checks / f"native-export-{cycle}.csv"
            digest = self.download_by_id(a, actual["fixture"]["id"], export,
                                         "--drive-skip-gdocs=false", "--drive-export-formats", "csv")
            ids = {name: row["id"] for name, row in actual.items()}
            self.evidence.outcomes.setdefault("drive_only", []).append({
                "cycle": cycle, "ids": ids, "shortcut_target_id": target_id, "export_sha256": digest,
                "initial_export": str(self.truth.truth_dir / "fixture-export.csv"),
            })
            if not cycle:
                if list(csv.reader(io.StringIO(export.read_text(encoding="utf-8-sig")))) != [["name", "value"], ["fixture", "retained"]]:
                    raise RuntimeError("Native import/export did not preserve the fixture values.")
                first_ids, first_sha = ids, digest
            if ids != first_ids or digest != first_sha:
                raise RuntimeError("Drive-only object identities or native export bytes changed.")
            self.checkpoint(key, f"drive-only-{cycle}", clients, baseline, {}, set())

    def replacement_hold(self) -> None:
        key = "replacement"
        a, b = clients = self.clients[:2]
        baseline = self.start(key, clients, ["original.txt"])
        folder_id = self.folder_ids[key]
        data_id = self.require_sentinel()
        if data_id != self.data_id:
            raise RuntimeError("The disposable Data folder ID changed during the run.")
        name = f"{self.prefix}-{key}"
        parked_name = f"{name}-original"
        rows = drive_children(self.runner, a, data_id)
        if [row["id"] for row in rows if row["name"].casefold() == name.casefold()] != [folder_id] or any(
            row["name"].casefold() == parked_name.casefold() for row in rows
        ):
            raise RuntimeError("Refused to move a collection with an ambiguous identity or destination.")
        self.runner.cloud(a, "moveto", f"{a.remote}:{name}", f"{a.remote}:{parked_name}",
                          "--drive-root-folder-id", data_id)
        if drive_directory(self.runner, a, data_id, parked_name) != folder_id:
            raise RuntimeError("Could not verify that the original folder was preserved by the move.")
        self.runner.cloud(a, "mkdir", f"{a.remote}:{name}", "--drive-root-folder-id", data_id)
        replacement_id = drive_directory(self.runner, a, data_id, name)
        if replacement_id == folder_id:
            raise RuntimeError("Replacement retained the original Drive ID.")
        self.folder_ids["replacement-new"] = replacement_id
        self.truth.generate_version("replacement-only", "replacement.txt", size_bytes=501, binary=True)
        self.upload(a, "replacement-new", "replacement.txt", Path(self.truth.records["replacement-only"]["truth_file"]))
        self.put(a, key, "pending.txt", "pending-local", 503)
        self.evidence.outcomes.update({"original_id": folder_id, "replacement_id": replacement_id})
        for cycle in range(1, 3):
            for client in clients:
                rc, _ = self.runner.cli(client, "run-once", self.mapping_ids[client.name, key], check=False)
                status = self.status(client, key)
                self.evidence.outcomes.setdefault("holds", []).append({
                    "client": client.name, "cycle": cycle, "readiness": status["readiness"],
                    "id": status["last_run_id"],
                })
                if rc == 0 or status["readiness"] != "EndpointChanged":
                    raise RuntimeError(f"{client.name} did not hold on the changed Drive folder identity.")
            trees = self.snapshot(key, f"hold-{cycle}", a, False)
            trees["replacement"] = self.remote_tree(
                a, f"{a.remote}:", self.cloud_checks / f"replacement-new-{cycle}",
                "--drive-root-folder-id", replacement_id,
            )
            for client in clients:
                trees[client.name] = self.truth.scan_tree_hashes(client.data(key))
            self.check(f"replacement-{cycle}", trees, {
                "drive": baseline, "replacement": {"replacement-only"}, a.name: baseline | {"pending-local"}, b.name: baseline,
            }, set(), {})

    def run(self, export: Path | None, root_id: str, *, only: set[str] | None = None) -> list[ScenarioEvidence]:
        self.parent_id = root_id
        selected = select_scenarios(self.account_b, only)
        cases = [
            ("preflight", "Verify the sentinel, accounts, independent devices, and paused scheduling.",
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
            ("rename-edit", "Preserve the renamed baseline and the new edit.",
             "A renames a file B edits", self.rename_edit),
            ("outside-edit", "Propagate an edit made directly on Drive and preserve the prior bytes.",
             "Drive-side upload outside the CLI clients", self.outside_edit),
            ("delayed-upload", "Account for overlapping same-file versions.",
             "A's slow Drive-side upload overlaps B's sync of a local edit", self.delayed_upload),
            ("duplicate", "Preserve both same-name Drive file IDs and verify both byte streams.",
             "Create two files with exactly the same Drive name", self.duplicate_names),
            ("native", "Verify a native Sheet and shortcut remain Drive-only and intact.",
             "Import a CSV as a Sheet and create a shortcut to a tracked file", self.native_and_shortcut),
            ("replacement", "Verify the mapping holds when a new folder takes the old name.",
             "Move the original aside and create a same-name folder", self.replacement_hold),
            ("two-account", "Download A's original bytes from B's Drive recovery directory.",
             "B overwrites and deletes files uploaded by A", self.two_account),
            ("cleanup", "Remove this run's mappings and verify recoverable trash for its collection IDs.",
             "none", self.cleanup),
        ]
        results = []
        preflight_ok = True
        interrupted = False
        for key, description, injected, run in cases:
            if key not in selected:
                continue
            self.runner.commands = []
            self.evidence = evidence = ScenarioEvidence(
                SCENARIOS[key], description, utc_now(), "", injected, False, "",
                commands=self.runner.commands,
            )
            if (not preflight_ok or interrupted) and key != "cleanup":
                evidence.status = "NOT RUN"
                evidence.conclusion = "The run was interrupted." if interrupted else "Drive preflight did not pass."
            else:
                self.truth = TruthSet(private_dir(self.truth_dir / key), self.seed)
                try:
                    if key not in REQUIRED_SCENARIOS and self.require_sentinel() != self.data_id:
                        raise RuntimeError("The disposable Data folder ID changed during the run.")
                    run()
                    if key == "preflight":
                        self.data_id = self.require_sentinel()
                    if not evidence.status:
                        evidence.status, evidence.passed = "PASS", True
                        evidence.conclusion = evidence.conclusion or "Verified."
                except SCENARIO_ERRORS as exc:
                    evidence.status = "FAIL"
                    evidence.conclusion = redact_credential_stderr(str(exc))
                except KeyboardInterrupt:
                    interrupted = True
                    evidence.status, evidence.conclusion = "NOT VERIFIED", "The run was interrupted."
                finally:
                    if key == "preflight" and export is not None:
                        export.unlink()
            evidence.finished_at = utc_now()
            results.append(evidence)
            if key == "preflight":
                preflight_ok = evidence.passed
        return results


def evidence_summary(scenarios: list[ScenarioEvidence]) -> list[dict[str, Any]]:
    rows = []
    for scenario in scenarios:
        outcomes = scenario.outcomes
        status = scenario.effective_status()
        checkpoint_errors = [error for check in outcomes.get("checkpoints", []) for error in check["errors"]]
        ids = [run["id"] for run in outcomes.get("runs", []) + outcomes.get("holds", []) if run["id"]]
        ids.extend(outcomes[field] for field in ("signaled_run", "aborted_run") if outcomes.get(field))
        rows.append({
            "scenario": scenario.name.removeprefix("drive_"),
            "status": status if status in ("PASS", "FAIL") else "INCOMPLETE",
            "conclusion": scenario.conclusion,
            "unaccounted_versions": [row["label"] for row in scenario.hash_accounting if not row["locations"]],
            "unexpected_copies": sorted({error for error in checkpoint_errors if "expected one copy" in error
                                         and error.rsplit(" ", 1)[-1].isdecimal() and int(error.rsplit(" ", 1)[-1]) > 1}),
            "observed_duplicate_ids": [item["id"] for item in outcomes.get("duplicates", [{}])[-1].get("objects", [])],
            "run_ids": list(dict.fromkeys(ids)),
        })
    return rows


def write_drive_evidence(out: Path, run_id: str, seed: int, started: str,
                         versions: dict[str, str], scenarios: list[ScenarioEvidence],
                         skipped: list[str] | None = None) -> str:
    status = compute_overall_status(scenarios)
    subset = skipped is not None
    if subset and status == "PASS":
        status = "PASS (subset)"
    run_type = "subset run" if subset else "full run"
    scope = f"{run_type}. Skipped scenarios — {', '.join(skipped) or 'none'}" if subset else run_type
    summary = evidence_summary(scenarios)
    out.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".evidence-", dir=out) as scratch:
        json_path, md_path = (Path(scratch) / f"drive-evidence.{suffix}" for suffix in ("json", "md"))
        write_evidence_files(json_path, md_path, "real-google-drive", seed, started, utc_now(), versions, scenarios,
                             f"{status} for this {run_type}. Use the owner decision checklist for release acceptance.")
        payload = json.loads(json_path.read_text(encoding="utf-8"))
        header = {"run_id": run_id, "run_type": run_type,
                  "skipped_scenarios": skipped or [], "summary": summary}
        payload["status"] = status
        json_path.write_text(json.dumps({**header, **payload}, indent=2) + "\n")
        detail = md_path.read_text(encoding="utf-8").split("\n", 1)[1]
        detail = "\n".join(line for line in detail.split("\n") if not line.startswith("- **Overall result:**"))
        lines = [
            "# Google Drive qualification", "",
            f"Run `{run_id}` · **{status}** · {started}", "",
            scope, "",
            "| Scenario | Result | Loss / unexpected duplication | Last sync run |",
            "| --- | --- | --- | --- |",
        ]
        for row in summary:
            discrepancy = "; ".join(row["unaccounted_versions"] + row["unexpected_copies"])
            if not discrepancy:
                if row["observed_duplicate_ids"] and row["status"] == "PASS":
                    discrepancy = "Both intentional duplicates retained"
                else:
                    discrepancy = "None observed" if row["status"] == "PASS" else "Unverified"
            last_run = row["run_ids"][-1][:12] if row["run_ids"] else "—"
            lines.append(f"| `{row['scenario']}` | **{row['status']}** | {discrepancy} | `{last_run}` |")
        md_path.write_text("\n".join(lines) + "\n\nFull run IDs are in the JSON summary.\n" + detail, encoding="utf-8")
        json_path.replace(out / json_path.name)
        md_path.replace(out / md_path.name)
    for scenario in scenarios:
        print(f"[{scenario.effective_status()}] {scenario.name}: {scenario.conclusion}")
    if subset:
        print(scope)
    print(f"Overall status: {status} (evidence: {out / 'drive-evidence.json'})")
    return status


def new_run_id() -> str:
    return f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{uuid.uuid4().hex[:12]}"


def show_plan(
    run_id: str, prefix: str, account_b: str | bool | None, selected: dict[str, str],
    skipped: list[str] | None, **paths: Any,
) -> None:
    collections = [f"Data/{prefix}-{key}" for key in selected if key not in REQUIRED_SCENARIOS]
    if "replacement" in selected:
        collections.append(f"Data/{prefix}-replacement-original")
    print(json.dumps({
        "mode": "dry-run", "run_id": run_id, "overall_expected_status": "INCOMPLETE", "account_b": account_b,
        "run_type": "subset run" if skipped is not None else "full run",
        "skipped_scenarios": skipped or [],
        "collections": collections,
        "scenarios": list(selected.values()),
        **paths,
    }, indent=2))


def publish_latest(evidence_dir: Path, run_id: str, keep: int | None) -> None:
    latest = evidence_dir / "latest"
    try:
        os.close(open_private_file(latest, os.O_RDONLY))
    except FileNotFoundError:
        pass
    with tempfile.TemporaryDirectory(prefix=".latest-", dir=evidence_dir) as scratch:
        pointer = Path(scratch) / "latest"
        pointer.write_text(run_id + "\n", encoding="utf-8")
        pointer.replace(latest)
    if keep is None:
        return
    previous = []
    for item in evidence_dir.iterdir():
        if item.name == run_id or not RUN_ID_RE.fullmatch(item.name) or item.is_symlink() or not item.is_dir():
            continue
        try:
            with os.fdopen(open_private_file(item / "drive-evidence.json", os.O_RDONLY), encoding="utf-8") as file:
                if json.load(file).get("run_id") == item.name:
                    previous.append(item)
        except (OSError, ValueError):
            continue  # A non-evidence directory is outside the retention policy.
    previous.sort(key=lambda item: item.stat().st_mtime_ns, reverse=True)
    for item in previous[keep - 1:]:
        shutil.rmtree(item)


def run_persistent(args: argparse.Namespace, run_id: str) -> int:
    if any((args.remote, args.account_b, args.rclone_config, args.state_dir, args.out_dir, args.sync_cli, args.prefix)):
        raise ValueError("Use --qual-home by itself for persistent accounts. Use the import flags for an export.")
    home, parent_id, account_b, cli_sha = read_qual_home(args.qual_home)
    selected = select_scenarios(account_b, args.only)
    skipped = skipped_scenarios(selected, args.only)
    prefix = f"FolderSyncQual-{run_id}"
    evidence_dir = checked_private_directory(home / "evidence")
    out = evidence_dir / run_id
    if args.dry_run:
        show_plan(run_id, prefix, account_b, selected, skipped, qual_home=str(home), evidence_dir=str(out))
        return 0
    with lock_qual_home(home):
        private_dir(out)
        started = utc_now()
        versions = {}
        try:
            rclone, version, digest = resolve_verified_pinned_rclone()
            sync_cli = home / "bin" / "sync-cli"
            os.close(open_private_file(sync_cli, os.O_RDONLY))
            if sha256_file(sync_cli) != cli_sha or not os.access(sync_cli, os.X_OK):
                raise ValueError("The credential helper binary changed. Re-run setup in a fresh qualification home.")
            fixture_roots = private_dir(out / "clients")
            clients = []
            for letter in "abc":
                name = f"client-{letter}"
                root = checked_private_directory(home / "clients" / name)
                client = Client(name, root, "qual", private_dir(fixture_roots / name))
                for location in (client.home, client.home / "tmp", client.state):
                    checked_private_directory(location)
                for location in (client.state / "state.sqlite", client.config):
                    os.close(open_private_file(location, os.O_RDONLY))
                for directory, subdirs, files in os.walk(client.state):
                    for name in subdirs + files:
                        if (Path(directory) / name).is_symlink():
                            raise ValueError(f"Refused a symlink in {client.name}'s qualification state.")
                private_dir(client.data(""))
                private_dir(client.recovery(""))
                clients.append(client)
            versions = get_tool_versions(sync_cli, rclone, version, digest, cli_env=clients[0].env(rclone))
            versions["sync_cli_sha256"] = cli_sha
            suite = DriveQualification(DriveRunner(sync_cli, rclone), clients, out, prefix, args.seed, "qual" if account_b else None)
            results = suite.run(None, parent_id, only=args.only)
        except SCENARIO_ERRORS as exc:
            results = [ScenarioEvidence(
                name, "", started, utc_now(), "none", False, redact_credential_stderr(str(exc)),
                "NOT VERIFIED" if key == "preflight" else "NOT RUN",
            ) for key, name in selected.items()]
        status = write_drive_evidence(out, run_id, args.seed, started, versions, results, skipped)
        publish_latest(evidence_dir, run_id, args.keep)
    return 0 if status in {"PASS", "PASS (subset)"} else 3


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--qual-home", help="Persistent qualification home created by drive-setup.sh")
    parser.add_argument("--keep", type=int, help="Keep this many published evidence runs under --qual-home")
    parser.add_argument("--remote", help="Required primary Drive remote in the disposable export")
    parser.add_argument("--account-b", "--remote-b", help="Second Google account's remote in the same export")
    parser.add_argument("--state-dir", help="Required fresh, empty, non-symlinked state directory")
    parser.add_argument("--rclone-config", help="Required disposable unencrypted Drive config export")
    parser.add_argument("--prefix", help="Import-mode prefix for newly named collections under Data")
    parser.add_argument("--out-dir", help="Import-mode evidence directory")
    parser.add_argument("--seed", type=int, default=20261120)
    parser.add_argument("--sync-cli", type=Path, help="Import-mode executable sync-cli")
    parser.add_argument("--only", metavar="SCENARIO[,SCENARIO...]", help="Run only the named scenarios plus preflight and cleanup")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without launching any tools")
    args = parser.parse_args(argv)
    os.umask(0o077)

    try:
        run_id = new_run_id()
        if args.only is not None:
            args.only = parse_only(args.only)
        if args.keep is not None and (not args.qual_home or args.keep < 1):
            raise ValueError("--keep requires --qual-home and a positive number of runs.")
        if args.qual_home:
            return run_persistent(args, run_id)
        args.prefix = args.prefix or "FolderSyncQual"
        args.out_dir = args.out_dir or str(REPO_ROOT / "target" / "qualification")
        remote, account_b, state_dir, config, out = validate_args(args)
        selected = select_scenarios(account_b, args.only)
        skipped = skipped_scenarios(selected, args.only)
        prefix = f"{args.prefix}-{run_id}"
        if args.dry_run:
            show_plan(run_id, prefix, account_b, selected, skipped,
                      remote=remote, state_dir=str(state_dir), evidence_dir=str(out))
            return 0

        export_bytes, root_id = read_export(config, remote, account_b)
        rclone, version, digest = resolve_verified_pinned_rclone()
        sync_cli = (args.sync_cli or REPO_ROOT / "target" / "release" / "sync-cli").resolve(strict=True)
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
            results = suite.run(export, root_id, only=args.only)
        status = write_drive_evidence(out, run_id, args.seed, started, versions, results, skipped)
        return 0 if status in {"PASS", "PASS (subset)"} else 3
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"Error: {redact_credential_stderr(str(exc))}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
