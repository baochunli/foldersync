from datetime import datetime
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch

from app_checks import (
    BUNDLE_ID, EXTENSION_ID, FAIL, LOG_SUBSYSTEM, PASS, UNVERIFIED, Qualification,
    console_status, finder_evidence, overall,
)


class AppChecksTest(unittest.TestCase):
    def test_console_requires_foreground_logged_in_user(self):
        signed_in = {
            "kCGSSessionUserIDKey": 501,
            "kCGSSessionOnConsoleKey": True,
            "kCGSessionLoginDoneKey": True,
        }
        root = {"IOConsoleLocked": False, "IOConsoleUsers": [signed_in]}
        self.assertEqual(console_status(root, 501), "unlocked")
        self.assertEqual(console_status(root, 502), "absent")
        self.assertEqual(console_status({**root, "IOConsoleLocked": True}, 501), "locked")
        self.assertEqual(console_status({}, 501), "unknown")
        signed_in["CGSSessionScreenIsLocked"] = True
        self.assertEqual(console_status(root, 501), "locked")

    def test_finder_log_requires_matching_extension_launch_and_exact_badge(self):
        shared = "/scratch/state/finder"
        root = "/scratch/fixture"
        logs = [
            {"processID": 10, "eventMessage": f"FolderSyncFinderExtension initialized sharedDir={shared} host=dev.foldersync.pkgcheck"},
            {"processID": 10, "eventMessage": "Updated directoryURLs count=1"},
            {"processID": 10, "eventMessage": f"beginObservingDirectory path={root}"},
            {"processID": 11, "eventMessage": f"Badged root path={root} badge=paused"},
            {"processID": 10, "eventMessage": f"Updated badge root={root} badge=paused"},
            {"processID": 10, "eventMessage": f"Badged root path={root}/child badge=paused"},
            {"processID": 10, "eventMessage": f"Badged root path={root} badge=stale"},
        ]
        evidence = finder_evidence(logs, shared, root, "paused")
        self.assertTrue(evidence["finder_snapshot"])
        self.assertTrue(evidence["finder_watch"])
        self.assertFalse(evidence["finder_badge"])
        logs.append({"processID": 10, "eventMessage": f"Badged root path={root} badge=paused"})
        self.assertTrue(finder_evidence(logs, shared, root, "paused")["finder_badge"])

    def test_summary_never_passes_with_item_unverified(self):
        self.assertEqual(overall(["PASS", "PASS"]), "PASS")
        self.assertEqual(overall(["PASS", "NOT VERIFIED"]), "NOT VERIFIED")
        self.assertEqual(overall(["NOT VERIFIED", "FAIL"]), "FAIL")


class QualificationTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        target = Path(temporary.name).resolve()
        scratch = target / "run"
        scratch.mkdir()
        target_patch = patch("app_checks.TARGET", target)
        target_patch.start()
        self.addCleanup(target_patch.stop)
        self.qualification = Qualification(SimpleNamespace(release=None, with_keychain=False),
                                           scratch, target / "original-home")

    def simulated_finder(self, rows, console="unlocked"):
        qualification = self.qualification
        qualification.console = console
        elected = f"+ {EXTENSION_ID}\nPath = {qualification.appex}"
        commands = []
        window_id = 15
        elapsed = 0

        def run(argv, **kwargs):
            nonlocal window_id
            commands.append((argv, kwargs["env"], elapsed))
            output = ""
            if argv[0] == "/usr/bin/log" and argv[-1] == f'subsystem == "{LOG_SUBSYSTEM}"':
                timestamp = datetime.now().astimezone().strftime("%Y-%m-%d %H:%M:%S.%f%z")
                output = "\n".join(json.dumps({"timestamp": timestamp, **row}) for row in rows)
            if argv[0] == "/usr/bin/osascript" and argv[2] == "open":
                window_id += 1
                output = f"{window_id}\n"
            return subprocess.CompletedProcess(argv, 0, output)

        def tick(seconds):
            nonlocal elapsed
            elapsed += seconds

        with (patch.object(qualification, "check_console", return_value=console),
              patch.object(qualification, "inventory", return_value=elected),
              patch("app_checks.subprocess.run", side_effect=run),
              patch("app_checks.time.sleep", side_effect=tick),
              patch("app_checks.time.monotonic", side_effect=lambda: elapsed)):
            qualification.run_finder()
        return commands

    def test_no_matching_launch_retries_and_marks_finder_unverified(self):
        root = self.qualification.local
        shared = self.qualification.state / "finder"
        rows = [
            {"processID": 23, "eventMessage": f"FolderSyncFinderExtension initialized sharedDir=/other/finder host={BUNDLE_ID}"},
            {"processID": 23, "eventMessage": f"Badged root path={root} badge=paused"},
            {"processID": 24, "timestamp": "2000-01-01 00:00:00.000000+0000",
             "eventMessage": f"FolderSyncFinderExtension initialized sharedDir={shared} host={BUNDLE_ID}"},
            {"processID": 24, "eventMessage": f"Badged root path={root} badge=paused"},
        ]
        commands = self.simulated_finder(rows)
        self.assertEqual([self.qualification.results[key]["status"]
                          for key in ("finder_snapshot", "finder_watch", "finder_badge")],
                         [UNVERIFIED, UNVERIFIED, UNVERIFIED])
        self.assertEqual([elapsed for argv, _, elapsed in commands if argv[:3] == ["/usr/bin/pluginkit", "-e", "use"]],
                         [0, 120, 240])
        self.assertEqual([attempt["launch_wait_seconds"] for attempt in self.qualification.finder_attempts],
                         [120, 120, 120])
        self.assertFalse(any(argv[0] in ("/usr/bin/open", "/usr/bin/osascript") for argv, _, _ in commands))

    def test_locked_console_skips_finder_and_marks_checks_unverified(self):
        commands = self.simulated_finder([], console="locked")
        self.assertEqual(commands, [])
        for key in ("finder_snapshot", "finder_watch", "finder_badge"):
            self.assertEqual(self.qualification.results[key]["status"], UNVERIFIED)
            self.assertIn("locked", self.qualification.results[key]["evidence"][0])

    def test_observed_root_without_exact_badge_fails_after_three_attempts(self):
        qualification = self.qualification
        shared = qualification.state / "finder"
        root = qualification.local
        messages = [
            f"FolderSyncFinderExtension initialized sharedDir={shared} host={BUNDLE_ID}",
            "Updated directoryURLs count=1",
            f"beginObservingDirectory path={root}",
            f"Updated badge root={root} badge=paused",
            f"Badged root path={root}/child badge=paused",
            f"Badged root path={root} badge=stale",
        ]
        rows = [{"processID": 10, "eventMessage": message} for message in messages]
        rows.append({"processID": 11, "eventMessage": f"Badged root path={root} badge=paused"})
        commands = self.simulated_finder(rows)
        self.assertEqual([qualification.results[key]["status"]
                          for key in ("finder_snapshot", "finder_watch", "finder_badge")],
                         [PASS, PASS, FAIL])
        self.assertEqual(sum(argv[:3] == ["/usr/bin/pluginkit", "-e", "use"] for argv, _, _ in commands), 3)
        opens = [argv for argv, _, _ in commands if argv[0] == "/usr/bin/osascript" and argv[2] == "open"]
        closes = [argv for argv, _, _ in commands if argv[0] == "/usr/bin/osascript" and argv[2] == "close"]
        self.assertEqual([argv[3] for argv in opens], [str(root.parent), str(root)] * 3)
        self.assertEqual([argv[3] for argv in closes], ["16", "17", "18", "19", "20", "21"])

    def test_finder_opens_root_and_parent_and_accepts_a_cached_image_path(self):
        qualification = self.qualification
        shared = qualification.state / "finder"
        root = qualification.local
        messages = [
            "Updated directoryURLs count=1",
            f"FolderSyncFinderExtension initialized sharedDir={shared} host={BUNDLE_ID}",
            f"beginObservingDirectory path={root}",
            f"Badged root path={root} badge=paused",
        ]
        rows = [{"processID": 10, "processImagePath": "/old/FolderSyncFinder", "eventMessage": message}
                for message in messages]
        commands = self.simulated_finder(rows)

        with self.subTest("Finder navigation"):
            first_window = next(i for i, (argv, _, _) in enumerate(commands) if argv[0] == "/usr/bin/osascript")
            self.assertTrue(any(argv[0] == "/usr/bin/log" for argv, _, _ in commands[:first_window]))
            self.assertEqual([(argv[2:], env["HOME"]) for argv, env, _ in commands if argv[0] == "/usr/bin/osascript"], [
                (["open", str(root.parent), "icon"], os.environ["HOME"]),
                (["close", "16", str(root.parent)], os.environ["HOME"]),
                (["open", str(root), "icon"], os.environ["HOME"]),
                (["close", "17", str(root)], os.environ["HOME"]),
            ])
        with self.subTest("cached image path"):
            self.assertEqual(
                [qualification.results[key]["status"] for key in ("finder_snapshot", "finder_watch", "finder_badge")],
                ["PASS", "PASS", "PASS"],
            )

    def test_release_checks_the_workspace_rclone_pin(self):
        qualification = self.qualification
        source = qualification.scratch / "Owner.app"
        source.mkdir()
        qualification.args.release = source

        def stop_at_staging(*argv, **_):
            if argv[0] == "/usr/bin/ditto":
                raise RuntimeError("stop before staging signed app")

        with (patch.object(qualification, "inventory", return_value="(no matches)"),
              patch.object(qualification, "command", side_effect=stop_at_staging) as command):
            with self.assertRaisesRegex(RuntimeError, "stop before staging signed app"):
                qualification.build_or_stage()

        paths = [entry.args[0] for entry in command.call_args_list]
        self.assertEqual([Path(path).name for path in paths], ["fetch-rclone.sh", "ditto"])

    def test_cleanup_leaves_unregistered_extension_alone(self):
        qualification = self.qualification
        qualification.appex.mkdir(parents=True)
        log = qualification.scratch / "app.log"
        log.touch()
        qualification.launched.append((Mock(pid=123, returncode=0, poll=Mock(return_value=0)), log))
        with (patch.object(qualification, "inventory", return_value="(no matches)"),
              patch.object(qualification, "command") as command,
              patch("app_checks.process_rows", return_value={})):
            qualification.cleanup()
        command.assert_not_called()

    def test_cleanup_unregisters_extension_discovered_without_a_launch(self):
        qualification = self.qualification
        qualification.appex.mkdir(parents=True)
        inventory = [f"+ {EXTENSION_ID}\nPath = {qualification.appex}", "(no matches)"]
        with (patch.object(qualification, "inventory", side_effect=inventory),
              patch.object(qualification, "command") as command,
              patch("app_checks.process_rows", return_value={})):
            qualification.cleanup()
        self.assertEqual(command.call_args_list, [
            call("/usr/bin/pluginkit", "-e", "ignore", "-i", EXTENSION_ID),
            call("/usr/bin/pluginkit", "-r", qualification.appex),
        ])


if __name__ == "__main__":
    unittest.main()
