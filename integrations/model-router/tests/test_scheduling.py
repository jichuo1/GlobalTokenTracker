import copy
import json
from pathlib import Path
import time
import unittest
import threading
from unittest.mock import patch

import test_router as fixtures
from gtt_router.api import dispatch
from gtt_router.models import Task, WorkerFailure, now_ms
from gtt_router.policy import rank_routes
from gtt_router.scheduling import DEFAULT, graph, settings


class SchedulingTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service

    def save(self, service, **changes):
        prior = settings(service.store)
        value = prior | changes
        return service.scheduling.control("save", value=value, expected_revision=prior["revision"])

    def definition(self, route, count=2):
        return {"id": "draft", "name": "Fixture workflow", "nodes": [{"id": str(i), "title": "step", "route_id": route.id,
            "tags": ["review"], "effort": route.effort or "low", "x": i * 260, "y": 20, "task": {"kind": "extract", "prompt": "fixture", "deadline_s": 5,
                "max_output_tokens": 1000, "token_budget": 5000}} for i in range(count)],
            "edges": [{"from": str(i), "to": str(i + 1)} for i in range(count - 1)], "token_budget": 10000, "max_parallel": 2}

    def run_done(self, service, ident):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            run = service.scheduling.status(ident)
            if run["state"] not in {"running", "cancelling"}:
                return run
            time.sleep(.05)
        self.fail("DAG did not terminate")

    def test_pool_is_an_ordered_hard_allowlist_and_expiry_fails_closed(self):
        first, second, outsider = fixtures.route(model="first", quality=30), fixtures.route(model="second", quality=99), fixtures.route(model="outside", quality=99)
        service = self.service([first, second, outsider], lambda *a: fixtures.response())
        expiry = now_ms() + 10000
        self.save(service, mode="directed", pool=[{"route_id": r.id, "expires_at": expiry, "tags": []} for r in [first, second]])
        candidates, rejected = rank_routes(Task("extract", "fixture", preferred_route=outsider.id), [first, second, outsider], {}, service.store)
        self.assertEqual([r["route"].id for r in candidates], [first.id, second.id])
        self.assertIn("route_not_selected", rejected[0]["reasons"])
        with patch("gtt_router.scheduling.now_ms", return_value=expiry + 1), patch("gtt_router.rust_kernel.now_ms", return_value=expiry + 1):
            candidates, rejected = rank_routes(Task("extract", "fixture"), [first, second], {}, service.store)
            self.assertFalse(candidates)
            self.assertTrue(all("directed_pool_expired" in r["reasons"] for r in rejected))

    def test_pool_never_bypasses_quality_or_verified_recovery(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: fixtures.response())
        self.save(service, mode="directed", pool=[{"route_id": route.id, "expires_at": now_ms() + 10000, "tags": []}])
        service.store.failure("route:" + route.id, "auth_failed", 0)
        self.assertFalse(rank_routes(Task("extract", "fixture"), [route], {}, service.store)[0])

    def test_switch_blocks_dispatch_and_probe_but_keeps_control_readable(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: self.fail("must not dispatch"))
        self.save(service, enabled=False)
        with self.assertRaisesRegex(ValueError, "scheduling_disabled"):
            service.submit({"kind": "extract", "prompt": "fixture"})
        self.assertEqual(service.recovery.recheck(route.id)["reason"], "scheduling_disabled")
        self.assertFalse(dispatch(service, "router_scheduling", {})["settings"]["enabled"])
        self.assertEqual(service.store.probe_budget(0)["count"], 0)

    def test_source_switch_and_optimistic_revision(self):
        a, b = fixtures.route(source="off"), fixtures.route(source="on")
        service = self.service([a, b], lambda *a: fixtures.response())
        self.save(service, sources={"off": False})
        self.assertEqual([c["route"].source for c in rank_routes(Task("extract", "fixture"), [a,b], {}, service.store)[0]], ["on"])
        rebased = service.scheduling.control("save", value=copy.deepcopy(DEFAULT), expected_revision=0)
        self.assertFalse(rebased["settings"]["sources"]["off"])
        conflicting = copy.deepcopy(DEFAULT)
        conflicting["sources"] = {"off": True}
        with self.assertRaisesRegex(ValueError, "revision_conflict"):
            service.scheduling.control("save", value=conflicting, expected_revision=0)

    def test_corrupt_settings_do_not_restore_broad_routing(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: fixtures.response())
        (service.state_dir / "scheduling.json").write_text('{"mode":"auto","mode":"directed"}')
        candidates, rejected = rank_routes(Task("extract", "fixture"), [route], {}, service.store)
        self.assertFalse(candidates)
        self.assertIn("scheduling_settings_invalid", rejected[0]["reasons"])

    def test_graph_validates_cycles_duplicates_positions_and_custom_task_fields(self):
        value = self.definition(fixtures.route())
        self.assertEqual(graph(value)["nodes"][0]["tags"], ["review"])
        negative=copy.deepcopy(value);negative["nodes"][0].update(x=-300,y=-200)
        self.assertEqual(graph(negative)["nodes"][0]["x"],-300)
        for changed in [value | {"edges": [{"from":"0","to":"1"},{"from":"1","to":"0"}]}, value | {"nodes": [value["nodes"][0]] * 2}]:
            with self.assertRaises(ValueError): graph(changed)
        for key, val in [("x", float("nan")), ("task", {"kind":"extract", "prompt":"x", "workflow_id":"other"})]:
            bad = copy.deepcopy(value);bad["nodes"][0][key] = val
            with self.assertRaises(ValueError): graph(bad)

    def test_dag_delivers_only_successful_upstream_evidence_and_accounts_once(self):
        route = fixtures.route()
        contexts = []
        def adapter(r, prompt, task, context):
            contexts.append(task.context)
            return fixtures.response("evidence")
        service = self.service([route], adapter)
        job = service.scheduling.submit(self.definition(route), "bounded input")
        run = self.run_done(service, job["run_id"])
        self.assertEqual(run["state"], "succeeded")
        self.assertEqual(run["tokens_used"], 200)
        self.assertEqual(len(contexts), 2)
        self.assertNotIn('"node_id": "0"', contexts[0])
        self.assertIn(run["nodes"]["0"]["job_id"], contexts[1])
        self.assertNotIn("definition", run)
        self.assertNotIn("input", run)
        self.assertEqual(len({n["job_id"] for n in run["nodes"].values()}), 2)

    def test_unknown_timeout_blocks_downstream_without_replay(self):
        route = fixtures.route()
        calls = []
        def adapter(*args):
            calls.append(True)
            raise WorkerFailure("timeout", usage={"total_tokens": 12})
        service = self.service([route], adapter)
        run = self.run_done(service, service.scheduling.submit(self.definition(route))["run_id"])
        self.assertEqual(run["nodes"]["0"]["state"], "unknown")
        self.assertEqual(run["nodes"]["1"]["state"], "blocked")
        self.assertEqual(run["tokens_used"], 12)
        self.assertEqual(len(calls), 1)

    def test_directed_mode_does_not_handoff_outside_pool(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: fixtures.response())
        self.save(service, mode="directed")
        with self.assertRaisesRegex(ValueError, "directed_pool_empty"):
            service.submit({"kind":"extract", "prompt":"fixture"})
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)

    def test_workflow_can_explicitly_disable_monitor_without_weakening_ordinary_calls(self):
        from gtt_router.service import RouterService
        r = fixtures.route()
        service = RouterService({"state_dir": str(self.root / "raw"), "limits": {}}, fixtures.FakeCatalog([r]), {"fake":lambda *a: fixtures.response()}, interface_paths=[self.root / "switch.json"])
        self.services.append(service)
        with self.assertRaisesRegex(ValueError, "luna_supervisor_required"):
            service.submit({"kind":"extract", "prompt":"fixture"})
        definition = self.definition(r);definition["luna_supervision"] = False
        run = self.run_done(service, service.scheduling.submit(definition)["run_id"])
        self.assertEqual(run["state"], "succeeded")
        self.assertEqual(run["supervision"], "workflow_opt_out")
        self.assertFalse(service._active_supervisors())
        definition["luna_supervision"] = True
        with self.assertRaisesRegex(ValueError, "luna_supervisor_required"):
            service.scheduling.submit(definition,request_id="new-supervised-request")

    def test_local_nodes_preserve_evidence_and_use_no_extra_model_budget(self):
        r = fixtures.route();calls = []
        def adapter(*a):calls.append(True);return fixtures.response("evidence")
        service = self.service([r], adapter)
        definition = self.definition(r, 1)
        base = {"title":"local", "route_id":"", "task":{"kind":"extract", "prompt":"local"}}
        definition["nodes"] = [base | {"id":"input", "node_type":"input"}, definition["nodes"][0],
            base | {"id":"merge", "node_type":"merge"}, base | {"id":"gate", "node_type":"gate"}, base | {"id":"output", "node_type":"output"}]
        definition["edges"] = [{"from":a,"to":b} for a,b in zip(["input","0","merge","gate"],["0","merge","gate","output"])]
        run = self.run_done(service, service.scheduling.submit(definition)["run_id"])
        self.assertEqual(run["state"], "succeeded");self.assertEqual(len(calls), 1)
        self.assertEqual(run["tokens_used"], 100)
        self.assertEqual(run["nodes"]["output"]["evidence_refs"], [run["nodes"]["0"]["job_id"]])

    def test_gate_without_model_evidence_blocks_output(self):
        r = fixtures.route();service = self.service([r], lambda *a:self.fail("no model request"))
        definition = self.definition(r)
        for node in definition["nodes"]: node.update(node_type="gate", route_id="")
        run = self.run_done(service, service.scheduling.submit(definition)["run_id"])
        self.assertEqual(run["nodes"]["0"]["reason"], "gate_requires_model_evidence")
        self.assertEqual(run["nodes"]["1"]["state"], "blocked")

    def test_pool_expiry_after_paid_success_blocks_next_node_and_retains_usage(self):
        r=fixtures.route();entered=threading.Event();release=threading.Event()
        def adapter(*a): entered.set();release.wait(3);return fixtures.response("preserved")
        service=self.service([r],adapter)
        self.save(service, mode="directed",pool=[{"route_id":r.id,"expires_at":now_ms()+10000,"tags":[]}])
        job=service.scheduling.submit(self.definition(r));self.assertTrue(entered.wait(2))
        self.save(service,pool=[{"route_id":r.id,"expires_at":now_ms()-1,"tags":[]}]);release.set()
        run=self.run_done(service,job["run_id"])
        self.assertEqual(run["nodes"]["0"]["state"],"succeeded")
        self.assertEqual(run["nodes"]["1"]["state"],"blocked")
        self.assertEqual(run["tokens_used"],100)

    def test_manual_probe_optional_monitor_keeps_explicit_unbudgeted_behavior(self):
        from gtt_router.service import RouterService
        r=fixtures.route();service=RouterService({"state_dir":str(self.root/"probe"),"limits":{},"recovery":{"max_probes_per_hour":1}},fixtures.FakeCatalog([r]),{"fake":lambda *a:fixtures.response("GTT_RECOVERY_OK")},interface_paths=[self.root/"switch.json"]);self.services.append(service)
        first=service.scheduling.control("check",route_id=r.id,luna_supervision=False)
        self.assertTrue(first["check"]["success"])
        second=service.scheduling.control("check",route_id=r.id,luna_supervision=False)
        self.assertTrue(second["check"]["success"])
        self.assertEqual(service.store.probe_budget(0)["count"], 2)
        self.assertFalse(service._active_supervisors())

    def test_no_settings_write_occurs_during_steady_read_only_ranking(self):
        route = fixtures.route();service = self.service([route], lambda *a: fixtures.response())
        self.save(service)
        path = service.state_dir / "scheduling.json";stamp = path.stat().st_mtime_ns
        for _ in range(8):rank_routes(Task("extract", "fixture"), [route], {}, service.store)
        self.assertEqual(path.stat().st_mtime_ns, stamp)

    def test_request_id_deduplicates_completed_runs_and_rejects_conflicts(self):
        r=fixtures.route();calls=[]
        def adapter(*a):calls.append(True);return fixtures.response()
        service=self.service([r],adapter);definition=self.definition(r,1)
        first=service.scheduling.submit(definition,request_id="stable")
        self.run_done(service,first["run_id"])
        again=service.scheduling.submit(definition,request_id="stable")
        self.assertEqual(first["run_id"],again["run_id"]);self.assertTrue(again["deduplicated"])
        self.assertEqual(len(calls),1)
        with self.assertRaisesRegex(ValueError,"request_id_conflict"):service.scheduling.submit(definition,"changed",request_id="stable")

    def test_optional_monitor_and_route_pool_are_preserved_during_verification(self):
        from gtt_router.service import RouterService
        r=fixtures.route();r.effort="low";outside=fixtures.route(source="outside",quality=99);calls=[]
        def adapter(route,*args):calls.append(route.id);return fixtures.response()
        service=RouterService({"state_dir":str(self.root/"verify"),"limits":{}},fixtures.FakeCatalog([r,outside]),{"fake":adapter},interface_paths=[self.root/"switch.json"]);self.services.append(service)
        definition=self.definition(r,1);definition["luna_supervision"]=False
        definition["nodes"][0]["task"].update(verify=True,max_attempts=2,token_budget=10000)
        run=self.run_done(service,service.scheduling.submit(definition)["run_id"])
        self.assertEqual(run["state"],"succeeded");self.assertEqual(calls,[r.id,r.id]);self.assertFalse(service._active_supervisors())

    def test_unknown_result_requires_explicit_risk_acknowledgement_for_new_run(self):
        r=fixtures.route();service=self.service([r],lambda *a:(_ for _ in ()).throw(WorkerFailure("timeout",usage={"total_tokens":12})))
        definition=self.definition(r,1)
        first=service.scheduling.submit(definition,request_id="first");self.run_done(service,first["run_id"])
        repeated=service.scheduling.submit(definition,request_id="second")
        self.assertEqual(repeated["blocked"],"unknown_paid_outcome_requires_review")
        self.assertEqual(repeated["run_id"],first["run_id"])
        accepted=service.scheduling.submit(definition,request_id="third",acknowledge_unknown_risk=True)
        self.assertNotEqual(accepted["run_id"],first["run_id"])

    def test_cancel_does_not_wait_for_slow_submission_or_issue_pending_model_request(self):
        r=fixtures.route();service=self.service([r],lambda *a:self.fail("cancelled before generation"))
        entered=threading.Event();release=threading.Event();original=service.submit
        def delayed(*args,**kwargs):entered.set();release.wait(3);return original(*args,**kwargs)
        service.submit=delayed
        run=service.scheduling.submit(self.definition(r,1));self.assertTrue(entered.wait(2))
        before=time.monotonic();service.scheduling.cancel(run["run_id"]);self.assertLess(time.monotonic()-before,.5)
        release.set();result=self.run_done(service,run["run_id"])
        self.assertEqual(result["state"],"cancelled");self.assertEqual(result["tokens_used"],0)
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0],0)

    def test_node_fallback_stays_inside_explicit_routes_even_when_global_pool_order_differs(self):
        primary=fixtures.route(source="primary",quality=80);backup=fixtures.route(source="backup",quality=50);outside=fixtures.route(source="outside",quality=99);calls=[]
        def adapter(route,*args):
            calls.append(route.source)
            if route.source=="primary":raise WorkerFailure("native_configuration_error",scope="source",metering={"generation_not_started":True})
            return fixtures.response()
        service=self.service([primary,backup,outside],adapter)
        self.save(service,mode="directed",pool=[{"route_id":r.id,"expires_at":now_ms()+10000,"tags":[]} for r in [backup,primary,outside]])
        definition=self.definition(primary,1);definition["nodes"][0]["fallback_routes"]=[backup.id]
        run=self.run_done(service,service.scheduling.submit(definition)["run_id"])
        self.assertEqual(run["state"],"succeeded");self.assertEqual(calls,["primary","backup"])
        self.assertEqual(run["tokens_used"],100)

    def test_information_support_binds_each_question_and_output_never_drops_unresolved_requests(self):
        for support_mode in ["exact", "missing", "unrelated"]:
            with self.subTest(support=support_mode):
                r=fixtures.route()
                def adapter(route,prompt,task,ctx):
                    data=json.loads(task.context);content=fixtures.content()
                    if not data["upstream_evidence"]:
                        content["information_requests"]=[{"question":"Definition A?","reason":"Missing contract"},{"question":"Definition B?","reason":"Missing contract"}]
                    elif support_mode!="missing":
                        ids=[q["id"] for q in data["unresolved_information_requests"]]
                        content["information_support"]=[{"request_id":ident if support_mode=="exact" else "unrelated:0", "summary":"Definition supplied", "evidence":"Input source paragraph 4"} for ident in ids]
                    result=fixtures.response();result["text"]=json.dumps(content);return result
                service=self.service([r],adapter);definition=self.definition(r)
                definition["nodes"].append({"id":"out","node_type":"output","route_id":"","task":{"kind":"extract","prompt":"output"}})
                definition["edges"].append({"from":"1","to":"out"})
                result=self.run_done(service,service.scheduling.submit(definition)["run_id"])
                self.assertEqual(result["state"],"succeeded" if support_mode=="exact" else "failed")
                self.assertEqual(len(result["nodes"]["0"]["information_requests"]),2)
                if support_mode=="missing":
                    self.assertEqual(result["nodes"]["out"]["reason"],"unresolved_information_requests")
                    self.assertEqual(len(result["nodes"]["out"]["information_requests"]),2)
                if support_mode=="unrelated":self.assertEqual(result["nodes"]["1"]["state"],"blocked")

    def test_partially_charged_running_node_does_not_double_reserve_budget(self):
        r=fixtures.route();r.max_parallel=4;release=threading.Event();third=threading.Event()
        def adapter(route,prompt,task,ctx):
            if task.prompt.startswith("职责：\nA") and 'counterexamples' in prompt:release.wait(5)
            if task.prompt.startswith("职责：\nC"):third.set()
            return fixtures.response()
        service=self.service([r],adapter,limits={"global_parallel":4,"combined_parallel":4,"max_active_jobs":4,"coordinator_threads":3})
        definition=self.definition(r,3);definition["edges"]=[]
        for node,label in zip(definition["nodes"],["A","B","C"]):node["task"]["prompt"]=label
        definition["nodes"][0]["task"].update(fanout=2,verify=False,max_attempts=2)
        definition["nodes"][2]["task"]["token_budget"]=4900
        started=service.scheduling.submit(definition)
        try:self.assertTrue(third.wait(4),"third node should start with the unspent reservation while A is still running")
        finally:release.set()
        result=self.run_done(service,started["run_id"])
        self.assertEqual(result["state"],"succeeded");self.assertEqual(result["tokens_used"],400)

    def test_default_node_budget_includes_actual_input_and_does_not_over_admit(self):
        r=fixtures.route();service=self.service([r],lambda *a:self.fail("budget must stop generation"))
        definition=self.definition(r,1);definition["nodes"][0]["task"].pop("token_budget")
        bare=service.parse_task(definition["nodes"][0]["task"]).token_budget
        definition["token_budget"]=bare
        result=self.run_done(service,service.scheduling.submit(definition,"x"*20000)["run_id"])
        self.assertEqual(result["nodes"]["0"]["reason"],"workflow_token_budget_exhausted")
        self.assertEqual(result["tokens_used"],0)

    def test_directed_plan_retains_selected_route_rejection_after_many_outsiders(self):
        outside=[fixtures.route(model=str(i)) for i in range(140)];selected=fixtures.route(model="selected")
        selected.auth_hint=False
        service=self.service([*outside,selected],lambda *a:self.fail("plan is read only"))
        plan=service.plan({"kind":"extract","prompt":"fixture","allowed_routes":[selected.id]})
        self.assertEqual([r["route_id"] for r in plan["rejected"]],[selected.id])
        self.assertEqual(plan["outside_allowed_routes_count"],140)

    def test_public_information_excerpt_is_explicit_and_does_not_mutate_full_state(self):
        r=fixtures.route();service=self.service([r],lambda *a:fixtures.response())
        start=service.scheduling.submit(self.definition(r,1));self.run_done(service,start["run_id"])
        raw=service.scheduling.status(start["run_id"],full=True)
        raw["nodes"]["0"]["information_requests"]=[{"id":str(i),"job_id":"fixture","question":"q"*1200,"reason":"r"*1200} for i in range(16)]
        service.scheduling._save(raw)
        public=service.scheduling.status(start["run_id"])["nodes"]["0"]
        self.assertEqual(public["information_request_count"],16);self.assertEqual(len(public["information_requests"]),8)
        self.assertTrue(public["information_requests_excerpt_only"])
        self.assertEqual(len(public["information_requests"][0]["question"]),300)
        self.assertEqual(len(service.scheduling.status(start["run_id"],full=True)["nodes"]["0"]["information_requests"]),16)
