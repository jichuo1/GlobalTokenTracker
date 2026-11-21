#!/usr/bin/env python3
"""Integrity gates for CI workflows and repository commit-msg hook (stdlib only)."""

from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
COMMIT_HOOK = REPO_ROOT / ".githooks" / "commit-msg"


class WorkflowGateIntegrityTests(unittest.TestCase):
    def read_workflow(self, name: str) -> str:
        return (WORKFLOWS / name).read_text(encoding="utf-8")

    def test_no_obsolete_depinfo_artifact_upload(self) -> None:
        for name in ("verify-branch.yml", "alpha-release.yml", "stable-release.yml"):
            text = self.read_workflow(name)
            self.assertNotIn("target/debug/deps/*.d", text, f"obsolete .d upload in {name}")

    def test_no_duplicate_setup_release_compile_before_package(self) -> None:
        for name in ("verify-branch.yml", "alpha-release.yml", "stable-release.yml"):
            text = self.read_workflow(name)
            self.assertNotIn(
                "-p globaltokentracker-setup",
                text,
                f"duplicate setup build before package.ps1 in {name}",
            )
            self.assertIn(
                "cargo build --release --locked -p globaltokentracker-ui -p globaltokentracker-cli",
                text,
                f"missing release payload build in {name}",
            )

    def test_release_and_dmg_builds_are_gated_by_static_and_unit_checks(self) -> None:
        for name in ("verify-branch.yml", "alpha-release.yml", "stable-release.yml"):
            text = self.read_workflow(name)
            self.assertIn(
                "steps.fmt.outcome == 'success' && steps.clippy.outcome == 'success' && steps.unit_tests.outcome == 'success'",
                text,
                f"Release build not gated by fmt/clippy/unit_tests in {name}",
            )
            self.assertIn(
                "steps.macos_clippy.outcome == 'success' && steps.macos_tests.outcome == 'success'",
                text,
                f"macOS universal build not gated by clippy/tests in {name}",
            )
            self.assertIn(
                "steps.rust_toolchain.outcome == 'success'",
                text,
                f"non-short-circuiting cargo steps must require rust_toolchain in {name}",
            )

    def test_paths_ignore_never_hides_release_templates(self) -> None:
        for name in ("verify-branch.yml", "alpha-release.yml"):
            text = self.read_workflow(name)
            self.assertNotRegex(
                text,
                r"-\s*['\"]?\*\*/\*\.md['\"]?",
                f"blanket **/*.md in {name} would skip .github/release-templates/*.md",
            )

    def test_stable_release_guards_concurrency_and_publish_tag(self) -> None:
        text = self.read_workflow("stable-release.yml")
        self.assertIn("cancel-in-progress: ${{ !inputs.publish_stable }}", text)
        self.assertIn(
            "inputs.publish_stable == true && needs.verify.outputs.release_tag == inputs.release_tag",
            text,
        )

    def test_verify_branch_runs_release_tooling_model_router_and_pull_requests(self) -> None:
        text = self.read_workflow("verify-branch.yml")
        self.assertIn("pull_request:", text)
        self.assertIn(".github/release-templates", text)
        self.assertIn("working-directory: integrations/model-router", text)
        self.assertIn("unittest discover", text)


class CommitMsgHookTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        bash = shutil.which("bash")
        if not bash:
            git = shutil.which("git")
            if git:
                candidate = Path(git).resolve().parent.parent / "bin" / "bash.exe"
                if candidate.is_file():
                    bash = str(candidate)
        cls.bash = bash

    def run_hook_batch(self, messages: list[str]) -> list[int]:
        if not self.bash:
            self.skipTest("bash not available")
        with tempfile.TemporaryDirectory(prefix="gtt-hook-") as tmp:
            paths = []
            for idx, message in enumerate(messages):
                msg = Path(tmp) / f"COMMIT_EDITMSG_{idx}"
                msg.write_text(message, encoding="utf-8")
                paths.append(msg.as_posix())
            script = (
                'hook="$1"; shift; '
                'for f in "$@"; do "$hook" "$f" >/dev/null 2>&1; printf "%d\\n" "$?"; done'
            )
            res = subprocess.run(
                [self.bash, "-c", script, "_", COMMIT_HOOK.as_posix(), *paths],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
            )
            return [int(line) for line in res.stdout.splitlines()]

    def test_allows_adapter_names_in_normal_commit_messages(self) -> None:
        messages = [
            "feat(antigravity): add gemini and codex speed metrics\n",
            "fix(core): reconcile claude and cursor token billing\n",
            "refactor(ci): prune obsolete workflow gates\n",
        ]
        codes = self.run_hook_batch(messages)
        for msg, code in zip(messages, codes):
            self.assertEqual(code, 0, f"unexpected rejection for {msg!r}")

    def test_rejects_ai_attribution_trailers(self) -> None:
        messages = [
            "feat: something\n\nGenerated with Claude Code\n",
            "feat: something\n\nCo-Authored-By: Gemini <noreply@google.com>\n",
            "feat: something\n\nCo-Authored-By: Codex <codex@openai.com>\n",
            "feat: something\n\nCo-Authored-By: Antigravity Bot <bot@example.com>\n",
            "feat: something\n\nSigned-off-by: Copilot <noreply@github.com>\n",
        ]
        codes = self.run_hook_batch(messages)
        for msg, code in zip(messages, codes):
            self.assertNotEqual(code, 0, f"expected rejection for {msg!r}")


if __name__ == "__main__":
    unittest.main()
