"""Exercise the actual Stable publish shell step against private Git fixtures."""

from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
TAG = "v0.8.5"


class StableTagGuardTests(unittest.TestCase):
    @staticmethod
    def _git(cwd, *args):
        return subprocess.check_output(["git", *args], cwd=cwd, text=True, stderr=subprocess.STDOUT).strip()

    @classmethod
    def setUpClass(cls):
        cls._base_temp = tempfile.TemporaryDirectory(prefix="gtt-tag-guard-base-")
        base = Path(cls._base_temp.name)
        cls._base_work = base / "work"
        cls._base_work.mkdir()
        cls._base_remote = base / "origin.git"
        cls._git(cls._base_work, "init", "--bare", str(cls._base_remote))
        cls._git(cls._base_work, "init", "-b", "main")
        cls._git(cls._base_work, "config", "user.name", "Release test fixture")
        cls._git(cls._base_work, "config", "user.email", "fixture@example.invalid")
        cls._git(cls._base_work, "remote", "add", "origin", cls._base_remote.as_posix())
        (cls._base_work / "fixture.txt").write_text("old source", encoding="utf-8")
        cls._git(cls._base_work, "add", "fixture.txt")
        cls._git(cls._base_work, "commit", "-m", "test: old source fixture")
        cls.old_sha = cls._git(cls._base_work, "rev-parse", "HEAD")
        (cls._base_work / "fixture.txt").write_text("verified source", encoding="utf-8")
        cls._git(cls._base_work, "commit", "-am", "test: verified source fixture")
        cls.sha = cls._git(cls._base_work, "rev-parse", "HEAD")
        cls._git(cls._base_work, "push", "origin", "main")
        bash = shutil.which("bash")
        if not bash:
            git_path = Path(shutil.which("git")).resolve()
            candidate = git_path.parent.parent / "bin" / "bash.exe"
            if candidate.is_file():
                bash = str(candidate)
        if bash is None:
            raise AssertionError("Stable publish guard needs the Git Bash used by the workflow")
        cls.bash = bash
        workflow = (ROOT / ".github/workflows/stable-release.yml").read_text(encoding="utf-8")
        step = workflow.split("      - name: Create GitHub Stable Release\n", 1)[1]
        cls._publish_script = textwrap.dedent(step.split("        run: |\n", 1)[1])

    @classmethod
    def tearDownClass(cls):
        cls._base_temp.cleanup()

    def git(self, *args):
        return self._git(self.work, *args)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="gtt-tag-guard-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name) / "work"
        self.remote = Path(self.temp.name) / "origin.git"
        shutil.copytree(self._base_work, self.work)
        shutil.copytree(self._base_remote, self.remote)
        self.git("remote", "set-url", "origin", self.remote.as_posix())

    def publish_step(self):
        # Git is real; only the irreversible GitHub release command is replaced.
        shim = 'gh() { printf "GTT_RELEASE_CALLED %s\\n" "$*"; }\n'
        # Mirrors the step's declared env (the macOS disk image ships too).
        env = {**os.environ, "RELEASE_TAG": TAG, "GITHUB_SHA": self.sha,
               "GITHUB_REPOSITORY": "fixture/repository", "ASSET_FILENAME": "fixture.exe",
               "MAC_ASSET_FILENAME": "fixture.dmg"}
        return subprocess.run([self.bash, "-c", shim + self._publish_script], cwd=self.work, env=env,
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30)

    def test_new_tag_is_pushed_at_verified_source_before_release(self):
        result = self.publish_step()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("GTT_RELEASE_CALLED", result.stdout)
        self.assertIn("--verify-tag", result.stdout)
        self.assertIn("release-asset/fixture.dmg", result.stdout)
        self.assertEqual(self.git("ls-remote", "origin", "refs/tags/" + TAG).split()[0], self.sha)

    def test_matching_annotated_tag_is_peeled_to_verified_commit(self):
        self.git("tag", "-a", TAG, "-m", "fixture release", self.sha)
        self.git("push", "origin", "refs/tags/" + TAG)
        self.assertNotEqual(self.git("rev-parse", TAG), self.sha)
        result = self.publish_step()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("GTT_RELEASE_CALLED", result.stdout)

    def test_wrong_existing_tag_prevents_release_and_is_not_moved(self):
        self.git("tag", TAG, self.old_sha)
        self.git("push", "origin", "refs/tags/" + TAG)
        result = self.publish_step()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("Refusing mismatched tag", result.stdout)
        self.assertNotIn("GTT_RELEASE_CALLED", result.stdout)
        self.assertEqual(self.git("ls-remote", "origin", "refs/tags/" + TAG).split()[0], self.old_sha)


if __name__ == "__main__":
    unittest.main()
