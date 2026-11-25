"""Write a synthetic Claude Code home for the README recording: no real sessions.

Usage: python3 home.py DIR. Timestamps end now, so the walk's monthly figures stay put.
"""

import json
import random
import sys
import time
from pathlib import Path

root = Path(sys.argv[1])
claude = root / ".claude"
app = (root / "app").resolve()
app.mkdir(parents=True, exist_ok=True)
# The sessions run in a checkout with its own AGENTS.md, which vibes' memory card reads.
(app / ".git").mkdir(exist_ok=True)
(app / "AGENTS.md").write_text("# Synthetic instructions\n")
# Claude Code names a project's directory after its path, with every non-alphanumeric as "-".
project = claude / "projects" / "".join(c if c.isalnum() else "-" for c in str(app))
project.mkdir(parents=True, exist_ok=True)
(claude / "settings.json").write_text(
    json.dumps(
        {
            "model": "opus",
            "promptCacheTtl": "5m",
            # Saves against the 5m default on the subagents' long waits, so the walk opens
            # with a credit.
            "subagentPromptCacheTtl": "1h",
            "fastMode": True,
            # A year already, so the recording asks only about the Bash cap.
            "cleanupPeriodDays": 365,
            "env": {"BASH_MAX_TIMEOUT_MS": "7200000"},
        },
        indent=2,
    )
    + "\n"
)

rng = random.Random(7)
now = time.time()
start = now - 14 * 86400


def stamp(t):
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(t)) + f".{int(t * 1000) % 1000:03d}Z"


def transcript(session, s, t, turns, pauses, rng, ttl, speed, context, agent=None):
    """One conversation's assistant lines from `t`, each cache write lasting `ttl` seconds."""
    cached_until = 0.0
    tag = f"{agent}-" if agent else ""
    lines = []
    for i in range(turns):
        write = context if t > cached_until else 0
        read = 0 if write else context
        step = rng.randint(800, 4000)
        line = {
            "type": "assistant",
            "uuid": f"{session}-{tag}{i}",
            "sessionId": session,
            "timestamp": stamp(t),
            "version": "2.1.314",
            "entrypoint": "cli",
            "cwd": str(app),
            "message": {
                "id": f"msg-{s}-{tag}{i}",
                "model": "claude-opus-5-5",
                "content": [],
                "usage": {
                    "input_tokens": step,
                    "output_tokens": rng.randint(200, 1500),
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": write,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": write if ttl == 300 else 0,
                        "ephemeral_1h_input_tokens": write if ttl == 3600 else 0,
                    },
                    "service_tier": "standard",
                    "speed": speed,
                    "inference_geo": "global",
                    "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                },
            },
        }
        if agent:
            line["isSidechain"] = True
            line["agentId"] = agent
        lines.append(line)
        context += step
        cached_until = t + ttl
        t += rng.choice(pauses)
    return "".join(json.dumps(l) + "\n" for l in lines)


for s in range(24):
    session = f"00000000-0000-4000-8000-{s:012d}"
    t = start + s * 13 * 3600 + rng.uniform(0, 3600)
    speed = "fast" if s % 12 == 0 else "standard"
    # Mostly quick turns, with pauses past five minutes that a 1h cache would survive.
    pauses = [20, 40, 60, 90, 120, 420, 900, 1500]
    (project / f"{session}.jsonl").write_text(
        transcript(session, s, t, rng.randint(30, 60), pauses, rng, 300, speed, 24_000)
    )
    # A subagent that waits on long builds, past five minutes but within the hour. Its own
    # generator leaves the main sessions as they were.
    agent = f"a{s:016x}"
    subagents = project / session / "subagents"
    subagents.mkdir(parents=True, exist_ok=True)
    sub = random.Random(1000 + s)
    (subagents / f"agent-{agent}.jsonl").write_text(
        transcript(session, s, t + 60, sub.randint(20, 40), [30, 400, 700, 1200], sub, 3600, "standard", 80_000, agent)
    )
