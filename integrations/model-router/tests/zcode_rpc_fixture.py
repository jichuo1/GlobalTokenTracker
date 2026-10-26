"""Synthetic RPC response: no provider, credentials or generation."""
import json
import sys

request = json.loads(sys.stdin.readline())
scenario = sys.argv[1] if len(sys.argv)>1 else "truncate"
cap = request["params"]["maxOutputTokens"]
if scenario != "truncate":
    selection = dict(request["params"]["selection"])
    if scenario == "canonical":
        selection["options"] = dict(selection.get("options", {})) | {"maxOutputTokens":cap, "vendor_private":"must-not-export"}
    if scenario == "wrong": selection["modelId"] = "other-model"
    if scenario == "messages":
        assert "prompt" not in request["params"] and request["params"]["messages"][0]["role"] == "user"
    report={"summary":"fixture complete","findings":[],"sources":[],"uncertainties":[],"complete":True}
    print(json.dumps({"id":request["id"],"result":{"selection":selection,"finishReason":"stop","text":json.dumps(report),
        "usage":{"inputTokens":{"total":10,"cacheRead":2},"outputTokens":{"total":5,"reasoning":1},"totalTokens":15}}}),flush=True)
    sys.exit(0)
print(json.dumps({"id": request["id"], "result": {
    "selection": request["params"]["selection"], "finishReason": "length", "text": "",
    "usage": {"inputTokens": 10, "outputTokens": cap, "reasoningTokens": cap, "totalTokens": cap + 10},
}}), flush=True)
