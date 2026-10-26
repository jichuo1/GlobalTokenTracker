import copy
import json
from pathlib import Path
import tempfile
import unittest

import test_router as fixtures
from gtt_router.settings_merge import merge_settings, remember, read_history


class SettingsMergeTests(unittest.TestCase):
    def value(self):
        return {"revision": 24, "sources": {"zcode": True}, "pool": [{"route_id": "r", "expires_at": 100}],
                "workflows": [{"id": "w", "nodes": [{"id": "a", "task": {"prompt": "old", "max_output_tokens": 8192}}]}]}

    def test_ui_source_edit_and_background_pool_lease_merge_without_losing_draft(self):
        base = self.value()
        local, remote = copy.deepcopy(base), copy.deepcopy(base)
        local["sources"]["codex"] = True
        remote["pool"][0]["expires_at"] = 200
        remote["revision"] = 25
        before = copy.deepcopy(local)
        result = merge_settings(base, local, remote)
        self.assertTrue(result["sources"]["codex"])
        self.assertEqual(result["pool"][0]["expires_at"], 200)
        self.assertEqual(result["revision"], 25)
        self.assertEqual(local, before)

    def test_same_field_conflicts_still_fail_closed(self):
        base = self.value()
        local, remote = copy.deepcopy(base), copy.deepcopy(base)
        local["pool"][0]["expires_at"] = 300
        remote["pool"][0]["expires_at"] = 200
        before = copy.deepcopy(remote)
        with self.assertRaisesRegex(ValueError, "pool.r.expires_at"):
            merge_settings(base, local, remote)
        self.assertEqual(remote, before)

    def test_keyed_workflow_nodes_merge_independent_fields_and_reject_delete_edit(self):
        base = self.value()
        local, remote = copy.deepcopy(base), copy.deepcopy(base)
        local["workflows"][0]["nodes"][0]["task"]["prompt"] = "user draft"
        remote["workflows"][0]["nodes"][0]["task"]["max_output_tokens"] = 16384
        result = merge_settings(base, local, remote)
        self.assertEqual(result["workflows"][0]["nodes"][0]["task"], {"prompt": "user draft", "max_output_tokens": 16384})
        local["workflows"] = []
        with self.assertRaisesRegex(ValueError, "workflows.w"):
            merge_settings(base, local, remote)

    def test_history_is_bounded_and_invalid_proof_is_not_used(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "history.json"
            for revision in range(12):
                remember(path, {**self.value(), "revision": revision})
            self.assertEqual(sorted(read_history(path)), list(range(4, 12)))
            data = json.loads(path.read_text())
            data["snapshots"][0]["value"]["sources"]["zcode"] = False
            path.write_text(json.dumps(data))
            self.assertEqual(read_history(path), {})


class RuntimeSettingsMergeTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service

    def test_real_save_rebases_stale_ui_and_unknown_base_does_not_write(self):
        service = self.service([], lambda *args: fixtures.response())
        current = service.scheduling.control()["settings"]
        current["pool"] = [{"route_id": "r", "expires_at": 100, "tags": []}]
        saved = service.scheduling.control(action="save", value=current, expected_revision=current["revision"])["settings"]
        draft, remote = copy.deepcopy(saved), copy.deepcopy(saved)
        draft["sources"]["codex"] = True
        remote["pool"][0]["expires_at"] = 200
        service.scheduling.control(action="save", value=remote, expected_revision=saved["revision"])
        merged = service.scheduling.control(action="save", value=draft, expected_revision=saved["revision"])["settings"]
        self.assertTrue(merged["sources"]["codex"])
        self.assertEqual(merged["pool"][0]["expires_at"], 200)
        before = (service.state_dir / "scheduling.json").read_bytes()
        with self.assertRaisesRegex(ValueError, "base revision"):
            service.scheduling.control(action="save", value=draft, expected_revision=999)
        self.assertEqual((service.state_dir / "scheduling.json").read_bytes(), before)
