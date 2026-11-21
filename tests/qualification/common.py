#!/usr/bin/env python3
"""Shared helpers for FolderSync qualification harnesses (plan §15, §20)."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import random
import re
import subprocess
import sys
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
MANIFEST_PATH = REPO_ROOT / "packaging" / "qualification-manifest.toml"


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(65536)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def host_platform_key() -> str:
    system = platform.system()
    machine = platform.machine()
    mapping = {
        ("Darwin", "arm64"): "osx-arm64",
        ("Darwin", "x86_64"): "osx-amd64",
        ("Linux", "x86_64"): "linux-amd64",
    }
    key = mapping.get((system, machine))
    if key is None:
        raise RuntimeError(f"Unsupported host platform for pinned rclone: {system}-{machine}")
    return key


def parse_qualification_manifest(manifest_path: Path = MANIFEST_PATH) -> dict[str, Any]:
    """Minimal parser for packaging/qualification-manifest.toml (no external deps)."""
    data: dict[str, Any] = {}
    section: str | None = None
    for raw_line in manifest_path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip()
            data.setdefault(section, {})
            continue
        m = re.match(r'^([A-Za-z0-9_.-]+)\s*=\s*"([^"]*)"$', line)
        if m:
            key, val = m.group(1), m.group(2)
            if section is None:
                data[key] = val
            else:
                data[section][key] = val
    return data


def resolve_verified_pinned_rclone(manifest_path: Path = MANIFEST_PATH) -> tuple[Path, str, str]:
    """Returns (binary_path, version, verified_sha256) or raises RuntimeError."""
    manifest = parse_qualification_manifest(manifest_path)
    version = manifest["rclone"]["version"]
    plat = host_platform_key()
    expected_sha = manifest[f"rclone.{plat}"]["binary_sha256"]

    override = os.environ.get("FOLDERSYNC_RCLONE")
    candidate = Path(override) if override else (REPO_ROOT / ".tools" / "rclone" / version / "rclone")
    if not candidate.is_file():
        raise RuntimeError(
            f"Pinned rclone {version} not found at {candidate}. Run scripts/fetch-rclone.sh first."
        )
    resolved = candidate.resolve()
    actual_sha = sha256_file(resolved)
    if actual_sha != expected_sha:
        raise RuntimeError(
            f"Pinned rclone SHA-256 mismatch at {resolved}: expected {expected_sha}, got {actual_sha}"
        )
    return resolved, version, actual_sha


def get_tool_versions(sync_cli: Path, rclone_bin: Path, rclone_version: str, rclone_sha256: str) -> dict[str, str]:
    def capture(cmd: list[str]) -> str:
        try:
            out = subprocess.check_output(cmd, stderr=subprocess.STDOUT, text=True, timeout=15)
            return out.strip().splitlines()[0] if out.strip() else "unknown"
        except Exception as exc:  # noqa: BLE001
            return f"unavailable ({exc})"

    return {
        "sync_cli": capture([str(sync_cli), "--version"]),
        "rclone_version": rclone_version,
        "rclone_sha256": rclone_sha256,
        "rclone_path": str(rclone_bin),
        "rustc": capture(["rustc", "--version"]),
        "git_sha": capture(["git", "-C", str(REPO_ROOT), "rev-parse", "--short=12", "HEAD"]),
        "os": f"{platform.system()} {platform.release()} ({platform.machine()})",
        "python": platform.python_version(),
    }


def is_forbidden_real_state_dir(path: Path) -> bool:
    """Refuses the real macOS Application Support FolderSync state directory."""
    real_home = Path.home().resolve()
    forbidden = (real_home / "Library" / "Application Support" / "FolderSync").resolve()
    try:
        candidate = path.expanduser().resolve()
    except OSError:
        candidate = path.expanduser().absolute()
    return candidate == forbidden or forbidden in candidate.parents or candidate in forbidden.parents and candidate == forbidden


def is_forbidden_rclone_config(path: Path) -> bool:
    """Refuses ~/.config/rclone or any file beneath it."""
    real_home = Path.home().resolve()
    forbidden_dir = (real_home / ".config" / "rclone").resolve()
    try:
        candidate = path.expanduser().resolve()
    except OSError:
        candidate = path.expanduser().absolute()
    return candidate == forbidden_dir or forbidden_dir in candidate.parents


class TruthSet:
    """Tracks uniquely identifiable generated versions kept OUTSIDE all sync trees."""

    def __init__(self, truth_dir: Path, seed: int) -> None:
        self.truth_dir = truth_dir
        self.truth_dir.mkdir(parents=True, exist_ok=True)
        self.rng = random.Random(seed)
        self.seed = seed
        self.records: dict[str, dict[str, Any]] = {}

    def generate_version(
        self,
        label: str,
        rel_path: str,
        size_bytes: int = 256,
        binary: bool = False,
    ) -> tuple[bytes, str]:
        """Generates deterministic uniquely identifiable bytes and saves a truth copy outside sync trees."""
        header = f"FOLDERSYNC-QUAL|seed={self.seed}|label={label}|path={rel_path}\n".encode("utf-8")
        remaining = max(0, size_bytes - len(header))
        if binary:
            body = self.rng.randbytes(remaining)
        else:
            alphabet = "abcdefghijklmnopqrstuvwxyz0123456789-_"
            chars = "".join(self.rng.choice(alphabet) for _ in range(remaining))
            body = (chars + "\n").encode("utf-8")[:remaining]
        payload = header + body
        digest = sha256_bytes(payload)

        safe_name = re.sub(r"[^A-Za-z0-9_.-]+", "_", f"{label}__{rel_path}")
        truth_file = self.truth_dir / safe_name
        truth_file.write_bytes(payload)

        self.records[label] = {
            "label": label,
            "rel_path": rel_path,
            "sha256": digest,
            "bytes": len(payload),
            "truth_file": str(truth_file),
        }
        return payload, digest

    def write_into(
        self,
        root: Path,
        rel_path: str,
        label: str,
        size_bytes: int = 256,
        binary: bool = False,
    ) -> str:
        payload, digest = self.generate_version(label, rel_path, size_bytes=size_bytes, binary=binary)
        target = root / rel_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(payload)
        return digest

    def scan_tree_hashes(self, root: Path, ignore_names: set[str] | None = None) -> dict[str, str]:
        """Returns {relative_posix_path: sha256} for all regular files under root."""
        ignored = ignore_names or {"RCLONE_TEST"}
        result: dict[str, str] = {}
        if not root.exists():
            return result
        for path in sorted(root.rglob("*")):
            if path.is_symlink() or not path.is_file():
                continue
            if path.name in ignored:
                continue
            rel = path.relative_to(root).as_posix()
            result[rel] = sha256_file(path)
        return result

    def find_hash_locations(self, expected_sha: str, named_roots: dict[str, Path]) -> list[str]:
        """Finds every regular file across named_roots whose contents match expected_sha."""
        found: list[str] = []
        for name, root in named_roots.items():
            if not root.exists():
                continue
            for path in sorted(root.rglob("*")):
                if path.is_symlink() or not path.is_file():
                    continue
                if path.name == "RCLONE_TEST":
                    continue
                if sha256_file(path) == expected_sha:
                    found.append(f"{name}:{path.relative_to(root).as_posix()}")
        return found


@dataclass
class ScenarioEvidence:
    name: str
    description: str
    started_at: str
    finished_at: str
    injected_failure: str
    passed: bool
    conclusion: str
    commands: list[dict[str, Any]] = field(default_factory=list)
    outcomes: dict[str, Any] = field(default_factory=dict)
    hash_accounting: list[dict[str, Any]] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)


def write_evidence_files(
    json_path: Path,
    md_path: Path,
    suite_name: str,
    seed: int,
    started_at: str,
    finished_at: str,
    versions: dict[str, str],
    scenarios: list[ScenarioEvidence],
    overall_conclusion: str,
) -> None:
    json_path.parent.mkdir(parents=True, exist_ok=True)
    md_path.parent.mkdir(parents=True, exist_ok=True)

    all_passed = all(s.passed for s in scenarios)
    payload = {
        "suite": suite_name,
        "seed": seed,
        "started_at": started_at,
        "finished_at": finished_at,
        "versions": versions,
        "passed": all_passed,
        "overall_conclusion": overall_conclusion,
        "scenarios": [asdict(s) for s in scenarios],
    }
    json_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    lines: list[str] = [
        f"# FolderSync Qualification Evidence — {suite_name}",
        "",
        f"- **Started (UTC):** `{started_at}`",
        f"- **Finished (UTC):** `{finished_at}`",
        f"- **Deterministic seed:** `{seed}`",
        f"- **Overall result:** `{'PASS' if all_passed else 'FAIL'}`",
        f"- **Conclusion:** {overall_conclusion}",
        "",
        "## Versions & Environment",
        "",
        "| Component | Value |",
        "| --- | --- |",
    ]
    for k, v in versions.items():
        lines.append(f"| `{k}` | `{v}` |")

    lines.extend([
        "",
        "## Scenario Summary",
        "",
        "| Scenario | Result | Injected Failure | Conclusion |",
        "| --- | --- | --- | --- |",
    ])
    for s in scenarios:
        res = "PASS" if s.passed else "FAIL"
        lines.append(f"| `{s.name}` | `{res}` | {s.injected_failure} | {s.conclusion} |")

    for s in scenarios:
        lines.extend([
            "",
            f"### `{s.name}`",
            "",
            f"- **Description:** {s.description}",
            f"- **Timestamps (UTC):** `{s.started_at}` → `{s.finished_at}`",
            f"- **Injected failure:** {s.injected_failure}",
            f"- **Result:** `{'PASS' if s.passed else 'FAIL'}`",
            f"- **Conclusion:** {s.conclusion}",
        ])
        if s.outcomes:
            lines.append(f"- **Observed outcomes:** `{json.dumps(s.outcomes, sort_keys=True)}`")
        if s.notes:
            lines.append("- **Notes:**")
            for note in s.notes:
                lines.append(f"  - {note}")
        if s.hash_accounting:
            lines.extend([
                "",
                "| Truth Version | SHA-256 (prefix) | Accounted Locations | Verified |",
                "| --- | --- | --- | --- |",
            ])
            for h in s.hash_accounting:
                locs = ", ".join(f"`{loc}`" for loc in h["locations"]) if h["locations"] else "_none_"
                lines.append(
                    f"| `{h['label']}` (`{h['rel_path']}`) | `{h['sha256'][:16]}…` | {locs} | `{'yes' if h['verified'] else 'no'}` |"
                )

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
