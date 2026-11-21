#!/usr/bin/env python3
"""Tests for validate_build.py (standard library only)."""

from __future__ import annotations

import contextlib
import io
import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

import validate_build

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / ".github" / "release-templates" / "validate_build.py"
CARGO = REPO_ROOT / "Cargo.toml"

FIXTURE = """\
[workspace]
members = ["crates/*"]

[workspace.package]
version = "1.4.2"
"""


@dataclass
class RunResult:
    returncode: int
    stdout: str
    stderr: str


def run_validate(*args: str) -> RunResult:
    stdout = io.StringIO()
    stderr = io.StringIO()
    returncode = 0
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        try:
            validate_build.main(list(args))
        except SystemExit as exc:
            code = exc.code
            if code is None or code == 0:
                returncode = 0
            elif isinstance(code, int):
                returncode = code
            else:
                stderr.write(f"{code}\n")
                returncode = 1
    return RunResult(returncode=returncode, stdout=stdout.getvalue(), stderr=stderr.getvalue())


class ValidateBuildTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls.cargo = Path(cls._tmp.name) / "Cargo.toml"
        cls.cargo.write_text(FIXTURE, encoding="utf-8")

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def test_reads_real_workspace_version(self) -> None:
        # Exercise the real CLI entrypoint once via subprocess.
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--cargo-toml",
                str(CARGO),
                "--channel",
                "stable",
                "--release-tag",
                "v0.0.0",
            ],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        # Fails tag-mismatch, not parse — proving the version was read.
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("workspace version not found", result.stderr)

    def test_stable_tag_must_match_workspace_version(self) -> None:
        ok = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "stable",
            "--release-tag", "v1.4.2",
        )
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertIn("release_tag=v1.4.2", ok.stdout)
        bad = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "stable",
            "--release-tag", "v1.4.3",
        )
        self.assertNotEqual(bad.returncode, 0)

    def test_stable_tag_rejects_prerelease_suffix(self) -> None:
        bad = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "stable",
            "--release-tag", "v1.4.2-alpha.1",
        )
        self.assertNotEqual(bad.returncode, 0)

    def test_alpha_tag_must_match_base_version(self) -> None:
        ok = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "alpha",
            "--release-tag", "v1.4.2-alpha.3",
        )
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertIn("GlobalTokenTracker-Setup-1.4.2-win-x64.exe", ok.stdout)
        bad = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "alpha",
            "--release-tag", "v1.5.0-alpha.1",
        )
        self.assertNotEqual(bad.returncode, 0)

    def test_alpha_rejects_stable_format(self) -> None:
        bad = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "alpha",
            "--release-tag", "v1.4.2",
        )
        self.assertNotEqual(bad.returncode, 0)

    def test_missing_tag_fails_for_stable(self) -> None:
        bad = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "stable",
        )
        self.assertNotEqual(bad.returncode, 0)

    def test_empty_alpha_tag_is_build_only(self) -> None:
        ok = run_validate(
            "--cargo-toml", str(self.cargo),
            "--channel", "alpha",
        )
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertIn("version_name=1.4.2", ok.stdout)
        self.assertNotIn("release_tag=", ok.stdout)


if __name__ == "__main__":
    unittest.main()
