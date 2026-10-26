import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from gtt_router.api import dispatch
from gtt_router.controls import control_paths, require_interface, DispatchBlocked
from gtt_router.mcp import TOOLS
from gtt_router.models import WorkerFailure, now_ms
from gtt_router.observations import load_observation
from gtt_router.service import RouterService
from gtt_router.group_contract import GroupRequest
from gtt_router.groups import GroupJob
from tests.test_router import FakeCatalog, response, route


class ControlsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.flag = self.root / "external-interface.json"
        self.services = []

    def tearDown(self):
        for service in self.services:
            service.shutdown()
        self.temp.cleanup()

    def flag_value(self, enabled):
        self.flag.write_text(json.dumps({"schema_version": 1, "enabled": enabled}), encoding="utf-8")

    def service(self, adapter=None, routes=None):
        service = RouterService({"state_dir": str(self.root / str(len(self.services))),
            "recovery": {"enabled": False}, "limits": {"global_parallel": 2, "combined_parallel": 4,
                "max_active_jobs": 2, "external_queue_grace_s": .1}},
            FakeCatalog(routes or [route()]), {"fake": adapter or (lambda *a: response())},
            interface_paths=[self.flag])
        self.services.append(service)
        return service

    def monitor(self, service, workflow="default", **kwargs):
        service.sync_codex_agents("monitor", [{"id": "real-fixture-luna", "model": "gpt-6-luna",
            "status": "running", "role": "supervisor", "workflow_id": workflow, **kwargs}], 600)

    def submit(self, service, **kwargs):
        return service.submit({"kind": "extract", "prompt": "bounded fixture", **kwargs})["job_id"]

    def wait(self, service, job):
        until = time.monotonic() + 4
        while time.monotonic() < until:
            if service.status(job)["state"] in {"failed", "partial", "succeeded", "needs_codex"}:
                return service.store.job(job)
            time.sleep(.01)
        self.fail("job did not finish")

    def usage_count(self, service):
        return service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0]

    def test_switch_missing_legacy_and_strict_formats(self):
        require_interface([self.flag])
        self.assertFalse(self.flag.exists())
        self.flag.write_bytes(b'\xef\xbb\xbf{"schema_version":1,"enabled":true}')
        require_interface([self.flag])
        for data in [b"", b"{}", b'{"schema_version":true,"enabled":true}',
            b'{"schema_version":1,"enabled":1}', b'{"schema_version":2,"enabled":true}',
            b'{"schema_version":1,"enabled":true,"unknown":0}',
            b'{"schema_version":1,"enabled":false,"enabled":true}', b" " * 4097, b"\xff"]:
            with self.subTest(data=data[:80]):
                self.flag.write_bytes(data)
                with self.assertRaisesRegex(DispatchBlocked, "settings_invalid"):
                    require_interface([self.flag])

    def test_global_switch_cannot_be_bypassed_by_custom_ledger(self):
        self.flag_value(False)
        custom = self.root / "custom" / "ledger.db"
        with patch.dict(os.environ, {"GTT_DATA_DIR": str(self.root)}):
            paths = control_paths({"ledger_path": str(custom)})
        self.assertEqual(paths[0], self.flag.resolve())
        self.assertEqual(paths[1], custom.resolve().parent / self.flag.name)
        with self.assertRaisesRegex(DispatchBlocked, "external_interface_disabled"):
            require_interface(paths)

    def test_legacy_root_matches_native_when_current_directory_has_no_ledger(self):
        current, legacy = self.root / ".globaltokentracker", self.root / ".codeledger"
        current.mkdir()
        legacy.mkdir()
        (legacy / "ledger.db").touch()
        config = {"ledger_path": str(self.root / "custom.db")}
        with patch.dict(os.environ, {}, clear=True), patch("gtt_router.controls.Path.home", return_value=self.root):
            self.assertEqual(control_paths(config)[0], legacy.resolve() / self.flag.name)
            (current / "ledger.db").touch()
            self.assertEqual(control_paths(config)[0], current.resolve() / self.flag.name)

    def test_production_gate_tracks_ledger_config_reload(self):
        old_ledger = self.root / "old" / "ledger.db"
        new_ledger = self.root / "new" / "ledger.db"
        new_ledger.parent.mkdir()
        (new_ledger.parent / self.flag.name).write_text('{"schema_version":1,"enabled":false}')
        config = {"schema_version": 1, "state_dir": str(self.root / "service"),
            "ledger_path": str(old_ledger), "benchmark_file": str(self.root / "benchmarks.json"),
            "sources": [], "recovery": {"enabled": False}, "limits": {"max_active_jobs": 1}}
        path = self.root / "config.json"
        config["_config_path"] = str(path)
        service = RouterService(config, FakeCatalog([route()]), {"fake": lambda *a: response()})
        self.services.append(service)
        with patch.dict(os.environ, {"GTT_DATA_DIR": str(self.root)}):
            service.require_external_interface()
            config = {**config, "ledger_path": str(new_ledger)}
            path.write_text(json.dumps(config))
            with self.assertRaisesRegex(DispatchBlocked, "external_interface_disabled"):
                service.ensure_catalog(force=True)
            with self.assertRaisesRegex(DispatchBlocked, "external_interface_disabled"):
                service.require_external_interface()

    def test_off_blocks_every_mcp_operation_and_reopen_restores_reads(self):
        service = self.service()
        self.flag_value(False)
        for tool in TOOLS:
            with self.subTest(method=tool["name"]):
                with self.assertRaisesRegex(DispatchBlocked, "external_interface_disabled"):
                    dispatch(service, tool["name"], {})
        self.flag_value(True)
        self.assertIn("routes", dispatch(service, "router_inventory", {}))
        self.assertEqual(self.usage_count(service), 0)

    def test_off_blocks_observation_and_sqlite_fallback(self):
        self.flag_value(False)
        config = {"ledger_path": str(self.root / "ledger.db"), "gtt_interface": {"allow_legacy_fallback": True}}
        with patch.dict(os.environ, {"GTT_DATA_DIR": str(self.root)}), \
                patch("gtt_router.observations.bounded_call") as cli, \
                patch("gtt_router.observations.snapshot") as legacy:
            with self.assertRaisesRegex(DispatchBlocked, "external_interface_disabled"):
                load_observation(config)
        cli.assert_not_called()
        legacy.assert_not_called()

    def test_monitor_is_required_for_both_submission_types(self):
        service = self.service()
        with self.assertRaisesRegex(DispatchBlocked, "luna_supervisor_required"):
            self.submit(service)
        with self.assertRaisesRegex(DispatchBlocked, "luna_supervisor_required"):
            service.groups.submit({"topic": "bounded", "needs_web": False})
        self.assertFalse(service.jobs)
        self.assertFalse(service.groups.jobs)

    def test_worker_wrong_workflow_idle_finished_expired_are_not_supervisors(self):
        service = self.service()
        for kwargs in [{"role": "worker"}, {"workflow_id": "other"}, {"status": "idle"}, {"status": "finished"}]:
            with self.subTest(kwargs=kwargs):
                self.monitor(service, **kwargs)
                with self.assertRaisesRegex(DispatchBlocked, "luna_supervisor_required"):
                    self.submit(service)
        self.monitor(service)
        service.codex_agents["monitor"]["expires_at"] = now_ms() - 1
        with self.assertRaisesRegex(DispatchBlocked, "luna_supervisor_required"):
            self.submit(service)

    def test_registration_rejects_non_luna_or_missing_scope(self):
        service = self.service()
        for kwargs in [{"model": "gpt-6-sol"}, {"workflow_id": ""}, {"id": ""}, {"role": "observer"}]:
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                self.monitor(service, **kwargs)

    def test_monitor_does_not_claim_worker_coverage_and_verifier_keeps_workflow(self):
        service = self.service()
        self.monitor(service, "non-default", work_key="review")
        job = self.submit(service, workflow_id="non-default", work_key="review", verify=True, max_attempts=2)
        data = self.wait(service, job)
        self.assertEqual(data["state"], "succeeded")
        self.assertEqual(len(data["attempts"]), 2)

    def test_expired_monitor_blocks_queued_call_without_billing(self):
        entered = []
        service = self.service(lambda *a: entered.append(True) or response())
        self.monitor(service)
        service.limits["source_parallel"] = {"fast": 0}
        job = self.submit(service, allow_fallback=False)
        service.codex_agents["monitor"]["expires_at"] = now_ms() - 1
        data = self.wait(service, job)
        self.assertEqual(data["results"][0]["error"]["code"], "luna_supervisor_required")
        self.assertFalse(entered)
        self.assertEqual(data["tokens_used"], 0)
        self.assertEqual(self.usage_count(service), 0)

    def test_switch_blocks_final_dispatch_after_reservation_without_charging(self):
        entered = []
        service = self.service(lambda *a: entered.append(True) or response())
        self.monitor(service)
        save = service._save
        def close_on_reserved_attempt(job):
            if job.attempts:
                self.flag_value(False)
            return save(job)
        service._save = close_on_reserved_attempt
        data = self.wait(service, self.submit(service))
        self.assertFalse(entered)
        self.assertEqual(data["tokens_used"], 0)
        self.assertEqual(data["tokens_reserved"], 0)
        self.assertEqual(data["attempt_count"], 0)
        self.assertFalse(data["attempts"])
        self.assertEqual(self.usage_count(service), 0)
        self.assertEqual(service.store.health("route:" + route().id)["failure_streak"], 0)

    def test_switch_blocks_retry_but_retains_first_call_usage(self):
        entered = []
        def fail(*a):
            entered.append(True)
            self.flag_value(False)
            raise WorkerFailure("provider_request_failed", usage={"total_tokens": 17})
        service = self.service(fail, [route("first"), route("second")])
        self.monitor(service)
        data = self.wait(service, self.submit(service))
        self.assertEqual(len(entered), 1)
        self.assertEqual(data["tokens_used"], 17)
        self.assertEqual(data["results"][0]["error"]["code"], "external_interface_disabled")
        self.assertNotIn("codex_fallback", data["results"][0])

    def test_inflight_finishes_and_bills_once_after_switch_off(self):
        entered, finish = threading.Event(), threading.Event()
        def adapter(*a):
            entered.set()
            finish.wait(2)
            return response()
        service = self.service(adapter)
        self.monitor(service)
        job = self.submit(service, verify=True, max_attempts=2)
        self.assertTrue(entered.wait(1))
        self.flag_value(False)
        finish.set()
        data = self.wait(service, job)
        self.assertEqual(data["state"], "partial")
        self.assertEqual(data["tokens_used"], 100)
        self.assertEqual(len(data["attempts"]), 1)
        self.assertEqual(self.usage_count(service), 1)

    def test_recovery_requires_monitor_and_switch(self):
        service = self.service(lambda *a: response("GTT_RECOVERY_OK"))
        r = service.catalog.routes[0]
        self.assertEqual(service.recovery.recheck(r.id)["reason"], "luna_supervisor_required")
        self.monitor(service, "probe-workflow")
        self.assertEqual(service.recovery.recheck(r.id)["reason"], "luna_supervisor_required")
        self.flag_value(False)
        self.assertEqual(service.recovery.tick()["reason"], "external_interface_disabled")
        self.assertEqual(self.usage_count(service), 0)
        self.flag_value(True)
        result = service.recovery.recheck(r.id, "probe-workflow")
        self.assertTrue(result["success"])
        self.assertEqual(self.usage_count(service), 1)

    def test_recovery_revalidates_before_adapter_without_penalty(self):
        entered = []
        service = self.service(lambda *a: entered.append(True) or response("GTT_RECOVERY_OK"))
        self.monitor(service)
        original = service.ensure_catalog
        def close_during_plan(*a):
            original(*a)
            self.flag_value(False)
        service.ensure_catalog = close_during_plan
        result = service.recovery.recheck(service.catalog.routes[0].id)
        self.assertEqual(result["reason"], "external_interface_disabled")
        self.assertFalse(entered)
        self.assertEqual(self.usage_count(service), 0)
        self.assertEqual(service.total_inflight, 0)
        self.assertEqual(service.store.health("route:" + route().id)["failure_streak"], 0)

    def test_monitor_uses_real_shared_capacity(self):
        service = self.service()
        self.monitor(service)
        service.limits["combined_parallel"] = 1
        self.assertEqual(service.recovery.recheck(route().id)["reason"], "foreground_busy")

    def test_group_controls_block_before_changes_and_rollback_late_denial(self):
        service = self.service()
        self.monitor(service)
        for action in ("extend", "adjust_seats", "resume"):
            for late in (False, True):
                with self.subTest(action=action, late=late):
                    self.flag_value(True)
                    job = GroupJob(GroupRequest.parse({"topic": "fixture", "needs_web": False}))
                    job.state = "needs_codex"
                    job.pending_action = {"type": {"extend": "request_extension", "adjust_seats": "request_seat_change", "resume": "round_limit"}[action]}
                    service.groups.jobs[job.id] = job
                    service._save(job)
                    before = (job.task.token_budget, job.member_count, job.roles, job.next_tasks,
                              job.phase, job.request.max_rounds, job.task.max_attempts, job.revision)
                    if late:
                        def deny(*args):
                            raise DispatchBlocked("external_interface_disabled")
                        gate = patch.object(service.groups, "_schedule_resume", side_effect=deny)
                    else:
                        self.flag_value(False)
                        gate = patch.object(service.groups, "_schedule_resume", wraps=service.groups._schedule_resume)
                    with gate, self.assertRaisesRegex(DispatchBlocked, "external_interface_disabled"):
                        if action == "extend":
                            service.groups.extend(job.id, "fixture", 60000000, True)
                        elif action == "adjust_seats":
                            with patch.object(service.groups, "plan", return_value={"roles": []}), \
                                    patch.object(service.groups, "_seed_tasks", return_value=[]):
                                service.groups.adjust_seats(job.id, 4, "fixture", True)
                        else:
                            service.groups.resume(job.id, "fixture")
                    after = (job.task.token_budget, job.member_count, job.roles, job.next_tasks,
                             job.phase, job.request.max_rounds, job.task.max_attempts, job.revision)
                    self.assertEqual(before, after)
                    self.assertEqual(job.state, "needs_codex")

    def test_mcp_advertises_supervisor_role_and_recheck_workflow(self):
        schemas = {t["name"]: t["inputSchema"] for t in TOOLS}
        role = schemas["router_sync_codex_agents"]["properties"]["agents"]["items"]["properties"]["role"]
        self.assertEqual(role["enum"], ["worker", "supervisor"])
        self.assertIn("workflow_id", schemas["router_recheck"]["properties"])

    def test_supervisor_status_shows_only_live_same_workflow_monitor(self):
        service = self.service()
        self.monitor(service, "round")
        info = service.coordination.supervisor("status", "round")["supervision"]
        self.assertFalse(info["identity_verified"])
        self.assertEqual(info["active_monitors"][0]["id"], "real-fixture-luna")
        self.assertFalse(service.coordination.supervisor("status", "other")["supervision"]["active_monitors"])
        service.codex_agents["monitor"]["expires_at"] = now_ms() - 1
        self.assertFalse(service.coordination.supervisor("status", "round")["supervision"]["active_monitors"])


if __name__ == "__main__":
    unittest.main()
