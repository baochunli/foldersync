"""Safety and evidence checks for the owner-run Drive entry points."""

import json
import io
import os
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import MagicMock, Mock, patch

import drive_setup
from common import ScenarioEvidence, compute_overall_status
from run_drive_qualification import (
    DRIVE_FOLDER, Client, DriveQualification, DriveRunner, capture_record, lock_qual_home,
    main, publish_latest, upload_progress, write_drive_evidence,
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
        self.assertTrue(all(row.effective_status() == "NOT RUN" for row in results[1:-1]))
        self.assertEqual(results[-1].effective_status(), "PASS")
        self.assertEqual(results[-1].outcomes["recorded_fixture_ids"], {})
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

    def test_cleanup_removes_owned_mappings_and_submits_only_recorded_folder_ids(self):
        runner = Mock()
        client = Client("client-a", self.root, "qual")
        suite = DriveQualification(runner, [client], self.root, "fixture", 42, None)
        suite.evidence = ScenarioEvidence("cleanup", "", "", "", "", False, "")
        suite.parent_id, suite.data_id = "parent-id", "data-id"
        suite.folder_ids["owned"] = "owned-id"
        suite.mapping_ids[client.name, "owned"] = "fixture-owned"
        runner.cli.side_effect = [
            (0, json.dumps({"mappings": [
                {"id": "fixture-owned", "local_root": str(client.data("owned")), "collection_folder_id": "owned-id"},
                {"id": "fixture-unrelated", "local_root": "/unrelated", "collection_folder_id": "other-id"},
            ]})),
            (0, ""),
        ]
        runner.trash_recorded_folders.return_value = [
            {"fixture": "owned", "folder_id": "owned-id", "status": "TRASHED"},
        ]
        suite.cleanup()
        self.assertEqual(runner.cli.call_args_list[-1].args, (client, "remove-mapping", "fixture-owned"))
        self.assertEqual(runner.cli.call_count, 2)
        runner.cloud.assert_not_called()
        runner.trash_recorded_folders.assert_called_once_with(
            client, "parent-id", "data-id", self.root, {"owned": "owned-id"},
        )
        self.assertEqual(suite.evidence.outcomes["drive_trash"][0]["status"], "TRASHED")

    def test_exact_id_trash_refuses_wrong_parent_shortcuts_and_unconfirmed_updates(self):
        runner = DriveRunner(Path("/fixture/sync-cli"), Path("/fixture/rclone"))
        client = Client("client-a", self.root, "qual")
        folders = {
            "original": "owned-id", "moved": "moved-id", "shortcut": "shortcut-id",
            "unconfirmed": "unconfirmed-id", "bad-path": "bad/id", "data": "data-id",
        }

        def metadata(folder_id, parent="data-id", mime_type=DRIVE_FOLDER, trashed=False):
            return {"id": folder_id, "parents": [parent], "mimeType": mime_type,
                    "trashed": trashed, "explicitlyTrashed": trashed}

        connection = Mock()
        responses = [
            metadata("owned-id"), metadata("owned-id", trashed=True),
            metadata("moved-id", parent="elsewhere-id"), metadata("shortcut-id", mime_type="application/vnd.google-apps.shortcut"),
            metadata("unconfirmed-id"), metadata("unconfirmed-id"),
        ]
        replies = []
        for document in responses:
            response = MagicMock(status=200)
            response.read.return_value = json.dumps(document).encode()
            response.__enter__.return_value = response
            replies.append(response)
        connection.getresponse.side_effect = replies
        with (
            patch("run_drive_qualification.require_sentinel", return_value="data-id"),
            patch.object(runner, "_qualification_access_token", return_value="access-canary"),
            patch("run_drive_qualification.http.client.HTTPSConnection", return_value=connection) as https,
        ):
            results = list(runner.trash_recorded_folders(client, "parent-id", "data-id", self.root, folders))
        self.assertTrue(all(call.args == ("www.googleapis.com",) for call in https.call_args_list))
        requests = connection.request.call_args_list
        request_ids = [(call.args[0], call.args[1].split("?")[0]) for call in requests]
        self.assertEqual(request_ids, [
            ("GET", "/drive/v3/files/owned-id"), ("PATCH", "/drive/v3/files/owned-id"),
            ("GET", "/drive/v3/files/moved-id"), ("GET", "/drive/v3/files/shortcut-id"),
            ("GET", "/drive/v3/files/unconfirmed-id"), ("PATCH", "/drive/v3/files/unconfirmed-id"),
        ])
        self.assertEqual(json.loads(requests[1].kwargs["body"]), {"trashed": True})
        self.assertEqual(requests[1].kwargs["headers"]["Authorization"], "Bearer access-canary")
        self.assertEqual([row["status"] for row in results], ["TRASHED", "REFUSED", "REFUSED", "FAILED", "REFUSED", "REFUSED"])
        self.assertNotIn("access-canary", json.dumps(results))

    def test_exact_id_rename_checks_sentinel_and_only_parent_before_updating_name(self):
        runner = DriveRunner(Path("/fixture/sync-cli"), Path("/fixture/rclone"))
        client = Client("client-a", self.root, "qual")
        metadata = {"id": "second-id", "name": "second.txt", "parents": ["fixture-id"],
                    "mimeType": "text/plain", "trashed": False}
        for before, data_id, error in (
            (metadata, "data-id", None),
            ({**metadata, "parents": ["elsewhere"]}, "data-id", "directly under"),
            ({**metadata, "parents": ["fixture-id", "elsewhere"]}, "data-id", "directly under"),
            ({**metadata, "id": "other-id"}, "data-id", "directly under"),
            (metadata, "replacement-data-id", "Data folder ID changed"),
        ):
            with self.subTest(before=before, data_id=data_id):
                track = Mock()
                connection = track.connection
                replies = []
                for document in (before, {**metadata, "name": "duplicate.txt"}):
                    response = MagicMock(status=200)
                    response.read.return_value = json.dumps(document).encode()
                    response.__enter__.return_value = response
                    replies.append(response)
                connection.getresponse.side_effect = replies
                track.sentinel.return_value = data_id
                track.access.return_value = "access-canary"
                with (
                    patch("run_drive_qualification.require_sentinel", track.sentinel),
                    patch.object(runner, "_qualification_access_token", track.access),
                    patch("run_drive_qualification.http.client.HTTPSConnection", return_value=connection),
                ):
                    args = (client, "parent-id", "data-id", self.root, "fixture-id", "second-id")
                    if error:
                        with self.assertRaisesRegex(RuntimeError, error):
                            runner.rename_duplicate(*args)
                    else:
                        runner.rename_duplicate(*args)
                track.sentinel.assert_called_once_with(runner, client, "parent-id", self.root)
                requests = connection.request.call_args_list
                if data_id != "data-id":
                    track.access.assert_not_called()
                    self.assertEqual(requests, [])
                    continue
                track.access.assert_called_once_with(client, "parent-id")
                self.assertEqual([call[0] for call in track.mock_calls[:3]], [
                    "sentinel", "access", "connection.request",
                ])
                self.assertEqual([call.args[0] for call in requests], ["GET"] if error else ["GET", "PATCH"])
                self.assertTrue(all(call.args[1].split("?")[0] == "/drive/v3/files/second-id" for call in requests))
                if not error:
                    self.assertEqual(json.loads(requests[1].kwargs["body"]), {"name": "duplicate.txt"})
                self.assertEqual(runner.commands, [])

    def test_trash_refuses_missing_sentinel_or_changed_data_before_reading_credentials(self):
        runner = DriveRunner(Path("/fixture/sync-cli"), Path("/fixture/rclone"))
        client = Client("client-a", self.root, "qual")
        for sentinel in (RuntimeError("sentinel missing"), "replacement-data-id"):
            guard = {"side_effect": sentinel} if isinstance(sentinel, Exception) else {"return_value": sentinel}
            with (
                patch("run_drive_qualification.require_sentinel", **guard),
                patch.object(runner, "_qualification_access_token") as access,
                patch("run_drive_qualification.http.client.HTTPSConnection") as https,
            ):
                results = list(runner.trash_recorded_folders(client, "parent-id", "data-id", self.root, {"fixture": "owned-id"}))
            self.assertEqual(results[0]["status"], "REFUSED")
            access.assert_not_called()
            https.assert_not_called()

    def test_trash_stops_mutating_if_sentinel_disappears_between_fixtures(self):
        runner = DriveRunner(Path("/fixture/sync-cli"), Path("/fixture/rclone"))
        client = Client("client-a", self.root, "qual")
        initial = {"id": "first-id", "parents": ["data-id"], "mimeType": DRIVE_FOLDER,
                   "trashed": False, "explicitlyTrashed": False}
        trashed = {**initial, "trashed": True, "explicitlyTrashed": True}
        with (
            patch("run_drive_qualification.require_sentinel", side_effect=["data-id", RuntimeError("sentinel missing")]),
            patch.object(runner, "_qualification_access_token", return_value="access-canary") as access,
            patch("run_drive_qualification.http.client.HTTPSConnection"),
            patch("run_drive_qualification.qualification_drive_file", side_effect=[initial, trashed]) as request,
        ):
            results = list(runner.trash_recorded_folders(
                client, "parent-id", "data-id", self.root, {"first": "first-id", "second": "second-id"},
            ))
        self.assertEqual([row["status"] for row in results], ["TRASHED", "REFUSED"])
        access.assert_called_once()
        self.assertEqual([call.args[1] for call in request.call_args_list], ["first-id", "first-id"])

    def test_access_token_is_read_through_private_pipe_and_errors_exclude_credentials(self):
        runner = DriveRunner(Path("/fixture/sync-cli"), Path("/fixture/rclone"))
        client = Client("client-a", self.root, "qual")
        config = client.state / "accounts" / "qual" / "rclone.conf"
        config.parent.mkdir(parents=True)
        config.write_bytes(b"# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nFakeCiphertext==\n")
        config.chmod(0o600)
        runner.device_ids[client.name] = "device-a"
        token = json.dumps({"access_token": "access-canary", "refresh_token": "refresh-canary"})
        raw = f"[qual]\ntype = drive\nroot_folder_id = parent-id\ntoken = {token}\n"
        with patch("run_drive_qualification.subprocess.run",
                   return_value=subprocess.CompletedProcess([], 0, stdout=raw)) as spawn:
            self.assertEqual(runner._qualification_access_token(client, "parent-id"), "access-canary")
            argv, = spawn.call_args.args
            self.assertEqual(argv[-3:], ["config", "show", "qual"])
            self.assertIn("--password-command", argv)
            self.assertEqual(spawn.call_args.kwargs["stdout"], subprocess.PIPE)
            self.assertEqual(spawn.call_args.kwargs["stderr"], subprocess.DEVNULL)
            self.assertNotIn("canary", repr(spawn.call_args))
        with patch("run_drive_qualification.subprocess.run", return_value=subprocess.CompletedProcess([], 1, stdout=raw)):
            with self.assertRaises(RuntimeError) as failure:
                runner._qualification_access_token(client, "parent-id")
        self.assertNotIn("canary", str(failure.exception))
        self.assertEqual(runner.commands, [])
        config.write_text(raw)
        with patch("run_drive_qualification.subprocess.run") as spawn, self.assertRaisesRegex(RuntimeError, "encrypted"):
            runner._qualification_access_token(client, "parent-id")
        spawn.assert_not_called()

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

    def test_subset_dry_run_parses_scenario_names_and_lists_skips(self):
        home = self.root / "qual"
        home.mkdir(mode=0o700)
        (home / "evidence").mkdir(mode=0o700)
        config = home / "qual.toml"
        config.touch(mode=0o600)
        config.write_text(f'parent_id = "fake-parent"\naccount_b = false\nsync_cli_sha256 = "{"0" * 64}"\n')
        args = ["--qual-home", str(home), "--dry-run"]
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(main(args + [
                "--only", "drive_duplicate_names, drive_native_document_and_shortcut",
            ]), 0)
        plan = json.loads(out.getvalue())
        self.assertEqual(plan["run_type"], "subset run")
        self.assertEqual(plan["scenarios"], [
            "drive_preflight_account_and_folder_ids", "drive_duplicate_names",
            "drive_native_document_and_shortcut", "drive_cleanup",
        ])
        self.assertIn("drive_delayed_upload_overlap", plan["skipped_scenarios"])
        self.assertEqual(len(plan["collections"]), 2)
        self.assertFalse(list((home / "evidence").iterdir()))
        for selection, message in (
            ("drive_unknown", "Valid names"),
            ("drive_duplicate_names,", "Valid names"),
            ("drive_two_account_ownership_and_recovery_move", "requires a second account"),
        ):
            with self.subTest(selection=selection), redirect_stderr(io.StringIO()) as error:
                self.assertEqual(main(args + ["--only", selection]), 2)
                self.assertIn(message, error.getvalue())
                if message == "Valid names":
                    self.assertIn("drive_duplicate_names", error.getvalue())

    def test_subset_pass_is_labeled_in_console_and_both_evidence_headers(self):
        case = ScenarioEvidence("drive_selected", "", "", "", "", True, "Verified.")
        skipped = ["drive_other"]
        out = self.root / "evidence"
        with redirect_stdout(io.StringIO()) as console:
            self.assertEqual(write_drive_evidence(out, "test-run", 42, "", {}, [case], skipped), "PASS (subset)")
        payload = json.loads((out / "drive-evidence.json").read_text())
        self.assertEqual(payload["status"], "PASS (subset)")
        self.assertEqual(payload["run_type"], "subset run")
        self.assertEqual(payload["skipped_scenarios"], skipped)
        markdown = (out / "drive-evidence.md").read_text()
        for summary in (console.getvalue(), markdown):
            self.assertIn("subset run", summary)
            self.assertIn("drive_other", summary)
            self.assertIn("PASS (subset)", summary)
        self.assertNotIn("Overall result:** `PASS`", markdown)

    def test_upload_overlap_requires_partial_progress_for_the_fixture(self):
        row = {"stats": {"transferring": [{"name": "fixture", "bytes": 4, "size": 9}]}}
        log = json.dumps(row).encode() + b'\n{"partial'
        self.assertEqual(upload_progress(log, "fixture"), 4)
        self.assertEqual(upload_progress(log, "other"), 0)
