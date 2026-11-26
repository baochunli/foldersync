"""Local macOS packaged-app qualification. All fixtures are disposable."""

import argparse
import datetime as dt
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from contextlib import closing

REPO = Path(__file__).resolve().parents[2]
TARGET = REPO / "target"
BUNDLE_ID = "dev.foldersync.pkgcheck"
EXTENSION_ID = BUNDLE_ID + ".finder-extension"
LOG_SUBSYSTEM = "dev.foldersync.app.finder-extension"
HELPER_PATH = "Contents/Helpers/rclone"
PLUGIN = ("/usr/bin/pluginkit",)
FINDER_WINDOW = ("/usr/bin/osascript", Path(__file__).with_name("finder-window.applescript"))
FINDER_CHECKS = ("finder_snapshot", "finder_watch", "finder_badge")
FINDER_LAUNCH_SECONDS = 120
FINDER_WINDOW_SECONDS = 12
PASS, FAIL, UNVERIFIED = "PASS", "FAIL", "NOT VERIFIED"
CHECKS = {
    "signature": "Packaged bundle and deep strict signature",
    "helper_digest": "Sealed bundled helper digest",
    "live_manager": "Live manager and exclusive lock",
    "second_instance": "Second instance refused state ownership",
    "sync_action": "ADR 0002 sync_now and bundled engine identity",
    "quit": "PID-targeted native quit and lock release",
    "interruption": "Interruption during an active multi-file run",
    "reconciliation": "Relaunch reconciles the run and holds recovery",
    "finder_snapshot": "Finder decoded the manager snapshot",
    "finder_watch": "Finder registered and observed the mapped root",
    "finder_badge": "Finder requested the expected root badge",
    "cleanup": "Disposable registration, processes, and fixtures removed",
}
COMMAND_ERRORS = (RuntimeError, OSError, subprocess.TimeoutExpired)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def overall(statuses):
    statuses = list(statuses)
    if FAIL in statuses:
        return FAIL
    return UNVERIFIED if UNVERIFIED in statuses else PASS


def console_status(root, uid):
    if not isinstance(root, dict):
        return "unknown"
    if root.get("IOConsoleLocked") is True:
        return "locked"
    sessions = root.get("IOConsoleUsers")
    if root.get("IOConsoleLocked") is not False or not isinstance(sessions, list):
        return "unknown"
    for session in sessions:
        if (isinstance(session, dict) and session.get("kCGSSessionUserIDKey") == uid
                and session.get("kCGSSessionOnConsoleKey") is True
                and session.get("kCGSessionLoginDoneKey") is True):
            return "locked" if session.get("CGSSessionScreenIsLocked") is True else "unlocked"
    return "absent"


def finder_evidence(logs, shared, root, expected_badge):
    # Init logs carry the unique handoff path. Correlate other events by PID.
    started = f"FolderSyncFinderExtension initialized sharedDir={shared} host={BUNDLE_ID}"
    pids = {row["processID"] for row in logs if row.get("eventMessage") == started and "processID" in row}
    messages = [row["eventMessage"] for row in logs
                if row.get("processID") in pids and "eventMessage" in row]
    # The extension sets directoryURLs only after a successful snapshot decode.
    registered = "Updated directoryURLs count=1"
    observed = f"beginObservingDirectory path={root}"
    requested = f"Badged root path={root} badge={expected_badge}"
    return {
        "finder_launch": [f"PID {pid}  {started}" for pid in sorted(pids)],
        "finder_snapshot": [m for m in messages if m in (started, registered)]
        if registered in messages else [],
        "finder_watch": [observed] if observed in messages else [],
        "finder_badge": [requested] if requested in messages else [],
    }


def checked_path(path, root):
    """Require a normalized descendant with no symlink components."""
    path = Path(path)
    require(path.is_absolute() and path != root and path.is_relative_to(root)
            and ".." not in path.parts, f"Path is outside {root}: {path}")
    for component in (path, *path.parents):
        require(not component.is_symlink(), f"Refusing symlink: {component}")
        if component == root:
            return path
    raise RuntimeError(f"Invalid path: {path}")


def dev_state(shared):
    require(shared.name == "finder" and shared.parent.parent == TARGET
            and re.fullmatch(r"finder-dev-shared\.[A-Za-z0-9]{8}", shared.parent.name),
            f"Unexpected dev handoff: {shared}")
    checked_path(shared, TARGET)
    for directory in (shared.parent, shared):
        info = directory.stat()
        require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o700, f"Handoff must be private: {directory}")
    require(set(shared.parent.iterdir()) == {shared} and not any(shared.iterdir()),
            f"Handoff is not fresh: {shared}")
    return shared.parent


def wait_for(description, check, seconds=25, interval=0.05):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(interval)
    raise RuntimeError(f"Timed out waiting for {description}")


def process_rows():
    output = subprocess.check_output(
        ["/bin/ps", "-ww", "-axo", "pid=,ppid=,pgid=,lstart=,comm="], text=True, timeout=5,
    )
    rows = {}
    for line in output.splitlines():
        fields = line.split(maxsplit=8)
        if len(fields) == 9:
            pid, parent, group = map(int, fields[:3])
            rows[pid] = (parent, group, " ".join(fields[3:8]), fields[8])
    return rows


class Qualification:
    def __init__(self, args, scratch, original_home):
        self.args = args
        self.scratch = scratch
        self.original_home = original_home
        self.labels = dict(CHECKS)
        if args.release:
            self.labels.update(gatekeeper="Gatekeeper assessment", stapler="Notarization staple")
        if args.with_keychain:
            self.labels["keychain"] = "Owner Keychain round trip"
        self.results = {key: {"name": name, "status": UNVERIFIED, "evidence": ["Check did not run."]}
                        for key, name in self.labels.items()}
        self.console = "unknown"
        self.app = scratch / "package" / "FolderSync.app"
        self.appex = self.app / "Contents/PlugIns/FolderSyncFinder.appex"
        self.binary = self.app / "Contents/MacOS/FolderSync"
        self.helper = self.app / HELPER_PATH
        self.extension = self.appex / "Contents/MacOS/FolderSyncFinder"
        self.quit_tool = scratch / "quit-app"
        self.state = scratch / "build-state"
        self.handoff_state = None
        self.local = scratch / "watched" / "Docs"
        self.remote = scratch / "peer"
        self.launched = []
        self.registration_attempted = False
        self.finder_attempts = []
        self.stage = "signature"
        self.env = {key: value for key, value in os.environ.items()
                    if key not in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR")
                    and not key.startswith(("FOLDERSYNC_", "RCLONE_", "XDG_"))}
        for variable, name in (("HOME", "home"), ("TMPDIR", "tmp"),
                               ("FOLDERSYNC_STATE_DIR", "build-state")):
            path = scratch / name
            path.mkdir(mode=0o700)
            self.env[variable] = str(path)
        self.env.setdefault("CARGO_HOME", str(original_home / ".cargo"))
        self.env.setdefault("RUSTUP_HOME", str(original_home / ".rustup"))

    def result(self, key, status, *evidence):
        self.results[key].update(status=status, evidence=list(evidence))
        print(f"{status:12} {self.labels[key]}", flush=True)

    def command(self, *argv, timeout=30, check=True, logfile=None, env=None):
        argv = [str(part) for part in argv]
        env = self.env if env is None else env
        if logfile is not None:
            print(f"Running {Path(argv[0]).name} (log: {logfile})", flush=True)
            with logfile.open("x") as output:
                proc = subprocess.run(argv, env=env, cwd=REPO, timeout=timeout,
                                      stdout=output, stderr=subprocess.STDOUT, text=True)
            proc.stdout = logfile.read_text(errors="replace")
        else:
            proc = subprocess.run(argv, env=env, cwd=REPO, timeout=timeout,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if check:
            require(proc.returncode == 0, f"{Path(argv[0]).name} exited {proc.returncode}: {proc.stdout[-4000:]}")
        return proc

    def inventory(self, verbosity="-v"):
        return self.command(*PLUGIN, "-m", "-A", "-D", verbosity, "-i", EXTENSION_ID).stdout.strip()

    def only_staged_registration(self):
        inventory = self.inventory("-vv")
        if inventory in ("", "(no matches)"):
            return False
        paths = re.findall(r"^\s*Path\s*=\s*(.+)$", inventory, flags=re.M)
        require(paths == [str(self.appex)], f"Unexpected existing disposable ID registration: {inventory}")
        return True

    def build_or_stage(self):
        require(self.inventory() in ("", "(no matches)"), f"{EXTENSION_ID} is already registered")
        # The workspace CLI initializes the local fixture in both modes.
        self.command(REPO / "scripts/fetch-rclone.sh", timeout=300)
        if not self.args.release:
            nsh = os.environ.get("NSH_BIN") or self.original_home / ".local/bin/nsh"
            log = self.scratch / "package.log"
            try:
                self.command(REPO / "packaging/build-app.sh", "--out", self.app.parent,
                             "--bundle-id", BUNDLE_ID, "--dev-shared-dir", "--nsh-bin", nsh,
                             timeout=1800, logfile=log)
            finally:
                if log.is_file():
                    paths = re.findall(r"^  State dir: +(.+)$", log.read_text(errors="replace"), re.M)
                    if len(paths) == 1:
                        self.handoff_state = dev_state(Path(paths[0]) / "finder")
        else:
            source = self.args.release.absolute()
            require(source.is_dir() and source.suffix == ".app" and not source.is_symlink(),
                    "--release requires a signed .app directory")
            self.app.parent.mkdir()
            self.command("/usr/bin/ditto", source, self.app, timeout=120)

        verified = self.command("/usr/bin/codesign", "--verify", "--deep", "--strict", "--verbose=2", self.app)
        host_info = plistlib.loads((self.app / "Contents/Info.plist").read_bytes())
        plugin_info = plistlib.loads((self.appex / "Contents/Info.plist").read_bytes())
        require(host_info["CFBundleIdentifier"] == plugin_info["FolderSyncHostBundleIdentifier"] == BUNDLE_ID
                and plugin_info["CFBundleIdentifier"] == EXTENSION_ID, "Expected disposable package IDs")
        shared = Path(plugin_info["FolderSyncSharedDir"])
        signed_state = dev_state(shared)
        require(self.handoff_state in (None, signed_state), "Signed handoff differs from build output")
        entitlements = self.scratch / "signed-entitlements.plist"
        self.command("/usr/bin/codesign", "-d", "--xml", "--entitlements", entitlements, self.appex)
        grants = plistlib.loads(entitlements.read_bytes())
        require(grants.get("com.apple.security.app-sandbox") is True
                and grants.get("com.apple.security.temporary-exception.files.absolute-path.read-write")
                == [f"{shared}/"], "Expected the signed sandbox exception for the isolated finder/ directory")
        self.result("signature", PASS, verified.stdout.strip(), f"{BUNDLE_ID}; signed isolated handoff {shared}")

        if self.args.release:
            self.stage = "gatekeeper"
            signature = self.command("/usr/bin/codesign", "-dv", "--verbose=4", self.app).stdout
            require("Authority=Developer ID Application:" in signature, "Expected Developer ID Application signature")
            gate = self.command("/usr/sbin/spctl", "--assess", "--type", "execute", "--verbose=4", self.app)
            self.result("gatekeeper", PASS, gate.stdout.strip())
            self.stage = "stapler"
            staple = self.command("/usr/bin/xcrun", "stapler", "validate", self.app, timeout=120)
            self.result("stapler", PASS, staple.stdout.strip())
        self.handoff_state = self.state = signed_state
        self.env["FOLDERSYNC_STATE_DIR"] = str(self.state)

        self.stage = "helper_digest"
        self.manifest = json.loads((self.app / "Contents/Resources/rclone-manifest.json").read_text())
        digest = hashlib.sha256(self.helper.read_bytes()).hexdigest()
        require(self.manifest["bundle_relative_path"] == HELPER_PATH
                and self.manifest["bundled_sha256"] == digest, "Bundled helper digest differs from sealed manifest")
        self.result("helper_digest", PASS, f"{HELPER_PATH} SHA-256 {digest}")

    def sql(self, query, values=()):
        db = self.state / "state.sqlite"
        with closing(sqlite3.connect(db.as_uri() + "?mode=ro", uri=True, timeout=2)) as conn:
            conn.row_factory = sqlite3.Row
            return [dict(row) for row in conn.execute(query, values)]

    def manager_locked(self):
        with (self.state / "manager.lock").open() as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError as exc:
                if exc.errno in (errno.EAGAIN, errno.EACCES):
                    return True
                raise
        return False

    def owns_lock(self, proc):
        return (proc.poll() is None and self.manager_locked()
                and (self.state / "manager.lock").read_text().strip() == f"pid={proc.pid}")

    @property
    def snapshot_path(self):
        return self.state / "finder/status.json"

    def snapshot_matches(self, badge):
        if not self.snapshot_path.is_file():
            return False
        mappings = json.loads(self.snapshot_path.read_text())["mappings"]
        return len(mappings) == 1 and (mappings[0]["id"], mappings[0]["root_path"], mappings[0]["state"]) == (
            "docs", str(self.local), badge,
        )

    def prepare_fixture(self):
        self.command("cargo", "build", "--locked", "-p", "sync-cli", timeout=1800,
                     logfile=self.scratch / "cli-build.log")
        cli = (TARGET / "debug/sync-cli", "--state-dir", self.state)
        self.local.mkdir(parents=True)
        self.remote.mkdir()
        (self.local / "baseline.txt").write_text("Packaged app qualification fixture\n")
        self.command(*cli, "add-mapping", "--id", "docs", "--local-dir", self.local,
                     "--remote-dir", self.remote, "--recovery-dir", self.scratch / "local-recovery",
                     "--remote-recovery-dir", self.scratch / "peer-recovery")
        self.command(*cli, "scheduling", "pause")
        self.command(*cli, "init", "docs", timeout=180)
        require((self.remote / "baseline.txt").read_bytes() == (self.local / "baseline.txt").read_bytes(),
                "Fixture initialization did not reach the local peer")
        self.command("swiftc", "-module-cache-path", self.scratch / "swift-modules",
                     Path(__file__).with_name("quit-app.swift"), "-o", self.quit_tool, timeout=120)

    def launch(self):
        path = self.scratch / f"app-{len(self.launched) + 1}.log"
        with path.open("x") as output:
            proc = subprocess.Popen([self.binary], env=self.env, cwd=self.scratch, start_new_session=True,
                                    stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
        self.launched.append((proc, path))
        return proc

    def launch_manager(self, badge):
        self.snapshot_path.unlink(missing_ok=True)
        proc = self.launch()
        wait_for("packaged manager lock and new snapshot",
                 lambda: self.owns_lock(proc) and self.snapshot_matches(badge), seconds=35)
        return proc

    def appkit(self, proc, operation, check=True):
        require(proc.poll() is None, f"App PID {proc.pid} has already exited")
        return self.command(self.quit_tool, operation, proc.pid, BUNDLE_ID, self.app, check=check)

    def quit(self, proc):
        wait_for(f"native app registration for PID {proc.pid}",
                 lambda: self.appkit(proc, "ready", check=False).returncode == 0, seconds=12)
        self.appkit(proc, "quit")
        require(proc.wait(timeout=35) == 0, f"Native quit of PID {proc.pid} did not exit cleanly")

    def queue_sync(self):
        actions = self.state / "finder/actions"
        actions.mkdir(exist_ok=True, mode=0o700)
        request_id = str(uuid.uuid4())
        request = {
            "version": 1, "id": request_id, "action": "sync_now", "mapping_id": "docs",
            "requested_at": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        path = actions / f"action-{request_id}.json"
        pending = actions / f".{request_id}.tmp"
        with pending.open("x") as output:
            json.dump(request, output)
        pending.rename(path)
        return path

    def latest_normal(self):
        rows = self.sql("SELECT * FROM runs WHERE mapping_id = 'docs' AND mode = 'normal' ORDER BY rowid DESC LIMIT 1")
        return rows[0] if rows else None

    def run_lifecycle(self):
        self.stage = "live_manager"
        self.prepare_fixture()
        first = self.launch_manager("paused")
        self.result("live_manager", PASS, f"PID {first.pid} acquired manager.lock and republished the paused fixture snapshot.")

        self.stage = "second_instance"
        before = self.snapshot_path.stat().st_mtime_ns
        second = self.launch()
        self.quit(second)
        require(self.owns_lock(first) and self.snapshot_path.stat().st_mtime_ns == before,
                "Second packaged instance changed state or displaced the first lock owner")
        self.result("second_instance", PASS,
                    f"Second PID {second.pid} launched and quit with status 0. "
                    f"PID {first.pid} retained the exclusive lock and snapshot timestamp {before}.")

        self.stage = "sync_action"
        payload = b"Written after the packaged manager started.\n"
        (self.local / "queued.txt").write_bytes(payload)
        request = self.queue_sync()

        def normal_finished():
            run = self.latest_normal()
            return run if run and run["finished_at"] else None

        run = wait_for("a completed queued sync", normal_finished, seconds=90)
        require(not request.exists() and (self.remote / "queued.txt").read_bytes() == payload,
                "Action remained queued or the synced file did not match")
        require((run["status"], run["outcome_kind"], run["exit_code"]) == ("completed", "success", 0),
                f"Unexpected normal run outcome: {run['status']}/{run['outcome_kind']}/{run['exit_code']}")
        require((run["child_executable"], run["engine_version"], run["engine_platform"], run["engine_sha256"])
                == (str(self.helper), self.manifest["version"], self.manifest["platform"],
                    self.manifest["bundled_sha256"]), "Run record did not identify the sealed bundled helper")
        argv = json.loads((Path(run["run_dir"]) / "argv.json").read_text())["argv"]
        config = self.state / "accounts/local-fixture/rclone.conf"
        require(argv[argv.index("--config") + 1] == str(config), "Run did not pass its fixture config explicitly")
        self.result("sync_action", PASS, f"Consumed {request.name}; byte-identical local copy.",
                    f"Run {run['id']} completed/success/0; helper={run['child_executable']}; "
                    f"{run['engine_version']} {run['engine_platform']} SHA-256 {run['engine_sha256']}",
                    f"--config {config}")

        try:
            self.run_finder()
        except (*COMMAND_ERRORS, ValueError, KeyError) as exc:
            self.result(self.stage, FAIL, str(exc))

        self.stage = "quit"
        self.quit(first)
        require(not self.manager_locked() and not self.sql("SELECT id FROM runs WHERE finished_at IS NULL"),
                "Native quit left the lock held or an undrained run")
        self.result("quit", PASS, f"Native quit of PID {first.pid} exited 0 after the queued run drained; "
                    "exclusive flock became available.")

        self.stage = "interruption"
        fixture = self.local / "batch"
        fixture.mkdir()
        count = 1000
        payload = os.urandom(128 * 1024)
        for index in range(count):
            (fixture / f"chunk-{index:04}.bin").write_bytes(index.to_bytes(4, "big") + payload)
        active_app = self.launch_manager("paused")
        workdir = Path(run["workdir_path"])
        listing_files = {path: hashlib.sha256(path.read_bytes()).digest()
                         for path in workdir.glob("*.lst")}
        require(listing_files, "Initialized workdir has no bisync listings")
        request = self.queue_sync()

        def progress():
            return sum(1 for _ in (self.remote / "batch").glob("chunk-*.bin"))

        def active_transfer():
            active = self.latest_normal()
            if not active or active["id"] == run["id"] or not active["child_pid"] or active["finished_at"]:
                return None
            progress_now = progress()
            if not 0 < progress_now < count:
                return None
            child = process_rows().get(active["child_pid"])
            if child and child[:2] == (active_app.pid, active_app.pid) and child[3] == str(self.helper):
                return active, child
            return None

        active, identity = wait_for("bundled helper and partial multi-file progress", active_transfer,
                                    seconds=60, interval=0.005)
        child_pid = active["child_pid"]
        # Pause the manager so it cannot commit an outcome during the crash injection.
        active_app.send_signal(signal.SIGSTOP)
        require(process_rows().get(child_pid) == identity, "Helper exited before interruption")
        os.kill(child_pid, signal.SIGSTOP)
        copied = progress()
        require(0 < copied < count and active["workdir_path"] == str(workdir) and not request.exists(),
                f"Unproven mid-transfer interruption: {copied}/{count} files")
        require(process_rows().get(child_pid) == identity, "Helper identity changed before SIGKILL")
        os.kill(child_pid, signal.SIGKILL)
        active_app.kill()
        active_app.wait(timeout=15)
        wait_for("interrupted helper to exit",
                 lambda: process_rows().get(child_pid, (None, None, None, None))[3] != str(self.helper), seconds=15)
        require(self.latest_normal()["id"] == active["id"] and self.latest_normal()["status"] == "running",
                "Interrupted run was not left unfinished for startup reconciliation")
        self.result("interruption", PASS,
                    f"Stopped fixture manager PID {active_app.pid} and helper PID {child_pid} at "
                    f"{copied}/{count} completed transfers, then killed both tracked PIDs. Run remained running in SQLite.")

        self.stage = "reconciliation"
        revived = self.launch_manager("attention")
        interrupted = self.latest_normal()
        require((interrupted["id"], interrupted["status"], interrupted["outcome_kind"],
                 interrupted["last_verified_phase"])
                == (active["id"], "interrupted", "interrupted", "reconciled_exited"),
                "Relaunch did not reconcile the unfinished run as interrupted")
        mapping = self.sql("SELECT readiness FROM mappings WHERE id = 'docs'")
        case = self.sql("SELECT status, reason_kind FROM recovery_cases WHERE run_id = ?", (active["id"],))
        outcome = json.loads((Path(interrupted["run_dir"]) / "outcome.json").read_text())
        require(mapping == [{"readiness": "recovery_required"}]
                and case == [{"status": "open", "reason_kind": "interrupted"}]
                and outcome["last_verified_phase"] == "reconciled_exited"
                and all(path.is_file() and hashlib.sha256(path.read_bytes()).digest() == digest
                        for path, digest in listing_files.items()),
                "RecoveryRequired hold, outcome evidence, or original workdir listings were missing")
        self.result("reconciliation", PASS,
                    f"Relaunch PID {revived.pid}: interrupted/reconciled_exited, RecoveryRequired, open recovery case.",
                    f"Original {workdir} retained {len(listing_files)} byte-identical listings. "
                    "Finder snapshot badge=attention.")
        self.stage = "quit"
        self.quit(revived)
        require(not self.manager_locked(), "Relaunched manager retained its lock on quit")
        self.results["quit"]["evidence"].append(f"Relaunched PID {revived.pid} also exited 0 and released its lock.")

    def check_console(self):
        try:
            result = self.command("/usr/sbin/ioreg", "-a", "-n", "Root", "-d", "1")
            self.console = console_status(plistlib.loads(result.stdout.encode()), os.getuid())
        except (*COMMAND_ERRORS, ValueError, plistlib.InvalidFileException):
            self.console = "unknown"
        return self.console

    def read_finder_evidence(self, start):
        # Unified logging can reuse the image path from an earlier bundle with the same Mach-O UUID.
        predicate = f'subsystem == "{LOG_SUBSYSTEM}"'
        raw = self.command("/usr/bin/log", "show", "--start", start.strftime("%Y-%m-%d %H:%M:%S"),
                           "--style", "ndjson", "--no-pager", "--predicate", predicate).stdout
        logs = []
        for line in raw.splitlines():
            if not line.startswith("{"):
                continue
            row = json.loads(line)
            if "timestamp" in row:
                timestamp = dt.datetime.strptime(row["timestamp"], "%Y-%m-%d %H:%M:%S.%f%z")
                # log show accepts whole seconds. Exclude activity before this election.
                if timestamp >= start:
                    logs.append(row)
        return finder_evidence(logs, self.state / "finder", self.local, "paused")

    def poll_finder(self, attempt, start, key, seconds):
        deadline = time.monotonic() + seconds
        while self.check_console() == "unlocked":
            attempt["observations"] = self.read_finder_evidence(start)
            remaining = deadline - time.monotonic()
            if attempt["observations"][key] or remaining <= 0:
                break
            time.sleep(min(3, remaining))

    def close_finder_window(self, window):
        self.command(*FINDER_WINDOW, "close", window["id"], window["target"], env=os.environ)
        window["closed"] = True

    def run_finder_attempt(self, attempt):
        # LaunchServices can discover the extension when the packaged host starts.
        self.only_staged_registration()
        self.registration_attempted = True
        if attempt["number"] > 1:
            self.command(*PLUGIN, "-e", "ignore", "-i", EXTENSION_ID)
        start = dt.datetime.now().astimezone()
        self.command(*PLUGIN, "-a", self.appex)
        self.command(*PLUGIN, "-e", "use", "-i", EXTENSION_ID)
        elected = self.inventory("-vv")
        require(elected.startswith("+") and str(self.appex) in elected,
                f"PluginKit did not elect the staged extension: {elected}")
        attempt["election"] = elected.splitlines()[0]
        print(f"Finder attempt {attempt['number']}/3. Waiting up to {FINDER_LAUNCH_SECONDS}s for initialization.",
              flush=True)

        wait_started = time.monotonic()
        self.poll_finder(attempt, start, "finder_launch", FINDER_LAUNCH_SECONDS)
        attempt["launch_wait_seconds"] = round(time.monotonic() - wait_started, 1)
        if self.console != "unlocked":
            return f"Console became {self.console} while waiting for Finder."
        if not attempt["observations"]["finder_launch"]:
            return f"No matching initialization within {FINDER_LAUNCH_SECONDS}s. No windows opened."

        # Show the root as an item in its parent first, then browse the root itself.
        for directory, key in ((self.local.parent, "finder_badge"), (self.local, "finder_watch")):
            if self.check_console() != "unlocked":
                return f"Console became {self.console} during Finder navigation."
            result = self.command(*FINDER_WINDOW, "open", directory, attempt["view"], env=os.environ)
            window = {"id": int(result.stdout.strip()), "target": str(directory), "closed": False}
            attempt["windows"].append(window)
            try:
                self.poll_finder(attempt, start, key, FINDER_WINDOW_SECONDS)
            finally:
                self.close_finder_window(window)
        return f"Finder navigation finished with console {self.console}."

    def run_finder(self):
        self.stage = "finder_snapshot"
        observations = finder_evidence([], self.state / "finder", self.local, "paused")
        for number, view in enumerate(("icon", "list", "icon"), start=1):
            if self.check_console() != "unlocked":
                break
            # Each election must earn its own evidence, independent of earlier attempts.
            attempt = {"number": number, "view": view, "windows": [],
                       "outcome": "Attempt interrupted before completion.",
                       "observations": {key: [] for key in observations}}
            self.finder_attempts.append(attempt)
            try:
                attempt["outcome"] = self.run_finder_attempt(attempt)
            except (*COMMAND_ERRORS, ValueError) as exc:
                attempt["outcome"] = str(exc)
            print(f"Finder attempt {number}/3. {attempt['outcome']}", flush=True)
            for key, messages in attempt["observations"].items():
                observations[key].extend(messages)
            if self.console != "unlocked" or all(observations.values()):
                break

        if self.console != "unlocked" or self.check_console() != "unlocked":
            for key in FINDER_CHECKS:
                self.result(key, UNVERIFIED, f"Console session is {self.console} (environment).")
            return
        if not observations["finder_launch"]:
            for key in FINDER_CHECKS:
                self.result(key, UNVERIFIED, "No matching disposable extension initialization in any attempt (environment).")
            return

        visited_root = any(window["target"] == str(self.local)
                           for attempt in self.finder_attempts for window in attempt["windows"])
        missing = {
            "finder_snapshot": (FAIL, "Extension initialized without a matching snapshot watch registration."),
            "finder_watch": (FAIL if visited_root else UNVERIFIED, "No observation of the fixture root."),
            "finder_badge": (
                FAIL if observations["finder_watch"] else UNVERIFIED,
                "No exact paused root badge request in any attempt.",
            ),
        }
        for key in FINDER_CHECKS:
            messages = observations[key]
            if messages:
                self.result(key, PASS, *dict.fromkeys(messages))
            else:
                self.result(key, *missing[key])

    def cleanup(self):
        problems = []
        for attempt in self.finder_attempts:
            for window in attempt["windows"]:
                if not window["closed"]:
                    try:
                        self.close_finder_window(window)
                    except COMMAND_ERRORS as exc:
                        problems.append(str(exc))
        registered = self.registration_attempted
        if self.appex.is_dir():
            try:
                registered = self.only_staged_registration() or registered
                if registered:
                    self.command(*PLUGIN, "-e", "ignore", "-i", EXTENSION_ID)
            except COMMAND_ERRORS as exc:
                problems.append(str(exc))
            if registered:
                try:
                    self.command(*PLUGIN, "-r", self.appex)
                except COMMAND_ERRORS as exc:
                    problems.append(str(exc))

        for proc, _ in self.launched:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=15)

        owned_paths = {str(self.binary), str(self.helper), str(self.extension)}
        leftovers = {pid: row for pid, row in process_rows().items() if row[3] in owned_paths}
        for pid, identity in leftovers.items():
            if process_rows().get(pid) == identity:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        wait_for("all disposable bundle processes to exit",
                 lambda: not any(row[3] in owned_paths for row in process_rows().values()), seconds=15)

        app_pids = ", ".join(f"{proc.pid}={proc.returncode}" for proc, _ in self.launched) or "none launched"
        for proc, path in self.launched:
            tail = path.read_text(errors="replace").strip()[-1000:]
            if tail:
                self.results[self.stage]["evidence"].append(f"App PID {proc.pid} log excerpt: {tail}")
        if self.handoff_state is not None:
            shutil.rmtree(checked_path(self.handoff_state, TARGET))
        shutil.rmtree(checked_path(self.scratch, TARGET))
        inventory = self.inventory()
        if (registered or self.launched) and inventory not in ("", "(no matches)"):
            problems.append("Disposable extension still registered: " + inventory)
        real_state = self.original_home / "Library/Application Support/FolderSync"
        absent = not os.path.lexists(real_state)
        self.result("cleanup", FAIL if problems else PASS,
                    f"pluginkit -m (disposable ID): {inventory or '(no matches)'}",
                    f"Reaped app PIDs and exit codes: {app_pids}. No disposable app, helper, or extension process remains.",
                    "Disposable staging bundle, fixture trees and isolated state removed.",
                    f"Real FolderSync state directory {'absent' if absent else 'exists and was left untouched'}.",
                    *problems)

    def run(self):
        try:
            self.check_console()
            self.build_or_stage()
            self.run_lifecycle()
        except (*COMMAND_ERRORS, ValueError, KeyError, sqlite3.Error) as exc:
            self.result(self.stage, FAIL, str(exc))
        finally:
            if self.args.with_keychain:
                self.result("keychain", UNVERIFIED,
                            "Blocked: the packaged helper only reads the production service; "
                            "it has no same-binary create/read/delete probe for dev.foldersync.test.")
            try:
                self.cleanup()
            except COMMAND_ERRORS as exc:
                self.result("cleanup", FAIL, str(exc))


def write_evidence(output, check):
    items = list(check.results.values())
    status = overall(item["status"] for item in items)
    evidence = {"status": status, "console": check.console, "checks": items,
                "finder_attempts": check.finder_attempts}
    markdown = [f"# Packaged app qualification — {status}", "", f"Console session: {check.console}", ""]
    for item in items:
        markdown += [f"## {item['status']} — {item['name']}", "", "```text", *item["evidence"], "```", ""]
    for attempt in check.finder_attempts:
        markdown += [f"## Finder attempt {attempt['number']} ({attempt['view']} view)", "", "```text"]
        if "election" in attempt:
            markdown.append(attempt["election"])
        if "launch_wait_seconds" in attempt:
            markdown.append(f"Waited {attempt['launch_wait_seconds']}s for initialization.")
        for window in attempt["windows"]:
            closed = "closed" if window["closed"] else "open"
            markdown.append(f"Window {window['id']} {closed}  {window['target']}")
        markdown += list(dict.fromkeys(message for messages in attempt["observations"].values()
                                       for message in messages))
        markdown += [attempt["outcome"], "```", ""]
    for suffix, content in (("json", json.dumps(evidence, indent=2)), ("md", "\n".join(markdown))):
        destination = checked_path(output / f"app-evidence.{suffix}", TARGET)
        with tempfile.NamedTemporaryFile(mode="w", dir=output, delete=False) as temporary:
            temporary.write(content + "\n")
        os.replace(temporary.name, destination)
    print(f"{status}: {output / 'app-evidence.md'}", flush=True)
    return {PASS: 0, FAIL: 1, UNVERIFIED: 2}[status]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--with-keychain", action="store_true", help="owner-only packaged Keychain probe")
    parser.add_argument("--release", type=Path, metavar="APP", help="owner-only signed, stapled disposable bundle")
    args = parser.parse_args()
    require(sys.platform == "darwin" and os.getuid() != 0, "Run as a regular user on macOS")
    os.umask(0o077)
    output = checked_path(TARGET / "app-qualification", TARGET)
    output.mkdir(parents=True, exist_ok=True)
    lock_fd = os.open(output / "run.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd, "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        scratch = Path(tempfile.mkdtemp(prefix="run.", dir=output))
        check = Qualification(args, scratch, Path.home())
        signal.signal(signal.SIGTERM, lambda *_: signal.raise_signal(signal.SIGINT))
        try:
            check.run()
        except KeyboardInterrupt:
            check.result(check.stage, FAIL, "Qualification interrupted.")
        return write_evidence(output, check)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, OSError) as exc:
        sys.exit(str(exc))
