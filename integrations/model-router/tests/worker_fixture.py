"""A deterministic external command adapter fixture, never a live model."""

import json
import sys

request = json.loads(sys.stdin.readline())
body = {"summary": "fixture response", "findings": [], "sources": [], "uncertainties": [], "complete": True}
print(json.dumps({"status": "SUCCESS", "model": request["model"], "response": json.dumps(body), "usage": {"total_tokens": 3}}), flush=True)
