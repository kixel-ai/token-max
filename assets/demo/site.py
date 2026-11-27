"""Write max.kixel.me's terminal: the maintainer's `token-max doctor --defaults`, as HTML.

Usage, from the repository root after `cargo build --release`, on the maintainer's host:
    python3 assets/demo/site.py site/public ~/.claude=p desktop:~/.claude=p --since 2026-10-25 --ssh desktop ...

Runs the release binary in a pseudo-terminal against this host's own transcripts and those of
any --ssh destination, passing every argument that is not a PROFILE=ANSWER on to doctor and
answering each "How is PROFILE paid for?" question with the PROFILE=ANSWER given for it, a
destination's PROFILE named after its host as in desktop:~/.claude, and replaces
what sits between index.html's <!-- doctor --> and <!-- /doctor --> markers. Each question
becomes a .step the page reveals in turn, then each section of the report; the run's timing
goes between style.css's run markers, since the page's CSP allows no inline styles. Two more
runs on a terminal too wide to wrap a row repeat it and answer every profile as API. The hosts'
totals, which --ssh prints, and their changes fill the results section between index.html's
<!-- results --> markers: the shown run's usage and saving on the subscription, the API run's
saving at API rates, and both wide runs' changes ranked by setting, each skill or MCP server
folded into its kind's row, since a project's own can be named after it. The
EVERYTHING ELSE section is cut to one line: a kind or setting with a single line, such as a
project's only instruction file or subagent, still names its project files, which can belong
to private repositories. So is each EDITS section, which names every skill and tool an edit
turns off, a project's own among them. Read the result before committing it.
"""

import html
import math
import os
import pty
import re
import sys
from datetime import date, datetime, time, timedelta, timezone
from pathlib import Path

COMMAND = ["token-max", "doctor", "--defaults"]
COLUMNS = 100
# The runs the results parse are this wide, so no change row wraps: a wrap can break at a column
# gap or inside a name, which leave the same text.
WIDE = 10000
# Every question ends in its dim choices, then ": ".
PROMPT = re.compile(rb"\x1b\[0m: $")
SGR = re.compile(r"\x1b\[([0-9;]*)m")
CLASSES = {"1": "b", "2": "d", "31": "r", "32": "g", "33": "y", "35": "p"}
# A report section opens with its bold uppercase heading.
HEADING = re.compile(r"^  [A-Z][A-Z ]+[A-Z]  ")
# The sections cut to a count of their lines, by heading, with what the count says was cut:
# both can name a project's own files, skills or tools, from repositories that may be private.
CUT = {
    "EVERYTHING ELSE": "lines on every other setting and feature",
    "EDITS": "lines naming each edit",
}
# The lines closing a report, which follow its last section and stay on the page.
FOOTER = re.compile(r"^  Prices: ")
# A report, and each host's report under --ssh, opens with its title.
TITLE = re.compile(r"^token-max[: ]")
# The finished run scrolls back to the hosts' total under --ssh, else to the count of changes.
TOTAL = re.compile(r"^token-max: total over ")
SUMMARY = re.compile(r"^  \d[\d,]* changes? ")


def run(command: list[str], answers: dict[str, str], columns: int) -> str:
    """Answer each question once its prompt shows, then read to exit, on a terminal `columns` wide."""
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

        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 50, columns, 0, 0))
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
            what = next((w for h, w in CUT.items() if plain.strip().startswith(h)), None)
            if what:
                # Skip to the next section or the report's footer, keeping only a count of
                # what was cut.
                j = i + 1
                while j < len(raws) and not (starts(plains[j]) or FOOTER.match(plains[j])):
                    j += 1
                cut = sum(1 for r in raws[i + 1 : j] if r.strip())
                if cut == 1:
                    what = what.replace("lines", "line", 1)
                lines.append(
                    f'<span class="elided">  … {cut} {what}, cut from this page</span>'
                )
                if j < len(raws) and FOOTER.match(plains[j]):
                    lines.append("")
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
    tidy = [art(step) for step in tidy]
    if rests != 1:
        sys.exit(f"expected one line for the run to rest on, matching {rest.pattern!r}, found {rests}")
    return tidy, questions


def art(step: list[str]) -> list[str]:
    """Join each run of block-letter lines into one tight span, so the page's line height
    leaves no gap between the halves of the wordmark."""
    out: list[str] = []
    for line in step:
        if "█" in line and out and out[-1].startswith('<span class="art">'):
            out[-1] = out[-1][: -len("</span>")] + "\n" + line + "</span>"
        elif "█" in line:
            out.append(f'<span class="art">{line}</span>')
        else:
            out.append(line)
    return out


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


def roots(api: set[str], host: str | None) -> set[str]:
    """The profiles in `api` on `host`, None for this one, as that host names them."""
    if host is None:
        return {p for p in api if p.startswith(("~", "/"))}
    return {p.removeprefix(f"{host}:") for p in api if p.startswith(f"{host}:")}


def changes(transcript: str, api: set[str], api_rates: bool) -> dict[str, list]:
    """Each setting's offered saving summed over every host's changes in a WIDE run, as
    [measured $/mo, $/mo that depends on how a profile pays, whether any part has a ~,
    how many lines, the measured $/mo outside the profiles in `api`, whether any part is
    a lower bound]. `api_rates` says
    every profile doctor asked about was answered as API."""
    rows: dict[str, list] = {}
    # Each change row. Under --details a section's members follow its row two cells in, so
    # the members count and the section's row doesn't.
    found: list[str] = []
    section = None
    for plain in plain_lines(transcript):
        if TITLE.match(plain):
            section = None
        elif HEADING.match(plain):
            section = plain.split()[0]
        elif section == "CHANGES" and plain.startswith("    ● "):
            if not found or found[-1].startswith("    "):
                found.append(plain)
            else:
                found[-1] = plain
        elif section == "CHANGES" and plain.startswith("  ● "):
            found.append(plain)
    for plain in found:
        plain = plain.removeprefix("  ") if plain.startswith("    ") else plain
        # A change whose saving was not measured, or one priced only as what it costs,
        # such as a Codex catalog skill's listing, promises no saving.
        if not re.search(r"\ssaves\s", plain):
            if not re.search(r"\ssaving not measured(\s{2}|$)|\scosts\s", plain.rstrip()):
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
        # After its saving, a change's tag names its host under --ssh and the file it edits
        # when its host has several profiles: [this host], [desktop], [~/.claude/settings.json]
        # or [desktop:~/.claude/settings.json].
        tag = re.match(r"[^\[]*?\s{2}\[([^\]]+)\]", plain[m.end():])
        host, path = None, None
        if tag and tag[1] != "this host":
            name, _, file = tag[1].partition(":")
            if tag[1].startswith(("~", "/")):
                path = tag[1]
            elif file.startswith(("~", "/")):
                host, path = name, file
            else:
                host = tag[1]
        here = roots(api, host)
        paid = any(path.startswith(root + "/") for root in here) if path else bool(here)
        # "<$0.01" counts as nothing.
        usd = 0.0 if m["lt"] else dollars(m["usd"])
        row = rows.setdefault(m["name"], [0.0, 0.0, False, 0, 0.0, False])
        row[1 if up_to else 0] += usd
        row[2] |= bool(m["about"])
        row[3] += 1
        # "At least" leaves part of the saving unpriced, so the figure is a lower bound.
        row[5] |= bool(m["verb"]) and m["verb"].startswith("at least")
        if not up_to and not paid:
            row[4] += usd
    return rows


# The hosts' total, which --ssh prints, with what they spent at API prices; what their changes
# offer follows under the headline.
TOTAL_LINE = re.compile(
    r"token-max: total over (\d+) hosts\s+([\d,]+) requests and ([\d,]+) tokens in [\d,]+ sessions?, "
    r"(.*?) at API prices"
)
# A whole spend, as doctor prints it without unpriced requests: an amount, perhaps with a margin.
SPENT = re.compile(r"(~?)\$([\d,.]+)(?: ± <?\$[\d,.]+|, margin unknown)?")
# One part of the sentence on what the changes save: its amount and its percent of the hosts'
# spend.
AMOUNT = re.compile(r"^(?:up to )?(?:about )?(~?)(<?)\$([\d,.]+)/mo.*?, (<?)(\d+)% of these hosts' spend")


def offered(transcript: str) -> tuple[re.Match, dict[str, tuple[float, bool, int, bool]]]:
    """The hosts' total, and what their changes offer as (usd, has a ~, percent, is a lower
    bound) for "measured"
    and "up to"; a run with nothing to offer prints no sentence on what its changes save."""
    plain = " ".join(line.strip() for line in plain_lines(transcript))
    total = TOTAL_LINE.search(plain)
    if not total:
        sys.exit("the results need the hosts' total that --ssh prints")
    offer = {"measured": (0.0, False, 0, False), "up to": (0.0, False, 0, False)}
    rest = plain[total.end():]
    sentence = re.search(r"(?:From the defaults|Together), these changes save (.*?)\.(?: |$)", rest)
    # Changes none of whose savings were measured get a sentence of their own.
    unmeasured = bool(re.search(r"(?:From the defaults, no|No) change's saving was measured\.", rest))
    for part in sentence[1].split(", plus ") if sentence else []:
        if re.search(r"not measured(; overlapping changes may save less)?$", part):
            unmeasured = True
            continue
        m = AMOUNT.match(part)
        if not m:
            sys.exit(f"unparsed offer {part!r}")
        # "<$1" and "<1%" count as nothing.
        usd = 0.0 if m[2] else dollars(m[3])
        pct = 0 if m[4] else int(m[5])
        # Changes "priced only in part" make the measured amount a lower bound.
        lower = ", more where " in part
        offer["up to" if part.startswith("up to ") else "measured"] = (usd, bool(m[1]), pct, lower)
    # A 0% beside changes whose savings went unmeasured would claim what doctor did not.
    if unmeasured and not any(usd for usd, *_ in offer.values()):
        sys.exit("the run measured none of its changes' savings, so the results have nothing to show")
    return total, offer


def usd(value: float, about: bool, lower: bool = False) -> str:
    return f"{'≥' if lower else ''}{'~' if about else ''}${value:,.0f}"


def billions(tokens: int) -> str:
    return f"{tokens / 1e9:.1f}B" if tokens >= 1e9 else f"{tokens / 1e6:.0f}M"


# The breakdown names this many settings, then one row for every other one.
LISTED = 5
# A skill or MCP server is named by whoever installed it, perhaps after a private project, so
# the public page shows each kind as one row of its members' sum, never a member's name.
FOLDED = {"Skill ": "Skills", "MCP server ": "MCP servers", "MCP tool ": "MCP tools"}


def results(shown: str, plan: str, api: str, paid: set[str], since: date, now: datetime) -> tuple[str, str]:
    """The results section and its bars' CSS: the shown run, whose totals the page's terminal
    prints, and two WIDE runs whose rows it parses: the plan run as the maintainer pays, and the
    run with every profile answered as API. A profile answered s keeps its main-TTL change "up
    to"; billing picks only that change's default, and for a change to 1h "up to" prices it
    against the API's 5m, so it counts in full at API rates."""
    # The boxes and the subscription column repeat the terminal's figures; the rows come from
    # a later run, so they are checked against that run's own totals.
    total, shown_offer = offered(shown)
    hosts, requests, tokens, spent_text = total.groups()
    spent = SPENT.fullmatch(spent_text)
    if not spent:
        sys.exit(f"the usage boxes need a whole spend, not {spent_text!r}")
    # Every host's spend covers --since to the run, so the window is the same for the total;
    # count its days as doctor does, rounded to the nearest and at least one.
    start = datetime.combine(since, time(), timezone.utc)
    days = max(1, math.floor((now - start) / timedelta(days=1) + 0.5))
    plan_usd, plan_about, plan_pct, plan_lower = shown_offer["measured"]
    _, plan_offer = offered(plan)
    _, api_offer = offered(api)
    api_about = api_offer["measured"][1] or api_offer["up to"][1]
    api_lower = api_offer["measured"][3]
    api_usd = api_offer["measured"][0] + api_offer["up to"][0]
    # Each percent is of the same spend, so they add; each was rounded, so the sum may be one off.
    api_pct = api_offer["measured"][2] + api_offer["up to"][2]
    plan_rows, api_rows = changes(plan, paid, False), changes(api, set(), True)
    # The rows must add up to the totals, each rounded to at most a dollar's half.
    for rows_, offer in ((plan_rows, plan_offer), (api_rows, api_offer)):
        slack = 0.5 * (sum(r[3] for r in rows_.values()) + 1)
        for part, key in ((0, "measured"), (1, "up to")):
            summed = sum(r[part] for r in rows_.values())
            if abs(summed - offer[key][0]) > slack:
                sys.exit(f"the changes add up to ${summed:,.2f}/mo, not the total's ${offer[key][0]:,.2f}/mo")
    # A setting saves on the subscription what the first run measured outside the profiles
    # answered as API; at API rates, everything the second run offers.
    folded: dict[str, list] = {}
    for name, (measured, up_to, about, _, _, lower) in api_rows.items():
        # A 1h main TTL makes cache writes dearer, so a change can save more on the plan.
        on_plan = min(plan_rows.get(name, [0.0] * 5)[4], measured + up_to)
        kind = next((k for p, k in FOLDED.items() if name.startswith(p)), name)
        row = folded.setdefault(kind, [0.0, 0.0, False, set(), False])
        row[0] += measured + up_to
        row[1] += on_plan
        row[2] |= about
        row[3].add(name)
        row[4] |= lower
    rows = [
        (whole, on_plan, about, f"{kind} ({len(names)})" if kind in FOLDED.values() else kind, lower, len(names))
        for kind, (whole, on_plan, about, names, lower) in folded.items()
    ]
    # A change only the plan run offers, such as a 1h main TTL moved to 5m, saves nothing at
    # API rates, so the breakdown leaves it out.
    rows.sort(key=lambda r: (-r[0], r[3]))
    listed, rest = rows[:LISTED], rows[LISTED:]
    if rest:
        listed.append(
            (sum(r[0] for r in rest), sum(r[1] for r in rest), any(r[2] for r in rest),
             f"Everything else ({sum(r[5] for r in rest)} settings)", any(r[4] for r in rest), 0)
        )
    top = max((r[0] for r in listed), default=0)
    items, css = [], []
    for n, (whole, on_plan, about, name, lower, _) in enumerate(listed, 1):
        tag = ' <span class="tag">API only</span>' if on_plan < 0.005 else ""
        # The bars are decorative, so a screen reader hears the split they draw as text.
        if on_plan < 0.005:
            split = ""
        elif usd(on_plan, about, lower) == usd(whole, about, lower):
            split = '<span class="sr">, all of it on my subscription too</span>'
        else:
            split = f'<span class="sr">, {usd(on_plan, about, lower)}/mo of it on my subscription too</span>'
        items.append(
            f'    <li><span class="name">{html.escape(name)}{tag}</span>'
            f'<span class="amt">{usd(whole, about, lower)}<small>/mo</small></span>{split}'
            f'<span class="track" aria-hidden="true"><i class="plan"></i><i class="api"></i></span></li>'
        )
        # A saving too small to see keeps a sliver; one of nothing keeps none.
        # Rows of only "<$0.01" leave no length to scale by.
        plan_w = 100 * on_plan / top if top else 0
        api_w = 100 * (whole - on_plan) / top if top else 0
        css.append(
            f".breakdown li:nth-child({n}) .plan {{ width: {max(plan_w, 0.3) if on_plan else 0:.1f}%; }}\n"
            f".breakdown li:nth-child({n}) .api {{ width: {max(api_w, 0.3) if whole - on_plan >= 0.005 else 0:.1f}%; }}\n"
        )
    lowers = plan_lower or api_lower or any(r[4] for r in listed)
    bound = " ≥ marks savings priced only in part." if lowers else ""
    breakdown = f"""  <h3 class="where">Where it comes from, at API rates</h3>
  <ol class="breakdown">
{chr(10).join(items)}
  </ol>
  <p class="key"><span class="k-plan">saves on my subscription too</span><span class="k-api">saves only at API rates</span></p>
""" if items else ""
    fragment = f"""
  <div class="stats">
    <div class="stat"><h3>Tokens</h3><p>{billions(int(tokens.replace(",", "")))}</p></div>
    <div class="stat"><h3>Requests</h3><p>{requests}</p></div>
    <div class="stat"><h3>API price, {days} day{"" if days == 1 else "s"}</h3><p>{spent[1]}${spent[2]}</p></div>
  </div>
  <h3 class="where">What token-max saves</h3>
  <div class="cols">
    <div class="col plan">
      <h3>On my subscription</h3>
      <p class="big">{"≥" if plan_lower else ""}{plan_pct}%<small> saved</small></p>
      <p>{usd(plan_usd, plan_about, plan_lower)}/mo at API rates</p>
    </div>
    <div class="col api">
      <h3>If I used the API</h3>
      <p class="big">{"≥" if api_lower else ""}{api_pct}%<small> saved</small></p>
      <p>{usd(api_usd, api_about, api_lower)}/mo</p>
    </div>
  </div>
{breakdown}  <p class="fine">Replayed at public API prices on {hosts} of my machines on {now.date().isoformat()}, each change
  priced alone, so together they may save less. ~ marks figures extrapolated over unpriced requests,
  assuming they resemble the priced ones.{bound}</p>
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
    # The usage boxes name the window, so the run must set its start.
    starts = [command[i + 1] for i, arg in enumerate(command[:-1]) if arg == "--since"]
    starts += [arg.removeprefix("--since=") for arg in command if arg.startswith("--since=")]
    if len(starts) != 1:
        sys.exit("pass one --since YYYY-MM-DD, so the usage boxes can name their window")
    since = date.fromisoformat(starts[0])
    typed = " ".join(command)
    # Doctor dates its window in UTC, ending when it runs.
    now = datetime.now(timezone.utc)
    transcript = run(command, answers, COLUMNS)
    # The results parse each change's row from runs too wide to wrap one: the same run, and one
    # with every profile answered as API for the API-rate column. --details lists each section's
    # changes, so the breakdown names settings rather than sections.
    plan = run(command + ["--details"], answers, WIDE)
    api = run(command + ["--details"], {profile: "a" for profile in answers}, WIDE)
    run_steps, questions = steps(transcript)
    figures, bars = results(transcript, plan, api, {p for p, a in answers.items() if a == "a"}, since, now)
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
