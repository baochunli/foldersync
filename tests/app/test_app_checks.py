import unittest

from app_checks import console_status, finder_evidence, overall


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


if __name__ == "__main__":
    unittest.main()
