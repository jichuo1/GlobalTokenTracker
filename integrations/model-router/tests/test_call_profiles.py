import copy
import threading
import time
import unittest

import test_router as fixtures
from gtt_router.models import Task, WorkerFailure
from gtt_router.profiles import validate, presets
from gtt_router.scheduling import settings
from gtt_router.slicing import output_budget


class CallProfileTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service
    wait = fixtures.RouterTests.wait

    def apply(self, service, value):
        prior = settings(service.store)
        return service.scheduling.control("save", value=prior | {"call_profile":value}, expected_revision=prior["revision"])

    def test_all_presets_validate_and_do_not_accept_credentials_or_commands(self):
        for preset in presets(): validate(preset["value"])
        for value in [{"source_options":{"native":{"command":["anything"]}}}, {"limits":{"max_output_tokens":True}},
                      {"defaults":{"common":{"prompt":"injected"}}}, {"parallel":65}, {"limits":{"max_output_tokens":float("inf")}}]:
            with self.assertRaises(ValueError): validate(value)

    def test_task_overrides_preset_defaults_and_effective_policy_is_visible(self):
        service=self.service([fixtures.route()],lambda *a:fixtures.response())
        reply=self.apply(service,{"preset":"balanced","defaults":{"common":{"deadline_s":240}}})
        self.assertEqual(service.parse_task({"kind":"extract","prompt":"fixture"}).deadline_s,240)
        self.assertEqual(service.parse_task({"kind":"extract","prompt":"fixture","deadline_s":30}).deadline_s,30)
        self.assertEqual(reply["effective_profile"]["defaults"]["common"]["deadline_s"],240)

    def test_large_output_report_and_wait_limits_require_explicit_policy(self):
        r=fixtures.route();r.output_limit=2000000
        service=self.service([r],lambda *a:fixtures.response())
        request={"kind":"extract","prompt":"fixture","max_output_tokens":1048576,"report_max_chars":8388608,"token_budget":5000000,"deadline_s":1800,"hard_deadline_s":7200}
        with self.assertRaises(ValueError):service.parse_task(request)
        self.apply(service,{"limits":{"max_output_tokens":1048576,"max_report_chars":8388608,"max_task_tokens":8000000,"max_deadline_s":3600,"max_protocol_frame_bytes":67108864}})
        task=service.parse_task(request)
        self.assertEqual(output_budget(task,r)["effective_output_tokens"],1048576)
        r.output_limit=524288
        self.assertEqual(output_budget(task,r)["effective_output_tokens"],524288)
        self.assertEqual(task.report_max_chars,8388608)

    def test_source_options_reach_generation_and_recovery_without_mutating_catalog(self):
        r=fixtures.route();observed=[]
        def adapter(route,*args):observed.append(copy.deepcopy(route.runtime));return fixtures.response("GTT_RECOVERY_OK")
        service=self.service([r],adapter)
        self.apply(service,{"source_options":{r.source:{"agent_steps":5,"http_timeout_s":10}},"recovery":{"max_probes_per_hour":1}})
        job=service.submit({"kind":"extract","prompt":"fixture","max_output_tokens":1000,"token_budget":5000,"max_attempts":1,"allow_fallback":False})
        self.assertEqual(self.wait(service,job["job_id"])["state"],"succeeded")
        self.assertTrue(service.recovery.recheck(r.id)["success"])
        self.assertTrue(all(item["gtt_call_options"]["agent_steps"]==5 for item in observed))
        self.assertNotIn("gtt_call_options",r.runtime)
        self.assertTrue(service.recovery.recheck(r.id)["success"])
        self.assertEqual(service.store.probe_budget(0)["count"], 2)

    def test_coordinator_and_native_capacity_can_grow_and_shrink_without_replay(self):
        r=fixtures.route();lock=threading.Lock();both=threading.Event();active=0;peak=0;calls=0;parallel=True
        def adapter(route,*args):
            nonlocal active,peak,calls
            with lock:
                active+=1;calls+=1;peak=max(peak,active)
                if active==2:both.set()
            if parallel:both.wait(2)
            else:time.sleep(.03)
            with lock:active-=1
            return fixtures.response()
        service=self.service([r],adapter)
        self.apply(service,{"coordinator_threads":2,"parallel":2,"source_parallel":{r.source:2}})
        job=service.submit({"kind":"extract","prompt":"fixture","fanout":2,"verify":False,"max_attempts":2,"max_output_tokens":1000,"token_budget":10000,"deadline_s":5})
        self.assertEqual(self.wait(service,job["job_id"])["state"],"succeeded")
        self.assertEqual(peak,2);self.assertEqual(calls,2)
        parallel=False;peak=0
        self.apply(service,{"coordinator_threads":1,"parallel":1,"source_parallel":{r.source:1}})
        jobs=[service.submit({"kind":"extract","prompt":str(i),"max_attempts":1,"max_output_tokens":1000,"token_budget":5000,"deadline_s":5}) for i in range(3)]
        self.assertTrue(all(self.wait(service,j["job_id"])["state"]=="succeeded" for j in jobs))
        self.assertEqual(peak,1);self.assertEqual(calls,5)
        self.assertEqual(service.applied_coordinator_threads,1)

    def test_proven_pre_generation_failure_creates_no_estimated_bill(self):
        r=fixtures.route()
        def adapter(*args):raise WorkerFailure("native_configuration_error",scope="source",metering={"generation_not_started":True})
        service=self.service([r],adapter)
        job=service.submit({"kind":"extract","prompt":"fixture","max_attempts":1,"allow_fallback":False})
        result=self.wait(service,job["job_id"])
        self.assertEqual(result["tokens_used"],0);self.assertEqual(result["tokens_reserved"],0)
        self.assertTrue(result["attempts"][0]["generation_not_started"])
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0],0)

    def test_profile_can_expand_workflow_capacity_while_drafts_remain_saveable(self):
        from gtt_router.scheduling import graph
        r=fixtures.route();service=self.service([r],lambda *a:fixtures.response())
        empty={"id":"draft","name":"draft","nodes":[],"edges":[],"luna_supervision":False}
        prior=settings(service.store)
        service.scheduling.control("save",value=prior|{"workflows":[empty]},expected_revision=prior["revision"])
        self.assertEqual(len(settings(service.store)["workflows"]),1)
        with self.assertRaises(ValueError):graph(empty)
