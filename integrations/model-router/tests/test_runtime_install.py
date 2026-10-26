"""Maintenance outcomes in temporary directories with process/HTTP doubles."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("runtime_installer", Path(__file__).resolve().parents[1] / "scripts/install_rust_runtime.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class RuntimeInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.state, self.install, self.skill, self.package = [self.root / name for name in ("state", "install", "skill", "package")]
        for path in (self.state, self.install, self.skill / "references", self.package / "model-router", self.package / "skill/references"):
            path.mkdir(parents=True)
        self.config, self.mcp, self.guard = self.state / "config.json", self.root / "config.toml", self.root / "interface.json"
        self.config.write_text(json.dumps({"schema_version": 1, "state_dir": str(self.state), "custom": {"keep": 42}}))
        self.mcp.write_text('[mcp_servers.gtt-router]\ncommand="python"\nargs=' + json.dumps([str(self.root / "old-router.py"), "mcp", "--config", str(self.config)]) + '\n[mcp_servers.unrelated]\nargs=["keep"]\n')
        (self.skill / "SKILL.md").write_text("existing skill")
        for name, body in {"model-router/router.py": "fixture", "gtt-scheduler-kernel.exe": "fixture binary",
                "skill/SKILL.md": "updated skill", "skill/references/rust-runtime.md": "reference"}.items():
            (self.package / name).write_text(body)
        connection = sqlite3.connect(self.state / "router-state.sqlite")
        connection.execute("CREATE TABLE retained(value)")
        connection.execute("INSERT INTO retained VALUES(42)")
        connection.commit()
        connection.close()
        self.plan = self.root / "plan.json"
        data = {"package": str(self.package), "install": str(self.install), "state": str(self.state), "config": str(self.config),
            "mcp": str(self.mcp), "skill": str(self.skill), "guards": [str(self.guard)], "runtime_directory_exists": False,
            "expected": {str(p): installer.sha(p) for p in (self.config, self.mcp, self.skill / "SKILL.md", self.skill / "references/rust-runtime.md", self.install / "gtt-scheduler-kernel.exe")},
            "manifest": {str(p.relative_to(self.package)).replace("\\", "/"): installer.sha(p) for p in self.package.rglob("*") if p.is_file()}}
        self.plan.write_text(json.dumps(data))
        self.record = {"jobs": 1, "active_jobs": 0, "usage_records": 2, "dispatches": 1, "operator_operations": 0, "visual_runs": 0}
        self.health_count = 0
        self.actions = []

    def tearDown(self):
        self.temp.cleanup()

    def rpc(self, state, method=None, params=None):
        if method:
            self.actions.append(params["action"])
            return {"routes": [1], "settings": {"revision": 22, "enabled": True, "mode": "directed", "pool": [1, 2]}}
        self.health_count += 1
        return {"pid": 100 + self.health_count, "version": "0.6.0" if self.health_count == 1 else "0.7.0",
            "state_owner": "rust", "scheduler_kernel_alive": True}

    def execute(self, fail_start=False):
        replies = [subprocess.CompletedProcess([], 1), subprocess.CompletedProcess([], 0)] if fail_start else None
        with patch.object(installer, "rpc", side_effect=self.rpc), patch.object(installer, "counts", return_value=self.record), \
             patch.object(installer, "stop_daemon") as stop, patch.object(installer.subprocess, "run", side_effect=replies, return_value=subprocess.CompletedProcess([], 0)), \
             contextlib.redirect_stdout(io.StringIO()):
            if fail_start:
                with self.assertRaises(RuntimeError):
                    installer.activate(self.plan)
            else:
                installer.activate(self.plan)
        return stop

    def test_install_preserves_other_settings_and_restores_absent_guard(self):
        stop = self.execute()
        self.assertEqual(stop.call_count, 1)
        config = json.loads(self.config.read_text())
        self.assertEqual(config["schema_version"], 2)
        self.assertEqual(config["custom"], {"keep": 42})
        mcp = tomllib.loads(self.mcp.read_text())
        self.assertEqual(mcp["mcp_servers"]["unrelated"], {"args": ["keep"]})
        self.assertEqual(Path(mcp["mcp_servers"]["gtt-router"]["args"][0]), self.install / "model-router/router.py")
        self.assertFalse(self.guard.exists())
        self.assertEqual(self.actions, ["refresh"])

    def test_failed_start_restores_configuration_and_keeps_durable_database(self):
        original = self.config.read_bytes(), self.mcp.read_bytes()
        self.execute(fail_start=True)
        self.assertEqual((self.config.read_bytes(), self.mcp.read_bytes()), original)
        self.assertFalse((self.install / "model-router").exists())
        self.assertTrue(list(self.install.glob("model-router.failed-*")))
        self.assertFalse(self.guard.exists())
        connection = sqlite3.connect(self.state / "router-state.sqlite")
        try:
            self.assertEqual(connection.execute("SELECT value FROM retained").fetchone()[0], 42)
        finally:
            connection.close()

    def test_disabled_interface_remains_disabled_and_does_not_refresh_sources(self):
        original = b'{"schema_version":1,"enabled":false}'
        self.guard.write_bytes(original)
        self.execute()
        self.assertEqual(self.guard.read_bytes(), original)
        self.assertEqual(self.actions, ["get"])

    def test_active_work_prevents_replacement_and_restores_switch(self):
        self.record["active_jobs"] = 1
        before = self.config.read_bytes()
        with patch.object(installer, "counts", return_value=self.record), patch.object(installer, "stop_daemon") as stop, self.assertRaises(RuntimeError):
            installer.activate(self.plan)
        stop.assert_not_called()
        self.assertEqual(self.config.read_bytes(), before)
        self.assertFalse((self.install / "model-router").exists())
        self.assertFalse(self.guard.exists())
