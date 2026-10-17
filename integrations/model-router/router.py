"""GTT coordinator entry point; no third-party Python packages are required."""

import argparse
import json
from pathlib import Path
import sys

from gtt_router.daemon import LocalClient, serve
from gtt_router.mcp import serve as serve_mcp


def main():
    parser = argparse.ArgumentParser(description="External-first model coordination via GTT, native adapters, MCP, and bounded Codex handoffs")
    parser.add_argument("command", choices=["mcp", "daemon", "call"])
    parser.add_argument("--config", default=str(Path.home() / ".globaltokentracker" / "router" / "config.json"))
    parser.add_argument("--method", default="router_inventory")
    parser.add_argument("--params-file", help="JSON argument file; keeps source excerpts out of process command lines")
    args = parser.parse_args()
    if args.command == "mcp":
        serve_mcp(args.config)
    elif args.command == "daemon":
        serve(args.config)
    else:
        params = json.loads(Path(args.params_file).read_text(encoding="utf-8-sig")) if args.params_file else {}
        print(json.dumps(LocalClient(args.config).call(args.method, params), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError) as error:
        print(type(error).__name__ + ": " + str(error)[:300], file=sys.stderr)
        sys.exit(1)
