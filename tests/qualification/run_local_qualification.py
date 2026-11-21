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
    compute_overall_status,
    get_tool_versions,
    redact_credential_stderr,
    resolve_verified_pinned_rclone,
    sha256_file,
    utc_now,
    write_evidence_files,
)


MTIME_STEP_SECS = 1.15


def is_mid_transfer_interruption_verified(
    *,
    signal_confirmed: bool,
    count_before_signal: int,
    count_after_signal: int,
    count_after_exit: int,
    batch_count: int,
    cli_returncode: int,
    readiness: str,
    run_log_or_outcome_shows_interrupt: bool,
) -> bool:
    """Returns True ONLY when positive evidence proves SIGINT landed mid-transfer."""
    return (
        signal_confirmed
        and 1 <= count_before_signal < batch_count
        and count_after_signal < batch_count
        and count_after_exit < batch_count
        and cli_returncode != 0
        and readiness == "RecoveryRequired"
        and run_log_or_outcome_shows_interrupt
    )


def inspect_interrupted_run_evidence(state_dir: Path, run_id: str | None) -> tuple[bool, dict[str, Any]]:
    """Checks `outcome.json` and `output.ndjson` for positive interrupted-transfer evidence."""
    details: dict[str, Any] = {
        "run_id": run_id,
        "outcome_kind": None,
        "log_has_interrupt_marker": False,
    }
    if not run_id:
        return False, details
    run_dir = state_dir / "runs" / run_id
    outcome_file = run_dir / "outcome.json"
    log_file = run_dir / "output.ndjson"

    outcome_interrupted = False
    if outcome_file.is_file():
        try:
            outcome = json.loads(outcome_file.read_text(encoding="utf-8"))
            kind = str(outcome.get("outcome_kind") or outcome.get("kind") or outcome.get("status") or "").lower()
            details["outcome_kind"] = kind
            if "interrupt" in kind:
                outcome_interrupted = True
        except Exception:  # noqa: BLE001
            pass

    log_interrupted = False
    if log_file.is_file():
        try:
            raw_log = log_file.read_text(encoding="utf-8", errors="replace").lower()
            if (
                "signal received: interrupt" in raw_log
                or "graceful shutdown" in raw_log
                or "interrupted" in raw_log
            ):
                log_interrupted = True
        except Exception:  # noqa: BLE001
            pass
    details["log_has_interrupt_marker"] = log_interrupted
    return (outcome_interrupted or log_interrupted), details


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

        # --- Phase B: Graceful SIGINT during observed active transfer + follow-up recovery cycle ---
        pb_remote = work_dir / "int-b-remote" / "Data" / "pb"
        pb_remote.mkdir(parents=True, exist_ok=True)
        pb_rec_remote = work_dir / "int-b-remote" / "Recovery" / "pb"
        pb_client = ClientHarness("client-int-b", work_dir / "clients" / "client-int-b", sync_cli, rclone_bin, fake_home)
        truth.write_into(pb_client.local_dir, "seed.txt", "int-b-seed", size_bytes=256)
        cmds.append(pb_client.add_mapping("pb", pb_remote, pb_rec_remote, collection="Data/pb"))
        cmds.append(pb_client.init("pb"))

        # Populate a 500-file batch (500 x 128 KiB = ~64 MiB), giving EVERY generated file
        # its own uniquely identifiable truth-set entry and SHA-256 outside the sync trees.
        batch_count = 500
        batch_dir = pb_client.local_dir / "batch"
        batch_dir.mkdir(parents=True, exist_ok=True)
        pb_truth_labels: list[str] = ["int-b-seed"]
        for i in range(batch_count):
            fname = f"batch/chunk-{i:04d}.bin"
            label = f"int-b-chunk-{i:04d}"
            truth.write_into(pb_client.local_dir, fname, label, size_bytes=131072, binary=True)
            pb_truth_labels.append(label)

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

        # Require BOTH: (1) a live `rclone bisync` child PID AND (2) observed partial transfer
        # progress on the destination (1 <= files_transferred < batch_count) BEFORE sending SIGINT,
        # THEN immediately re-sample AFTER os.kill() and AFTER process exit to prove the signal
        # landed mid-transfer (not during finalization after all files had arrived).
        signaled_pid: int | None = None
        signal_confirmed = False
        transferred_before_sigint = 0
        transferred_after_sigint = batch_count
        pb_remote_batch = pb_remote / "batch"

        def count_remote_batch() -> int:
            if not pb_remote_batch.is_dir():
                return 0
            try:
                return sum(1 for _ in pb_remote_batch.iterdir())
            except OSError:
                return 0

        deadline = time.monotonic() + 30.0
        while time.monotonic() < deadline and proc_b.poll() is None:
            cur_transferred = count_remote_batch()
            if 1 <= cur_transferred < batch_count:
                try:
                    pg = subprocess.run(
                        ["pgrep", "-P", str(proc_b.pid)],
                        capture_output=True,
                        text=True,
                        timeout=2,
                    )
                    pids = [int(x) for x in pg.stdout.split() if x.isdigit()]
                except Exception:  # noqa: BLE001
                    pids = []
                for pid in pids:
                    ps_out = subprocess.run(
                        ["ps", "-p", str(pid), "-o", "args="],
                        capture_output=True,
                        text=True,
                        timeout=2,
                    ).stdout
                    if "bisync" in ps_out:
                        try:
                            os.kill(pid, signal.SIGINT)
                            # Re-sample destination count immediately after sending SIGINT.
                            transferred_after_sigint = count_remote_batch()
                            signaled_pid = pid
                            signal_confirmed = True
                            transferred_before_sigint = cur_transferred
                        except ProcessLookupError:
                            signal_confirmed = False
                        break
                if signal_confirmed:
                    break
            time.sleep(0.005)

        out_b, err_b = proc_b.communicate(timeout=90)
        transferred_after_exit = count_remote_batch()
        cmds.append(
            {
                "client": "client-int-b",
                "args": [
                    "run-once",
                    "pb",
                    f"(SIGINT confirmed={signal_confirmed} pid={signaled_pid} progress={transferred_before_sigint}->{transferred_after_sigint}->{transferred_after_exit}/{batch_count})",
                ],
                "returncode": proc_b.returncode,
                "stdout": redact_credential_stderr(out_b).strip()[:300],
                "stderr": redact_credential_stderr(err_b).strip()[:300],
            }
        )
        pb_status_after = pb_client.status_json()["mappings"][0]
        pb_runs = pb_client.sqlite_runs()
        last_run_id = pb_status_after.get("last_run_id") or (pb_runs[-1]["id"] if pb_runs else None)
        log_or_outcome_interrupt, interrupt_evidence = inspect_interrupted_run_evidence(
            pb_client.state_dir, last_run_id
        )
        pb_bisync_dir = pb_client.state_dir / "mappings" / "pb" / "generation-1" / "bisync"
        pb_filters_file = pb_client.state_dir / "mappings" / "pb" / "generation-1" / "filters.txt"
        pb_rclone_conf = pb_client.state_dir / "accounts" / "local-fixture" / "rclone.conf"
        pb_workdir_preserved = pb_bisync_dir.is_dir() and any(pb_bisync_dir.iterdir())
        pb_hit_mid_run = is_mid_transfer_interruption_verified(
            signal_confirmed=signal_confirmed,
            count_before_signal=transferred_before_sigint,
            count_after_signal=transferred_after_sigint,
            count_after_exit=transferred_after_exit,
            batch_count=batch_count,
            cli_returncode=proc_b.returncode,
            readiness=pb_status_after["readiness"],
            run_log_or_outcome_shows_interrupt=log_or_outcome_interrupt,
        )

        # Verify sync-cli refuses automatic re-run while mapping is held in RecoveryRequired.
        held_refusal = pb_client.run_once("pb", check=False)
        cmds.append(held_refusal)
        pb_hold_enforced = held_refusal["returncode"] != 0

        # Execute the follow-up `--resilient --recover` sync cycle + a second clean normal cycle
        # against the preserved sync-manager bisync workdir using the verified pinned rclone,
        # confirming all 500/500 batch files + seed.txt converge with exact truth-set SHA-256.
        followup_ok = False
        all_pb_converged = False
        if pb_hit_mid_run and pb_workdir_preserved:
            rclone_env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(fake_home),
                "TMPDIR": str(fake_home),
                "LANG": "en_US.UTF-8",
            }
            for cycle_idx in (1, 2):
                b1 = pb_client.recovery_dir / "pb" / f"followup-cycle-{cycle_idx}"
                b2 = pb_rec_remote / f"followup-cycle-{cycle_idx}"
                b1.mkdir(parents=True, exist_ok=True)
                b2.mkdir(parents=True, exist_ok=True)
                rc_proc = subprocess.run(
                    [
                        str(rclone_bin),
                        "bisync",
                        str(pb_client.local_dir.resolve()),
                        str(pb_remote.resolve()),
                        "--config",
                        str(pb_rclone_conf.resolve()),
                        "--ask-password=false",
                        "--workdir",
                        str(pb_bisync_dir.resolve()),
                        "--filters-file",
                        str(pb_filters_file.resolve()),
                        "--check-access",
                        "--check-filename",
                        "RCLONE_TEST",
                        "--max-delete",
                        "10",
                        "--compare",
                        "size,modtime",
                        "--conflict-resolve",
                        "none",
                        "--conflict-loser",
                        "num",
                        "--check-sync",
                        "true",
                        "--resilient",
                        "--recover",
                        "--backup-dir1",
                        str(b1.resolve()),
                        "--backup-dir2",
                        str(b2.resolve()),
                        "--use-json-log",
                        "--log-level",
                        "INFO",
                    ],
                    env=rclone_env,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    text=True,
                    timeout=120,
                )
                cmds.append(
                    {
                        "client": "client-int-b",
                        "args": ["rclone", "bisync", f"(follow-up recovery cycle {cycle_idx})"],
                        "returncode": rc_proc.returncode,
                        "stdout": redact_credential_stderr(rc_proc.stdout).strip()[-300:],
                        "stderr": redact_credential_stderr(rc_proc.stderr).strip()[-400:],
                    }
                )
                if rc_proc.returncode != 0:
                    followup_ok = False
                    break
                followup_ok = True

            if followup_ok:
                local_hashes = truth.scan_tree_hashes(pb_client.local_dir)
                remote_hashes = truth.scan_tree_hashes(pb_remote)
                expected_pb = {
                    truth.records[lbl]["rel_path"]: truth.records[lbl]["sha256"]
                    for lbl in pb_truth_labels
                }
                all_pb_converged = local_hashes == expected_pb and remote_hashes == expected_pb

        phase_b_verified = (
            pb_hit_mid_run
            and pb_workdir_preserved
            and pb_hold_enforced
            and followup_ok
            and all_pb_converged
        )
        notes.append(
            f"Phase B (mid-transfer SIGINT + recovery cycle): signal_confirmed={signal_confirmed}, "
            f"pid={signaled_pid}, progress={transferred_before_sigint}->{transferred_after_sigint}->{transferred_after_exit}/{batch_count}, "
            f"rc={proc_b.returncode}, readiness={pb_status_after['readiness']}, "
            f"interrupt_evidence={interrupt_evidence}, hold_enforced={pb_hold_enforced}, "
            f"workdir_preserved={pb_workdir_preserved}, followup_recovery_cycles_ok={followup_ok}, "
            f"all_{len(pb_truth_labels)}_truth_hashes_converged={all_pb_converged}, recorded_runs={len(pb_runs)}"
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

        # Build truth-set hash accounting for every single generated file across Phases A, B (all 501), and C.
        pa_local_h = truth.scan_tree_hashes(pa_client.local_dir)
        pa_remote_h = truth.scan_tree_hashes(pa_remote)
        pb_local_h = truth.scan_tree_hashes(pb_client.local_dir)
        pb_remote_h = truth.scan_tree_hashes(pb_remote)
        pc_local_h = truth.scan_tree_hashes(pc_client.local_dir)
        pc_remote_h = truth.scan_tree_hashes(pc_remote)

        sha_index: dict[str, list[str]] = {}
        for root_name, tree_map in (
            ("pa-local", pa_local_h),
            ("pa-remote", pa_remote_h),
            ("pb-local", pb_local_h),
            ("pb-remote", pb_remote_h),
            ("pc-local", pc_local_h),
            ("pc-remote", pc_remote_h),
        ):
            for rel, digest in tree_map.items():
                sha_index.setdefault(digest, []).append(f"{root_name}:{rel}")

        acct: list[dict[str, Any]] = []
        acct_ok = True
        for label in ["int-a-keep-v1", "int-a-keep-v2", "int-c-keep", *pb_truth_labels]:
            rec = truth.records[label]
            locs = sha_index.get(rec["sha256"], [])
            if label in pb_truth_labels:
                ok = f"pb-local:{rec['rel_path']}" in locs and f"pb-remote:{rec['rel_path']}" in locs
            else:
                ok = len(locs) > 0
            if not ok:
                acct_ok = False
            acct.append(
                {
                    "label": label,
                    "rel_path": rec["rel_path"],
                    "sha256": rec["sha256"],
                    "locations": locs,
                    "verified": ok,
                }
            )

        if not pb_hit_mid_run:
            scenario_status = "NOT VERIFIED"
            passed = False
            conclusion = (
                "NOT VERIFIED: did not prove SIGINT landed mid-transfer "
                f"(before={transferred_before_sigint}, after_signal={transferred_after_sigint}, "
                f"after_exit={transferred_after_exit}/{batch_count}, log_or_outcome_interrupt={log_or_outcome_interrupt})."
            )
        elif phase_a_ok and phase_b_verified and phase_c_ok and acct_ok:
            scenario_status = "PASS"
            passed = True
            conclusion = (
                f"supported (confirmed mid-transfer SIGINT at {transferred_before_sigint}->{transferred_after_sigint}->{transferred_after_exit}/{batch_count} files "
                f"with interrupted run log/outcome, latched RecoveryRequired, and converged all {len(pb_truth_labels)}/{len(pb_truth_labels)} truth-set hashes across follow-up cycles)"
            )
        else:
            scenario_status = "FAIL"
            passed = False
            conclusion = "failed (interruption hold or follow-up truth-set verification failed)"

        return ScenarioEvidence(
            name="interruption_at_phases",
            description="Inject faults across phases: pre-listing missing RCLONE_TEST, verified mid-transfer SIGINT + follow-up recovery cycles over 500 truth-set files, and abrupt crash + startup reconciliation.",
            started_at=start,
            finished_at=utc_now(),
            injected_failure=f"missing RCLONE_TEST before listing; confirmed mid-transfer SIGINT ({transferred_before_sigint}->{transferred_after_sigint}->{transferred_after_exit}/{batch_count}); unfinished running row on restart",
            passed=passed,
            status=scenario_status,
            conclusion=conclusion,
            commands=cmds,
            outcomes={
                "phase_a_ok": phase_a_ok,
                "phase_b_signal_confirmed": signal_confirmed,
                "phase_b_transferred_before_sigint": transferred_before_sigint,
                "phase_b_transferred_after_sigint": transferred_after_sigint,
                "phase_b_transferred_after_exit": transferred_after_exit,
                "phase_b_batch_count": batch_count,
                "phase_b_log_or_outcome_interrupt": log_or_outcome_interrupt,
                "phase_b_hold_enforced": pb_hold_enforced,
                "phase_b_followup_ok": followup_ok,
                "phase_b_all_truth_converged": all_pb_converged,
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
    overall_status = compute_overall_status(scenarios)
    all_passed = overall_status == "PASS"
    conclusion = (
        "All local-to-local qualification scenarios PASSED with verified truth-set SHA-256 preservation. "
        "Real Google Drive (two accounts, OAuth, shared-parent folder IDs, rate limits) and physical "
        "external-disk unplug tests remain owner-run scenarios (see docs/qualification.md)."
        if all_passed
        else f"Local qualification ended with status {overall_status}; see scenario details above."
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
        print(f"[{s.effective_status()}] {s.name}: {s.conclusion}")
    print(f"Overall status: {overall_status}")
    print(f"Evidence written to:\n  {json_path}\n  {md_path}")
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
