#!/usr/bin/env python3
"""Unit and safety-guardrail tests for the FolderSync qualification harness."""

from __future__ import annotations

import json
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

        p2 = subprocess.run(
            [
                sys.executable,
                str(script),
                "--remote",
                "qualdrive",
                "--state-dir",
                "/tmp/qual-safe-state",
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
            self.assertIn("drive_three_client_same_file_conflict", plan["scenarios"])

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


if __name__ == "__main__":
    unittest.main()
