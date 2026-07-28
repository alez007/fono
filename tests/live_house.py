#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Close the real loop: spoken words -> model picks a tool -> the call fires
at a REAL Home Assistant -> a physical light changes -> we read the new state
back and check it.

Everything else in the test suite scores against fixtures, which cannot catch
the failures that actually bite: a model that picks the brightness tool when
asked simply to switch a light on, a room name that exists in the prompt but
not in the house, an argument the server rejects. Those need a real house.

This is deliberately *not* wired into `tests/check.sh` — it needs a Home
Assistant, a running model server, and it moves real lights, none of which
belong in CI. It lives here so it survives, and so the next person debugging
a smart-home failure has something to reach for.

Usage
-----
    HA_BASE=http://homeassistant.local:8123 \\
    python3 tests/live_house.py http://127.0.0.1:18099/v1/chat/completions \\
            --model my-model --area kitchen

    --area NAME    room to test in (default: kitchen)
    --lang en|ro   run only one language (default: both)
    --reasoning    leave the model's thinking on (default: off, as Fono ships)

Requires `HOMEASSISTANT_TOKEN` in `tests/secrets.toml`, and `HA_BASE` (or
`--base`) pointing at the Home Assistant instance.
"""

import argparse
import json
import os
import queue
import re
import sys
import threading
import time
import tomllib
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
SECRETS = HERE / "secrets.toml"
MAX_STEPS = 5

# The prompts Fono ships, so the live loop measures production behaviour
# rather than something invented for the test.
SYS = {
    "en": (
        "Fono voice assistant. Use the tool only for clear light on/off commands. "
        "If the room/light is missing, ask briefly; do not call the tool. "
        "Confirm briefly after the result."
    ),
    "ro": (
        "Fono voice assistant. Folosește unealta numai pentru comenzi clare de "
        "aprindere/stingere lumini. Dacă lipsește camera/lampa, întreabă scurt; "
        "nu apela unealta. Confirmă scurt după rezultat."
    ),
}

# (language, spoken words, expected end state). Romanian is not decoration:
# every smart-home failure worth fixing so far has been a model routing a
# non-English command to the wrong tool.
COMMANDS = [
    ("en", "turn on the {area} lights", "on"),
    ("en", "turn off the {area} lights", "off"),
    ("ro", "aprinde luminile din {area}", "on"),
    ("ro", "stinge luminile din {area}", "off"),
]


def ha_token() -> str:
    if not SECRETS.exists():
        raise SystemExit(f"missing {SECRETS} — needs keys.HOMEASSISTANT_TOKEN")
    with SECRETS.open("rb") as fh:
        keys = tomllib.load(fh).get("keys", {})
    tok = keys.get("HOMEASSISTANT_TOKEN")
    if not tok:
        raise SystemExit(f"{SECRETS} has no keys.HOMEASSISTANT_TOKEN")
    return str(tok)


class SseMcp:
    """Minimal MCP client over the SSE transport.

    Home Assistant's `mcp_server` integration serves SSE, not streamable
    HTTP: a GET opens an event stream whose first event carries the POST
    endpoint, and JSON-RPC *responses arrive back on the stream* rather than
    on the POST reply.
    """

    def __init__(self, base: str, tok: str) -> None:
        self.base = base.rstrip("/")
        self.tok = tok
        self.messages: "queue.Queue[dict]" = queue.Queue()
        self.endpoint: str | None = None
        self._id = 0
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._pump, daemon=True)

    def _pump(self) -> None:
        req = urllib.request.Request(
            f"{self.base}/mcp_server/sse",
            headers={"Authorization": f"Bearer {self.tok}", "Accept": "text/event-stream"},
        )
        # No read timeout: a long-lived stream that can idle for minutes
        # while a slow local model decodes. A socket timeout kills the pump.
        with urllib.request.urlopen(req, timeout=None) as resp:
            event = None
            for raw in resp:
                line = raw.decode(errors="replace").rstrip("\n")
                if line.startswith("event:"):
                    event = line.split(":", 1)[1].strip()
                elif line.startswith("data:"):
                    data = line.split(":", 1)[1].strip()
                    if event == "endpoint":
                        self.endpoint = data if data.startswith("http") else self.base + data
                        self._ready.set()
                    else:
                        try:
                            self.messages.put(json.loads(data))
                        except json.JSONDecodeError:
                            pass
                elif not line:
                    event = None

    def start(self) -> "SseMcp":
        self._thread.start()
        if not self._ready.wait(timeout=20):
            raise SystemExit("SSE stream never delivered an endpoint event")
        self.request(
            "initialize",
            {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "fono-live-house", "version": "0"},
            },
        )
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"}, None)
        return self

    def _post(self, payload: dict, expect_id: int | None) -> dict | None:
        req = urllib.request.Request(
            self.endpoint,
            data=json.dumps(payload).encode(),
            headers={"Authorization": f"Bearer {self.tok}", "Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=60):
            pass
        if expect_id is None:
            return None
        for _ in range(60):
            try:
                msg = self.messages.get(timeout=1)
            except queue.Empty:
                continue
            if msg.get("id") == expect_id:
                return msg
        raise SystemExit(f"no response for request id={expect_id}")

    def request(self, method: str, params: dict | None = None) -> dict:
        self._id += 1
        return self._post(
            {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or {}}, self._id
        )

    def call(self, name: str, args: dict) -> dict:
        return self.request("tools/call", {"name": name, "arguments": args})

    def live_context(self) -> str:
        res = self.call("GetLiveContext", {})
        text = "\n".join(c.get("text", "") for c in res["result"]["content"])
        return json.loads(text)["result"] if text.strip().startswith("{") else text


def to_openai_tools(mcp_tools: list[dict]) -> list[dict]:
    return [
        {
            "type": "function",
            "function": {
                "name": t["name"],
                "description": (t.get("description") or "")[:600],
                "parameters": t.get("inputSchema", {"type": "object", "properties": {}}),
            },
        }
        for t in mcp_tools
    ]


def chat(endpoint: str, model: str, messages: list, tools: list, reasoning: bool) -> dict:
    payload = {
        "model": model,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "temperature": 0.0,
        "max_tokens": 400,
    }
    if not reasoning:
        # What Fono ships for local backends — see `thinking_switches` in
        # crates/fono-assistant/src/openai_compat_chat.rs. Both switches go
        # out because the server may be Ollama or llama.cpp.
        payload["think"] = False
        payload["chat_template_kwargs"] = {"enable_thinking": False}
    req = urllib.request.Request(
        endpoint,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=600) as resp:
        return json.loads(resp.read())["choices"][0]["message"]


def area_light_states(mcp: SseMcp, area: str) -> dict[str, str]:
    """Real state of the lights in `area`, straight from the house."""
    out = {}
    for block in mcp.live_context().split("- names: ")[1:]:
        name = block.split("\n")[0].strip()
        if "domain: light" in block and area.lower() in name.lower():
            m = re.search(r"state: '?([^'\n]+)", block)
            out[name] = m.group(1).strip() if m else "?"
    return out


def force(mcp: SseMcp, area: str, state: str) -> None:
    """Put the lights in a known pre-state *directly*, never via the model,
    so a command that changes nothing can't be scored as a trivial pass."""
    mcp.call(
        "HassTurnOn" if state == "on" else "HassTurnOff", {"area": area, "domain": ["light"]}
    )
    time.sleep(2.0)


def peak_rss_mb() -> float | None:
    for p in Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            if "llama-server" not in (p / "comm").read_text():
                continue
            for line in (p / "status").read_text().splitlines():
                if line.startswith("VmHWM:"):
                    return int(line.split()[1]) / 1024
        except OSError:
            continue
    return None


def run_command(mcp, endpoint, model, tools, reasoning, area, lang, text, want) -> dict:
    print(f"-- [{lang}] {text!r}  (expect lights {want})")
    force(mcp, area, "off" if want == "on" else "on")
    before = area_light_states(mcp, area)
    print(f"   before: {before}")

    messages = [
        {"role": "system", "content": SYS[lang]},
        {"role": "user", "content": text},
    ]
    t0 = time.monotonic()
    calls, actuated, msg = [], False, {}
    for _ in range(MAX_STEPS):
        msg = chat(endpoint, model, messages, tools, reasoning)
        tool_calls = msg.get("tool_calls") or []
        if not tool_calls:
            break
        messages.append(msg)
        for tc in tool_calls:
            fn = tc["function"]
            args = fn.get("arguments") or "{}"
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except json.JSONDecodeError:
                    args = {}
            calls.append((fn["name"], args))
            print(f"   -> calls {fn['name']}({json.dumps(args)})")
            payload = mcp.call(fn["name"], args)
            payload = payload.get("result", payload)
            content = "\n".join(
                c.get("text", "") for c in payload.get("content", []) if isinstance(c, dict)
            )
            if fn["name"] in ("HassTurnOn", "HassTurnOff"):
                actuated = True
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": tc.get("id", "0"),
                    "content": content[:4000] or json.dumps(payload)[:4000],
                }
            )
    elapsed = time.monotonic() - t0

    time.sleep(2.0)  # let HA settle so the post-state read is true
    after = area_light_states(mcp, area)
    ok = actuated and all(v == want for v in after.values() if v != "unavailable")

    print(f"   reply : {(msg.get('content') or '').strip()[:110]!r}")
    print(f"   after : {after}")
    print(f"   {'PASS' if ok else 'FAIL'}  {elapsed:.1f}s  steps={len(calls)}\n")
    return {"lang": lang, "text": text, "ok": ok, "elapsed": elapsed, "steps": len(calls)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("endpoint", nargs="?", default="http://127.0.0.1:18099/v1/chat/completions")
    ap.add_argument("--model", default="local")
    ap.add_argument("--base", default=os.environ.get("HA_BASE", ""))
    ap.add_argument("--area", default="kitchen")
    ap.add_argument("--lang", choices=["en", "ro"])
    ap.add_argument("--reasoning", action="store_true")
    args = ap.parse_args()

    if not args.base:
        raise SystemExit("set HA_BASE or pass --base http://<home-assistant>:8123")

    mcp = SseMcp(args.base, ha_token()).start()
    tools = to_openai_tools(mcp.request("tools/list").get("result", {}).get("tools", []))

    print(f"== live light test :: {args.model} :: area {args.area!r}")
    print(f"   {len(tools)} real HA tools, catalogue only")
    print(f"   reasoning: {'ON' if args.reasoning else 'OFF (as Fono ships)'}\n")

    results = [
        run_command(
            mcp,
            args.endpoint,
            args.model,
            tools,
            args.reasoning,
            args.area,
            lang,
            text.format(area=args.area),
            want,
        )
        for lang, text, want in COMMANDS
        if not args.lang or lang == args.lang
    ]

    print("== summary ==")
    for r in results:
        mark = "PASS" if r["ok"] else "FAIL"
        print(f"  {mark} [{r['lang']}] {r['text']:<34} {r['elapsed']:>6.1f}s  {r['steps']} call(s)")
    times = sorted(r["elapsed"] for r in results)
    passed = sum(1 for r in results if r["ok"])
    print(f"\n  passed  : {passed}/{len(results)}")
    print(f"  time    : min {times[0]:.1f}s / median {times[len(times) // 2]:.1f}s / max {times[-1]:.1f}s")
    rss = peak_rss_mb()
    print(f"  peak RSS: {rss:.0f} MB" if rss else "  peak RSS: n/a")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
