"""Validate the stdio wire protocol with an independently installed MCP SDK."""

import asyncio
import json
from pathlib import Path
import sys

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

root = Path(__file__).resolve().parents[1]
state = Path.home() / ".globaltokentracker" / "router"
config = state / "config.json"
system_python = Path.home() / "AppData" / "Local" / "Programs" / "Python" / "Python313" / "python.exe"


def body(result):
    if getattr(result, "is_error", getattr(result, "isError", False)):
        raise ValueError(result.content[0].text)
    return getattr(result, "structured_content", getattr(result, "structuredContent", None)) or json.loads(result.content[0].text)


async def main():
    parameters = StdioServerParameters(command=str(system_python), args=[str(root / "router.py"), "mcp", "--config", str(config)])
    async with stdio_client(parameters) as (read_a, write_a):
        async with ClientSession(read_a, write_a) as a:
            await a.initialize()
            names = [t.name for t in (await a.list_tools()).tools]
            assert len(names) == 10 and len(set(names)) == 10
            first = body(await a.call_tool("router_inventory", {}))
            async with stdio_client(parameters) as (read_b, write_b):
                async with ClientSession(read_b, write_b) as b:
                    await b.initialize()
                    second = body(await b.call_tool("router_inventory", {}))
                    assert [r["route_id"] for r in first["routes"]] == [r["route_id"] for r in second["routes"]]
                    # Use an existing synthetic live job to test shared state,
                    # without spending tokens on another model request.
                    live_path = state / "live-validation.json"
                    shared = None
                    if live_path.exists():
                        live = json.loads(live_path.read_text(encoding="utf-8"))
                        shared = next(iter(live["checks"].values()))["job_id"]
                    if shared:
                        left = body(await a.call_tool("router_status", {"job_id": shared}))
                        right = body(await b.call_tool("router_status", {"job_id": shared}))
                        assert left["job_id"] == right["job_id"] == shared
                    else:
                        raise ValueError("Run live_probe.py first so shared job identity can be validated")
            # Closing one MCP client must not terminate the shared resident service.
            after = body(await a.call_tool("router_inventory", {}))
            assert len(after["routes"]) == len(first["routes"])
    report = {"ok": True, "client_count": 2, "tool_count": len(names), "shared_job_id": shared,
              "route_count": len(first["routes"]), "transport": "official MCP Python SDK stdio client"}
    (state / "mcp-validation.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report))


asyncio.run(main())
