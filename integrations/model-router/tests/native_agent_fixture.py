"""Synthetic native protocols; no credentials or real model requests."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sys

kind = sys.argv[1]
scenario = sys.argv[2]
report = {"summary": "fixture native result", "findings": [], "sources": [], "uncertainties": [], "complete": True}
if kind == "print":
    args = sys.argv[3:]
    model = args[args.index("--model") + 1]
    prompt = sys.stdin.read()
    Path("received.json").write_text(json.dumps({"args": args, "prompt": prompt}), encoding="utf-8")
    print(json.dumps({"type": "system", "subtype": "init", "model": model}), flush=True)
    if scenario == "tool":
        print(json.dumps({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash"}]}}), flush=True)
    print(json.dumps({"type": "result", "subtype": "error_max_turns" if scenario == "truncate" else "success",
                      "result": json.dumps(report), "modelUsage": {"other" if scenario == "mismatch" else model: {}},
                      "usage": {"input_tokens": 10, "output_tokens": 5}}), flush=True)
elif kind == "opencode":
    port = int(sys.argv[sys.argv.index("--port") + 1])
    overlay = json.loads(os.environ["OPENCODE_CONFIG_CONTENT"])
    selected = {}
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass
        def send(self, data, status=200):
            body = json.dumps(data).encode() if data is not None else b""
            self.send_response(status);self.end_headers();self.wfile.write(body)
        def do_GET(self):
            if self.path == "/global/health":self.send({"healthy": True})
            elif self.path == "/agent":
                rules = [{"permission": k, "pattern": "*", "action": v} for k,v in overlay["permission"].items()]
                if scenario == "permission":rules.append({"permission": "read", "pattern": "*", "action": "allow"})
                self.send([{"name": "gtt-bounded-worker", "mode": "primary", "steps": 1, "permission": rules}])
            elif self.path == "/config/providers":
                self.send({"providers": [{"id": "p1", "models": {"m1": {"variants": {"low": {}, "max": {}}}, "m2": {}, "retired": {"status": "deprecated"}}},
                                          {"id": "p2", "models": {"m1": {}, "disabled": {"disabled": True}, "image": {"capabilities": {"output": {"text": False}}}}}]})
            elif self.path == "/experimental/tool/ids":self.send(["read", "bash", "custom_tool"])
            elif self.path.startswith("/session/"):
                if scenario == "pending":self.send([]);return
                model = selected.get("model", {"providerID": "p1", "modelID": "m1"})
                parts = [{"type": "text", "text": json.dumps(report)}]
                if scenario == "tool":parts.append({"type": "tool", "tool": "read"})
                self.send([{"info": {"role": "assistant", "providerID": model["providerID"],
                                      "modelID": "other" if scenario == "mismatch" else model["modelID"],
                                      "time": {"completed": 1}, "finish": "length" if scenario == "truncate" else "stop",
                                      "tokens": {"input": 10, "output": 4, "reasoning": 2, "total": 16}}, "parts": parts}])
            else:self.send({},404)
        def do_POST(self):
            body=json.loads(self.rfile.read(int(self.headers.get("Content-Length","0"))))
            if self.path=="/session":self.send({"id":"ses_fixture"})
            else:
                selected.update(body)
                Path("received.json").write_text(json.dumps(body),encoding="utf-8")
                self.send(None,204)
    ThreadingHTTPServer(("127.0.0.1",port),Handler).serve_forever()
elif kind == "dsh":
    args=sys.argv[3:]
    overlay=json.loads(Path(args[args.index("--patch")+1]).read_text())
    Path("received.json").write_text(json.dumps({"overlay":overlay,"prompt":sys.stdin.read()}),encoding="utf-8")
    print(json.dumps({"type":"status","phase":"tools" if scenario=="tool" else "finished","usage":{"totalTokens":21}}),flush=True)
    print(json.dumps({"type":"final","text":json.dumps(report)}),flush=True)
