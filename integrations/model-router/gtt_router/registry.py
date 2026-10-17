"""Discover installed agent entry points without importing/copying credentials."""
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.request

from .models import now_ms, stable_id
from .bridge import identifier
from .processes import background_options

DESKTOPS = {"opencode": "@opencode-aidesktop", "cursor": "cursor", "devin": "Devin", "qoder": "Qoder",
            "workbuddy": "WorkBuddy", "codebuddy_ide": "CodeBuddy CN", "dsh": "DSH Desktop",
            "lmstudio": "LM Studio", "anythingllm": "AnythingLLM", "zcode": "ZCode", "antigravity": "antigravity"}


def cli_output(command, cwd, env=None, timeout=20):
    result = subprocess.run(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout,
                            **background_options())
    if result.returncode:
        raise ValueError("native_metadata_command_failed")
    if len(result.stdout) + len(result.stderr) > 2097152:
        raise ValueError("native_metadata_output_limit")
    return result.stdout.decode("utf-8", "replace"), result.stderr.decode("utf-8", "replace")


def resolved_opencode(command, cwd):
    env=os.environ.copy()
    env["OPENCODE_CONFIG_CONTENT"]=json.dumps({"autoupdate":False,"permission":{"*":"deny"}})
    stdout,_=cli_output(command+["debug","config"],cwd,env)
    decoder=json.JSONDecoder()
    # Some native plugins write banners before the configuration JSON.
    for match in re.finditer(r"\{",stdout):
        try:
            value,_=decoder.raw_decode(stdout,match.start())
        except ValueError:
            continue
        if isinstance(value,dict) and "$schema" in value and "agent" in value:
            return {"default_model":value.get("model"),"mcp_names":list(value.get("mcp",{})),
                    "agent_roles":sorted(value.get("agent",{}))}
    raise ValueError("native_configuration_metadata_missing")


def discover_candidates(config, allow_paid_dsh=False):
    home=Path.home()
    programs=Path(os.environ.get("LOCALAPPDATA",str(home/"AppData/Local")))/"Programs"
    state=Path(config["state_dir"])
    work=state/"discovery/registration"
    work.mkdir(parents=True,exist_ok=True)
    sources=[]
    report={key:{"id":key,"installed":(programs/folder).is_dir(),"registered":False,"reason":"headless_interface_not_found"}
            for key,folder in DESKTOPS.items()}
    existing={source["id"] for source in config.get("sources",[]) if source.get("enabled",True)}
    for key in existing:
        report.setdefault(key,{"id":key,"installed":True})
        report[key].update(registered=True,reason="already_registered")
    opencode=home/".opencode/bin/opencode.exe"
    if opencode.is_file():
        command=[str(opencode)]
        meta=resolved_opencode(command,work)
        sources.append({"id":"opencode","adapter":"opencode_http","enabled":True,"command":command,
                        "discovery_dir":str(state/"discovery/opencode-catalog"),"pool_root":str(state/"native-worker-slots/opencode"),
                        "max_parallel":2,"quota_app":"",**meta})
        report["opencode"].update(installed=True,registered=True,reason="native_provider_catalog",agent_roles=len(meta["agent_roles"]))
    claude=home/".local/bin/claude.exe"
    if claude.is_file():
        report["claude"]={"id":"claude","installed":True,"registered":False,"reason":"native_auth_unavailable"}
        stdout,_=cli_output([str(claude),"auth","status","--json"],work)
        if json.loads(stdout).get("loggedIn") is True:
            models=[{"id":"opus","effort":"max"},{"id":"sonnet","effort":"high"},{"id":"haiku","effort":"high"}]
            receipt=state/"discovery/claude-models.json"
            if receipt.is_file():
                cached=json.loads(receipt.read_text(encoding="utf-8"))
                if now_ms()-cached.get("verified_at",0)<604800000 and isinstance(cached.get("models"),list):
                    models=cached["models"]
            sources.append({"id":"claude","adapter":"claude_cli","enabled":True,"command":[str(claude)],
                            "provider":"anthropic-account","quota_app":"claude","max_parallel":2,
                            "pool_root":str(state/"native-worker-slots/claude"),
                            "models":models})
            report["claude"].update(registered=True,reason="native_auth_detected")
    workbuddy=programs/"WorkBuddy/WorkBuddy.exe"
    if workbuddy.is_file():
        command=[str(workbuddy),str(workbuddy.parent/"resources/app.asar/cli/dist/codebuddy-headless.js")]
        env=os.environ.copy();env["ELECTRON_RUN_AS_NODE"]="1"
        stdout,stderr=cli_output(command+["--help"],work,env)
        text=stdout+stderr
        if "--tools" in text and "--strict-mcp-config" in text and "--model" in text:
            match=re.search(r"Currently supported:\s*\(([^)]+)\)",text)
            ids=[m.strip() for m in match.group(1).split(",")] if match else ["glm-5.1"]
            models=[{"id":m,"effort":"max","provider":"custom:"+stable_id(m) if m.startswith("custom-local:") else "codebuddy-account"}
                    for m in ids if m!="auto" and identifier(m)]
            sources.append({"id":"workbuddy","adapter":"codebuddy_cli","enabled":True,"command":command,
                            "process_env":{"ELECTRON_RUN_AS_NODE":"1"},"quota_app":"","max_parallel":2,
                            "pool_root":str(state/"native-worker-slots/workbuddy"),"models":models})
            report["workbuddy"].update(registered=True,reason="bundled_headless_cli",model_count=len(models))
    dsh=programs/"DSH Desktop/DSH Desktop.exe"
    if dsh.is_file() and allow_paid_dsh:
        command=[str(dsh),str(dsh.parent/"resources/app.asar/node_modules/@deepseek-ai/dsh/lib/bin.js")]
        process_env={"ELECTRON_RUN_AS_NODE":"1","DSH_HOME":str(Path(os.environ.get("APPDATA",str(home/"AppData/Roaming")))/"dsh-desktop/harness")}
        env=os.environ.copy();env.update(process_env)
        stdout,_=cli_output(command+["--profile","headless","--dump-config-schema"],work,env)
        schema=json.loads(stdout)
        rows=schema.get("x-cordis",{}).get("entries",[])
        entries=[{"id":r["id"],"name":r.get("name","")} for r in rows]
        if entries and all(row["name"].startswith("@deepseek-ai/") for row in entries):
            sources.append({"id":"dsh","adapter":"dsh_headless","enabled":True,"command":command,
                            "process_env":process_env,"profile_entries":entries,"max_parallel":2,"quota_app":"",
                            "pool_root":str(state/"native-worker-slots/dsh"),
                            "models":[{"id":"deepseek-flash","provider":"deepseek-official","effort":"max"},
                                      {"id":"deepseek-v4-pro","provider":"deepseek-official","effort":"max"}]})
            report["dsh"].update(registered=True,reason="bundled_headless_profile")
        else:report["dsh"]["reason"]="unverified_native_profile"
    elif dsh.is_file():
        report["dsh"].update(registered=False,reason="paid_source_requires_explicit_opt_in")
    report["codex"]={"id":"codex","installed":True,"registered":True,"reason":"caller_native_subagent_handoff"}
    for key in ["lmstudio","anythingllm"]:
        if report[key]["installed"]:
            port,path=(1234,"/v1/models") if key=="lmstudio" else (3001,"/api/ping")
            try:
                opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
                with opener.open("http://127.0.0.1:"+str(port)+path,timeout=1) as reply:reply.read(4096)
                report[key]["reason"]="local_service_requires_execution_adapter"
            except OSError:
                report[key]["reason"]="local_model_service_inactive"
    return sources,list(report.values())


def merge_registration(config, sources, inventory):
    updated=json.loads(json.dumps(config))
    original={source["id"]:source for source in updated.get("sources",[])}
    for source in sources:
        # Existing explicit registrations win; discovery never silently rewrites
        # their endpoints, models, permissions or user-chosen disabled state.
        original.setdefault(source["id"],source)
    updated["sources"]=list(original.values())
    updated["source_registry"]=inventory
    for item in updated["source_registry"]:
        if item["id"] in original and original[item["id"]].get("enabled",True) is False:
            item.update(registered=False,reason=original[item["id"]].get("disabled_reason","explicitly_disabled"))
    limits=updated.setdefault("limits",{})
    parallel=limits.setdefault("source_parallel",{})
    for source in sources:parallel.setdefault(source["id"],source.get("max_parallel",1))
    return updated
