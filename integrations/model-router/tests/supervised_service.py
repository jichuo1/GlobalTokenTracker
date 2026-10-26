"""Explicit synthetic monitor for adapter/routing tests, never a product bypass.

Tests of the supervision gate must instantiate the real RouterService instead.
"""
from pathlib import Path
from gtt_router.service import RouterService


class SupervisedService(RouterService):
    def __init__(self, config, *args, **kwargs):
        kwargs.setdefault("interface_paths", [Path(config["state_dir"]) / "external-interface.json"])
        super().__init__(config, *args, **kwargs)
        self.register_fixture_monitor("default")

    def register_fixture_monitor(self, workflow_id):
        self.sync_codex_agents("fixture-monitor:" + workflow_id, [{"id": "fixture-luna:" + workflow_id,
            "model": "gpt-6-luna", "role": "supervisor", "status": "running", "workflow_id": workflow_id}], lease_s=600)

    def require_supervisor(self, workflow_id):
        if not self._active_supervisors(workflow_id):
            self.register_fixture_monitor(workflow_id)
        super().require_supervisor(workflow_id)
