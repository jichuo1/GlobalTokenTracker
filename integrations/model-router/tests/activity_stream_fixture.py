"""Local activity protocols; no real models, credentials or provider requests."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sys
import time

mode = sys.argv[1]
report = {"summary": "activity fixture", "findings": [], "sources": [], "uncertainties": [], "complete": True}


def emit(value):
    print(json.dumps(value), flush=True)


if mode.startswith("http_"):
    overlay = json.loads(os.environ["OPENCODE_CONFIG_CONTENT"])
    selected, started = {}, [None]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def send(self, value, status=200):
            data = json.dumps(value).encode() if value is not None else b""
            self.send_response(status)
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path == "/global/health":
                self.send({"healthy": True})
            elif self.path == "/agent":
                self.send([{"name": "gtt-bounded-worker", "mode": "primary", "steps": overlay["agent"]["gtt-bounded-worker"]["steps"],
                            "permission": [{"permission": k, "pattern": "*", "action": v}
                                           for k, v in overlay["permission"].items()]}])
            elif self.path == "/experimental/tool/ids":
                self.send(["read", "bash"])
            elif self.path.startswith("/session/"):
                age = time.monotonic() - (started[0] or time.monotonic())
                done = mode == "http_active" and age >= 1.6
                text = json.dumps(report) if done else ("x" * (int(age / .08) + 1) if mode == "http_active" else "fixed")
                model = selected["model"]
                self.send([{"info": {"id": "msg_activity", "role": "assistant", **model,
                                      "time": {"updated": time.time(), **({"completed": 1} if done else {})},
                                      "tokens": {"input": 10, "output": 2, "reasoning": 3, "total": 15}},
                            "parts": [{"id": "part_activity", "type": "text", "text": text}]}])
            else:
                self.send({}, 404)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
            if self.path == "/session":
                self.send({"id": "ses_activity"})
            else:
                selected.update(body)
                started[0] = time.monotonic()
                self.send(None, 204)

    port = int(sys.argv[sys.argv.index("--port") + 1])
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
elif mode == "zcode":
    request = json.loads(sys.stdin.readline())
    Path("native-timeout.json").write_text(json.dumps({"timeout_ms": int(os.environ["ZCODE_HTTP_TIMEOUT"])}))
    for index in range(16):
        emit({"method": "workspace/generateTextProgress", "params": {
            "operationId": request["params"]["operationId"], "text": "x" * (index + 1),
            "usage": {"inputTokens": 10, "outputTokens": index + 1}}})
        time.sleep(.08)
    emit({"id": request["id"], "result": {"selection": request["params"]["selection"],
        "text": json.dumps(report), "usage": {"inputTokens": 10, "outputTokens": 16}}})
elif mode == "agy":
    args = sys.argv[2:]
    Path("native-timeout.json").write_text(json.dumps({"timeout_s": int(args[args.index("--print-timeout") + 1][:-1])}))
    emit({"event": "init", "init": {"model": args[args.index("--model") + 1]}})
    json.loads(sys.stdin.readline())
    for index in range(16):
        emit({"event": "step_update", "step_update": {"step_id": "response", "step_type": "response",
                                                       "state": "RUNNING", "text": "x" * (index + 1)}})
        time.sleep(.08)
    emit({"event": "result", "result": {"status": "SUCCESS", "response": json.dumps(report),
                                          "usage": {"inputTokens": 10, "outputTokens": 16}}})
elif mode in {"print_active", "print_stall"}:
    args = sys.argv[2:]
    Path("print-args.json").write_text(json.dumps(args))
    model = args[args.index("--model") + 1]
    sys.stdin.read()
    emit({"type": "system", "subtype": "init", "model": model})
    emit({"type": "stream_event", "event": {"type": "message_start", "message": {"usage": {"input_tokens": 10}}}})
    for index in range(16 if mode == "print_active" else 1):
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0,
                                                 "delta": {"type": "thinking_delta", "thinking": "x"}}})
        emit({"type": "stream_event", "event": {"type": "message_delta", "usage": {"output_tokens": index + 1}}})
        time.sleep(.08)
    if mode == "print_stall":
        time.sleep(30)
    emit({"type": "result", "subtype": "success", "result": json.dumps(report), "modelUsage": {model: {}},
          "usage": {"input_tokens": 10, "output_tokens": 16}})
else:
    for index in range(16):
        if mode == "output":
            emit({"type": "assistant", "message": {"id": "m", "content": [{"type": "text", "text": "x" * (index + 1)}]}})
        elif mode == "thinking":
            emit({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "sequence": index,
                                                     "delta": {"type": "thinking_delta", "thinking": "x"}}})
        elif mode == "heartbeat":
            emit({"type": "status", "state": "thinking", "timestamp": time.time(), "process_alive": True})
        elif mode == "repeat":
            emit({"event": "step_update", "step_update": {"step_id": "step", "step_type": "thinking", "state": "RUNNING"}})
        elif mode == "empty":
            emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"text": ""}, "sequence": index}})
        time.sleep(.08)
    if mode in {"output", "thinking"}:
        emit({"type": "final", "text": json.dumps(report)})
    else:
        time.sleep(30)
