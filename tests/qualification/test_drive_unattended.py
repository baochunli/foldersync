"""Safety and evidence checks for the owner-run Drive entry points."""

import json
import io
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

import drive_setup
from common import ScenarioEvidence, compute_overall_status
from run_drive_qualification import (
    DRIVE_FOLDER, Client, DriveQualification, DriveRunner, capture_record, lock_qual_home,
    publish_latest, upload_progress, write_drive_evidence,
)


class UnattendedDriveTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def test_setup_refuses_existing_home_and_symlink_ancestors(self):
        existing = self.root / "existing"
        existing.mkdir(mode=0o700)
        link = self.root / "link"
        link.symlink_to(existing, target_is_directory=True)
        for path in (existing, link, link / "child"):
            with self.subTest(path=path), self.assertRaises((OSError, ValueError)):
                drive_setup.create_qual_home(str(path))
        self.assertEqual(list(existing.iterdir()), [])

    def test_missing_sentinel_blocks_all_scenarios(self):
        runner = Mock()
        runner.cloud.side_effect = [
            json.dumps([{"name": "Data", "mimeType": DRIVE_FOLDER, "id": "data-id", "parents": ["parent-id"]}]),
            "null",
        ]
        suite = DriveQualification(
            runner, [Client("client-a", self.root, "qual")], self.root, "fixture", 42, None,
        )
        suite.preflight = Mock()
        results = suite.run(None, "parent-id")
        self.assertEqual(results[0].effective_status(), "FAIL")
        self.assertIn("sentinel", results[0].conclusion.lower())
        self.assertTrue(all(row.effective_status() == "NOT RUN" for row in results[1:]))
        self.assertEqual([call.args[1:3] for call in runner.cloud.call_args_list], [("backend", "query")] * 2)

    def test_oauth_secret_only_reaches_the_child_stdin(self):
        runner = DriveRunner(Path("/fixture/bin/sync-cli"), Path("/fixture/bin/rclone"))
        client = Client("client-a", self.root, "qual")
        child = Mock(returncode=0)
        child.communicate.return_value = (None, "")
        with patch("run_drive_qualification.subprocess.Popen", return_value=child) as spawn:
            runner.add_account(client, "parent-id", "client-id", "secret-canary")
        args, = spawn.call_args.args
        self.assertIn("--client-secret-stdin", args)
        self.assertNotIn("secret-canary", args)
        self.assertNotIn("secret-canary", spawn.call_args.kwargs["env"].values())
        child.communicate.assert_called_once()
        self.assertEqual(child.communicate.call_args.kwargs["input"], "secret-canary\n")
        self.assertEqual(runner.commands, [])

    def test_cleanup_only_removes_recorded_mappings_and_preserves_collection_ids(self):
        runner = Mock()
        client = Client("client-a", self.root, "qual")
        suite = DriveQualification(runner, [client], self.root, "fixture", 42, None)
        suite.evidence = ScenarioEvidence("cleanup", "", "", "", "", False, "")
        suite.folder_ids["owned"] = "owned-id"
        suite.mapping_ids[client.name, "owned"] = "fixture-owned"
        runner.cli.side_effect = [
            (0, json.dumps({"mappings": [
                {"id": "fixture-owned", "local_root": str(client.data("owned")), "collection_folder_id": "owned-id"},
                {"id": "fixture-unrelated", "local_root": "/unrelated", "collection_folder_id": "other-id"},
            ]})),
            (0, ""),
        ]
        suite.cleanup()
        self.assertEqual(runner.cli.call_args_list[-1].args, (client, "remove-mapping", "fixture-owned"))
        self.assertEqual(runner.cli.call_count, 2)
        runner.cloud.assert_not_called()
        self.assertEqual(suite.evidence.outcomes["collections_retained_by_id"], {"owned": "owned-id"})
        self.assertEqual(suite.evidence.effective_status(), "NOT VERIFIED")

    def test_not_run_is_incomplete_and_credential_stderr_is_redacted(self):
        unfinished = ScenarioEvidence("missed", "", "", "", "", False, "", "NOT RUN")
        self.assertEqual(compute_overall_status([unfinished]), "INCOMPLETE")
        entry, _ = capture_record([], 1, "", "--password-command stderr: secret\nnext line", "a", "rclone")
        self.assertNotIn("secret", entry["stderr"])
        self.assertNotIn("next line", entry["stderr"])

    def test_nightly_lock_rejects_overlap_and_releases_afterward(self):
        with lock_qual_home(self.root):
            with self.assertRaisesRegex(RuntimeError, "already running"):
                with lock_qual_home(self.root):
                    self.fail("the second run acquired the lock")
        with lock_qual_home(self.root):
            pass

    def test_evidence_summary_stays_incomplete_and_retention_keeps_latest(self):
        ids = [f"2026010{n}T000000Z-aaaaaaaaaaaa" for n in range(1, 4)]
        unfinished = ScenarioEvidence("drive_missed", "", "", "", "", False, "No overlap", "NOT VERIFIED")
        with redirect_stdout(io.StringIO()):
            for n, run_id in enumerate(ids):
                out = self.root / run_id
                self.assertEqual(write_drive_evidence(out, run_id, 42, "", {}, [unfinished]), "INCOMPLETE")
                (out / "drive-evidence.json").chmod(0o600)
                os.utime(out, (n, n))
        unrelated = self.root / "20260104T000000Z-bbbbbbbbbbbb"
        unrelated.mkdir()
        publish_latest(self.root, ids[-1], keep=2)
        self.assertFalse((self.root / ids[0]).exists())
        self.assertTrue((self.root / ids[1]).exists())
        self.assertTrue(unrelated.exists())
        self.assertEqual((self.root / "latest").read_text().strip(), ids[-1])
        latest = self.root / ids[-1]
        payload = json.loads((latest / "drive-evidence.json").read_text())
        self.assertEqual(payload["summary"][0]["status"], "INCOMPLETE")
        self.assertIn("| `missed` | **INCOMPLETE**", (latest / "drive-evidence.md").read_text())

    def test_upload_overlap_requires_partial_progress_for_the_fixture(self):
        row = {"stats": {"transferring": [{"name": "fixture", "bytes": 4, "size": 9}]}}
        log = json.dumps(row).encode() + b'\n{"partial'
        self.assertEqual(upload_progress(log, "fixture"), 4)
        self.assertEqual(upload_progress(log, "other"), 0)
