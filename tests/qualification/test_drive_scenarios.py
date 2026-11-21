"""Exercise Drive scenario orchestration with a fake cloud and CLI."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from common import TruthSet, compute_overall_status
from run_drive_qualification import Client, DriveQualification, read_export, transfer_progress


class FakeRunner:
    """Implement only the fixture operations the suite needs, without launching a process."""

    def __init__(self) -> None:
        self.commands = []
        self.accounts = {}
        self.paused = set()
        self.mappings = {}
        self.folders = {}
        self.recoveries = {}
        self.run_id = 0
        self.runs = {}
        self.skip_signal = False

    @staticmethod
    def device_id(client):
        return f"device-{client.name[-1]}"

    def cli(self, client, *args, check=True):
        self.commands.append({"client": client.name, "args": args})
        verb = args[0]
        if verb == "scheduling":
            self.paused.add(client.name)
        elif verb == "account" and args[1] == "import":
            self.accounts[client.name] = [{
                "kind": "drive", "remote_name": args[args.index("--remote") + 1], "auth_status": "ready",
                "remote_root_folder_id": args[args.index("--root-folder-id") + 1],
            }]
        elif verb == "account" and args[1] == "list":
            return 0, json.dumps(self.accounts[client.name])
        elif verb == "collections":
            return 0, json.dumps({"candidates": [
                {"name": name.split("/")[1], "collection": {"display_path": name, "folder_id": folder_id}}
                for folder_id, (name, _) in self.folders.items()
            ]})
        elif verb == "status":
            return 0, json.dumps({
                "scheduling_paused": client.name in self.paused,
                "mappings": [
                    mapping["status"] for (owner, _), mapping in self.mappings.items() if owner == client.name
                ],
            })
        elif verb == "add-mapping":
            key = args[args.index("--id") + 1]
            self.mappings[client.name, key] = {
                "folder_id": args[args.index("--folder-id") + 1], "last": {},
                "status": {"id": key, "intent": "Enabled", "readiness": "Ready", "next_poll_at": None},
            }
        elif verb in ("pause", "resume"):
            self.mappings[client.name, args[1]]["status"]["intent"] = "Paused" if verb == "pause" else "Enabled"
        elif verb in ("init", "run-once"):
            return self.sync(client, args[1])
        else:
            raise AssertionError(args)
        return 0, ""

    def read_files(self, path):
        return {file.relative_to(path).as_posix(): file.read_bytes() for file in path.rglob("*") if file.is_file()}

    def write_files(self, path, files):
        for name, data in files.items():
            file = path / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_bytes(data)

    def terminal(self, client, mapping, outcome, kind, workdir=None):
        self.run_id += 1
        run_id = f"run-{self.run_id}"
        mapping["status"].update({
            "last_run_id": run_id, "last_run_outcome": outcome,
            "readiness": "Ready" if outcome.startswith("Success") else "RecoveryRequired",
        })
        self.runs[run_id] = {"outcome_kind": kind, "workdir_path": workdir}
        return run_id

    def sync(self, client, key):
        mapping = self.mappings[client.name, key]
        status = mapping["status"]
        if status["intent"] == "Paused" or status["readiness"] == "RecoveryRequired":
            return 1, ""
        folder_id = mapping["folder_id"]
        _, cloud = self.folders[folder_id]
        local = self.read_files(client.data(key))
        last = mapping["last"]
        result = cloud.copy()
        backups = self.recoveries.setdefault(folder_id, {})
        if key == "guard" and len(last.keys() - local.keys()) >= 4:
            self.terminal(client, mapping, "CriticalFailure", "deletion_guard_aborted")
            return 1, ""
        conflict = False
        for name in sorted(last.keys() | local.keys()):
            before, mine, remote = last.get(name), local.get(name), cloud.get(name)
            if before == mine:
                continue
            if remote is not None and mine != remote:
                backups[f"{self.device_id(client)}/run-{self.run_id + 1}/{name}"] = remote
            if before != remote and mine is not None and remote is not None and mine != remote:
                result[name + ".conflict1"] = mine
                result[name + ".conflict2"] = remote
                result.pop(name)
                conflict = True
            elif mine is not None:
                result[name] = mine
            elif before == remote:
                result.pop(name, None)
        cloud.clear()
        cloud.update(result)
        for name in local.keys() - result.keys():
            client.data(key).joinpath(name).unlink()
        self.write_files(client.data(key), result)
        mapping["last"] = result
        self.terminal(client, mapping, "SuccessWithConflicts" if conflict else "Success", "success")
        return 0, ""

    def cloud(self, client, *args):
        verb, target = args[:2]
        path = target.split(":", 1)[1]
        if verb == "mkdir":
            self.folders[f"folder-{len(self.folders)}"] = (path, {})
            return ""
        if path == "Data":
            return json.dumps([{"Name": name.split("/")[1]} for name, _ in self.folders.values()])
        if path.startswith("Recovery/"):
            files = self.recoveries[path.split("/")[1]]
        else:
            folder_id = args[args.index("--drive-root-folder-id") + 1]
            files = self.folders[folder_id][1]
        if verb == "lsjson":
            return json.dumps([{"Path": name} for name in files])
        if verb == "copy":
            self.write_files(Path(args[2]), files)
            return ""
        raise AssertionError(args)

    def interrupt(self, client, key, batch):
        if self.skip_signal:
            self.sync(client, key)
            return {
                "signaled_run": None, "returncode": 0,
                "transferred_before_signal": 0, "transferred_after_signal": len(batch),
            }
        mapping = self.mappings[client.name, key]
        _, remote = self.folders[mapping["folder_id"]]
        for path in sorted(batch)[:2]:
            remote[path] = client.data(key).joinpath(path).read_bytes()
        workdir = client.root / "workdir"
        workdir.mkdir()
        workdir.joinpath("baseline").touch()
        run_id = self.terminal(client, mapping, "Interrupted", "interrupted", workdir)
        return {
            "signaled_run": run_id, "returncode": 1,
            "transferred_before_signal": 1, "transferred_after_signal": 2,
        }


class DriveScenarioTests(unittest.TestCase):
    def run_suite(self, *, account_b="drive-b", skip_signal=False, lose_recovery=False):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clients = [Client(f"client-{name}", root / name, account_b if name == "b" and account_b else "drive-a")
                       for name in ("a", "b", "c")]
            for client in clients:
                client.root.mkdir()
                (client.root / "data").mkdir()
                (client.root / "recovery").mkdir()
            runner = FakeRunner()
            runner.skip_signal = skip_signal
            suite = DriveQualification(runner, clients, root, "Fixture", 42, account_b)
            export = root / "disposable.conf"
            export.touch()
            cloud = runner.cloud

            def read_cloud(client, *args):
                if lose_recovery and args[0] == "copy" and ":Recovery/" in args[1]:
                    cloud(client, *args)
                    for file in Path(args[2]).rglob("*"):
                        if file.is_file():
                            file.write_bytes(b"wrong bytes")
                    return ""
                return cloud(client, *args)

            with (
                patch("run_drive_qualification.run_rows", side_effect=lambda _client, _sql, params: [runner.runs[params[0]]]),
                patch.object(runner, "cloud", side_effect=read_cloud),
                patch("run_drive_qualification.BATCH_COUNT", 4),
            ):
                results = suite.run(export, "parent-id")
            self.assertFalse(export.exists())
            return results

    def test_all_scenarios_drive_three_paused_clients_and_verify_versions(self):
        results = self.run_suite()
        self.assertEqual(compute_overall_status(results), "PASS", [(s.name, s.conclusion) for s in results])
        self.assertEqual(len(results), 8)
        preflight, distinct, conflict, _, offline, guard, interrupted, cross_account = results
        self.assertEqual(len(set(preflight.outcomes["devices"].values())), 3)
        self.assertEqual(preflight.outcomes["sources"]["client-b"], "drive-b")
        self.assertEqual(len(distinct.outcomes["checkpoints"]), 3)
        self.assertEqual(len(offline.outcomes["checkpoints"]), 5)
        self.assertTrue(any(":conflict:" in loc for row in conflict.hash_accounting for loc in row["locations"]))
        self.assertEqual(len(guard.outcomes["checkpoints"]), 3)
        self.assertEqual(interrupted.outcomes["downloaded_after_exit"], 2)
        self.assertTrue(any("drive-recovery:recovery:device-b/" in loc
                            for row in cross_account.hash_accounting for loc in row["locations"]))

    def test_missed_signal_is_incomplete_and_wrong_recovery_bytes_fail(self):
        incomplete = self.run_suite(account_b=None, skip_signal=True)
        self.assertEqual(len(incomplete), 7)
        self.assertEqual(incomplete[-1].effective_status(), "NOT VERIFIED")
        self.assertEqual(compute_overall_status(incomplete), "INCOMPLETE")
        failed = self.run_suite(lose_recovery=True)
        self.assertEqual(compute_overall_status(failed), "FAIL")
        self.assertTrue(any("Missing required recovery copy" in s.conclusion for s in failed), failed)

    def test_interruption_progress_counts_only_unique_completed_batch_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "output.ndjson"
            copied = json.dumps({"object": "batch/a", "msg": "Copied (new)"})
            listing = json.dumps({"object": "batch/b", "msg": "Listed"})
            unrelated = json.dumps({"object": "elsewhere", "msg": "Copied (new)"})
            log.write_text("\n".join([copied, copied, listing, unrelated, '{"broken']))
            self.assertEqual(transfer_progress(log, {"batch/a", "batch/b"}), 1)

    def test_export_refuses_non_drive_and_different_shared_parents(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "disposable.conf"
            path.touch(mode=0o600)
            path.write_text("[safe]\ntype = drive\nroot_folder_id = parent-a\n[other]\ntype = local\n")
            with self.assertRaisesRegex(ValueError, "Drive section"):
                read_export(path, "safe", "other")
            path.write_text(path.read_text().replace("type = local", "type = drive\nroot_folder_id = parent-b"))
            with self.assertRaisesRegex(ValueError, "same disposable shared parent"):
                read_export(path, "safe", "other")
            target = Path(tmp) / "link.conf"
            target.symlink_to(path)
            with self.assertRaises(OSError):
                read_export(target, "safe", None)
