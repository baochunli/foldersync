"""Exercise Drive scenario orchestration with a fake cloud and CLI."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from common import ScenarioEvidence, TruthSet, compute_overall_status
from run_drive_qualification import (
    DRIVE_FOLDER, DRIVE_SHEET, DRIVE_SHORTCUT, SENTINEL, Client, DriveQualification,
    read_export, require_sentinel, sentinel_bytes, transfer_progress,
)


class FakeRunner:
    """Implement only the fixture operations the suite needs, without launching a process."""

    def __init__(self, client_b_visible_on=1) -> None:
        self.commands = []
        self.accounts = {}
        self.paused = set()
        self.mappings = {}
        self.folders = {}
        self.objects = {}
        self.recoveries = {}
        self.trashed = set()
        self.deny_trash = False
        self.run_id = 0
        self.runs = {}
        self.skip_signal = False
        self.client_b_visible_on = client_b_visible_on
        self.client_b_collection_attempts = 0

    @staticmethod
    def device_id(client):
        return f"device-{client.name[-1]}"

    def cli(self, client, *args, check=True, timeout=None):
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
            if client.name == "client-b" and self.folders:
                self.client_b_collection_attempts += 1
                if (
                    self.client_b_visible_on is None
                    or self.client_b_collection_attempts < self.client_b_visible_on
                ):
                    return 0, json.dumps({"candidates": []})
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
                "key": Path(args[args.index("--local") + 1]).name,
                "status": {
                    "id": key, "intent": "Enabled", "readiness": "Ready", "next_poll_at": None,
                    "local_root": args[args.index("--local") + 1],
                    "collection_folder_id": args[args.index("--folder-id") + 1],
                },
            }
        elif verb == "remove-mapping":
            del self.mappings[client.name, args[1]]
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
        path, cloud = self.folders[folder_id]
        if path != f"Data/{key}":
            status["readiness"] = "EndpointChanged"
            return 1, ""
        key = mapping["key"]
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
                if mine is not None and mine != remote:
                    self.write_files(client.recovery(key), {f"run-{self.run_id + 1}/{name}": mine})
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
        if verb == "backend":
            return self.backend(target, *args[3:])
        folder_id = args[args.index("--drive-root-folder-id") + 1] if "--drive-root-folder-id" in args else None
        if verb == "copyto":
            name = args[2].split(":", 1)[1]
            data = Path(target).read_bytes()
            if "--drive-import-formats" in args:
                self.objects["native-id"] = (folder_id, "fixture", DRIVE_SHEET, data, {})
            elif self.folders[folder_id][0].endswith("-duplicate"):
                self.objects[f"object-{name.replace('.', '-')}"] = (folder_id, name, "text/plain", data, {})
            else:
                self.folders[folder_id][1][name] = data
            return ""
        path = target.split(":", 1)[1]
        if verb == "mkdir":
            if folder_id == "data-id":
                path = f"Data/{path}"
            self.folders[f"folder-{len(self.folders)}"] = (path, {})
            return ""
        if verb == "moveto":
            original_id = next(fid for fid, (name, _) in self.folders.items() if name == f"Data/{path}")
            self.folders[original_id] = (f"Data/{args[2].split(':', 1)[1]}", self.folders[original_id][1])
            return ""
        if path == "Data":
            return json.dumps([{"Name": name.split("/")[1]} for name, _ in self.folders.values()])
        if folder_id and folder_id.startswith("recovery-for-"):
            files = self.recoveries[folder_id.removeprefix("recovery-for-")]
        else:
            files = self.folders[folder_id][1]
        if verb == "lsjson":
            return json.dumps([{"Path": name} for name in files])
        if verb == "copy":
            self.write_files(Path(args[2]), files)
            return ""
        raise AssertionError(args)

    def backend(self, action, *args):
        if action == "query":
            parent = args[0].split("'")[1]
            if parent == "parent-id":
                rows = [("data-id", "Data", DRIVE_FOLDER, b"", {}), ("recovery-id", "Recovery", DRIVE_FOLDER, b"", {})]
            elif parent == "data-id":
                rows = [(fid, name.split("/")[1], DRIVE_FOLDER, b"", {}) for fid, (name, _) in self.folders.items()]
                rows.append(("sentinel-id", SENTINEL, "application/octet-stream", sentinel_bytes("parent-id"), {}))
            elif parent == "recovery-id":
                rows = [(f"recovery-for-{fid}", fid, DRIVE_FOLDER, b"", {}) for fid in self.recoveries]
            else:
                rows = [(f"{parent}-{name.replace('.', '-')}", name, "text/plain", data, {})
                        for name, data in self.folders[parent][1].items()]
                rows += [(fid, name, mime, data, extra)
                         for fid, (folder, name, mime, data, extra) in self.objects.items() if folder == parent]
            return json.dumps([
                {"id": fid, "name": name, "mimeType": mime, "size": len(data), "parents": [parent], **extra}
                for fid, name, mime, data, extra in rows
            ])
        if action == "copyid":
            fid, destination = args[:2]
            data = sentinel_bytes("parent-id") if fid == "sentinel-id" else self.objects[fid][3]
            Path(destination).write_bytes(data)
            return ""
        if action == "moveid":
            folder, _, mime, data, extra = self.objects[args[0]]
            self.objects[args[0]] = (folder, args[1].split(":", 1)[1], mime, data, extra)
            return ""
        if action == "shortcut":
            folder = args[args.index("--drive-root-folder-id") + 1]
            self.objects["shortcut-id"] = (folder, args[1], DRIVE_SHORTCUT, b"",
                                           {"shortcutDetails": {"targetId": f"{folder}-source-bin"}})
            return ""
        raise AssertionError((action, args))

    def trash_recorded_folders(self, client, parent_id, data_id, scratch, folder_ids):
        assert require_sentinel(self, client, parent_id, scratch) == data_id
        for fixture, folder_id in folder_ids.items():
            if self.deny_trash:
                yield {"fixture": fixture, "folder_id": folder_id, "status": "FAILED"}
                self.deny_trash = False
                continue
            assert self.folders[folder_id][0].startswith("Data/")
            self.trashed.add(folder_id)
            yield {"fixture": fixture, "folder_id": folder_id, "status": "TRASHED"}

    def overlap_upload(self, client, source, folder_id, during):
        during()
        self.folders[folder_id][1]["shared.txt"] = source.read_bytes()
        return {"uploaded_bytes_before_peer": 1, "upload_active_after_peer": True}

    def interrupt(self, client, key, batch):
        if self.skip_signal:
            self.sync(client, key)
            return {
                "signaled_run": None, "returncode": 0,
                "transferred_before_signal": 0, "transferred_after_signal": len(batch),
            }
        mapping = self.mappings[client.name, key]
        key = mapping["key"]
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
    def collection_fixture(self, visible_on):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        clients = [Client(f"client-{name}", root / name, f"drive-{name}") for name in ("a", "b")]
        for client in clients:
            (client.root / "data").mkdir(parents=True)
            (client.root / "recovery").mkdir()
        suite = DriveQualification(FakeRunner(visible_on), clients, root, "Fixture", 42, "drive-b")
        suite.data_id = "data-id"
        suite.evidence = ScenarioEvidence("visibility", "", "", "", "", False, "")
        clock_patch = patch("run_drive_qualification.time")
        clock = clock_patch.start()
        self.addCleanup(clock_patch.stop)
        now = 0.0

        def sleep(seconds):
            nonlocal now
            now += seconds

        clock.monotonic.side_effect = lambda: now
        clock.sleep.side_effect = sleep
        return suite

    def test_client_b_proceeds_when_collection_appears_on_third_listing(self):
        suite = self.collection_fixture(visible_on=3)
        suite.make_collection("visibility", suite.clients)

        self.assertEqual(suite.runner.client_b_collection_attempts, 3)
        self.assertEqual(suite.runner.mappings["client-b", "Fixture-visibility"]["folder_id"], "folder-0")
        visibility = suite.evidence.outcomes["collection_visibility"]
        self.assertEqual(visibility["attempts"], 3)
        self.assertEqual(visibility["elapsed_seconds"], 3.0)

    def test_client_b_aborts_when_collection_never_appears(self):
        suite = self.collection_fixture(visible_on=None)
        with self.assertRaisesRegex(
            RuntimeError, r"client-b could not see Data/Fixture-visibility after .* listings"
        ):
            suite.make_collection("visibility", suite.clients)

        self.assertNotIn(("client-b", "Fixture-visibility"), suite.runner.mappings)
        visibility = suite.evidence.outcomes["collection_visibility"]
        self.assertGreater(visibility["attempts"], 1)
        self.assertEqual(visibility["attempts"], suite.runner.client_b_collection_attempts)
        self.assertEqual(visibility["elapsed_seconds"], 60.0)

    def run_suite(self, *, account_b="drive-b", skip_signal=False, lose_recovery=False, deny_trash=False):
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
            runner.deny_trash = deny_trash
            suite = DriveQualification(runner, clients, root, "Fixture", 42, account_b)
            export = root / "disposable.conf"
            export.touch()
            cloud = runner.cloud

            def read_cloud(client, *args):
                if lose_recovery and args[0] == "copy" and args[-1].startswith("recovery-for-"):
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
            self.assertEqual(runner.mappings, {})
            self.assertEqual(runner.trashed, {row["folder_id"] for row in results[-1].outcomes["drive_trash"]
                                              if row["status"] == "TRASHED"})
            return results

    def test_all_scenarios_drive_three_paused_clients_and_verify_versions(self):
        results = self.run_suite()
        self.assertEqual(compute_overall_status(results), "PASS", [(s.name, s.conclusion) for s in results])
        self.assertEqual(len(results), 15)
        self.assertTrue(all(s.passed for s in results), [(s.name, s.conclusion) for s in results])
        preflight, distinct, conflict, _, offline, guard, interrupted = results[:7]
        cross_account = results[-2]
        self.assertEqual(len(set(preflight.outcomes["devices"].values())), 3)
        self.assertEqual(preflight.outcomes["sources"]["client-b"], "drive-b")
        self.assertEqual(len(distinct.outcomes["checkpoints"]), 3)
        self.assertEqual(len(offline.outcomes["checkpoints"]), 5)
        self.assertTrue(any(":conflict:" in loc for row in conflict.hash_accounting for loc in row["locations"]))
        self.assertEqual(len(guard.outcomes["checkpoints"]), 3)
        self.assertEqual(interrupted.outcomes["downloaded_after_exit"], 2)
        self.assertTrue(any("drive-recovery:recovery:device-b/" in loc
                            for row in cross_account.hash_accounting for loc in row["locations"]))
        self.assertEqual([len(results[i].outcomes["checkpoints"]) for i in range(7, 13)], [3, 3, 3, 4, 4, 3])
        self.assertEqual(len(results[-1].outcomes["recorded_fixture_ids"]), 14)
        self.assertEqual(len(results[-1].outcomes["drive_trash"]), 14)

    def test_failed_trash_keeps_the_run_from_passing(self):
        results = self.run_suite(deny_trash=True)
        self.assertTrue(all(s.passed for s in results[:-1]))
        self.assertEqual(compute_overall_status(results), "FAIL")
        self.assertEqual([row["status"] for row in results[-1].outcomes["drive_trash"]],
                         ["FAILED"] + ["TRASHED"] * 13)

    def test_missed_signal_is_incomplete_and_wrong_recovery_bytes_fail(self):
        incomplete = self.run_suite(account_b=None, skip_signal=True)
        self.assertEqual(len(incomplete), 14)
        self.assertEqual(incomplete[6].effective_status(), "NOT VERIFIED")
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
