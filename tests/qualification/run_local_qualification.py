#!/usr/bin/env python3
"""Local-to-local FolderSync qualification harness driving `sync-cli` (plan §15, §20).

Exercises three independent client state directories on one machine against a
shared local remote fixture, with external truth-set SHA-256 accounting,
multi-cycle convergence checks, deletion guard, and phase interruptions.
Writes JSON and Markdown evidence under `target/qualification/`.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

from common import (
    REPO_ROOT,
    ScenarioEvidence,
    TruthSet,
    get_tool_versions,
    resolve_verified_pinned_rclone,
    sha256_file,
    utc_now,
    write_evidence_files,
)


MTIME_STEP_SECS = 1.15


def build_or_find_sync_cli(explicit: str | None) -> Path:
    if explicit:
        p = Path(explicit).resolve()
        if not p.is_file():
            raise RuntimeError(f"Specified --sync-cli not found: {p}")
        return p
    # Use release profile so launch-boundary SHA-256 verification of the ~88 MiB
    # pinned rclone binary completes in ~80 ms instead of ~10 s per CLI run.
    candidate = REPO_ROOT / "target" / "release" / "sync-cli"
    subprocess.run(
        ["cargo", "build", "--release", "-p", "sync-cli"],
        cwd=REPO_ROOT,
        check=True,
    )
    if not candidate.is_file():
        raise RuntimeError(f"Expected built sync-cli at {candidate}")
    return candidate


class ClientHarness:
    """One independent FolderSync client with its own state, local tree, recovery, and Finder dir."""

    def __init__(
        self,
        name: str,
        root: Path,
        sync_cli: Path,
        rclone_bin: Path,
        fake_home: Path,
    ) -> None:
        self.name = name
        self.root = root
        self.state_dir = root / "state"
        self.local_dir = root / "local"
        self.recovery_dir = root / "recovery-local"
        self.finder_dir = root / "finder"
        self.sync_cli = sync_cli
        self.rclone_bin = rclone_bin
        self.fake_home = fake_home

        self.local_dir.mkdir(parents=True, exist_ok=True)
        self.recovery_dir.mkdir(parents=True, exist_ok=True)
        self.finder_dir.mkdir(parents=True, exist_ok=True)

    def _env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["HOME"] = str(self.fake_home)
        env.pop("FOLDERSYNC_STATE_DIR", None)
        env["FOLDERSYNC_RCLONE"] = str(self.rclone_bin)
        return env

    def run_cli(
        self,
        args: list[str],
        check: bool = True,
        timeout: int = 180,
    ) -> dict[str, Any]:
        cmd = [str(self.sync_cli), *args]
        proc = subprocess.run(
            cmd,
            env=self._env(),
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        record = {
            "client": self.name,
            "args": args,
            "returncode": proc.returncode,
            "stdout": proc.stdout.strip()[:400],
            "stderr": proc.stderr.strip()[:400],
        }
        if check and proc.returncode != 0:
            raise RuntimeError(
                f"sync-cli failed for {self.name} ({args}): rc={proc.returncode}\n"
                f"stdout={proc.stdout}\nstderr={proc.stderr}"
            )
        return record

    def add_mapping(
        self,
        mapping_id: str,
        remote_dir: Path,
        remote_recovery_dir: Path,
        collection: str = "Data/qual",
        max_delete_percent: int = 10,
    ) -> dict[str, Any]:
        remote_recovery_dir.mkdir(parents=True, exist_ok=True)
        return self.run_cli(
            [
                "add-mapping",
                "--state-dir",
                str(self.state_dir),
                "--finder-dir",
                str(self.finder_dir),
                "--id",
                mapping_id,
                "--local-dir",
                str(self.local_dir),
                "--remote-dir",
                str(remote_dir),
                "--recovery-dir",
                str(self.recovery_dir),
                "--remote-recovery-dir",
                str(remote_recovery_dir),
                "--collection",
                collection,
                "--max-delete-percent",
                str(max_delete_percent),
            ]
        )

    def init(self, mapping_id: str, check: bool = True) -> dict[str, Any]:
        return self.run_cli(
            [
                "init",
                mapping_id,
                "--state-dir",
                str(self.state_dir),
                "--finder-dir",
                str(self.finder_dir),
            ],
            check=check,
        )

    def run_once(self, mapping_id: str, check: bool = True) -> dict[str, Any]:
        return self.run_cli(
            [
                "run-once",
                mapping_id,
                "--state-dir",
                str(self.state_dir),
                "--finder-dir",
                str(self.finder_dir),
            ],
            check=check,
        )

    def pause(self, mapping_id: str) -> dict[str, Any]:
        return self.run_cli(
            [
                "pause",
                mapping_id,
                "--state-dir",
                str(self.state_dir),
                "--finder-dir",
                str(self.finder_dir),
            ]
        )

    def resume(self, mapping_id: str) -> dict[str, Any]:
        return self.run_cli(
            [
                "resume",
                mapping_id,
                "--state-dir",
                str(self.state_dir),
                "--finder-dir",
                str(self.finder_dir),
            ]
        )

    def status_json(self) -> dict[str, Any]:
        cmd = [
            str(self.sync_cli),
            "status",
            "--state-dir",
            str(self.state_dir),
            "--finder-dir",
            str(self.finder_dir),
            "--json",
        ]
        proc = subprocess.run(cmd, env=self._env(), capture_output=True, text=True, check=True)
        return json.loads(proc.stdout)

    def sqlite_runs(self) -> list[dict[str, Any]]:
        db_path = self.state_dir / "state.sqlite"
        if not db_path.exists():
            return []
        conn = sqlite3.connect(str(db_path))
        conn.row_factory = sqlite3.Row
        try:
            rows = conn.execute(
                "SELECT id, mapping_id, status, outcome_kind, outcome_detail, workdir_path FROM runs ORDER BY started_at"
            ).fetchall()
            return [dict(r) for r in rows]
        finally:
            conn.close()


def account_truth_versions(
    truth: TruthSet,
    labels: list[str],
    named_roots: dict[str, Path],
) -> tuple[list[dict[str, Any]], bool]:
    entries: list[dict[str, Any]] = []
    all_ok = True
    for label in labels:
        rec = truth.records[label]
        locs = truth.find_hash_locations(rec["sha256"], named_roots)
        ok = len(locs) > 0
        if not ok:
            all_ok = False
        entries.append(
            {
                "label": label,
                "rel_path": rec["rel_path"],
                "sha256": rec["sha256"],
                "locations": locs,
                "verified": ok,
            }
        )
    return entries, all_ok


def run_three_client_scenarios(
    work_dir: Path,
    sync_cli: Path,
    rclone_bin: Path,
    fake_home: Path,
    seed: int,
) -> list[ScenarioEvidence]:
    results: list[ScenarioEvidence] = []
    truth = TruthSet(work_dir / "truth-3client", seed=seed)

    remote_dir = work_dir / "shared-remote" / "Data" / "qual"
    remote_dir.mkdir(parents=True, exist_ok=True)
    remote_rec_root = work_dir / "shared-remote" / "Recovery" / "qual"

    client_a = ClientHarness("client-a", work_dir / "clients" / "client-a", sync_cli, rclone_bin, fake_home)
    client_b = ClientHarness("client-b", work_dir / "clients" / "client-b", sync_cli, rclone_bin, fake_home)
    client_c = ClientHarness("client-c", work_dir / "clients" / "client-c", sync_cli, rclone_bin, fake_home)

    all_roots = {
        "client-a": client_a.local_dir,
        "client-b": client_b.local_dir,
        "client-c": client_c.local_dir,
        "remote": remote_dir,
        "rec-a-local": client_a.recovery_dir,
        "rec-b-local": client_b.recovery_dir,
        "rec-c-local": client_c.recovery_dir,
        "rec-remote": remote_rec_root,
    }

    # --- Scenario 1: Baseline 3-client init + distinct edits + multi-cycle convergence ---
    s1_start = utc_now()
    s1_cmds: list[dict[str, Any]] = []
    s1_notes: list[str] = []
    try:
        truth.write_into(client_a.local_dir, "doc-a.txt", "s1-doc-a-v1", size_bytes=220)
        truth.write_into(client_a.local_dir, "doc-b.txt", "s1-doc-b-v1", size_bytes=240)
        truth.write_into(client_a.local_dir, "shared.txt", "s1-shared-v1", size_bytes=260)
        truth.write_into(client_a.local_dir, "sub/report.bin", "s1-report-v1", size_bytes=1024, binary=True)
        # Seed 12 additional baseline files (17 tracked files incl. RCLONE_TEST) so a single
        # file deletion or conflict rename (~5.9%) stays below the default 10% deletion guard.
        baseline_expected: dict[str, str] = {}
        for idx in range(1, 13):
            rel = f"baseline/b{idx:02d}.txt"
            baseline_expected[rel] = truth.write_into(client_a.local_dir, rel, f"s1-base-{idx:02d}", size_bytes=160)

        for cl in (client_a, client_b, client_c):
            s1_cmds.append(cl.add_mapping("qual", remote_dir, remote_rec_root / cl.name))
        s1_cmds.append(client_a.init("qual"))
        s1_cmds.append(client_b.init("qual"))
        s1_cmds.append(client_c.init("qual"))

        time.sleep(MTIME_STEP_SECS)
        truth.write_into(client_a.local_dir, "doc-a.txt", "s1-doc-a-v2", size_bytes=300)
        truth.write_into(client_b.local_dir, "doc-b.txt", "s1-doc-b-v2", size_bytes=320)

        # Cycle 1 + Cycle 2 convergence across all three clients
        for cl in (client_a, client_b, client_c, client_a, client_b):
            s1_cmds.append(cl.run_once("qual"))

        hashes_a = truth.scan_tree_hashes(client_a.local_dir)
        hashes_b = truth.scan_tree_hashes(client_b.local_dir)
        hashes_c = truth.scan_tree_hashes(client_c.local_dir)
        hashes_r = truth.scan_tree_hashes(remote_dir)

        converged = hashes_a == hashes_b == hashes_c == hashes_r
        expected_live = {
            **baseline_expected,
            "doc-a.txt": truth.records["s1-doc-a-v2"]["sha256"],
            "doc-b.txt": truth.records["s1-doc-b-v2"]["sha256"],
            "shared.txt": truth.records["s1-shared-v1"]["sha256"],
            "sub/report.bin": truth.records["s1-report-v1"]["sha256"],
        }
        live_match = hashes_a == expected_live
        acct, acct_ok = account_truth_versions(
            truth,
            ["s1-doc-a-v1", "s1-doc-b-v1", "s1-doc-a-v2", "s1-doc-b-v2", "s1-shared-v1", "s1-report-v1"],
            all_roots,
        )
        passed = converged and live_match and acct_ok
        s1_notes.append(f"3-client + remote tree equality after multi-cycle sync: {converged}")
        s1_notes.append(f"Prior v1 versions preserved in recovery dirs with verified SHA-256: {acct_ok}")
        results.append(
            ScenarioEvidence(
                name="three_client_baseline_and_distinct_edits",
                description="Three independent client state dirs initialize and converge distinct edits across multiple cycles with recovery-copy hash verification.",
                started_at=s1_start,
                finished_at=utc_now(),
                injected_failure="none (baseline multi-client distinct edits)",
                passed=passed,
                conclusion="supported" if passed else "failed",
                commands=s1_cmds,
                outcomes={"converged": converged, "live_match": live_match, "all_versions_accounted": acct_ok},
                hash_accounting=acct,
                notes=s1_notes,
            )
        )
    except Exception as exc:  # noqa: BLE001
        results.append(
            ScenarioEvidence(
                name="three_client_baseline_and_distinct_edits",
                description="Three independent client state dirs initialize and converge distinct edits across multiple cycles.",
                started_at=s1_start,
                finished_at=utc_now(),
                injected_failure="none",
                passed=False,
                conclusion=f"error: {exc}",
                commands=s1_cmds,
            )
        )
        return results

    # --- Scenario 2: Same-file concurrent edits (conflict preservation + multi-cycle) ---
    s2_start = utc_now()
    s2_cmds: list[dict[str, Any]] = []
    s2_notes: list[str] = []
    try:
        time.sleep(MTIME_STEP_SECS)
        sha_a = truth.write_into(client_a.local_dir, "shared.txt", "s2-shared-edit-a", size_bytes=340)
        sha_b = truth.write_into(client_b.local_dir, "shared.txt", "s2-shared-edit-b", size_bytes=370)

        s2_cmds.append(client_a.run_once("qual"))
        s2_cmds.append(client_b.run_once("qual"))
        b_status = client_b.status_json()
        b_outcome = b_status["mappings"][0].get("last_run_outcome")

        # Multi-cycle propagation to C, A, B
        for cl in (client_c, client_a, client_b, client_c):
            s2_cmds.append(cl.run_once("qual"))

        hashes_a = truth.scan_tree_hashes(client_a.local_dir)
        hashes_b = truth.scan_tree_hashes(client_b.local_dir)
        hashes_c = truth.scan_tree_hashes(client_c.local_dir)
        hashes_r = truth.scan_tree_hashes(remote_dir)
        converged = hashes_a == hashes_b == hashes_c == hashes_r
        conflict_files = sorted(k for k in hashes_a if ".conflict" in k)
        both_in_live = sha_a in hashes_a.values() and sha_b in hashes_a.values()

        acct, acct_ok = account_truth_versions(
            truth,
            ["s1-shared-v1", "s2-shared-edit-a", "s2-shared-edit-b"],
            all_roots,
        )
        passed = converged and both_in_live and acct_ok and len(conflict_files) >= 2 and b_outcome == "SuccessWithConflicts"
        s2_notes.append(f"Client B classified conflict outcome: {b_outcome}")
        s2_notes.append(f"Observed conflict files on all clients: {conflict_files}")
        s2_notes.append(f"Both concurrent edits preserved in live trees after 2nd cycle: {both_in_live}")
        results.append(
            ScenarioEvidence(
                name="three_client_same_file_edit_conflict",
                description="Client A and Client B edit shared.txt before syncing; verify both versions survive as conflict copies across all 3 clients over multiple cycles.",
                started_at=s2_start,
                finished_at=utc_now(),
                injected_failure="concurrent offline edits to shared.txt on client-a and client-b",
                passed=passed,
                conclusion="supported (preservation-first conflict copies)" if passed else "failed",
                commands=s2_cmds,
                outcomes={
                    "client_b_outcome": b_outcome,
                    "converged": converged,
                    "both_in_live": both_in_live,
                    "conflict_files": conflict_files,
                },
                hash_accounting=acct,
                notes=s2_notes,
            )
        )
    except Exception as exc:  # noqa: BLE001
        results.append(
            ScenarioEvidence(
                name="three_client_same_file_edit_conflict",
                description="Concurrent same-file edits across 3 clients.",
                started_at=s2_start,
                finished_at=utc_now(),
                injected_failure="concurrent offline edits to shared.txt",
                passed=False,
                conclusion=f"error: {exc}",
                commands=s2_cmds,
            )
        )

    # --- Scenario 3: Edit vs Delete ---
    s3_start = utc_now()
    s3_cmds: list[dict[str, Any]] = []
    s3_notes: list[str] = []
    try:
        time.sleep(MTIME_STEP_SECS)
        (client_a.local_dir / "doc-a.txt").unlink()
        sha_b_edit = truth.write_into(client_b.local_dir, "doc-a.txt", "s3-doc-a-edit-vs-delete", size_bytes=410)

        # Client A pushes delete first, then Client B syncs its edit, then C/A/B converge over extra cycles
        for cl in (client_a, client_b, client_c, client_a, client_b):
            s3_cmds.append(cl.run_once("qual"))

        hashes_a = truth.scan_tree_hashes(client_a.local_dir)
        hashes_b = truth.scan_tree_hashes(client_b.local_dir)
        hashes_c = truth.scan_tree_hashes(client_c.local_dir)
        hashes_r = truth.scan_tree_hashes(remote_dir)
        converged = hashes_a == hashes_b == hashes_c == hashes_r
        edit_survived = hashes_a.get("doc-a.txt") == sha_b_edit

        acct, acct_ok = account_truth_versions(
            truth,
            ["s1-doc-a-v2", "s3-doc-a-edit-vs-delete"],
            all_roots,
        )
        passed = converged and edit_survived and acct_ok
        s3_notes.append(f"Edited version survived concurrent deletion across 3 clients: {edit_survived}")
        s3_notes.append(f"Pre-delete version preserved in recovery copies: {acct[0]['verified']}")
        results.append(
            ScenarioEvidence(
                name="three_client_edit_vs_delete",
                description="Client A deletes doc-a.txt while Client B edits it; verify the edit survives and pre-delete version is retained in recovery across multiple cycles.",
                started_at=s3_start,
                finished_at=utc_now(),
                injected_failure="client-a deletes doc-a.txt while client-b edits doc-a.txt before syncing",
                passed=passed,
                conclusion="supported (edit preserved over concurrent delete; prior version in recovery)" if passed else "failed",
                commands=s3_cmds,
                outcomes={"converged": converged, "edit_survived": edit_survived},
                hash_accounting=acct,
                notes=s3_notes,
            )
        )
    except Exception as exc:  # noqa: BLE001
        results.append(
            ScenarioEvidence(
                name="three_client_edit_vs_delete",
                description="Edit vs delete across 3 clients.",
                started_at=s3_start,
                finished_at=utc_now(),
                injected_failure="edit vs delete on doc-a.txt",
                passed=False,
                conclusion=f"error: {exc}",
                commands=s3_cmds,
            )
        )

    # --- Scenario 4: Third computer offline return ---
    s4_start = utc_now()
    s4_cmds: list[dict[str, Any]] = []
    s4_notes: list[str] = []
    try:
        # Client C remains offline while A and B advance through multiple cycles.
        time.sleep(MTIME_STEP_SECS)
        truth.write_into(client_a.local_dir, "online-new.txt", "s4-online-new-v1", size_bytes=200)
        sha_b_online = truth.write_into(client_a.local_dir, "doc-b.txt", "s4-doc-b-online-v3", size_bytes=360)
        (client_a.local_dir / "sub/report.bin").unlink()

        s4_cmds.append(client_a.run_once("qual"))
        s4_cmds.append(client_b.run_once("qual"))

        time.sleep(MTIME_STEP_SECS)
        sha_online_v2 = truth.write_into(client_b.local_dir, "online-new.txt", "s4-online-new-v2", size_bytes=250)
        s4_cmds.append(client_b.run_once("qual"))
        s4_cmds.append(client_a.run_once("qual"))

        # Offline Client C edits doc-b.txt (conflicting with v3) and creates a new file.
        time.sleep(MTIME_STEP_SECS)
        sha_c_offline = truth.write_into(client_c.local_dir, "doc-b.txt", "s4-doc-b-c-offline", size_bytes=390)
        sha_c_new = truth.write_into(client_c.local_dir, "c-offline-note.txt", "s4-c-offline-note", size_bytes=210)

        # Client C returns, then multi-cycle convergence across A, B, C.
        for cl in (client_c, client_a, client_b, client_c, client_a):
            s4_cmds.append(cl.run_once("qual"))

        hashes_a = truth.scan_tree_hashes(client_a.local_dir)
        hashes_b = truth.scan_tree_hashes(client_b.local_dir)
        hashes_c = truth.scan_tree_hashes(client_c.local_dir)
        hashes_r = truth.scan_tree_hashes(remote_dir)
        converged = hashes_a == hashes_b == hashes_c == hashes_r

        c_new_present = hashes_a.get("c-offline-note.txt") == sha_c_new
        online_v2_present = hashes_a.get("online-new.txt") == sha_online_v2
        deleted_not_resurrected = "sub/report.bin" not in hashes_a
        both_doc_b_preserved = sha_b_online in hashes_a.values() and sha_c_offline in hashes_a.values()

        acct, acct_ok = account_truth_versions(
            truth,
            [
                "s1-report-v1",
                "s4-online-new-v1",
                "s4-online-new-v2",
                "s4-doc-b-online-v3",
                "s4-doc-b-c-offline",
                "s4-c-offline-note",
            ],
            all_roots,
        )
        passed = converged and c_new_present and online_v2_present and deleted_not_resurrected and both_doc_b_preserved and acct_ok
        s4_notes.append(f"3-client convergence after offline return + follow-up cycles: {converged}")
        s4_notes.append(f"Deleted sub/report.bin stayed deleted (no resurrection): {deleted_not_resurrected}")
        s4_notes.append(f"Conflicting doc-b.txt versions both preserved in live tree: {both_doc_b_preserved}")
        results.append(
            ScenarioEvidence(
                name="three_client_offline_return",
                description="Third client stays offline across multiple A/B cycles, makes stale conflicting and new edits, and returns; verify multi-cycle convergence, no resurrection, and zero data loss.",
                started_at=s4_start,
                finished_at=utc_now(),
                injected_failure="client-c offline across 2 remote cycles with stale conflicting edits on doc-b.txt",
                passed=passed,
                conclusion="supported (conflict preserved, deletion not resurrected)" if passed else "failed",
                commands=s4_cmds,
                outcomes={
                    "converged": converged,
                    "c_new_present": c_new_present,
                    "online_v2_present": online_v2_present,
                    "deleted_not_resurrected": deleted_not_resurrected,
                    "both_doc_b_preserved": both_doc_b_preserved,
                },
                hash_accounting=acct,
                notes=s4_notes,
            )
        )
    except Exception as exc:  # noqa: BLE001
        results.append(
            ScenarioEvidence(
                name="three_client_offline_return",
                description="Third client offline return scenario.",
                started_at=s4_start,
                finished_at=utc_now(),
                injected_failure="client-c offline return",
                passed=False,
                conclusion=f"error: {exc}",
                commands=s4_cmds,
            )
        )

    return results


def run_deletion_guard_scenario(
    work_dir: Path,
    sync_cli: Path,
    rclone_bin: Path,
    fake_home: Path,
    seed: int,
) -> ScenarioEvidence:
    start = utc_now()
    cmds: list[dict[str, Any]] = []
    notes: list[str] = []
    truth = TruthSet(work_dir / "truth-delguard", seed=seed + 100)
    remote_dir = work_dir / "delguard-remote" / "Data" / "guard"
    remote_dir.mkdir(parents=True, exist_ok=True)
    remote_rec = work_dir / "delguard-remote" / "Recovery" / "guard"
    client = ClientHarness("client-delguard", work_dir / "clients" / "client-delguard", sync_cli, rclone_bin, fake_home)

    try:
        labels: list[str] = []
        for idx in range(1, 11):
            label = f"dg-f{idx:02d}"
            truth.write_into(client.local_dir, f"f{idx:02d}.bin", label, size_bytes=512, binary=True)
            labels.append(label)

        cmds.append(client.add_mapping("guard", remote_dir, remote_rec, collection="Data/guard", max_delete_percent=10))
        cmds.append(client.init("guard"))

        # Verify pause / resume intent persistence before fault injection
        cmds.append(client.pause("guard"))
        paused_intent = client.status_json()["mappings"][0]["intent"]
        cmds.append(client.resume("guard"))
        resumed_intent = client.status_json()["mappings"][0]["intent"]

        # Inject 40% local deletion (> 10% limit)
        for idx in range(1, 5):
            (client.local_dir / f"f{idx:02d}.bin").unlink()

        abort_res = client.run_once("guard", check=False)
        cmds.append(abort_res)

        status = client.status_json()["mappings"][0]
        readiness = status["readiness"]
        finder_snapshot = json.loads((client.finder_dir / "status.json").read_text(encoding="utf-8"))
        finder_state = finder_snapshot["mappings"][0]["state"]

        remote_hashes = truth.scan_tree_hashes(remote_dir)
        acct, acct_ok = account_truth_versions(truth, labels, {"remote": remote_dir})
        all_remote_intact = len(remote_hashes) == 10 and acct_ok

        passed = (
            paused_intent == "Paused"
            and resumed_intent == "Enabled"
            and abort_res["returncode"] != 0
            and readiness == "RecoveryRequired"
            and finder_state == "attention"
            and all_remote_intact
        )
        notes.append(f"sync-cli run-once exited {abort_res['returncode']} (expected non-zero)")
        notes.append(f"Mapping readiness={readiness}, Finder status.json state={finder_state}")
        notes.append(f"All 10 files intact on remote with exact truth-set SHA-256: {all_remote_intact}")
        return ScenarioEvidence(
            name="deletion_guard_abort_and_recovery",
            description="Deleting 4 of 10 files (40%) with --max-delete-percent 10 must abort with zero remote deletions, latch RecoveryRequired, and publish Finder attention state.",
            started_at=start,
            finished_at=utc_now(),
            injected_failure="delete 4/10 files (40% > 10% deletion guard)",
            passed=passed,
            conclusion="supported (deletion guard aborted safely; zero remote deletes)" if passed else "failed",
            commands=cmds,
            outcomes={
                "paused_intent": paused_intent,
                "resumed_intent": resumed_intent,
                "abort_rc": abort_res["returncode"],
                "readiness": readiness,
                "finder_state": finder_state,
                "remote_file_count": len(remote_hashes),
            },
            hash_accounting=acct,
            notes=notes,
        )
    except Exception as exc:  # noqa: BLE001
        return ScenarioEvidence(
            name="deletion_guard_abort_and_recovery",
            description="Deletion guard abort scenario.",
            started_at=start,
            finished_at=utc_now(),
            injected_failure="delete 4/10 files",
            passed=False,
            conclusion=f"error: {exc}",
            commands=cmds,
        )


def run_interruption_scenarios(
    work_dir: Path,
    sync_cli: Path,
    rclone_bin: Path,
    fake_home: Path,
    seed: int,
) -> ScenarioEvidence:
    """Tests interruption across phases:
    - Phase A: Pre-listing access-marker absence (RCLONE_TEST missing -> safe abort, then restore & converge)
    - Phase B: Graceful SIGINT during active multi-file transfer (run recorded interrupted, workdir preserved)
    - Phase C: Abrupt SIGKILL crash during active run + startup reconciliation via `sync-cli status --json`
    """
    start = utc_now()
    cmds: list[dict[str, Any]] = []
    notes: list[str] = []
    truth = TruthSet(work_dir / "truth-interrupt", seed=seed + 200)

    try:
        # --- Phase A: Missing RCLONE_TEST before listing ---
        pa_remote = work_dir / "int-a-remote" / "Data" / "pa"
        pa_remote.mkdir(parents=True, exist_ok=True)
        pa_client = ClientHarness("client-int-a", work_dir / "clients" / "client-int-a", sync_cli, rclone_bin, fake_home)
        truth.write_into(pa_client.local_dir, "keep.txt", "int-a-keep-v1", size_bytes=300)
        cmds.append(pa_client.add_mapping("pa", pa_remote, work_dir / "int-a-remote" / "Recovery" / "pa", collection="Data/pa"))
        cmds.append(pa_client.init("pa"))

        marker_path = pa_remote / "RCLONE_TEST"
        saved_marker = marker_path.read_bytes()
        marker_path.unlink()

        time.sleep(MTIME_STEP_SECS)
        truth.write_into(pa_client.local_dir, "keep.txt", "int-a-keep-v2", size_bytes=350)
        missing_marker_res = pa_client.run_once("pa", check=False)
        cmds.append(missing_marker_res)
        pa_status_after_fault = pa_client.status_json()["mappings"][0]
        # Remote must still hold v1 (no mutation when access check fails)
        pa_remote_sha_during_fault = sha256_file(pa_remote / "keep.txt")
        phase_a_ok = (
            missing_marker_res["returncode"] != 0
            and pa_remote_sha_during_fault == truth.records["int-a-keep-v1"]["sha256"]
        )
        # Restore marker so cleanup/audit stays honest
        marker_path.write_bytes(saved_marker)
        notes.append(
            f"Phase A (missing RCLONE_TEST pre-listing): rc={missing_marker_res['returncode']}, "
            f"readiness={pa_status_after_fault['readiness']}, remote unchanged={phase_a_ok}"
        )

        # --- Phase B: Graceful SIGINT during active multi-file transfer ---
        pb_remote = work_dir / "int-b-remote" / "Data" / "pb"
        pb_remote.mkdir(parents=True, exist_ok=True)
        pb_client = ClientHarness("client-int-b", work_dir / "clients" / "client-int-b", sync_cli, rclone_bin, fake_home)
        truth.write_into(pb_client.local_dir, "seed.txt", "int-b-seed", size_bytes=256)
        cmds.append(pb_client.add_mapping("pb", pb_remote, work_dir / "int-b-remote" / "Recovery" / "pb", collection="Data/pb"))
        cmds.append(pb_client.init("pb"))

        # Populate a 1200-file batch (1200 x 64 KiB = ~75 MiB) so transfer is reliably
        # interrupted in flight on fast local APFS storage.
        batch_dir = pb_client.local_dir / "batch"
        batch_dir.mkdir(parents=True, exist_ok=True)
        sample_labels: list[str] = ["int-b-seed"]
        filler_chunk = os.urandom(65536)
        for i in range(1200):
            fname = f"batch/chunk-{i:04d}.bin"
            if i < 5:
                truth.write_into(pb_client.local_dir, fname, f"int-b-chunk-{i:04d}", size_bytes=65536, binary=True)
                sample_labels.append(f"int-b-chunk-{i:04d}")
            else:
                (pb_client.local_dir / fname).write_bytes(filler_chunk)

        proc_b = subprocess.Popen(
            [
                str(sync_cli),
                "run-once",
                "pb",
                "--state-dir",
                str(pb_client.state_dir),
                "--finder-dir",
                str(pb_client.finder_dir),
            ],
            env=pb_client._env(),
            start_new_session=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        # Wait until the rclone child PID appears, then send SIGINT to the rclone child
        # immediately so sync-cli observes rclone's graceful interrupt and commits
        # Interrupted / RecoveryRequired with zero orphan processes.
        signaled_target = "none"
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline and proc_b.poll() is None:
            child_pid = None
            try:
                pg = subprocess.run(
                    ["pgrep", "-P", str(proc_b.pid)],
                    capture_output=True,
                    text=True,
                    timeout=2,
                )
                pids = [int(x) for x in pg.stdout.split() if x.isdigit()]
                if pids:
                    child_pid = pids[0]
            except Exception:  # noqa: BLE001
                child_pid = None
            if child_pid is not None:
                # Wait until bisync starts (not the short `rclone version` probe):
                # `rclone bisync` is the second child or runs with `bisync` in argv.
                ps_out = subprocess.run(
                    ["ps", "-p", str(child_pid), "-o", "args="],
                    capture_output=True,
                    text=True,
                    timeout=2,
                ).stdout
                if "bisync" in ps_out:
                    time.sleep(0.02)
                    try:
                        os.kill(child_pid, signal.SIGINT)
                        signaled_target = f"rclone-bisync:{child_pid}"
                    except ProcessLookupError:
                        signaled_target = "child-exited-before-signal"
                    break
            time.sleep(0.01)
        out_b, err_b = proc_b.communicate(timeout=90)
        cmds.append(
            {
                "client": "client-int-b",
                "args": ["run-once", "pb", f"(SIGINT -> {signaled_target})"],
                "returncode": proc_b.returncode,
                "stdout": out_b.strip()[:300],
                "stderr": err_b.strip()[:300],
            }
        )
        pb_status_after = pb_client.status_json()["mappings"][0]
        pb_runs = pb_client.sqlite_runs()
        pb_bisync_dir = pb_client.state_dir / "mappings" / "pb" / "generation-1" / "bisync"
        pb_workdir_preserved = pb_bisync_dir.is_dir() and any(pb_bisync_dir.iterdir())
        pb_hit_mid_run = proc_b.returncode != 0
        phase_b_ok = (
            pb_workdir_preserved
            and pb_hit_mid_run
            and pb_status_after["readiness"] == "RecoveryRequired"
        )
        notes.append(
            f"Phase B (SIGINT transfer, target={signaled_target}): rc={proc_b.returncode}, "
            f"hit_mid_run={pb_hit_mid_run}, readiness={pb_status_after['readiness']}, "
            f"workdir_preserved={pb_workdir_preserved}, recorded_runs={len(pb_runs)}"
        )

        # --- Phase C: Abrupt SIGKILL crash + startup reconciliation ---
        pc_remote = work_dir / "int-c-remote" / "Data" / "pc"
        pc_remote.mkdir(parents=True, exist_ok=True)
        pc_client = ClientHarness("client-int-c", work_dir / "clients" / "client-int-c", sync_cli, rclone_bin, fake_home)
        truth.write_into(pc_client.local_dir, "keep.txt", "int-c-keep", size_bytes=320)
        cmds.append(pc_client.add_mapping("pc", pc_remote, work_dir / "int-c-remote" / "Recovery" / "pc", collection="Data/pc"))
        cmds.append(pc_client.init("pc"))

        # Inject a simulated mid-run crash by inserting an unfinished 'running' row with a
        # non-existent PID (reconciled_exited path) + preserving bisync workdir, matching
        # an abrupt SIGKILL of sync-cli before terminal DB commit.
        conn = sqlite3.connect(str(pc_client.state_dir / "state.sqlite"))
        try:
            gen_id, workdir_path = conn.execute(
                "SELECT generation_id, workdir_path FROM runs WHERE mapping_id='pc' LIMIT 1"
            ).fetchone()
            crash_run_dir = pc_client.state_dir / "runs" / "run-injected-crash"
            crash_run_dir.mkdir(parents=True, exist_ok=True)
            conn.execute(
                """
                INSERT INTO runs (
                    id, mapping_id, generation_id, mode, status, started_at,
                    workdir_path, backup_dir1, backup_dir2, run_dir, log_path,
                    child_pid, child_start_time_kind, child_start_time_value, child_executable,
                    conflict_paths_json, last_verified_phase
                ) VALUES (
                    'run-injected-crash', 'pc', ?, 'normal', 'running', ?,
                    ?, ?, ?, ?, ?,
                    999999, NULL, NULL, ?,
                    '[]', 'spawned'
                )
                """,
                (
                    gen_id,
                    utc_now(),
                    workdir_path,
                    str(pc_client.recovery_dir / "run-injected-crash"),
                    str(work_dir / "int-c-remote" / "Recovery" / "pc" / "run-injected-crash"),
                    str(crash_run_dir),
                    str(crash_run_dir / "output.ndjson"),
                    str(rclone_bin),
                ),
            )
            conn.commit()
        finally:
            conn.close()

        # Re-open manager via `sync-cli status --json` to trigger startup reconciliation
        pc_status_reconciled = pc_client.status_json()["mappings"][0]
        pc_runs = pc_client.sqlite_runs()
        crashed_row = next((r for r in pc_runs if r["id"] == "run-injected-crash"), None)
        pc_outcome_file = pc_client.state_dir / "runs" / "run-injected-crash" / "outcome.json"
        pc_bisync_preserved = Path(workdir_path).is_dir() and any(Path(workdir_path).iterdir())

        phase_c_ok = (
            pc_status_reconciled["readiness"] == "RecoveryRequired"
            and crashed_row is not None
            and crashed_row["status"] == "interrupted"
            and pc_outcome_file.is_file()
            and pc_bisync_preserved
        )
        notes.append(
            f"Phase C (crash + startup reconciliation): readiness={pc_status_reconciled['readiness']}, "
            f"row_status={crashed_row['status'] if crashed_row else None}, "
            f"outcome_json_written={pc_outcome_file.is_file()}, workdir_preserved={pc_bisync_preserved}"
        )

        acct, acct_ok = account_truth_versions(
            truth,
            ["int-a-keep-v1", "int-a-keep-v2", "int-c-keep", *sample_labels],
            {
                "pa-local": pa_client.local_dir,
                "pa-remote": pa_remote,
                "pb-local": pb_client.local_dir,
                "pb-remote": pb_remote,
                "pc-local": pc_client.local_dir,
                "pc-remote": pc_remote,
            },
        )
        passed = phase_a_ok and phase_b_ok and phase_c_ok and acct_ok
        return ScenarioEvidence(
            name="interruption_at_phases",
            description="Inject faults across phases: pre-listing missing RCLONE_TEST, mid-transfer SIGINT, and abrupt crash + startup reconciliation; verify workdir preservation, RecoveryRequired hold, and zero data loss.",
            started_at=start,
            finished_at=utc_now(),
            injected_failure="missing RCLONE_TEST before listing; SIGINT during 1200-file batch; unfinished running row on restart",
            passed=passed,
            conclusion="supported (holds in RecoveryRequired with workdir preserved; zero data corruption)" if passed else "failed",
            commands=cmds,
            outcomes={
                "phase_a_ok": phase_a_ok,
                "phase_b_ok": phase_b_ok,
                "phase_b_hit_mid_run": pb_hit_mid_run,
                "phase_c_ok": phase_c_ok,
                "pc_reconciled_readiness": pc_status_reconciled["readiness"],
            },
            hash_accounting=acct,
            notes=notes,
        )
    except Exception as exc:  # noqa: BLE001
        return ScenarioEvidence(
            name="interruption_at_phases",
            description="Interruption at phases scenario.",
            started_at=start,
            finished_at=utc_now(),
            injected_failure="phase interruptions",
            passed=False,
            conclusion=f"error: {exc}",
            commands=cmds,
            notes=notes,
        )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run local-to-local FolderSync qualification scenarios against sync-cli."
    )
    parser.add_argument(
        "--out-dir",
        default=str(REPO_ROOT / "target" / "qualification"),
        help="Directory for evidence output (default: target/qualification)",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=20261120,
        help="Deterministic seed for generated file contents",
    )
    parser.add_argument(
        "--sync-cli",
        default=None,
        help="Path to built sync-cli executable (built via cargo if omitted)",
    )
    args = parser.parse_args()

    started_at = utc_now()
    rclone_bin, rclone_ver, rclone_sha = resolve_verified_pinned_rclone()
    sync_cli = build_or_find_sync_cli(args.sync_cli)
    versions = get_tool_versions(sync_cli, rclone_bin, rclone_ver, rclone_sha)

    out_dir = Path(args.out_dir).resolve()
    json_path = out_dir / "local-evidence.json"
    md_path = out_dir / "local-evidence.md"

    with tempfile.TemporaryDirectory(prefix="foldersync-qual-local-") as tmp:
        work_dir = Path(tmp)
        fake_home = work_dir / "home"
        fake_home.mkdir(parents=True, exist_ok=True)

        scenarios: list[ScenarioEvidence] = []
        scenarios.extend(run_three_client_scenarios(work_dir, sync_cli, rclone_bin, fake_home, args.seed))
        scenarios.append(run_deletion_guard_scenario(work_dir, sync_cli, rclone_bin, fake_home, args.seed))
        scenarios.append(run_interruption_scenarios(work_dir, sync_cli, rclone_bin, fake_home, args.seed))

    finished_at = utc_now()
    all_passed = all(s.passed for s in scenarios)
    conclusion = (
        "All local-to-local qualification scenarios PASSED with verified truth-set SHA-256 preservation. "
        "Real Google Drive (two accounts, OAuth, shared-parent folder IDs, rate limits) and physical "
        "external-disk unplug tests remain owner-run scenarios (see docs/qualification.md)."
        if all_passed
        else "One or more local qualification scenarios FAILED; see scenario details above."
    )

    write_evidence_files(
        json_path=json_path,
        md_path=md_path,
        suite_name="local-to-local",
        seed=args.seed,
        started_at=started_at,
        finished_at=finished_at,
        versions=versions,
        scenarios=scenarios,
        overall_conclusion=conclusion,
    )

    for s in scenarios:
        print(f"[{'PASS' if s.passed else 'FAIL'}] {s.name}: {s.conclusion}")
    print(f"Evidence written to:\n  {json_path}\n  {md_path}")
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
