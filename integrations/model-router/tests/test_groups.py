import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from concurrent.futures import ThreadPoolExecutor

from gtt_router.api import dispatch
from gtt_router.group_contract import GroupRequest
from gtt_router.groups import GroupJob
from gtt_router.models import Route, Task, WorkerFailure, now_ms
from tests.supervised_service import SupervisedService as RouterService


class Catalog:
    def __init__(self, routes):
        self.routes = routes
        self.ledger = {"sources": [], "models": [], "quotas": []}
        self.refreshed_at = now_ms()
    def refresh(self, force=False):
        self.refreshed_at = now_ms()
    def public(self):
        return {"routes": [r.public() for r in self.routes], "ledger": self.ledger}


def routes():
    return [Route("smart", "p", "strong", auth_hint=True, adapter="fake", output_limit=100000,
                  profile={"intelligence":90,"captured_at":now_ms(),"latency_prior_s":1}),
            Route("fast", "q", "quick", auth_hint=True, adapter="fake", max_parallel=2, output_limit=4096,
                  capabilities=["text","web"], profile={"intelligence":25,"captured_at":now_ms(),"latency_prior_s":.1}),
            Route("other-smart", "r", "strong", auth_hint=True, adapter="fake", output_limit=100000,
                  profile={"intelligence":80,"captured_at":now_ms(),"latency_prior_s":1})]


def reply(decision=None):
    content={"summary":"fixture evidence","findings":[],"sources":[],"uncertainties":[],"complete":True}
    if decision:content["group_decision"]=decision
    return {"text":json.dumps(content),"usage":{"totalTokens":10},"observed_urls":[]}


def is_leader(task):return task.prompt.startswith("You lead a research group")
def initial(task):return "Plan initial division" in task.prompt


class GroupTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);self.services=[]
    def tearDown(self):
        for service in self.services:service.shutdown()
        self.temp.cleanup()
    def service(self, adapter=None, candidates=None, directory=None):
        def normal(route,prompt,task,ctx):
            return reply({"action":"continue_research" if initial(task) else "submit_codex"}) if is_leader(task) else reply()
        s=RouterService({"state_dir":str(directory or self.root/str(len(self.services))),
            "limits":{"global_parallel":4,"combined_parallel":6,"max_active_jobs":2,"max_active_groups":2}},
            Catalog(candidates if candidates is not None else routes()),{"fake":adapter or normal})
        self.services.append(s);return s
    def wait(self,s,group,state="needs_codex",timeout=5):
        until=time.monotonic()+timeout
        while time.monotonic()<until:
            data=s.groups.status(group)
            if data["state"]==state and (state != "needs_codex" or data["quiescent"]):return data
            time.sleep(.01)
        self.fail(f"group did not reach {state}: {data}")
    def submit(self,s,**kw):
        return s.groups.submit({"topic":"fixture research","needs_web":False,**kw})["group_id"]

    def test_group_plans_three_mixed_roles_and_requires_codex_final_review(self):
        s=self.service();p=s.groups.plan({"topic":"fixture","needs_web":False})
        self.assertEqual(p["member_count"],3);self.assertTrue(p["available"])
        self.assertEqual([r["role"] for r in p["roles"]],["leader","collector","analyst"])
        g=self.submit(s);status=self.wait(s,g)
        self.assertEqual((status["tokens_used"],status["tokens_reserved"]),(40,0))
        self.assertEqual(status["pending_action"]["type"],"submit_codex")
        collected=s.groups.collect(g);self.assertEqual(len(collected["results"]),4)
        self.assertEqual(s.groups.complete(g,True,"verified")["state"],"succeeded")
        rows=s.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0]
        self.assertEqual(rows,4)

    def test_no_low_quality_model_is_promoted_to_leader(self):
        calls=[]
        s=self.service(lambda *a:calls.append(a),candidates=[routes()[1]])
        g=self.submit(s);state=self.wait(s,g)
        self.assertEqual(state["pending_action"]["type"],"no_qualified_leader")
        self.assertEqual(calls,[])

    def test_budget_extension_requires_explicit_codex_approval_and_caps_at_100m(self):
        def adapter(route,prompt,task,ctx):
            if is_leader(task):return reply({"action":"request_extension" if initial(task) else "submit_codex","rationale":"more scope"})
            return reply()
        s=self.service(adapter);g=self.submit(s);self.wait(s,g)
        with self.assertRaises(ValueError):s.groups.extend(g,"needed")
        with self.assertRaises(ValueError):dispatch(s,"router_group_extend",{"group_id":g,"reason":"needed"})
        with self.assertRaises(ValueError):s.groups.extend(g,"needed",100000001,True)
        s.groups.extend(g,"verified need",100000000,True);state=self.wait(s,g)
        self.assertEqual(state["budget_total"],100000000)
        self.assertEqual(state["pending_action"]["type"],"submit_codex")

    def test_seats_change_only_after_approval_and_remain_within_eight(self):
        def adapter(route,prompt,task,ctx):
            if is_leader(task):return reply({"action":"request_seat_change","member_count":4}) if initial(task) else reply({"action":"submit_codex"})
            return reply()
        s=self.service(adapter);g=self.submit(s);self.wait(s,g)
        with self.assertRaises(ValueError):s.groups.adjust_seats(g,4,"more analysis")
        with self.assertRaises(ValueError):s.groups.adjust_seats(g,9,"more analysis",True)
        s.groups.adjust_seats(g,4,"approved division",True);state=self.wait(s,g)
        self.assertEqual(state["member_count"],4);self.assertEqual(len(state["roles"]),4)

    def test_leader_can_replace_low_quality_role_without_changing_seats(self):
        seen=[];reviews=[0]
        def adapter(route,prompt,task,ctx):
            if is_leader(task):
                if initial(task):return reply({"action":"continue_research"})
                reviews[0]+=1
                if reviews[0]==1:return reply({"action":"continue_research","replace_roles":["member-1"],
                    "feedback":[{"worker":1,"accepted":False,"reason":"unsupported claim"}]})
                return reply({"action":"submit_codex"})
            seen.append((route.source,task.kind));return reply()
        s=self.service(adapter);g=self.submit(s);state=self.wait(s,g)
        self.assertEqual(state["round"],2);self.assertEqual(state["member_count"],3)
        self.assertIn(routes()[1].id,state["blocked_routes"])
        self.assertEqual(sum(source=="fast" for source,_ in seen),1)

    def test_failed_source_is_replaced_and_successful_work_is_still_metered(self):
        failures=[]
        def adapter(route,prompt,task,ctx):
            if is_leader(task):return reply({"action":"continue_research" if initial(task) else "submit_codex"})
            if route.source=="fast":failures.append(route.id);raise WorkerFailure("quota_exhausted",scope="provider")
            return reply()
        s=self.service(adapter);g=self.submit(s);state=self.wait(s,g)
        self.assertTrue(failures)
        self.assertTrue(any(r.get("role")=="collector" and r.get("ok") and r["route"]["source"]!="fast" for r in s.store.job(g)["results"]))
        self.assertGreaterEqual(state["tokens_used"],40)

    def test_two_groups_have_isolated_budgets_and_scope_deduplication(self):
        s=self.service();one=self.submit(s,work_key="one");two=self.submit(s,work_key="two")
        self.wait(s,one);self.wait(s,two)
        duplicate=s.groups.submit({"topic":"fixture research","needs_web":False,"work_key":"one"})
        self.assertEqual(duplicate["group_id"],one);self.assertTrue(duplicate["deduplicated"])
        self.assertEqual(s.groups.status(one)["tokens_used"],40)
        self.assertEqual(s.groups.status(two)["tokens_used"],40)
        self.assertEqual(s.total_inflight,0)

    def test_cancel_preserves_actual_late_usage_and_does_not_revive_group(self):
        gate=threading.Event();entered=threading.Event();count=[0];lock=threading.Lock()
        def adapter(route,prompt,task,ctx):
            if is_leader(task):return reply({"action":"continue_research" if initial(task) else "submit_codex"})
            with lock:
                count[0]+=1
                if count[0]==2:entered.set()
            gate.wait(3);return reply()
        s=self.service(adapter);g=self.submit(s);self.assertTrue(entered.wait(2))
        s.groups.cancel(g);gate.set()
        time.sleep(.15);state=s.groups.status(g)
        self.assertEqual(state["state"],"cancelled")
        self.assertEqual((state["tokens_used"],state["tokens_reserved"]),(30,0))

    def test_restart_requires_explicit_resume_without_replaying_members(self):
        directory=self.root/"restart";s=self.service(directory=directory);g=self.submit(s);before=self.wait(s,g)
        s.shutdown();self.services.remove(s)
        member_calls=[]
        def adapter(route,prompt,task,ctx):
            if is_leader(task):return reply({"action":"submit_codex"})
            member_calls.append(route.id);return reply()
        restored=self.service(adapter,directory=directory)
        self.assertEqual(restored.groups.status(g)["state"],"interrupted")
        self.assertEqual(member_calls,[])
        restored.groups.resume(g,"review existing evidence");after=self.wait(restored,g)
        self.assertEqual(member_calls,[])
        self.assertEqual(after["tokens_used"],before["tokens_used"]+10)

    def test_explicit_small_group_budget_blocks_large_native_reservation(self):
        calls=[];s=self.service(lambda *a:calls.append(a));g=self.submit(s,token_budget=1000)
        state=self.wait(s,g)
        self.assertEqual(calls,[]);self.assertEqual(state["tokens_used"],0)
        self.assertEqual(state["tokens_reserved"],0)

    def test_group_uses_native_ceiling_above_ordinary_limit(self):
        caps=[]
        def adapter(route,prompt,task,ctx):
            caps.append(task.max_output_tokens)
            return reply({"action":"continue_research" if initial(task) else "submit_codex"}) if is_leader(task) else reply()
        s=self.service(adapter);g=self.submit(s,output_mode="native");self.wait(s,g)
        self.assertIn(100000,caps)
        self.assertTrue(all(cap in (4096,100000) for cap in caps))

    def test_initial_submit_codex_does_not_start_unnecessary_members(self):
        seen=[]
        def adapter(route,prompt,task,ctx):
            seen.append(task.prompt);return reply({"action":"submit_codex"})
        s=self.service(adapter);g=self.submit(s);state=self.wait(s,g)
        self.assertEqual(len(seen),1);self.assertEqual(state["tokens_used"],10)

    def test_partial_restart_marks_missing_evidence_and_never_claims_completion(self):
        directory=self.root/"partial";s=self.service(directory=directory)
        request=GroupRequest.parse({"topic":"partial fixture","needs_web":False})
        job=GroupJob(request);job.roles=s.groups.plan({"topic":"partial fixture","needs_web":False})["roles"]
        job.round=1;job.phase="research_round";job.state="running";job.expected_slots=[1,2]
        job.next_tasks=s.groups._seed_tasks(job);job.report_worker=2
        job.results=[{"worker":0,"slot":1,"role":"collector","round":1,"phase":"research_round","ok":True,
            "content":{"summary":"paid evidence","findings":[],"sources":[],"uncertainties":[],"complete":True},"route":routes()[1].public()}]
        job.calls=[{"worker":0,"slot":1,"role":"collector","round":1,"phase":"research_round"},
                   {"worker":1,"slot":2,"role":"analyst","round":1,"phase":"research_round"}]
        job.attempts=[{"worker":1,"route_id":routes()[0].id,"source":"smart","model":"strong","started_at":now_ms(),
            "state":"running","decision":{"reserved_token_estimate":100000}}]
        job.tokens_used=10;s._save(job);s.shutdown();self.services.remove(s)
        member_calls=[]
        def adapter(route,prompt,task,ctx):
            if not is_leader(task):member_calls.append(route.id)
            return reply({"action":"submit_codex"})
        restored=self.service(adapter,directory=directory);restored.groups.resume(job.id,"inspect partial evidence")
        state=self.wait(restored,job.id)
        self.assertEqual(member_calls,[]);self.assertEqual(state["pending_action"]["type"],"incomplete_member_evidence")
        self.assertEqual(state["tokens_estimated"],100000)
        with self.assertRaises(ValueError):restored.groups.complete(job.id,True)

    def test_failed_explicit_replacement_is_temporarily_avoided(self):
        s=self.service();job=GroupJob(GroupRequest.parse({"topic":"fixture","needs_web":False}))
        job.roles=s.groups.plan({"topic":"fixture","needs_web":False})["roles"];job.round=1
        old=job.roles[1]["route"]["route_id"]
        job.results=[{"worker":0,"slot":1,"round":1,"role":"collector","ok":False,"error":{"code":"output_truncated"}}]
        s.groups._apply_feedback(job,{"replace_roles":["member-1"]})
        self.assertGreater(job.avoid_until[old],now_ms());self.assertNotIn(old,job.blocked_routes)
        task=s.groups._task(job.request,kind="extract",prompt="fixture")
        self.assertFalse(any(c["route"].id==old for c in s.groups._candidates(task,job)))
        job.avoid_until[old]=now_ms()-1
        self.assertTrue(any(c["route"].id==old for c in s.groups._candidates(task,job)))

    def test_group_concurrency_keeps_one_global_slot_for_mainline(self):
        gate=threading.Event();three=threading.Event();count=[0];lock=threading.Lock();normal=threading.Event()
        def adapter(route,prompt,task,ctx):
            if getattr(task,"_research_group",False):
                with lock:
                    count[0]+=1
                    if count[0]==3:three.set()
                gate.wait(3)
            else:normal.set()
            return reply()
        candidates=routes()
        for r in candidates:r.max_parallel=10
        s=self.service(adapter,candidates);job=GroupJob(GroupRequest.parse({"topic":"fixture","needs_web":False}));job.state="running"
        task=s.groups._task(job.request,kind="extract",prompt="fixture")
        with ThreadPoolExecutor(max_workers=3) as pool:
            futures=[pool.submit(s._worker,job,task,i,job.deadline) for i in range(3)]
            self.assertTrue(three.wait(2))
            main=s.submit({"kind":"extract","prompt":"normal priority","max_output_tokens":256})
            self.assertTrue(normal.wait(1));gate.set()
            for f in futures:f.result()
        self.assertEqual(count[0],3)

    def test_cancel_between_approval_check_and_transition_cannot_revive_group(self):
        def adapter(route,prompt,task,ctx):return reply({"action":"request_seat_change","member_count":4})
        s=self.service(adapter);g=self.submit(s);self.wait(s,g)
        original=s.groups._approved_job
        def raced(*args):
            job=original(*args);s.groups.cancel(g);return job
        with patch.object(s.groups,"_approved_job",side_effect=raced),self.assertRaises(ValueError):
            s.groups.adjust_seats(g,4,"approved",True)
        self.assertEqual(s.groups.status(g)["state"],"cancelled")
        self.assertEqual(s.groups.status(g)["member_count"],3)

    def test_caller_member_handoff_is_metered_and_still_needs_leader_review(self):
        s=self.service();job=GroupJob(GroupRequest.parse({"topic":"fixture","needs_web":False}))
        job.state="needs_codex";job.round=1;job.phase="leader_review";job.expected_slots=[1]
        job.results=[{"worker":5,"slot":1,"round":1,"phase":"research_round","role":"collector","ok":False,"codex_fallback":{"recommended_model":"gpt-6-luna"}}]
        job.pending_action={"type":"incomplete_member_evidence"};s.groups.jobs[job.id]=job;s._save(job)
        content={"summary":"caller checked evidence","findings":[],"sources":[],"uncertainties":[],"complete":True}
        s.complete_codex(job.id,5,"gpt-6-luna",content,{"total_tokens":23})
        state=s.groups.status(job.id)
        self.assertEqual((state["state"],state["tokens_used"]),("needs_codex",23))
        self.assertEqual(state["pending_action"]["type"],"caller_member_completed")
        self.assertEqual(s.groups._missing_slots(job),[])

    def test_approval_reason_cannot_export_credentials(self):
        s=self.service()
        with self.assertRaises(ValueError):dispatch(s,"router_group_resume",{"group_id":"fixture","reason":"sk-"+"x"*32})

    def test_group_shared_reservation_cannot_start_two_when_only_one_fits(self):
        gate=threading.Event();entered=threading.Event();calls=[]
        def adapter(route,prompt,task,ctx):calls.append(route.id);entered.set();gate.wait(2);return reply()
        candidates=routes()
        for r in candidates:r.output_limit=256;r.max_parallel=10
        s=self.service(adapter,candidates);request=GroupRequest.parse({"topic":"fixture","needs_web":False,"token_budget":1800})
        job=GroupJob(request);job.state="running";task=s.groups._task(request,kind="extract",prompt="fixture")
        with ThreadPoolExecutor(max_workers=2) as pool:
            first=pool.submit(s._worker,job,task,0,job.deadline);self.assertTrue(entered.wait(1))
            second=pool.submit(s._worker,job,task,1,job.deadline)
            time.sleep(.1)
            self.assertFalse(second.done())
            self.assertEqual(len(calls),1)
            self.assertLessEqual(job.tokens_reserved,1800);gate.set();first.result()
            self.assertTrue(second.result(2)["ok"])
        self.assertEqual(len(calls),2);self.assertEqual((job.tokens_used,job.tokens_reserved),(20,0))

    def test_members_receive_original_topic_even_when_context_is_empty(self):
        seen=[]
        def adapter(route,prompt,task,ctx):
            if is_leader(task):return reply({"action":"continue_research" if initial(task) else "submit_codex"})
            seen.append(json.loads(task.context)["original_topic"]);return reply()
        s=self.service(adapter)
        g=s.groups.submit({"topic":"def original_source(): return 23","needs_web":False})["group_id"]
        self.wait(s,g)
        self.assertEqual(seen,["def original_source(): return 23"]*2)

    def test_saved_valid_leader_decision_is_reused_without_another_paid_call(self):
        s=self.service();job=GroupJob(GroupRequest.parse({"topic":"fixture","needs_web":False}))
        job.round=1;job.phase="leader_review";job.state="needs_codex";job.expected_slots=[]
        job.results=[{"worker":0,"slot":0,"role":"leader","round":1,"ok":True,
            "content":{"group_decision":{"action":"submit_codex","next_tasks":[]}}}]
        job.pending_action={"type":"invalid_leader_decision"};s.groups.jobs[job.id]=job;s._save(job)
        s.groups.resume(job.id,"revalidate saved output")
        self.assertEqual(s.groups.status(job.id)["pending_action"]["type"],"submit_codex")
        self.assertEqual(s.groups.status(job.id)["tokens_used"],0)
