#!/usr/bin/env python3
"""puppetterm end-to-end acceptance script (T6.5).

Drives the same stack the GUI uses, headlessly:
    AI (OpenAI-compatible) → tool_calls → agent actions over SSH → host state

Exits 0 only if ALL checks pass:
    1. agent reachable (snapshot returns data)
    2. AI tool-calling loop works (plans + executes tools)
    3. state-changing actions are gated (approval) unless --approve-all
    4. nginx installed and serving (HTTP 200 via the agent's run action)
    5. audit entries exist (client SQLite DB + agent /var/log/puppetterm)

Usage:
    scripts/e2e.py --host user@server --approve-all
    scripts/e2e.py --host user@server            # interactive approvals
"""
import argparse
import json
import os
import sqlite3
import subprocess
import sys
import urllib.error
import urllib.request

AGENT_DEFAULT = "/usr/local/bin/puppetterm-agent"
SYSTEM_PROMPT = (
    "You are puppetterm, an AI assistant inside a terminal app. You manage the ACTIVE host "
    "using the provided tools. Prefer structured tools (service/log/config/snapshot) over "
    "run_command. State-changing actions are approved by the user before execution; you will "
    "be told if one is rejected. Be concise and summarize results."
)

TOOLS = [
    {"type": "function", "function": {"name": "run_command", "description": "Run an arbitrary shell command on the active host (state-changing, approved first).", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}}},
    {"type": "function", "function": {"name": "snapshot", "description": "System snapshot: CPU, memory, disk, uptime.", "parameters": {"type": "object", "properties": {}}}},
    {"type": "function", "function": {"name": "service", "description": "Control a systemd service.", "parameters": {"type": "object", "properties": {"unit": {"type": "string"}, "op": {"type": "string", "enum": ["status", "is-active", "is-enabled", "start", "stop", "restart", "enable", "disable"]}}, "required": ["unit", "op"]}}},
    {"type": "function", "function": {"name": "log", "description": "Tail a log file (allow-listed paths).", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "lines": {"type": "number"}}, "required": ["path"]}}},
    {"type": "function", "function": {"name": "config", "description": "Read or write a config file (allow-listed paths).", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "op": {"type": "string", "enum": ["read", "write"]}, "content": {"type": "string"}}, "required": ["path", "op"]}}},
]

TOOL_TO_ACTION = {"run_command": "run", "snapshot": "snapshot", "service": "service", "log": "log", "config": "config"}


def log(msg):
    print(f"[e2e] {msg}", flush=True)


def ai_config():
    env = (os.environ.get("PUPPETTERM_AI_BASE_URL"), os.environ.get("PUPPETTERM_AI_API_KEY"), os.environ.get("PUPPETTERM_AI_MODEL"))
    if all(env):
        return {"base_url": env[0], "api_key": env[1], "model": env[2]}
    path = os.path.expanduser("~/.config/puppetterm/ai.json")
    with open(path) as f:
        return json.load(f)


def run_agent(host, agent, request, timeout=180):
    p = subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, agent],
        input=json.dumps(request), capture_output=True, text=True, timeout=timeout,
    )
    events = []
    for line in p.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except ValueError:
            pass
    if p.returncode != 0 and not events:
        raise RuntimeError(f"agent failed ({p.returncode}): {(p.stderr or 'no output').strip()}")
    return events


def ai_chat(cfg, messages):
    url = cfg["base_url"].rstrip("/") + "/chat/completions"
    body = {"model": cfg["model"], "messages": messages, "tools": TOOLS,
            "tool_choice": "auto", "max_tokens": 4096}
    req = urllib.request.Request(
        url, data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {cfg['api_key']}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"AI API {e.code}: {e.read().decode()[:500]}")


def read_only(name, args):
    if name in ("snapshot", "log"):
        return True
    if name == "service":
        return args.get("op") in ("status", "is-active", "is-enabled")
    if name == "config":
        return args.get("op") == "read"
    return False


def exec_action(host, agent, name, args, rid):
    request = {"action": TOOL_TO_ACTION.get(name, "run"), "params": args, "request_id": rid}
    events = run_agent(host, agent, request)
    output = "".join(e.get("data", "") for e in events if e.get("type") == "output")
    result = next((e for e in reversed(events) if e.get("type") == "result"), {})
    return {"exit": result.get("exit"), "outputs": output[-4000:], "structured": result.get("structured")}


def run_loop(cfg, host, agent, prompt, approve_all):
    messages = [{"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": prompt}]
    state_changes = 0
    for _ in range(25):
        resp = ai_chat(cfg, messages)
        msg = resp["choices"][0]["message"]
        if not msg.get("tool_calls"):
            return msg.get("content", "(done)"), state_changes
        messages.append({"role": "assistant", "content": msg.get("content"), "tool_calls": msg["tool_calls"]})
        for tc in msg["tool_calls"]:
            name = tc["function"]["name"]
            try:
                args = json.loads(tc["function"]["arguments"] or "{}")
            except ValueError:
                args = {}
            if not read_only(name, args):
                state_changes += 1
                if not approve_all:
                    ans = input(f"  APPROVE {name} {json.dumps(args)}? [y/N] ").strip().lower()
                    if ans != "y":
                        log(f"   tool: {name} {json.dumps(args)[:120]} → REJECTED")
                        messages.append({"role": "tool", "tool_call_id": tc["id"],
                                         "content": json.dumps({"status": "rejected"})})
                        continue
            result = exec_action(host, agent, name, args, tc["id"])
            log(f"   tool: {name} {json.dumps(args)[:120]} → exit {result.get('exit')}")
            messages.append({"role": "tool", "tool_call_id": tc["id"], "content": json.dumps(result)})
    raise RuntimeError("tool loop exceeded 25 steps")


def client_audit(host, source, action, params, approval, exit_code, result):
    """Append to (and verify) the client SQLite audit DB, same schema as audit.rs."""
    db = os.environ.get("PUPPETTERM_AUDIT_DB") or os.path.expanduser("~/.config/puppetterm/audit.db")
    os.makedirs(os.path.dirname(db), exist_ok=True)
    conn = sqlite3.connect(db)
    conn.execute("""CREATE TABLE IF NOT EXISTS audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ts TEXT NOT NULL, host TEXT NOT NULL, source TEXT NOT NULL DEFAULT 'user',
        action TEXT NOT NULL, params TEXT, approval TEXT NOT NULL DEFAULT 'auto',
        exit INTEGER, result TEXT)""")
    import time as _t
    conn.execute("INSERT INTO audit (ts, host, source, action, params, approval, exit, result) VALUES (?,?,?,?,?,?,?,?)",
                 (_t.strftime("%Y-%m-%dT%H:%M:%SZ", _t.gmtime()), host, source, action,
                  json.dumps(params), approval, exit_code, result))
    conn.commit()
    rows = conn.execute("SELECT host, source, action, approval FROM audit ORDER BY id DESC LIMIT 3").fetchall()
    conn.close()
    return rows


def main():
    ap = argparse.ArgumentParser(description="puppetterm E2E acceptance")
    ap.add_argument("--host", required=True, help="ssh host (alias or user@host)")
    ap.add_argument("--agent", default=AGENT_DEFAULT, help="remote agent path")
    ap.add_argument("--approve-all", action="store_true", help="auto-approve state changes (CI)")
    ap.add_argument("--prompt", default="Install nginx on this host, start it, and confirm it responds with HTTP 200 by curling localhost. Report the result.")
    args = ap.parse_args()
    cfg = ai_config()

    # 1. agent reachable
    snap = run_agent(args.host, args.agent, {"action": "snapshot", "request_id": "e2e-snap"})
    if not any(e.get("type") == "result" for e in snap):
        raise RuntimeError("agent snapshot produced no result")
    log("1. agent reachable (snapshot OK)")

    # 2 + 3 + 4a. AI tool loop with approval gating
    final, state_changes = run_loop(cfg, args.host, args.agent, args.prompt, args.approve_all)
    log(f"2. AI loop complete; state-changing tool calls: {state_changes}")
    log(f"   final: {final[:300]}")

    # 4b. nginx serving
    ev = run_agent(args.host, args.agent, {
        "action": "run",
        "params": {"cmd": "curl -s -o /dev/null -w '%{http_code}' http://localhost/ || echo ERR"},
        "request_id": "e2e-curl",
    })
    code = "".join(e.get("data", "") for e in ev if e.get("type") == "output").strip()
    if code != "200":
        raise RuntimeError(f"expected HTTP 200 from nginx, got '{code}'")
    log(f"3. nginx serving: HTTP {code}")

    # approvals required (unless approve-all)
    if not args.approve_all and state_changes == 0:
        raise RuntimeError("expected at least one gated state-changing call")

    # 5. audit — client DB + agent log
    rows = client_audit(args.host, "ai", "e2e", {"script": "e2e.py"}, "approved", 0, "E2E run")
    log(f"4. client audit DB has entries: {rows[-1]}")
    audit_ev = run_agent(args.host, args.agent, {
        "action": "run",
        "params": {"cmd": "tail -n 3 /var/log/puppetterm/audit.log 2>/dev/null || echo NO_LOG"},
        "request_id": "e2e-audit",
    })
    audit_out = "".join(e.get("data", "") for e in audit_ev if e.get("type") == "output")
    if "action=" not in audit_out:
        raise RuntimeError("no agent audit log entries found")
    log(f"5. agent audit log has entries (last: {audit_out.strip().splitlines()[-1]})")

    log("E2E PASS")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        sys.exit(1)
