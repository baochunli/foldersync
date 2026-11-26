import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch

from app_checks import (
    BUNDLE_ID, EXTENSION_ID, LOG_SUBSYSTEM, Qualification, console_status, finder_evidence, overall,
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

    def test_finder_opens_root_and_parent_and_accepts_a_cached_image_path(self):
        qualification = self.qualification
        qualification.console = "unlocked"
        shared = qualification.state / "finder"
        root = qualification.local
        messages = [
            "Updated directoryURLs count=1",
            f"FolderSyncFinderExtension initialized sharedDir={shared} host={BUNDLE_ID}",
            f"beginObservingDirectory path={root}",
            f"Badged root path={root} badge=paused",
        ]
        archive = "\n".join(json.dumps({"processID": 10, "processImagePath": "/old/FolderSyncFinder",
                                       "eventMessage": message}) for message in messages)
        opens = []

        def run(argv, **kwargs):
            output = ""
            if argv[0] == "/usr/bin/open":
                opens.append((argv[1:], kwargs["env"]["HOME"]))
            if argv[0] == "/usr/bin/log" and argv[-1] == f'subsystem == "{LOG_SUBSYSTEM}"':
                output = archive
            return subprocess.CompletedProcess(argv, 0, output)

        elected = f"+ {EXTENSION_ID}\nPath = {qualification.appex}"
        with (patch.object(qualification, "check_console", return_value="unlocked"),
              patch.object(qualification, "inventory", side_effect=["(no matches)", elected]),
              patch("app_checks.subprocess.run", side_effect=run),
              patch("app_checks.time.sleep")):
            qualification.run_finder()

        with self.subTest("Finder navigation"):
            self.assertEqual(opens, [
                (["-a", "Finder", str(root)], os.environ["HOME"]),
                (["-a", "Finder", str(root.parent)], os.environ["HOME"]),
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
