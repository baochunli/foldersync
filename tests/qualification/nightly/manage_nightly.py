"""Install or remove the owner's Aqua LaunchAgent."""

from __future__ import annotations

import argparse
import os
import plistlib
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from run_drive_qualification import open_private_file, read_qual_home

LABEL = "dev.foldersync.drive-qualification"
LAUNCHCTL = "/bin/launchctl"


def make_plist(home: Path, python: Path, keep: int, hour: int, minute: int) -> dict:
    with (HERE / f"{LABEL}.plist").open("rb") as file:
        config = plistlib.load(file)
    config["ProgramArguments"] = [str(HERE / "run-nightly.sh"), "--qual-home", str(home), "--keep", str(keep)]
    config["EnvironmentVariables"]["FOLDERSYNC_QUAL_PYTHON"] = str(python)
    config["StartCalendarInterval"] = {"Hour": hour, "Minute": minute}
    config["StandardErrorPath"] = str(home / "nightly.log")
    return config


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest="action", required=True)
    install = actions.add_parser("install")
    install.add_argument("--qual-home", required=True)
    install.add_argument("--keep", type=int, default=14)
    install.add_argument("--hour", type=int, default=2)
    install.add_argument("--minute", type=int, default=0)
    actions.add_parser("uninstall")
    args = parser.parse_args()
    os.umask(0o077)
    domain = f"gui/{os.getuid()}"
    agent = Path.home() / "Library" / "LaunchAgents" / f"{LABEL}.plist"
    try:
        if sys.platform != "darwin":
            raise ValueError("The nightly LaunchAgent requires macOS.")
        if args.action == "install":
            if args.keep < 1 or not 0 <= args.hour <= 23 or not 0 <= args.minute <= 59:
                raise ValueError("Pass a positive --keep and a valid local --hour and --minute.")
            home, _, _, _ = read_qual_home(args.qual_home)
            os.close(open_private_file(home / "nightly.log", os.O_WRONLY | os.O_CREAT))
            subprocess.run([LAUNCHCTL, "print", domain], stdout=subprocess.DEVNULL, check=True)
            agent.parent.mkdir(mode=0o700, exist_ok=True)
            config = make_plist(home, Path(sys.executable).resolve(), args.keep, args.hour, args.minute)
            with os.fdopen(open_private_file(agent, os.O_WRONLY | os.O_CREAT | os.O_EXCL), "wb") as file:
                plistlib.dump(config, file)
            try:
                subprocess.run([LAUNCHCTL, "bootstrap", domain, str(agent)], check=True)
            except subprocess.CalledProcessError:
                agent.unlink()
                raise
            print(f"Installed {LABEL} for {args.hour:02d}:{args.minute:02d} local time.")
        else:
            try:
                os.close(open_private_file(agent, os.O_RDONLY))
            except FileNotFoundError:
                return 0
            service = f"{domain}/{LABEL}"
            subprocess.run([LAUNCHCTL, "print", domain], stdout=subprocess.DEVNULL, check=True)
            query = subprocess.run([LAUNCHCTL, "print", service], capture_output=True)
            if query.returncode == 0:
                subprocess.run([LAUNCHCTL, "bootout", service], check=True)
            elif b"Could not find service" not in query.stderr:
                raise subprocess.CalledProcessError(query.returncode, query.args, stderr=query.stderr)
            agent.unlink()
            print(f"Uninstalled {LABEL}.")
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"{exc}. Run from a Terminal in the logged-in macOS GUI session.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
