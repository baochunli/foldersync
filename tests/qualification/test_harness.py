#!/usr/bin/env python3
"""Unit and safety-guardrail tests for the FolderSync qualification harness."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402
import run_drive_qualification  # noqa: E402


class QualificationHarnessTests(unittest.TestCase):
    def test_deterministic_truth_set_and_hash_accounting(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            t1 = common.TruthSet(root / "truth1", seed=42)
            t2 = common.TruthSet(root / "truth2", seed=42)

            b1, h1 = t1.generate_version("v1", "docs/notes.txt", size_bytes=200)
            b2, h2 = t2.generate_version("v1", "docs/notes.txt", size_bytes=200)
            self.assertEqual(b1, b2)
            self.assertEqual(h1, h2)

            live = root / "live"
            h_written = t1.write_into(live, "docs/notes.txt", "v1-live", size_bytes=200)
            locs = t1.find_hash_locations(h_written, {"live": live})
            self.assertEqual(locs, ["live:docs/notes.txt"])

    def test_manifest_parses_pinned_rclone(self) -> None:
        manifest = common.parse_qualification_manifest()
        self.assertEqual(manifest["rclone"]["version"], "1.76.0")
        self.assertEqual(len(manifest["rclone.osx-arm64"]["binary_sha256"]), 64)
        self.assertEqual(len(manifest["rclone.linux-amd64"]["binary_sha256"]), 64)

    def test_drive_harness_refuses_missing_required_flags(self) -> None:
        script = Path(__file__).resolve().parent / "run_drive_qualification.py"
        for bad_args in (
            [],
            ["--remote", "qualdrive"],
            ["--state-dir", "/tmp/qual-state"],
            ["--remote", "qualdrive", "--state-dir", "/tmp/qual-state"],
        ):
            proc = subprocess.run(
                [sys.executable, str(script), *bad_args],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(proc.returncode, 0, f"expected refusal for {bad_args}")

    def test_drive_harness_refuses_real_state_dir_and_global_rclone_config(self) -> None:
        script = Path(__file__).resolve().parent / "run_drive_qualification.py"
        real_state = Path.home() / "Library" / "Application Support" / "FolderSync"
        global_cfg = Path.home() / ".config" / "rclone" / "rclone.conf"

        p1 = subprocess.run(
            [
                sys.executable,
                str(script),
                "--remote",
                "qualdrive",
                "--state-dir",
                str(real_state),
                "--rclone-config",
                "/tmp/disposable.conf",
                "--dry-run",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(p1.returncode, 0)
        self.assertIn("real FolderSync state directory", p1.stderr)

        with tempfile.TemporaryDirectory() as tmp:
            safe_state = Path(tmp).resolve() / "qual-safe-state"
            p2 = subprocess.run(
                [
                    sys.executable,
                    str(script),
                    "--remote",
                    "qualdrive",
                    "--state-dir",
                    str(safe_state),
                    "--rclone-config",
                    str(global_cfg),
                    "--dry-run",
                ],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(p2.returncode, 0)
            self.assertIn(".config/rclone", p2.stderr)

    def test_drive_harness_dry_run_validates_cleanly_without_touching_drive(self) -> None:
        script = Path(__file__).resolve().parent / "run_drive_qualification.py"
        with tempfile.TemporaryDirectory() as tmp:
            p = subprocess.run(
                [
                    sys.executable,
                    str(script),
                    "--remote",
                    "qualdrive",
                    "--state-dir",
                    str(Path(tmp) / "state"),
                    "--rclone-config",
                    str(Path(tmp) / "disposable.conf"),
                    "--dry-run",
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            plan = json.loads(p.stdout)
            self.assertEqual(plan["mode"], "dry-run")
            self.assertEqual(plan["remote"], "qualdrive")
            self.assertEqual(plan["overall_expected_status"], "INCOMPLETE")
            self.assertIn("drive_preflight_account_and_folder_ids", plan["preflight_scenarios"])
            self.assertIn("drive_three_client_same_file_edit_conflict", plan["not_run_scenarios"])

    def test_evidence_writer_emits_json_and_markdown(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            ev = common.ScenarioEvidence(
                name="demo",
                description="Demo scenario",
                started_at="2026-11-20T00:00:00Z",
                finished_at="2026-11-20T00:00:05Z",
                injected_failure="none",
                passed=True,
                conclusion="supported",
                hash_accounting=[
                    {
                        "label": "v1",
                        "rel_path": "a.txt",
                        "sha256": "a" * 64,
                        "locations": ["client-a:a.txt"],
                        "verified": True,
                    }
                ],
            )
            common.write_evidence_files(
                json_path=out / "ev.json",
                md_path=out / "ev.md",
                suite_name="test-suite",
                seed=1,
                started_at="2026-11-20T00:00:00Z",
                finished_at="2026-11-20T00:00:05Z",
                versions={"rclone_version": "1.76.0"},
                scenarios=[ev],
                overall_conclusion="ok",
            )
            parsed = json.loads((out / "ev.json").read_text(encoding="utf-8"))
            self.assertTrue(parsed["passed"])
            md = (out / "ev.md").read_text(encoding="utf-8")
            self.assertIn("FolderSync Qualification Evidence", md)
            self.assertIn("`demo`", md)

    def test_packaging_and_notarize_safety_guardrails(self) -> None:
        build_script = common.REPO_ROOT / "packaging" / "build-app.sh"
        notarize_script = common.REPO_ROOT / "packaging" / "notarize.sh"

        # build-app.sh must refuse system /Applications output paths.
        p_sys = subprocess.run(
            ["bash", str(build_script), "--out", "/Applications"],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(p_sys.returncode, 0)
        self.assertIn("refusing", p_sys.stderr.lower())

        # notarize.sh must require --keychain-profile and refuse whitespace.
        p_missing = subprocess.run(
            ["bash", str(notarize_script), "--dry-run"],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(p_missing.returncode, 0)
        self.assertIn("--keychain-profile", p_missing.stderr)

        p_space = subprocess.run(
            ["bash", str(notarize_script), "--keychain-profile", "bad profile", "--dry-run"],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(p_space.returncode, 0)
        self.assertIn("whitespace", p_space.stderr)

        # build-app.sh and finder-extension/build.sh must refuse --out outside target/,
        # parent traversal out of target/, symlinked parents inside target/, and --shared-dir.
        ext_script = common.REPO_ROOT / "apps" / "finder-extension" / "build.sh"
        with tempfile.TemporaryDirectory() as tmp:
            outside = Path(tmp) / "outside-out"
            for script in (build_script, ext_script):
                for bad_out in (str(outside), str(common.REPO_ROOT / "target" / ".." / "escaped")):
                    p = subprocess.run(["bash", str(script), "--out", bad_out], capture_output=True, text=True)
                    self.assertNotEqual(p.returncode, 0, f"{script.name} must reject --out {bad_out}")
                    self.assertFalse(outside.exists(), f"{script.name} must not create {outside}")

                p_shared = subprocess.run(
                    ["bash", str(script), "--shared-dir", str(outside)],
                    capture_output=True,
                    text=True,
                )
                self.assertNotEqual(p_shared.returncode, 0, f"{script.name} must reject arbitrary --shared-dir")
                self.assertFalse(outside.exists())

            # Symlinked directory inside target/ pointing outside must be rejected BEFORE any write/rm.
            target_dir = common.REPO_ROOT / "target"
            target_dir.mkdir(exist_ok=True)
            sym = target_dir / f"test-symlink-escape-{os.getpid()}"
            if sym.is_symlink() or sym.exists():
                sym.unlink()
            outside.mkdir()
            sentinel = outside / "FolderSync.app"
            sentinel.mkdir()
            (sentinel / "keep.txt").write_text("precious", encoding="utf-8")
            sym.symlink_to(outside)
            try:
                p_sym = subprocess.run(
                    ["bash", str(build_script), "--out", str(sym)],
                    capture_output=True,
                    text=True,
                )
                self.assertNotEqual(p_sym.returncode, 0, "build-app.sh must refuse symlinked --out inside target/")
                self.assertTrue((sentinel / "keep.txt").is_file(), "must never touch or delete through symlinked --out")

                p_sym_ext = subprocess.run(
                    ["bash", str(ext_script), "--out", str(sym)],
                    capture_output=True,
                    text=True,
                )
                self.assertNotEqual(p_sym_ext.returncode, 0, "finder build.sh must refuse symlinked --out inside target/")
            finally:
                if sym.is_symlink():
                    sym.unlink()

    def test_redacts_credential_helper_stderr_and_blocks_pass_on_not_run(self) -> None:
        raw_json = '{"level":"error","msg":"--password-command stderr: TOP_SECRET_PASSPHRASE","source":"config/crypt.go:199"}'
        raw_plain = "2026/11/20 19:00:00 ERROR : --password-command stderr: ANOTHER_SECRET_123\nsecond secret line"
        benign = "2026/11/20 19:00:00 INFO : Copied notes/--password-command stderr: draft.txt"

        red_json = common.redact_credential_stderr(raw_json)
        self.assertNotIn("TOP_SECRET_PASSPHRASE", red_json)
        self.assertIn("[REDACTED]", red_json)

        red_plain = common.redact_credential_stderr(raw_plain)
        self.assertNotIn("ANOTHER_SECRET_123", red_plain)
        self.assertNotIn("second secret line", red_plain)
        self.assertIn("[REDACTED]", red_plain)

        self.assertEqual(common.redact_credential_stderr(benign), benign)

        # Overall evidence status must NEVER be PASS when any scenario is NOT RUN or NOT VERIFIED.
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            s_ok = common.ScenarioEvidence(
                name="preflight",
                description="ok",
                started_at="2026-11-20T00:00:00Z",
                finished_at="2026-11-20T00:00:01Z",
                injected_failure="none",
                passed=True,
                status="PASS",
                conclusion="ok",
            )
            s_nr = common.ScenarioEvidence(
                name="drive_conflict",
                description="pending",
                started_at="2026-11-20T00:00:01Z",
                finished_at="2026-11-20T00:00:01Z",
                injected_failure="none",
                passed=False,
                status="NOT RUN",
                conclusion="blocked on drive-t4",
            )
            common.write_evidence_files(
                json_path=out / "ev.json",
                md_path=out / "ev.md",
                suite_name="drive-test",
                seed=1,
                started_at="2026-11-20T00:00:00Z",
                finished_at="2026-11-20T00:00:01Z",
                versions={},
                scenarios=[s_ok, s_nr],
                overall_conclusion="incomplete",
            )
            parsed = json.loads((out / "ev.json").read_text(encoding="utf-8"))
            self.assertFalse(parsed["passed"])
            self.assertEqual(parsed["status"], "INCOMPLETE")
            self.assertIn("`NOT RUN`", (out / "ev.md").read_text(encoding="utf-8"))

    def test_drive_harness_refuses_non_empty_or_symlinked_state_dir(self) -> None:
        script = Path(__file__).resolve().parent / "run_drive_qualification.py"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cfg = root / "disposable.conf"
            cfg.write_text("", encoding="utf-8")

            non_empty = root / "existing-state"
            non_empty.mkdir()
            (non_empty / "preexisting.txt").write_text("x", encoding="utf-8")
            p_ne = subprocess.run(
                [
                    sys.executable,
                    str(script),
                    "--remote",
                    "qualdrive",
                    "--state-dir",
                    str(non_empty),
                    "--rclone-config",
                    str(cfg),
                    "--dry-run",
                ],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(p_ne.returncode, 0)
            self.assertIn("empty", p_ne.stderr.lower())

            target_dir = root / "real-dir"
            target_dir.mkdir()
            sym_state = root / "sym-state"
            sym_state.symlink_to(target_dir)
            p_sym = subprocess.run(
                [
                    sys.executable,
                    str(script),
                    "--remote",
                    "qualdrive",
                    "--state-dir",
                    str(sym_state),
                    "--rclone-config",
                    str(cfg),
                    "--dry-run",
                ],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(p_sym.returncode, 0)
            self.assertIn("symlink", p_sym.stderr.lower())


if __name__ == "__main__":
    unittest.main()
