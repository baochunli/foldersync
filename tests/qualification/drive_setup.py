"""Authorize the persistent qualification clients from an owner's GUI session."""

from __future__ import annotations

import argparse
import getpass
import os
import shutil
import sys
import tempfile
import warnings
from pathlib import Path

from common import REPO_ROOT, redact_credential_stderr, resolve_verified_pinned_rclone, sha256_file
from run_drive_qualification import (
    DRIVE_ID_RE, QUAL_HOME, SCENARIO_ERRORS, SENTINEL, Client, DriveRunner,
    _open_parent_fd_no_symlinks_from_root, drive_children, drive_directory,
    open_private_file, private_dir, qual_path, require_sentinel, sentinel_bytes,
)


def create_qual_home(raw: str) -> Path:
    home = qual_path(raw)
    parent_fd, leaf, _ = _open_parent_fd_no_symlinks_from_root(str(home))
    try:
        os.mkdir(leaf, 0o700, dir_fd=parent_fd)
    except FileExistsError:
        raise ValueError("Refused existing qualification home. Choose a new directory.") from None
    finally:
        os.close(parent_fd)
    return home


def install_sentinel(runner: DriveRunner, client: Client, parent_id: str, scratch: Path) -> None:
    data_id = drive_directory(runner, client, parent_id, "Data")
    drive_directory(runner, client, parent_id, "Recovery")
    if not any(row["name"].casefold() == SENTINEL.casefold() for row in drive_children(runner, client, data_id)):
        with tempfile.TemporaryDirectory(prefix="setup-sentinel-", dir=scratch) as temp:
            source = Path(temp) / SENTINEL
            source.write_bytes(sentinel_bytes(parent_id))
            runner.cloud(
                client, "copyto", str(source), f"{client.remote}:{SENTINEL}",
                "--drive-root-folder-id", data_id, "--ignore-existing",
            )
    require_sentinel(runner, client, parent_id, scratch)


def authorize_clients(runner: DriveRunner, clients: list[Client], parent_id: str, client_id: str,
                      secret: str, account_b: bool) -> None:
    for client in clients:
        account = "B" if client.name == "client-b" and account_b else "A"
        print(f"Authorize {client.name} with Account {account} in the browser.", flush=True)
        runner.cli(client, "scheduling", "pause")
        runner.add_account(client, parent_id, client_id, secret)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qual-home", default=QUAL_HOME, help="A new private directory with an existing parent")
    parser.add_argument("--parent-id", required=True, help="ID of the disposable Drive parent containing Data and Recovery")
    parser.add_argument("--client-id", required=True, help="Desktop OAuth client ID")
    parser.add_argument("--account-b", action="store_true", help="Authorize client B with the second Google account")
    parser.add_argument("--sync-cli", type=Path, default=REPO_ROOT / "target" / "release" / "sync-cli")
    args = parser.parse_args()
    os.umask(0o077)
    try:
        if not DRIVE_ID_RE.fullmatch(args.parent_id) or not args.client_id.strip():
            raise ValueError("Pass a valid disposable Drive parent ID and desktop OAuth client ID.")
        rclone, _, _ = resolve_verified_pinned_rclone()
        release = args.sync_cli.resolve(strict=True)
        if not release.is_file() or not os.access(release, os.X_OK):
            raise ValueError("Build an executable release sync-cli before setup.")
        home = create_qual_home(args.qual_home)
        sync_cli = private_dir(home / "bin") / "sync-cli"
        with release.open("rb") as source, os.fdopen(
            open_private_file(sync_cli, os.O_WRONLY | os.O_CREAT | os.O_EXCL), "wb",
        ) as destination:
            shutil.copyfileobj(source, destination)
            os.fchmod(destination.fileno(), 0o700)
        clients_dir = private_dir(home / "clients")
        private_dir(home / "evidence")
        clients = [Client(f"client-{letter}", private_dir(clients_dir / f"client-{letter}"), "qual")
                   for letter in ("a", "b", "c")]
        for client in clients:
            private_dir(client.home)
            private_dir(client.home / "tmp")
            private_dir(client.state)
        with warnings.catch_warnings():
            warnings.simplefilter("error", getpass.GetPassWarning)
            secret = getpass.getpass("Desktop OAuth client secret: ").strip()
        if not secret:
            raise ValueError("The OAuth client secret is required.")
        runner = DriveRunner(sync_cli, rclone)
        try:
            authorize_clients(runner, clients, args.parent_id, args.client_id, secret, args.account_b)
        finally:
            del secret
        install_sentinel(runner, clients[0], args.parent_id, home)
        with os.fdopen(open_private_file(home / "qual.toml", os.O_WRONLY | os.O_CREAT | os.O_EXCL), "w") as file:
            file.write(f'parent_id = "{args.parent_id}"\naccount_b = {str(args.account_b).lower()}\n'
                       f'sync_cli_sha256 = "{sha256_file(sync_cli)}"\n')
        print(f"Qualification setup completed at {home}.")
        return 0
    except (EOFError, getpass.GetPassWarning, *SCENARIO_ERRORS) as exc:
        print(f"Setup failed: {redact_credential_stderr(str(exc)) or 'an interactive terminal is required'}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
