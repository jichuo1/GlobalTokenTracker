import json
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import patch

from tests import test_groups as fixtures
from gtt_router.api import dispatch
from gtt_router.group_contract import GroupRequest, parse_leader_decision
from gtt_router.groups import GroupJob
from gtt_router.mcp import TOOLS, TASK_SCHEMA
from gtt_router.models import now_ms, Task
from gtt_router.service import Job


class CoordinationTests(unittest.TestCase):
    setUp = fixtures.GroupTests.setUp
    tearDown = fixtures.GroupTests.tearDown
    service = fixtures.GroupTests.service
    wait = fixtures.GroupTests.wait

    def seed(self, service, seats=3, workflow="coord", budget=1000000, used=100, estimated=25):
        request = GroupRequest.parse({"topic": "fixture original evidence", "needs_web": False,
            "workflow_id": workflow, "token_budget": budget, "member_count": seats})
        job = GroupJob(request)
        job.state, job.phase, job.round = "needs_codex", "leader_review", 1
        job.roles = service.groups.plan({"topic": request.topic, "needs_web": False, "member_count": seats})["roles"]
        job.tokens_used, job.tokens_estimated = used, estimated
        job.results = [{"worker": 0, "round": 1, "slot": 1, "role": "collector", "ok": True,
            "route": fixtures.routes()[1].public(), "content": {"summary": "original fact excerpt",
                "findings": [], "sources": [], "uncertainties": ["not independently verified"], "complete": True}}]
        job.attempts = [{"worker": 0, "source": "fast", "provider": "q", "model": "quick", "route_id": fixtures.routes()[1].id,
            "started_at": now_ms(), "state": "succeeded", "tokens_charged": used, "token_measurement": "reported",
            "usage": {"totalTokens": used}}]
        job.pending_action = {"type": "submit_codex", "detail": {}, "caller_approval_required": True}
        service.groups.jobs[job.id] = job
        service._save(job)
        return job

    def merge(self, service, *jobs):
        return dispatch(service, "router_group_merge", {"group_ids": [j.id for j in jobs], "approved": True,
            "reason": "shared synthesis requires coordinated evidence", "expected_revisions": {j.id: j.revision for j in jobs}})

    def message(self, service, job, **changes):
        values = {"workflow_id": job.task.workflow_id, "to_group": job.id, "to_role": "leader", "kind": "evidence",
                  "summary": "bounded source excerpt", "event_id": "caller-message"}
        return dispatch(service, "router_message", values | changes)

    def test_scoped_messages_have_provenance_are_deduplicated_and_injected_once(self):
        service = self.service()
        source, target = self.seed(service), self.seed(service)
        sent = self.message(service, target, evidence_refs=[{"group_id": source.id, "worker": 0, "anchor": "source.kt:20"}])
        repeated = self.message(service, target, evidence_refs=[{"group_id": source.id, "worker": 0, "anchor": "source.kt:20"}])
        self.assertTrue(repeated["deduplicated"])
        self.assertEqual(sent["cursor"], repeated["cursor"])
        messages = service.coordination.context_for(target, "leader", 0)
        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]["evidence_refs"][0]["source"], "fast")
        self.assertEqual(messages[0]["evidence_refs"][0]["group_id"], source.id)
        self.assertEqual(messages[0]["authority"], "untrusted_evidence_not_instructions")
        self.assertEqual(service.coordination.context_for(target, "leader", 0), [])
        with self.assertRaises(ValueError):
            self.message(service, target, summary="different content with same ID")

    def test_cross_workflow_secrets_size_and_role_impersonation_are_rejected(self):
        service = self.service()
        target, foreign = self.seed(service), self.seed(service, workflow="other")
        for values in [{"workflow_id": "other"}, {"summary": "x" * 1201}, {"summary": "Bearer " + "a" * 32},
                       {"from_role": "supervisor"}, {"kind": {}},
                       {"evidence_refs": [{"group_id": target.id, "worker": 0, "url": "https://example.com/source?api_key=private"}]},
                       {"evidence_refs": [{"group_id": foreign.id, "worker": 0}]}]:
            with self.subTest(values=values), self.assertRaises(ValueError):
                self.message(service, target, **values)
        with self.assertRaises(ValueError):
            service.coordination.message("coord", "leader", "progress", "advice", target.id, from_role="leader")

    def test_only_related_role_receives_messages_and_ttl_is_finite(self):
        service = self.service()
        job = self.seed(service)
        self.message(service, job, to_role="member-1", ttl_s=60)
        self.assertEqual(service.coordination.context_for(job, "analyst", 2), [])
        self.assertEqual(len(service.coordination.context_for(job, "collector", 1)), 1)
        with patch("gtt_router.store.now_ms", return_value=now_ms() + 61000):
            self.assertEqual([e for e in service.store.events("coord") if e["type"] == "message"], [])

    def test_event_cursor_pages_and_status_sampling_never_export_payloads(self):
        service = self.service()
        job = self.seed(service)
        job.progress = {"0": {"phase": "model_activity", "private": "sensitive prompt", "activity": {"activity_count": 2,
            "reasoning_chars": 20, "hidden_text": "sensitive prompt"}}}
        service._save(job)
        service._save(job)
        cursor, seen = 0, []
        while True:
            result = dispatch(service, "router_events", {"workflow_id": "coord", "cursor": cursor, "limit": 1})
            if result["next_cursor"] == cursor:
                break
            cursor = result["next_cursor"]
            seen.extend(result["events"])
        self.assertEqual(len({e["event_id"] for e in seen}), len(seen))
        self.assertNotIn("sensitive prompt", json.dumps(seen))
        status = dispatch(service, "router_supervisor", {"action": "status", "workflow_id": "coord"})
        self.assertNotIn("sensitive prompt", json.dumps(status))
        self.assertEqual(status["groups"][0]["progress"]["0"]["activity"]["reasoning_chars"], 20)

    def test_supervisor_protects_live_thinking_and_only_reminds_with_deadline_risk(self):
        service = self.service()
        current = now_ms()
        base = {"action": "status", "workflow_id": "coord", "actor_state": {"phase": "thinking",
            "last_progress_at": current - 600000, "active_until": current + 60000,
            "deadline_at": current + 60000, "idle_threshold_s": 300}}
        self.assertEqual(dispatch(service, "router_supervisor", base)["reminders"], [])
        base["actor_state"]["active_until"] = current - 1
        self.assertEqual(len(dispatch(service, "router_supervisor", base)["reminders"]), 1)
        base["actor_state"]["deadline_at"] = current + 3600000
        self.assertEqual(dispatch(service, "router_supervisor", base)["reminders"], [])
        for values in [{"active_until": current + 7200000}, {"last_progress_at": current + 100000}]:
            with self.assertRaises(ValueError):
                dispatch(service, "router_supervisor", base | {"actor_state": base["actor_state"] | values})
        with self.assertRaises(ValueError):
            dispatch(service, "router_supervisor", {"action": "status", "workflow_id": "coord", "group_ids": [{}]})

    def test_monitor_proposal_is_advisory_and_saturation_gap_is_visible(self):
        service = self.service()
        job = self.seed(service)
        before = job.task.token_budget
        report = dispatch(service, "router_supervisor", {"action": "report", "workflow_id": "coord",
            "proposal": {"kind": "suggestion", "summary": "review a stalled route", "group_ids": [job.id]}, "event_id": "monitor-1"})
        self.assertTrue(report["approval_required_for_changes"])
        self.assertEqual(job.task.token_budget, before)
        # Fill the actual journal so both Python and the native atomic path
        # exercise the same capacity gate, independently of helper call shape.
        with service.store.lock, service.store.conn:
            service.store.conn.execute("WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<10000) "
                "INSERT INTO coordination_events(workflow_id,event_id,created_at,expires_at,payload) "
                "SELECT ?, 'capacity:'||n, ?, ?, '{}' FROM rows", ("coord", now_ms(), now_ms() + 1000000))
        job.tokens_used += 1
        service._save(job)
        status = service.coordination.supervisor("status", "coord")
        self.assertTrue(status["events_incomplete"])
        self.assertEqual(status["dropped_state_events"], 1)

    def test_merge_accumulates_more_than_eight_seats_and_consumption_without_new_billing(self):
        service = self.service()
        one, two = self.seed(service, seats=5), self.seed(service, seats=5, used=140, estimated=40)
        count = service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0]
        earliest = min(one.deadline_at, two.deadline_at)
        merged = self.merge(service, one, two)
        self.assertEqual((merged["member_count"], merged["budget_total"]), (10, 2000000))
        self.assertEqual((merged["tokens_used"], merged["tokens_estimated"], merged["tokens_reserved"]), (240, 65, 0))
        self.assertEqual(merged["deadline_at"], earliest)
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], count)
        self.assertEqual(service.store.job(merged["group_id"])["attempts"], [])
        self.assertEqual(service.groups.status(one.id)["state"], "merged")
        self.assertEqual(len(service.groups._source_evidence(service.groups.jobs[merged["group_id"]])), 2)

    def test_merge_rejects_running_reserved_stale_cross_scope_and_expired_groups(self):
        for change in ["running", "reserved", "revision", "workflow", "expired", "active", "budget"]:
            with self.subTest(change=change):
                service = self.service()
                one, two = self.seed(service), self.seed(service)
                expected = {one.id: 0, two.id: 0}
                if change == "running":
                    two.state = "running"
                elif change == "reserved":
                    two.tokens_reserved = 1
                elif change == "revision":
                    two.revision += 1
                elif change == "workflow":
                    two.task.workflow_id = "elsewhere"
                elif change == "expired":
                    two.deadline_at = now_ms() - 1
                elif change == "active":
                    two.active_calls = 1
                elif change == "budget":
                    one.task.token_budget = two.task.token_budget = 60000000
                service._save(two)
                with self.assertRaises(ValueError):
                    service.groups.merge([one.id, two.id], True, "synthesis", expected)
                self.assertNotEqual(service.groups.status(one.id)["state"], "merged")

    def test_merge_transaction_rolls_back_and_stale_parallel_approval_only_commits_once(self):
        service = self.service()
        one, two = self.seed(service), self.seed(service)
        prior = service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0]
        with patch.object(service.store, "_append_event", side_effect=ValueError("fixture injected rollback")):
            with self.assertRaises(ValueError):
                self.merge(service, one, two)
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], prior)
        self.assertEqual(one.state, "needs_codex")
        def attempt():
            try:
                return self.merge(service, one, two)["group_id"]
            except ValueError:
                return None
        with ThreadPoolExecutor(max_workers=2) as pool:
            outcomes = list(pool.map(lambda _: attempt(), range(2)))
        self.assertEqual(sum(bool(o) for o in outcomes), 1)

    def test_merged_sources_are_permanently_read_only_and_restart_keeps_one_bill(self):
        directory = self.root / "merged-restart"
        service = self.service(directory=directory)
        one, two = self.seed(service), self.seed(service)
        merged = self.merge(service, one, two)
        count = service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0]
        for action in [lambda: service.groups.resume(one.id, "try revive"),
                       lambda: service.groups.extend(one.id, "try budget", 2000000, True),
                       lambda: service.groups.adjust_seats(one.id, 4, "try seats", True),
                       lambda: service.groups.complete(one.id, True),
                       lambda: self.message(service, one, event_id="no-new-message")]:
            with self.assertRaises(ValueError):
                action()
        self.assertFalse(service.groups.cancel(one.id)["cancelled"])
        service.shutdown()
        self.services.remove(service)
        restored = self.service(directory=directory)
        self.assertEqual(restored.groups.status(one.id)["state"], "merged")
        self.assertEqual(restored.groups.status(merged["group_id"])["tokens_used"], 200)
        self.assertEqual(restored.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], count)
        with self.assertRaises(ValueError):
            restored.groups.resume(one.id, "restart revive")
        with self.assertRaises(ValueError):
            restored.groups.resume(merged["group_id"], "new window", deadline_s=100)

    def test_merged_research_batches_roles_with_existing_capacity(self):
        gate, entered = threading.Event(), threading.Event()
        count, maximum = [0], [0]
        mutex = threading.Lock()
        def adapter(route, prompt, task, context):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            with mutex:
                count[0] += 1
                maximum[0] = max(maximum[0], count[0])
                entered.set()
            time.sleep(.05)
            with mutex:
                count[0] -= 1
            return fixtures.reply()
        service = self.service(adapter)
        service.limits["global_parallel"] = 3
        one, two = self.seed(service, seats=5), self.seed(service, seats=5)
        merged = self.merge(service, one, two)
        service.groups.resume(merged["group_id"], "approved scoped synthesis")
        self.assertTrue(entered.wait(2))
        result = self.wait(service, merged["group_id"])
        self.assertEqual(result["member_count"], 10)
        self.assertLessEqual(maximum[0], 2)
        self.assertEqual(service.total_inflight, 0)

    def test_native_leader_communication_and_merge_are_proposals_not_approval(self):
        decision = parse_leader_decision({"action": "propose_merge", "merge_groups": ["a", "b"],
            "communications": [{"to_role": "supervisor", "kind": "progress", "summary": "related evidence"}]})
        self.assertEqual(decision["action"], "propose_merge")
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "propose_merge", "merge_groups": ["a", "b"], "approved": True})
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "continue_research", "communications": [{"summary": "x"}] * 5})
        self.assertTrue({"router_events", "router_message", "router_supervisor", "router_group_merge"} <= {t["name"] for t in TOOLS})
        self.assertIn("timeout_mode", TASK_SCHEMA["properties"])

    def test_leader_assignment_and_member_conflict_reach_actual_next_call_context(self):
        observed = []
        def adapter(route, prompt, task, context):
            payload = json.loads(task.context)
            observed.append((fixtures.is_leader(task), payload.get("messages", [])))
            if fixtures.is_leader(task):
                decision = {"action": "continue_research" if fixtures.initial(task) else "submit_codex"}
                if fixtures.initial(task):
                    decision["communications"] = [{"to_role": "collector", "kind": "assignment", "summary": "check the primary contract first"}]
                return fixtures.reply(decision)
            result = fixtures.reply()
            body = json.loads(result["text"])
            body["communications"] = [{"to_role": "leader", "kind": "conflict", "summary": "one assumption is still unverified"}]
            result["text"] = json.dumps(body)
            return result
        service = self.service(adapter)
        group = service.groups.submit({"topic": "communication fixture", "needs_web": False, "workflow_id": "coord"})["group_id"]
        self.wait(service, group)
        self.assertTrue(any(not leader and any(m["kind"] == "assignment" for m in messages) for leader, messages in observed))
        self.assertTrue(any(leader and any(m["kind"] == "conflict" for m in messages) for leader, messages in observed))
        messages = [e for e in service.store.events("coord") if e["type"] == "message"]
        self.assertTrue(all(e["origin"].get("worker") is not None for e in messages))

    def test_persisted_reservation_rejects_merge_and_expired_events_do_not_block_it(self):
        service = self.service()
        one, two = self.seed(service), self.seed(service)
        stored = service.store.job(two.id)
        stored["tokens_reserved"] = 10
        service.store.save_job(two.id, "interrupted", two.task, {k: v for k, v in stored.items() if k not in {"job_id", "state", "created_at", "updated_at", "task"}})
        with self.assertRaises(ValueError):
            self.merge(service, one, two)
        service._save(two)
        with service.store.lock, service.store.conn:
            service.store.conn.execute("UPDATE coordination_events SET expires_at=0")
        merged = self.merge(service, one, two)
        self.assertEqual(merged["member_count"], 6)
        self.assertTrue(all(e["expires_at"] > now_ms() for e in service.store.events("coord")))

    def test_message_summary_clipping_is_explicit_and_original_is_preserved(self):
        service = self.service()
        target = self.seed(service)
        service.coordination.message("coord", "leader", "evidence", "x" * 1100, to_group=target.id, event_id="long-summary")
        messages = service.coordination.context_for(target, "leader", 0)
        self.assertEqual(len(messages), 1)
        row = messages[0]
        self.assertEqual(len(row["summary"]), 600)
        self.assertTrue(row["summary_excerpt_only"])
        self.assertEqual((row["summary_chars"], row["full_message_event"]), (1100, "long-summary"))
        events = service.coordination.events("coord")["events"]
        original = next(e for e in events if e["event_id"] == "long-summary")
        self.assertEqual(len(original["summary"]), 1100)
        self.assertEqual(service.coordination.context_for(target, "leader", 0), [])

    def test_valid_large_message_cannot_permanently_pin_recipient_cursor(self):
        service = self.service()
        source, target = self.seed(service), self.seed(service)
        source.results[0]["content"]["uncertainties"] = ["u" * 1000] * 4
        service._save(source)
        ref = {"group_id": source.id, "worker": 0, "url": "https://example.com/" + "a" * 580, "anchor": "b" * 300}
        self.message(service, target, summary="s" * 1200, evidence_refs=[ref] * 4, event_id="large-valid")
        self.message(service, target, summary="later evidence", event_id="later-valid")
        first = service.coordination.context_for(target, "leader", 0)
        rest = service.coordination.context_for(target, "leader", 0)
        self.assertTrue(any(m["event_id"] == "large-valid" and m.get("bounded_omission") for m in first))
        self.assertTrue(any(m["event_id"] == "later-valid" for m in first + rest))
        self.assertLessEqual(len(json.dumps(first, ensure_ascii=False)), 4100)

    def test_role_inbox_is_not_delayed_by_unrelated_progress_event_backlog(self):
        service = self.service()
        job = self.seed(service)
        for index in range(100):
            service.store.append_event("coord", "progress-backlog:" + str(index), {"type": "job_state", "phase": "running"})
        self.message(service, job, event_id="fresh-after-backlog")
        messages = service.coordination.context_for(job, "leader", 0)
        self.assertEqual([m["event_id"] for m in messages], ["fresh-after-backlog"])

    def test_silent_supervisor_age_is_derived_without_events_usage_or_renewal(self):
        service = self.service()
        job = self.seed(service)
        captured = now_ms()
        job.state = "running"
        job.attempts[-1]["state"] = "running"
        job.progress = {"0": {"phase": "model_activity", "activity": {"captured_at": captured,
            "activity_count": 3, "last_kind": "reasoning", "elapsed_s": 10, "idle_s": 1,
            "hard_remaining_s": 100, "soft_remaining_s": 4, "expires_in_s": 2}}}
        service._save(job)
        before = (service.store.conn.execute("SELECT COUNT(*) FROM coordination_events").fetchone()[0],
                  service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0])
        with patch("gtt_router.coordination.now_ms", return_value=captured + 3000):
            first = service.coordination.supervisor("status", "coord")["groups"][0]["progress"]["0"]["activity"]
            again = service.coordination.public_progress({"0": {"activity": first}})["0"]["activity"]
        self.assertEqual((first["elapsed_s"], first["idle_s"], first["hard_remaining_s"]), (13, 4, 97))
        self.assertEqual((first["soft_remaining_s"], first["expires_in_s"]), (1, 0))
        self.assertEqual(first["activity_count"], 3)
        self.assertEqual(first["captured_at"], captured)
        self.assertEqual(first["age_source"], "derived_from_last_capture")
        self.assertEqual(first, again)  # projection does not apply twice
        after = (service.store.conn.execute("SELECT COUNT(*) FROM coordination_events").fetchone()[0],
                 service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0])
        self.assertEqual(before, after)
        self.assertEqual(job.progress["0"]["activity"]["elapsed_s"], 10)
        self.assertEqual(job.progress["0"]["activity"]["captured_at"], captured)
        job.state = "needs_codex"
        service._save(job)
        end = service.store.job(job.id)["updated_at"]
        with patch("gtt_router.coordination.now_ms", return_value=end + 10000):
            frozen = service.coordination.supervisor("status", "coord")["groups"][0]["progress"]["0"]["activity"]
        self.assertTrue(frozen["frozen"])
        self.assertLess(frozen["elapsed_s"], 11)

    def test_finished_worker_freezes_while_other_group_work_continues(self):
        service = self.service()
        job = self.seed(service)
        job.state = "running"
        captured = now_ms()
        job.progress = {str(i): {"activity": {"captured_at": captured, "activity_count": 1,
            "elapsed_s": 10, "idle_s": 1, "expires_in_s": 180}} for i in (0, 1)}
        job.attempts = [{"worker": 0, "state": "succeeded", "started_at": captured - 1000, "duration_ms": 1000},
                        {"worker": 1, "state": "running", "started_at": captured}]
        service._save(job)
        with patch("gtt_router.coordination.now_ms", return_value=captured + 10000):
            progress = service.coordination.supervisor("status", "coord")["groups"][0]["progress"]
        self.assertTrue(progress["0"]["activity"]["frozen"])
        self.assertEqual(progress["0"]["activity"]["elapsed_s"], 10)
        self.assertFalse(progress["1"]["activity"]["frozen"])
        self.assertEqual(progress["1"]["activity"]["elapsed_s"], 20)
        final = service.coordination.public_progress(job.progress, captured + 5000, job.attempts)
        self.assertEqual(final["0"]["activity"]["elapsed_s"], 10)
        self.assertTrue(final["1"]["activity"]["frozen"])
        malformed = service.coordination.public_progress({"0": {"activity": "invalid"}}, attempts=job.attempts)
        self.assertEqual(malformed["0"]["activity"], {})

    def test_information_response_waits_for_full_coverage_and_is_idempotent(self):
        service = self.service()
        job = self.seed(service)
        job.request.context = "x" * 300
        source = job.results[0]
        source["content"]["information_requests"] = [{"question": "Need definition", "reason": "Missing scope", "context_range": [100, 200]}]
        source["information_requests"] = service.groups._information_requests(job, source)
        support = {"id": "support-scope", "start": 100, "end": 200}
        for worker, bounds in [(1, [100, 150]), (2, [150, 200])]:
            job.results.append({"worker": worker, "ok": True, "responds_to_requests": ["0:0"],
                "support": support, "slice": {"start": bounds[0], "end": bounds[1]}, "content": {}})
            service._save(job)
            events = [e for e in service.store.events("coord") if e["type"] == "information_response"]
            self.assertEqual(len(events), worker - 1)
        service._save(job)
        self.assertEqual(len([e for e in service.store.events("coord") if e["type"] == "information_response"]), 1)
        self.assertEqual(service.groups.status(job.id)["information_status"]["resolved_request_ids"], ["0:0"])

    def test_oversized_event_cannot_pin_monitor_cursor(self):
        service = self.service()
        service.store.append_event("coord", "oversized-state", {"type": "job_state", "progress": {"0": {"payload": "x" * 20000}}})
        service.store.append_event("coord", "after-large-state", {"type": "job_state", "state": "succeeded"})
        first = service.coordination.events("coord")
        self.assertTrue(first["events"][0].get("bounded_omission"))
        self.assertEqual(first["events"][1]["state"], "succeeded")
        self.assertEqual(service.coordination.events("coord", cursor=first["next_cursor"])["events"], [])

    def test_progress_sampling_preserves_immediate_attempt_failure(self):
        service = self.service()
        job = self.seed(service)
        job.state = "running"
        job.attempts[-1]["state"] = "running"
        job.progress = {"0": {"activity": {"activity_count": 1, "output_chars": 1, "last_kind": "output"}}}
        service._save(job)
        before = len(service.store.events("coord"))
        job.progress["0"]["activity"]["output_chars"] = 100
        service._save(job)
        self.assertEqual(len(service.store.events("coord")), before)
        job.attempts[-1].update({"state": "failed", "error": {"code": "incomplete_output"}})
        service._save(job)
        latest = service.store.events("coord")[-1]
        self.assertEqual(latest["attempt_states"][0]["error_code"], "incomplete_output")
        self.assertGreater(len(service.store.events("coord")), before)

    def test_native_step_service_update_preserves_activity_capture(self):
        def adapter(route, prompt, task, context):
            context.progress({"phase": "model_activity", "activity": {"activity_count": 2, "elapsed_s": .1,
                "idle_s": 0, "hard_remaining_s": 100, "soft_remaining_s": 10, "expires_in_s": 10}})
            context.progress({"phase": "native_step", "step_type": "thinking", "state": "RUNNING"})
            return fixtures.reply()
        service = self.service(adapter)
        job = Job(Task("extract", "progress fixture", workflow_id="coord"))
        service.jobs[job.id] = job
        service._save(job)
        result = service._worker(job, job.task, 0, job.deadline)
        self.assertTrue(result["ok"])
        stored = service.store.job(job.id)["progress"]["0"]
        self.assertEqual(stored["phase"], "native_step")
        self.assertEqual(stored["activity"]["activity_count"], 2)
        self.assertIn("captured_at", stored["activity"])
        with patch("gtt_router.coordination.now_ms", return_value=stored["activity"]["captured_at"] + 2000):
            view = service.coordination.supervisor("status", "coord")["jobs"][0]["progress"]["0"]
        self.assertTrue(view["activity"]["frozen"])
        with patch("gtt_router.coordination.now_ms", return_value=stored["activity"]["captured_at"] + 10000):
            later = service.coordination.supervisor("status", "coord")["jobs"][0]["progress"]["0"]["activity"]
        self.assertEqual(view["activity"]["elapsed_s"], later["elapsed_s"])
        self.assertEqual(view["activity"]["hard_remaining_s"], later["hard_remaining_s"])

    def test_three_default_budget_groups_merge_with_explicit_override_and_survive_restart(self):
        directory = self.root / "budget-override-restart"
        service = self.service(directory=directory)
        groups = [self.seed(service, budget=50000000) for _ in range(3)]
        params = {"group_ids": [j.id for j in groups], "approved": True, "reason": "approve one combined 100m ceiling",
                  "expected_revisions": {j.id: j.revision for j in groups}}
        with self.assertRaisesRegex(ValueError, "override is required"):
            dispatch(service, "router_group_merge", params)
        before = service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0]
        merged = dispatch(service, "router_group_merge", params | {"token_budget": 100000000})
        self.assertEqual((merged["member_count"], merged["budget_total"], merged["tokens_used"], merged["tokens_estimated"]), (9, 100000000, 300, 75))
        event = next(e for e in service.store.events("coord") if e["type"] == "group_merge")
        self.assertEqual(event["source_budget_sum"], 150000000)
        self.assertTrue(event["explicit_budget_override"])
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], before)
        service.shutdown()
        self.services.remove(service)
        restored = self.service(directory=directory)
        status = restored.groups.status(merged["group_id"])
        self.assertEqual((status["member_count"], status["budget_total"], status["tokens_used"], status["tokens_estimated"]), (9, 100000000, 300, 75))
        self.assertEqual(len(status["merged_sources"]), 3)
        self.assertEqual(restored.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], before)

    def test_budget_override_cannot_reduce_consumed_tokens_or_increase_source_allowance(self):
        service = self.service()
        one, two = self.seed(service, budget=1000000, used=600), self.seed(service, budget=1000000, used=600)
        params = {"group_ids": [one.id, two.id], "approved": True, "reason": "explicit constrained merge ceiling",
                  "expected_revisions": {one.id: one.revision, two.id: two.revision}}
        for value in [1000, 2000001, True, 100000001]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                dispatch(service, "router_group_merge", params | {"token_budget": value})
        self.assertEqual(service.groups.status(one.id)["state"], "needs_codex")
        self.assertEqual(service.groups.status(two.id)["state"], "needs_codex")
        merged = dispatch(service, "router_group_merge", params | {"token_budget": 1200})
        self.assertEqual((merged["budget_total"], merged["tokens_used"]), (1200, 1200))

    def test_merge_and_ordinary_resume_expose_one_actual_deadline_everywhere(self):
        service = self.service()
        one, two = self.seed(service), self.seed(service)
        one.deadline_at = now_ms() + 120123
        two.deadline_at = now_ms() + 300999
        service._save(one)
        service._save(two)
        merged = self.merge(service, one, two)
        expected = one.deadline_at
        stored = service.store.job(merged["group_id"])
        direct = dispatch(service, "router_status", {"job_id": merged["group_id"]})
        supervised = service.coordination.supervisor("status", "coord", group_ids=[merged["group_id"]])["groups"][0]
        self.assertEqual(stored["deadline_at"], expected)
        self.assertEqual(stored["group"]["deadline_at"], expected)
        self.assertEqual(direct["deadline_at"], expected)
        self.assertEqual(supervised["deadline_at"], expected)
        self.assertLessEqual(stored["soft_deadline_at"], expected)

        ordinary = self.seed(service)
        resumed = dispatch(service, "router_group_resume", {"group_id": ordinary.id, "reason": "bounded review window", "deadline_s": 90})
        expected = resumed["deadline_at"]
        self.wait(service, ordinary.id)
        stored = service.store.job(ordinary.id)
        direct = dispatch(service, "router_status", {"job_id": ordinary.id})
        supervised = service.coordination.supervisor("status", "coord", group_ids=[ordinary.id])["groups"][0]
        self.assertEqual(stored["deadline_at"], expected)
        self.assertEqual(stored["group"]["deadline_at"], expected)
        self.assertEqual(direct["deadline_at"], expected)
        self.assertEqual(supervised["deadline_at"], expected)
        self.assertLessEqual(stored["soft_deadline_at"], expected)


if __name__ == "__main__":
    unittest.main()
