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


for s in range(24):
    session = f"00000000-0000-4000-8000-{s:012d}"
    t = start + s * 13 * 3600 + rng.uniform(0, 3600)
    context = 24_000
    cached_until = 0.0
    speed = "fast" if s % 8 == 0 else "standard"
    lines = []
    for i in range(rng.randint(30, 60)):
        write = context if t > cached_until else 0
        read = 0 if write else context
        step = rng.randint(800, 4000)
        lines.append(
            {
                "type": "assistant",
                "uuid": f"{session}-{i}",
                "sessionId": session,
                "timestamp": stamp(t),
                "version": "2.1.314",
                "entrypoint": "cli",
                "cwd": str(app),
                "message": {
                    "id": f"msg-{s}-{i}",
                    "model": "claude-opus-5-5",
                    "content": [],
                    "usage": {
                        "input_tokens": step,
                        "output_tokens": rng.randint(200, 1500),
                        "cache_read_input_tokens": read,
                        "cache_creation_input_tokens": write,
                        "cache_creation": {"ephemeral_5m_input_tokens": write, "ephemeral_1h_input_tokens": 0},
                        "service_tier": "standard",
                        "speed": speed,
                        "inference_geo": "global",
                        "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                    },
                },
            }
        )
        context += step
        cached_until = t + 300
        # Mostly quick turns, with pauses past five minutes that a 1h cache would survive.
        t += rng.choice([20, 40, 60, 90, 120, 420, 900, 1500])
    (project / f"{session}.jsonl").write_text("".join(json.dumps(l) + "\n" for l in lines))
