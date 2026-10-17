import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from gtt_router.adapters import ExecutionContext, validate_result
from gtt_router.catalog import Catalog
from gtt_router.models import Route, Task, WorkerFailure
from gtt_router.native_agents import discover_opencode, opencode, print_agent, dsh_overlay, dsh
from gtt_router.registry import merge_registration


class NativeAgentTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix="gtt-native-")
        self.root=Path(self.temp.name)
        self.task=Task("extract","fixture",max_output_tokens=4096)
    def tearDown(self):
        self.temp.cleanup()
    def source(self,kind="opencode",scenario="ok"):
        return {"id":"native","command":[sys.executable,str(Path(__file__).with_name("native_agent_fixture.py")),kind,scenario],
                "discovery_dir":str(self.root/"discovery"),"mcp_names":["existing-mcp"],"max_parallel":1}
    def route(self,source,model="m2",provider="p1",adapter="opencode_http"):
        return Route(source="native",provider=provider,model=model,effort="max",runtime=source,adapter=adapter,max_parallel=1)
    def context(self,timeout=10):
        return ExecutionContext(threading.Event(),time.monotonic()+timeout,self.root/"work")
    def received(self):
        return json.loads(next(self.root.rglob("received.json")).read_text())
    def test_catalog_keeps_all_providers_and_models_not_only_default(self):
        rows=discover_opencode(self.source())
        self.assertEqual({(r["provider"],r["id"]) for r in rows},{("p1","m1"),("p1","m2"),("p2","m1")})
        self.assertEqual(next(r for r in rows if r["provider"]=="p1" and r["id"]=="m1")["effort"],"max")
    def test_explicit_model_variant_and_all_tool_denials_are_sent(self):
        result=validate_result(opencode(self.route(self.source()),"bounded input",self.task,self.context()),False)
        self.assertEqual(result["actual_selection"]["model"],"m2")
        self.assertEqual(result["usage"]["total_tokens"],16)
        body=self.received()
        self.assertEqual(body["model"],{"providerID":"p1","modelID":"m2"})
        self.assertEqual(body["variant"],"max")
        self.assertTrue(all(value is False for value in body["tools"].values()))
        self.assertFalse(body["tools"]["custom_tool"])
    def test_permission_overrides_are_verified_before_prompt(self):
        with self.assertRaises(WorkerFailure) as error:
            opencode(self.route(self.source(scenario="permission")),"input",self.task,self.context())
        self.assertEqual(error.exception.code,"native_permission_contract_failed")
        self.assertFalse(list(self.root.rglob("received.json")))
    def test_model_mismatch_tool_and_truncation_are_not_success(self):
        for scenario,code in [("mismatch","model_selection_mismatch"),("tool","unexpected_tool_call"),("truncate","output_truncated")]:
            with self.subTest(scenario=scenario),self.assertRaises(WorkerFailure) as error:
                opencode(self.route(self.source(scenario=scenario)),"input",self.task,self.context())
            self.assertEqual(error.exception.code,code)
    def test_cancellation_interrupts_pending_response_and_closes_process(self):
        context=self.context()
        timer=threading.Timer(.5,context.cancel.set);timer.start()
        try:
            with self.assertRaises(WorkerFailure) as error:
                opencode(self.route(self.source(scenario="pending")),"input",self.task,context)
            self.assertEqual(error.exception.code,"cancelled")
        finally:timer.join()
    def test_print_native_prompt_is_stdin_and_customizations_are_disabled(self):
        r=self.route(self.source("print"),model="m",adapter="claude_cli")
        result=validate_result(print_agent(r,"bounded-secret-free-input",self.task,self.context()),False)
        body=self.received()
        self.assertNotIn("bounded-secret-free-input",body["args"])
        self.assertEqual(body["prompt"].strip(),"bounded-secret-free-input")
        self.assertIn("--safe-mode",body["args"])
        self.assertIn("--strict-mcp-config",body["args"])
        self.assertEqual(body["args"][body["args"].index("--tools")+1],"")
        self.assertEqual(result["usage"]["total_tokens"],15)
    def test_print_tool_model_mismatch_or_turn_cutoff_is_not_success(self):
        for scenario,code in [("tool","unexpected_tool_call"),("mismatch","model_selection_mismatch"),("truncate","output_truncated")]:
            with self.subTest(scenario=scenario),self.assertRaises(WorkerFailure) as error:
                print_agent(self.route(self.source("print",scenario),model="m",adapter="codebuddy_cli"),"input",self.task,self.context())
            self.assertEqual(error.exception.code,code)
    def test_discovery_failure_is_isolated_to_one_source(self):
        config={"benchmark_file":str(self.root/"missing.json"),"ledger_path":str(self.root/"missing.db"),
                "sources":[self.source()|{"adapter":"opencode_http"},{"id":"other","adapter":"command_json","trusted":True,"command":[sys.executable],"models":[{"id":"m"}]}]}
        with patch("gtt_router.native_agents.discover_opencode",side_effect=WorkerFailure("native_connection_failed")):
            result=Catalog(config).refresh()
        self.assertEqual(len(result["routes"]),1)
        self.assertEqual(result["discovery_errors"][0]["reason"],"native_connection_failed")
    def test_dsh_disables_tools_but_keeps_factory_services_and_pins_model(self):
        entries=[{"id":i,"name":"@deepseek-ai/"+i} for i in ["agent-loop","agent-default-model","tools","tool-bash","tool-subagent","subagent","mcp-client"]]
        r=self.route({},model="deepseek-flash",provider="deepseek-official")
        overlay=dsh_overlay({"profile_entries":entries},r,self.root,4096)
        blocked={p["id"] for p in overlay if p.get("disabled")}
        self.assertTrue({"tool-bash","tool-subagent","mcp-client"}<=blocked)
        self.assertNotIn("subagent",blocked)
        main=next(p for p in overlay if p["id"]=="agent-loop")["config"]["agents"][0]
        self.assertEqual(main["model"],"deepseek-flash")
        self.assertEqual(main["maxTokens"],4096)
    def test_dsh_unknown_profile_cannot_silently_enable_tools(self):
        with self.assertRaises(WorkerFailure):
            dsh_overlay({"profile_entries":[{"id":"tools","name":"third-party"}]},self.route({}),self.root,4096)

    def test_registration_preserves_existing_manual_sources_and_limits(self):
        old={"sources":[{"id":"opencode","enabled":False,"command":["old"]},{"id":"custom","enabled":True}],
             "limits":{"global_parallel":4,"source_parallel":{"opencode":1}}}
        result=merge_registration(old,[{"id":"opencode","enabled":True,"command":["new"]},{"id":"claude","max_parallel":2}],[])
        self.assertFalse(result["sources"][0]["enabled"])
        self.assertEqual(result["sources"][0]["command"],["old"])
        self.assertEqual(result["limits"]["source_parallel"]["opencode"],1)
        self.assertEqual(result["limits"]["global_parallel"],4)
        self.assertEqual(len(result["sources"]),3)

    def test_dsh_final_and_reported_usage_are_parsed_and_tools_rejected(self):
        for scenario in ["ok","tool"]:
            source=self.source("dsh",scenario)
            source["profile_entries"]=[{"id":i,"name":"@deepseek-ai/"+i} for i in ["agent-loop","agent-default-model","tools","tool-bash"]]
            r=self.route(source,model="deepseek-flash",provider="deepseek-official",adapter="dsh_headless")
            if scenario=="tool":
                with self.assertRaises(WorkerFailure) as error:dsh(r,"input",self.task,self.context())
                self.assertEqual(error.exception.code,"unexpected_tool_call")
            else:
                result=validate_result(dsh(r,"input",self.task,self.context()),False)
                self.assertEqual(result["usage"]["total_tokens"],21)
                self.assertEqual(result["actual_selection"]["evidence"],"validated_process_overlay")

    def test_max_effort_profile_does_not_grade_a_lower_effort_route(self):
        bench=self.root/"benchmark.json"
        bench.write_text(json.dumps({"schema_version":1,"models":[{"aliases":["m"],"effort":"max","intelligence":51}]}))
        config={"benchmark_file":str(bench),"ledger_path":str(self.root/"missing.db"),"sources":[{"id":"s","adapter":"command_json","trusted":True,"command":[sys.executable],"models":[{"id":"m","effort":"low"}]}]}
        profile=Catalog(config).refresh()["routes"][0]["profile"]
        self.assertNotIn("intelligence",profile)
        self.assertTrue(profile["benchmark_effort_mismatch"])


if __name__=="__main__":
    unittest.main()
