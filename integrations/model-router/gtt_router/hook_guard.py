"""Antigravity PreToolUse gate; runs locally before native tool execution."""

import ipaddress
import json
from pathlib import Path
import sys
import re
from urllib.parse import urlsplit


def public_url(url):
    try:
        if any(ord(c) < 32 for c in url):
            return False
        parsed = urlsplit(url)
        host = parsed.hostname.rstrip(".").lower() if parsed.hostname else None
        if parsed.scheme not in {"http", "https"} or not host or parsed.username or parsed.password:
            return False
        if host in {"localhost", "metadata.google.internal"} or host.startswith("localhost.") or host.endswith((".localhost", ".local", ".internal")) or "." not in host:
            return False
        try:
            return ipaddress.ip_address(host).is_global
        except ValueError:
            return True
    except ValueError:
        return False


def decide(payload, web, audit=None):
    call = payload.get("toolCall", {})
    tool = call.get("name", "")
    if web and audit and Path(audit).is_file() and tool in {"search_web", "read_url_content", "view_file"}:
        policy_path = Path(audit).parent / "web-policy.json"
        policy = json.loads(policy_path.read_text(encoding="utf-8")) if policy_path.exists() else {}
        prior = [json.loads(line) for line in Path(audit).read_text(encoding="utf-8").splitlines()]
        used = sum(r.get("phase", "pre") == "pre" and r.get("decision") == "allow" and r.get("tool") in {"search_web", "read_url_content", "view_file"} for r in prior)
        if used >= policy.get("web_tool_budget", 6):
            return "deny", tool
    if tool in {"finish", "wait", "wait_5_seconds"}:
        return "allow", tool
    if web and tool == "search_web":
        return "allow", tool
    if web and tool == "read_url_content":
        values = call.get("args", {})
        url = values.get("Url") or values.get("url") or values.get("URL")
        return ("allow" if isinstance(url, str) and public_url(url) else "deny"), tool
    if web and tool == "view_file" and audit and Path(audit).is_file():
        absolute = call.get("args", {}).get("AbsolutePath")
        if isinstance(absolute, str):
            target = Path(absolute).resolve()
            for line in Path(audit).read_text(encoding="utf-8").splitlines():
                entry = json.loads(line)
                allowed_path = entry.get("excerpt_file") or entry.get("content_file")
                if entry.get("phase") == "post" and entry.get("tool") == "read_url_content" and entry.get("success") and allowed_path and target == Path(allowed_path).resolve():
                    if not entry.get("excerpt_file"):
                        first = call.get("args", {}).get("StartLine", 1)
                        last = call.get("args", {}).get("EndLine")
                        if not isinstance(first, int) or not isinstance(last, int) or not 0 < last - first + 1 <= 120:
                            return "deny", tool
                    return "allow", tool
    return "deny", tool


def excerpt(text, terms, max_chars=6000):
    lines = text.splitlines()
    hits = [i for i, line in enumerate(lines) if any(term.lower() in line.lower() for term in terms)] if terms else []
    selected = set()
    for i in hits:
        selected.update(range(max(0, i - 3), min(len(lines), i + 5)))
    if not selected:
        selected.update(range(min(120, len(lines))))
    output = ["Bounded excerpt: omitted lines are not represented. Original line numbers follow."]
    size = len(output[0])
    for i in sorted(selected):
        row = f"[{i + 1}] {lines[i]}"
        if size + len(row) > max_chars:
            break
        output.append(row)
        size += len(row) + 1
    return "\n".join(output)


def main():
    try:
        raw = sys.stdin.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise ValueError("oversize hook payload")
        payload = json.loads(raw)
        audit = Path(sys.argv[2])
        if sys.argv[1] == "guide":
            entries = [json.loads(line) for line in audit.read_text(encoding="utf-8").splitlines()] if audit.exists() else []
            seen_path = audit.parent / "guided-excerpts.json"
            seen = json.loads(seen_path.read_text(encoding="utf-8")) if seen_path.exists() else []
            new = [a for a in entries if a.get("excerpt_file") and a["excerpt_file"] not in seen]
            for a in new:
                seen.append(a["excerpt_file"])
            seen_path.write_text(json.dumps(seen), encoding="utf-8")
            messages = [{"ephemeralMessage": "Use view_file only on this bounded webpage excerpt: " + a["excerpt_file"] + ", lines 1 through " + str(a.get("excerpt_lines", 100)) + ". Source URL: " + a["url"] + ". Read it once, then finish the requested short JSON report if sufficient."} for a in new[-2:]]
            print(json.dumps({"injectSteps": messages}))
            return
        post = sys.argv[1] == "post"
        decision, tool = decide(payload, sys.argv[1] == "web", audit)
        if post:
            tool = payload.get("toolCall", {}).get("name", "unknown")
    except (ValueError, KeyError, IndexError, TypeError):
        decision, tool, post = "deny", "unknown", False
    audit = Path(sys.argv[2])
    record = {"tool": tool, "decision": decision, "phase": "post" if post else "pre"}
    if post:
        record.pop("decision")
        record["success"] = not bool(payload.get("error"))
        args = payload.get("toolCall", {}).get("args", {})
        if tool == "view_file" and record["success"] and isinstance(args.get("AbsolutePath"), str):
            record["viewed_file"] = str(Path(args["AbsolutePath"]).resolve())
        url = args.get("Url") or args.get("url") or args.get("URL")
        if tool == "read_url_content" and record["success"] and isinstance(url, str) and public_url(url):
            record["url"] = url
            artifact = payload.get("artifactDirectoryPath")
            step = payload.get("stepIdx")
            if isinstance(artifact, str) and isinstance(step, int) and step >= 0:
                record["content_file"] = str(Path(artifact) / ".system_generated" / "steps" / str(step) / "content.md")
                cached = Path(record["content_file"])
                base = Path.home() / ".gemini" / "antigravity-cli" / "brain"
                try:
                    cached.resolve().relative_to(base.resolve())
                    if cached.is_file() and cached.stat().st_size <= 4 * 1024 * 1024:
                        policy_file = audit.parent / "web-policy.json"
                        policy = json.loads(policy_file.read_text(encoding="utf-8")) if policy_file.exists() else {}
                        derived = cached.with_name("gtt_excerpt.md")
                        clipped = "Source: " + url + "\n" + excerpt(cached.read_text(encoding="utf-8", errors="replace"), policy.get("focus_terms", []))
                        derived.write_text(clipped, encoding="utf-8")
                        record["excerpt_file"] = str(derived)
                        record["excerpt_lines"] = len(clipped.splitlines())
                except (ValueError, OSError):
                    pass
    with audit.open("a", encoding="utf-8") as file:
        file.write(json.dumps(record) + "\n")
    print(json.dumps({} if post else {"decision": decision, "reason": "GTT coordinator permits only this task's bounded read tools"}))


if __name__ == "__main__":
    main()
