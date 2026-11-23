"""Write max.kixel.me's terminal: the maintainer's `token-max doctor --defaults`, as HTML.

Usage, from the repository root after `cargo build --release`, on the maintainer's host:
    python3 assets/demo/site.py site/public ~/.claude=p ~/.claude-api-credits=a --ssh desktop ...

Runs the release binary in a pseudo-terminal against this host's own transcripts and those of
any --ssh destination, passing every argument that is not a PROFILE=ANSWER on to doctor and
answering each "How is PROFILE paid for?" question with the PROFILE=ANSWER given for it, and replaces
what sits between index.html's <!-- doctor --> and <!-- /doctor --> markers. Each question
becomes a .step the page reveals in turn, then each section of the report; the run's timing
goes between style.css's run markers, since the page's CSP allows no inline styles. A second
run answers every profile as API. The hosts' totals, which --ssh prints, and their changes fill
the results section between index.html's <!-- results --> markers: the first run's saving on
the subscription, the second's at API rates, and the second's changes ranked by setting. The
EVERYTHING ELSE section is cut to one line: it is hundreds of lines long, and it names
project files, which can belong to private repositories. Read the result before committing it.
"""

import html
import os
import pty
import re
import sys
from datetime import date
from pathlib import Path

COMMAND = ["token-max", "doctor", "--defaults"]
COLUMNS = 100
# Every question ends in its dim choices, then ": ".
PROMPT = re.compile(rb"\x1b\[0m: $")
SGR = re.compile(r"\x1b\[([0-9;]*)m")
CLASSES = {"1": "b", "2": "d", "31": "r", "32": "g", "33": "y"}
# A report section opens with its bold uppercase heading.
HEADING = re.compile(r"^  [A-Z][A-Z ]+[A-Z]  ")
CUT = "EVERYTHING ELSE"
# A report, and each host's report under --ssh, opens with its title.
TITLE = re.compile(r"^token-max[: ]")
# The finished run scrolls back to the hosts' total under --ssh, else to the count of changes.
TOTAL = re.compile(r"^token-max: total over ")
SUMMARY = re.compile(r"^  \d[\d,]* changes? ")


def run(command: list[str], answers: dict[str, str]) -> str:
    """Answer each question once its prompt shows, then read to exit."""
    root = Path.cwd()
    env = {
        "HOME": os.environ["HOME"],
        "PATH": f"{root / 'target/release'}:{os.environ['PATH']}",
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
    }
    pid, fd = pty.fork()
    if pid == 0:
        import fcntl
        import struct
        import termios

        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 50, COLUMNS, 0, 0))
        # Progress goes to stderr on a terminal; the page shows only the report.
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 2)
        os.execvpe("token-max", command, env)
    out, pending, asked = b"", b"", set()
    while True:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        out += data
        pending += data
        if PROMPT.search(pending):
            question = SGR.sub("", pending.decode().rsplit("\n", 1)[-1])
            match = re.search(r"How is (\S+) paid for\?", question)
            if not match or match.group(1) not in answers:
                os.kill(pid, 9)
                sys.exit(f"no answer given for {question.strip()!r}")
            asked.add(match.group(1))
            os.write(fd, answers[match.group(1)].encode() + b"\r")
            pending = b""
    _, status = os.waitpid(pid, 0)
    # 1 means it recommends a change, as any report on real transcripts should.
    if os.waitstatus_to_exitcode(status) not in (0, 1):
        sys.exit(f"{' '.join(command)} exited with {os.waitstatus_to_exitcode(status)}")
    if unasked := set(answers) - asked:
        sys.exit(f"doctor never asked about {', '.join(sorted(unasked))}")
    return out.decode()


def line_html(line: str) -> str:
    """One terminal line, its SGR colors as classes; the line's last overwrite wins."""
    line = [part for part in line.split("\r") if part.strip()][-1:] or [""]
    out, open_span, pos = [], False, 0
    text = line[0].rstrip()
    for m in SGR.finditer(text):
        out.append(html.escape(text[pos : m.start()], quote=False))
        pos = m.end()
        if open_span:
            out.append("</span>")
            open_span = False
        classes = [CLASSES[c] for c in m.group(1).split(";") if c in CLASSES]
        if classes:
            out.append(f'<span class="{" ".join(classes)}">')
            open_span = True
        elif m.group(1) not in ("", "0"):
            sys.exit(f"unhandled color {m.group(1)!r}")
    out.append(html.escape(text[pos:], quote=False))
    if open_span:
        out.append("</span>")
    return "".join(out)


def steps(transcript: str) -> tuple[list[list[str]], int]:
    """Split the run into steps: each question with its answer, then each report section.

    Returns the steps as lines of HTML and how many of them are questions."""
    out, lines, questions, rests = [], [], 0, 0
    raws = transcript.replace("\r\n", "\n").split("\n")
    plains = [SGR.sub("", raw.split("\r")[-1]) for raw in raws]
    rest = TOTAL if any(TOTAL.match(p) for p in plains) else SUMMARY

    def starts(plain: str) -> bool:
        return bool(HEADING.match(plain) or TITLE.match(plain))
    i = 0
    while i < len(raws):
        raw, plain = raws[i], plains[i]
        line = line_html(raw)
        if rest.match(plain):
            line = f'<span class="rest">{line}</span>'
            rests += 1
        if re.search(r"\[0m: [a-z]$", raw.rstrip()):
            prompt, answer = line[:-1], line[-1]
            lines.append(f'{prompt}<span class="ans">{answer}</span><span class="cur"></span>')
            out.append(lines)
            lines, questions = [], questions + 1
        elif starts(plain):
            while lines and not lines[-1]:
                lines.pop()
            if lines:
                out.append(lines + [""])
            lines = [line]
            if plain.strip().startswith(CUT):
                # Skip to the next section, keeping only a count of what was cut.
                j = i + 1
                while j < len(raws) and not starts(plains[j]):
                    j += 1
                cut = sum(1 for r in raws[i + 1 : j] if r.strip())
                lines.append(
                    f'<span class="elided">  … {cut} lines on every other setting and feature, cut from this page</span>'
                )
                i = j
                continue
        else:
            lines.append(line)
        i += 1
    while lines and not lines[-1]:
        lines.pop()
    out.append(lines)
    # Each step keeps none of the terminal's blank lines around it; a report section opens
    # with one, so the sections read apart.
    tidy = []
    for step in out:
        while step and not step[0]:
            step = step[1:]
        while step and not step[-1]:
            step = step[:-1]
        if step:
            tidy.append(step if len(tidy) < questions else [""] + step)
    if rests != 1:
        sys.exit(f"expected one line for the run to rest on, matching {rest.pattern!r}, found {rests}")
    return tidy, questions


def timing(typed: str, count: int, questions: int) -> str:
    """CSS for when each step and its answer show: questions wait for a reader, the report streams."""
    typing = 0.03 * len(typed)
    rules = [
        f".term.play .typed {{ animation: type {typing:.1f}s steps({len(typed)}) 0.3s both; }}",
        f"@keyframes type {{ from {{ width: 0; }} to {{ width: {len(typed)}ch; }} }}",
    ]
    t = typing + 0.7
    for n in range(count):
        # The first step is the command line's sibling at nth-child(2).
        gap = 1.4 if n < questions else 0
        rules.append(f".step:nth-child({n + 2}) {{ --t: {t:.1f}s; --gap: {gap:.1f}s; }}")
        # A pause for the scan after the last answer, and for the summary that ends the
        # report's first section before the CHANGES push it up.
        t += gap + (1.5 if n == questions - 1 else 2.5 if n == questions else 0.6)
    return "\n".join(rules) + "\n"


def plain_lines(transcript: str) -> list[str]:
    """The run's lines without colors, each as its last overwrite left it."""
    return [SGR.sub("", line.split("\r")[-1]) for line in transcript.replace("\r\n", "\n").split("\n")]


def dollars(text: str) -> float:
    return float(text.replace(",", ""))


# One change in a report's CHANGES section: its setting, the saving's verb, ~ and amount.
CHANGE = re.compile(
    r"^  ● (?P<name>\S.*?\S)\s{2,}\S.*?→ (?P<to>\S+).*?\s{2,}saves\s+(?P<verb>up to\s+|at least\s+)?"
    r"(?P<about>~?)(?P<lt><?)\$(?P<usd>[\d,.]+)/mo"
)


def changes(transcript: str, api: set[str], api_rates: bool) -> dict[str, list]:
    """Each setting's offered saving summed over every host's CHANGES section, as
    [measured $/mo, $/mo that depends on how a profile pays, whether any part has a ~,
    how many lines, the measured $/mo outside this host's profiles in `api`]. `api_rates` says
    every profile doctor asked about was answered as API."""
    rows: dict[str, list] = {}
    section, here = None, False
    lines = plain_lines(transcript)
    for i, plain in enumerate(lines):
        if TITLE.match(plain):
            section, here = None, plain.startswith("token-max: this host ")
        elif HEADING.match(plain):
            section = plain.split()[0]
        elif section == "CHANGES" and plain.startswith("  ● "):
            # A change whose saving was not measured, or one priced only as what it costs,
            # such as a Codex catalog skill's listing, promises no saving.
            if not re.search(r"\ssaves\s", plain):
                if not re.search(r"saving not measured$|\scosts\s", plain.rstrip()):
                    sys.exit(f"unparsed change line {plain.strip()!r}")
                continue
            m = CHANGE.match(plain)
            if not m:
                sys.exit(f"unparsed change line {plain.strip()!r}")
            up_to = bool(m["verb"]) and m["verb"].startswith("up to")
            # "Up to" prices an unset main TTL against the default it would replace, which the
            # API's 5m is only for a change to 1h.
            if api_rates and up_to and m["to"] != "1h":
                sys.exit(f"cannot price {plain.strip()!r} at API rates without that profile's billing")
            # The line after a change names the file it edits, such as [~/.claude/settings.json],
            # unless its host has one profile.
            path = re.match(r"^    \[([^\]]+)\]", lines[i + 1] if i + 1 < len(lines) else "")
            paid = here and (
                any(path[1].startswith(root + "/") for root in api) if path else bool(api)
            )
            # "<$0.01" counts as nothing.
            usd = 0.0 if m["lt"] else dollars(m["usd"])
            row = rows.setdefault(m["name"], [0.0, 0.0, False, 0, 0.0])
            row[1 if up_to else 0] += usd
            row[2] |= bool(m["about"])
            row[3] += 1
            if not up_to and not paid:
                row[4] += usd
    if not rows:
        sys.exit("the run offered no change with a saving")
    return rows


# The hosts' total, which --ssh prints, then what their changes offer.
TOTAL_LINE = re.compile(
    r"token-max: total over (\d+) hosts\s+([\d,]+) requests and ([\d,]+) tokens in [\d,]+ sessions?"
)
OFFERED = re.compile(
    r"Changes offered: (~?)\$([\d,.]+)/mo[^,]*, (\d+)% of these hosts' spend"
    r"(?:, plus up to (~?)\$([\d,.]+)/mo[^,]*, (\d+)% of these hosts' spend, from \d+ changes?)?"
)


def offered(transcript: str) -> tuple[re.Match, re.Match]:
    plain = " ".join(line.strip() for line in plain_lines(transcript))
    total, offer = TOTAL_LINE.search(plain), OFFERED.search(plain)
    if not total or not offer:
        sys.exit("the results need the hosts' total that --ssh prints")
    return total, offer


def usd(value: float, about: bool) -> str:
    return f"{'~' if about else ''}${value:,.0f}"


def billions(tokens: int) -> str:
    return f"{tokens / 1e9:.1f} billion" if tokens >= 1e9 else f"{tokens / 1e6:.0f} million"


# The breakdown names this many settings, then one row for every other one.
LISTED = 5


def results(plan: str, api: str, paid: set[str]) -> tuple[str, str]:
    """The results section and its bars' CSS: the plan run as the maintainer pays, and the
    run with every profile answered as API. Doctor asks only about this host's profiles, so a
    destination's main-TTL change stays "up to"; billing picks only that change's default, and
    for a change to 1h "up to" prices it against the API's 5m, so it counts in full at API rates.
    A destination's profiles count as the subscription, since their billing is unknown."""
    total, plan_offer = offered(plan)
    hosts, requests, tokens = total.groups()
    plan_pct = plan_offer[3]
    _, api_offer = offered(api)
    api_about = bool(api_offer[1] or api_offer[4])
    api_usd = dollars(api_offer[2]) + (dollars(api_offer[5]) if api_offer[5] else 0)
    # Each percent is of the same spend, so they add; each was rounded, so the sum may be one off.
    api_pct = int(api_offer[3]) + int(api_offer[6] or 0)
    plan_rows, api_rows = changes(plan, paid, False), changes(api, set(), True)
    # The rows must add up to the totals, each rounded to at most a dollar's half.
    for rows_, offer in ((plan_rows, plan_offer), (api_rows, api_offer)):
        slack = 0.5 * (sum(r[3] for r in rows_.values()) + 1)
        for part, figure in ((0, offer[2]), (1, offer[5])):
            summed = sum(r[part] for r in rows_.values())
            if abs(summed - (dollars(figure) if figure else 0)) > slack:
                sys.exit(f"the changes add up to ${summed:,.2f}/mo, not the total's ${figure}/mo")
    # A setting saves on the subscription what the first run measured outside the profiles
    # answered as API; at API rates, everything the second run offers.
    rows = []
    for name, (measured, up_to, about, _, _) in api_rows.items():
        # A 1h main TTL makes cache writes dearer, so a change can save more on the plan.
        on_plan = min(plan_rows.get(name, [0.0] * 5)[4], measured + up_to)
        rows.append((measured + up_to, on_plan, about, name))
    # A change only the plan run offers, such as a 1h main TTL moved to 5m, saves nothing at
    # API rates, so the breakdown leaves it out.
    rows.sort(key=lambda r: (-r[0], r[3]))
    listed, rest = rows[:LISTED], rows[LISTED:]
    if rest:
        listed.append(
            (sum(r[0] for r in rest), sum(r[1] for r in rest), any(r[2] for r in rest),
             f"Everything else ({len(rest)} settings)")
        )
    top = max(r[0] for r in listed)
    items, css = [], []
    for n, (whole, on_plan, about, name) in enumerate(listed, 1):
        tag = ' <span class="tag">API only</span>' if on_plan < 0.005 else ""
        items.append(
            f'    <li><span class="name">{html.escape(name)}{tag}</span>'
            f'<span class="amt">{usd(whole, about)}<small>/mo</small></span>'
            f'<span class="track" aria-hidden="true"><i class="plan"></i><i class="api"></i></span></li>'
        )
        # A saving too small to see keeps a sliver; one of nothing keeps none.
        plan_w = 100 * on_plan / top
        api_w = 100 * (whole - on_plan) / top
        css.append(
            f".breakdown li:nth-child({n}) .plan {{ width: {max(plan_w, 0.3) if on_plan else 0:.1f}%; }}\n"
            f".breakdown li:nth-child({n}) .api {{ width: {max(api_w, 0.3) if whole - on_plan >= 0.005 else 0:.1f}%; }}\n"
        )
    fragment = f"""
  <p class="context">{billions(int(tokens.replace(",", "")))} tokens in {requests} requests on {hosts} machines</p>
  <div class="cols">
    <div class="col plan">
      <h3>On my subscription</h3>
      <p class="big">{plan_pct}%</p>
      <p>less of my usage limits, so they last longer</p>
    </div>
    <div class="col api">
      <h3>If I paid API rates</h3>
      <p class="big">{api_pct}%</p>
      <p>saved, {usd(api_usd, api_about)}/mo <span class="nope">(hell no)</span></p>
    </div>
  </div>
  <h3 class="where">Where it comes from, at API rates</h3>
  <ol class="breakdown">
{chr(10).join(items)}
  </ol>
  <p class="key"><span class="k-plan">saves on my subscription too</span><span class="k-api">saves only at API rates</span></p>
  <p class="fine">Replayed at public API prices on {hosts} of my machines on {date.today().isoformat()}, each change
  priced alone, so together they may save less. ~ marks figures extrapolated over unpriced requests.</p>
"""
    return fragment, "".join(css)


def replace(path: Path, start: str, end: str, body: str) -> None:
    text = path.read_text(encoding="utf-8")
    if text.count(start) != 1 or text.count(end) != 1:
        sys.exit(f"{path} needs one {start} and one {end}")
    head, rest = text.split(start)
    _, tail = rest.split(end)
    path.write_text(f"{head}{start}{body}{end}{tail}", encoding="utf-8")


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    site = Path(sys.argv[1])
    answers, command = {}, list(COMMAND)
    for arg in sys.argv[2:]:
        profile, sep, answer = arg.rpartition("=")
        if not sep or arg.startswith("-"):
            command.append(arg)
            continue
        if answer not in ("p", "a", "s"):
            sys.exit(f"{arg!r} is not PROFILE=p, PROFILE=a or PROFILE=s")
        home = os.environ["HOME"]
        answers["~" + profile[len(home) :] if profile.startswith(home) else profile] = answer
    typed = " ".join(command)
    transcript = run(command, answers)
    # The same run with every profile answered as API, for the results' API-rate column.
    api = run(command, {profile: "a" for profile in answers})
    run_steps, questions = steps(transcript)
    figures, bars = results(transcript, api, {p for p, a in answers.items() if a == "a"})
    body = "".join(f'<span class="step">{chr(10).join(s)}\n</span>' for s in run_steps)
    fragment = f'<pre class="screen"><code><span class="cmdline">$ <span class="typed">{typed}</span>\n</span>{body}</code></pre>'
    replace(site / "index.html", "<!-- doctor -->", "<!-- /doctor -->", fragment)
    replace(site / "index.html", "<!-- results -->", "<!-- /results -->", figures)
    replace(
        site / "style.css",
        "/* run: assets/demo/site.py writes the run's timing and the breakdown's bars here */\n",
        "/* /run */",
        timing(typed, len(run_steps), questions) + bars,
    )


if __name__ == "__main__":
    main()
