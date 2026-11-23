"""Write max.kixel.ai's terminal: the maintainer's `token-max doctor --defaults`, as HTML.

Usage, from the repository root after `cargo build --release`, on the maintainer's host:
    python3 assets/demo/site.py site/public ~/.claude=p ~/.claude-api-credits=a --ssh desktop ...

Runs the release binary in a pseudo-terminal against this host's own transcripts and those of
any --ssh destination, passing every argument that is not a PROFILE=ANSWER on to doctor and
answering each "How is PROFILE paid for?" question with the PROFILE=ANSWER given for it, and replaces
what sits between index.html's <!-- doctor --> and <!-- /doctor --> markers. Each question
becomes a .step the page reveals in turn, then each section of the report; the run's timing
goes between style.css's run markers, since the page's CSP allows no inline styles. The
hosts' total, which --ssh prints, fills the results section between index.html's
<!-- results --> markers, so the page shows one set of figures. The
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
CLASSES = {"1": "b", "2": "d", "31": "r", "32": "g", "33": "y", "35": "p"}
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


def results(transcript: str) -> tuple[str, str]:
    """The results section's figures and its meter's CSS, from the hosts' total."""
    plain = " ".join(SGR.sub("", line).strip() for line in transcript.replace("\r", "").split("\n"))
    total = re.search(
        r"token-max: total over (\d+) hosts\s+([\d,]+) requests in ([\d,]+) sessions, (~?)\$([\d,]+)",
        plain,
    )
    offered = re.search(
        r"Changes offered: (~?)\$([\d,]+)/mo[^,]*, (\d+)% of these hosts' spend"
        r"(?:, plus up to ~?\$([\d,]+)/mo[^,]*, \d+% of these hosts' spend, from (\d+) changes?)?",
        plain,
    )
    if not total or not offered:
        sys.exit("the results need the hosts' total that --ssh prints")
    hosts, requests, sessions, about, spend = total.groups()
    saves_about, saves, pct, up_to, depends = offered.groups()
    kept = int(spend.replace(",", "")) - int(saves.replace(",", ""))
    caveat = (
        f", except {depends} whose saving depends on how a profile pays, which would save up to ~${up_to}/mo more"
        if up_to
        else ""
    )
    fragment = f"""
  <dl class="stats">
    <div class="stat"><dt>API cost</dt><dd>{about}${spend}</dd></div>
    <div class="stat"><dt>Sessions</dt><dd>{sessions}</dd></div>
    <div class="stat save"><dt>Estimated savings<a href="#fn" class="star">*</a></dt><dd>{saves_about}${saves}<small>/mo</small></dd><dd class="pct">{pct}% of spend</dd></div>
  </dl>
  <div class="meter" role="img" aria-label="Savings are {pct}% of the ${spend} spent">
    <span class="kept"></span><span class="cut"></span>
  </div>
  <p class="meter-key"><span>${kept:,} still spent</span><span>${saves} saved</span></p>
  <p class="fine" id="fn">* At public API prices, across {requests} Claude Code, Codex and T3 Code
  requests on {hosts} of my machines, in the run above on {date.today().isoformat()}. Most of it ran on a
  subscription, which is subsidized, so on a plan the saving shows up as usage limits that
  last longer rather than dollars. The savings are every change
  <code>token-max doctor --defaults</code> would make for a user with no settings files{caveat}.
  ~ marks figures extrapolated over requests that could not be priced.</p>
"""
    meter = f".meter .kept {{ flex: {100 - int(pct)}; }}\n.meter .cut {{ flex: {pct}; }}\n"
    return fragment, meter


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
    run_steps, questions = steps(transcript)
    figures, meter = results(transcript)
    body = "".join(f'<span class="step">{chr(10).join(s)}\n</span>' for s in run_steps)
    fragment = f'<pre class="screen"><code><span class="cmdline">$ <span class="typed">{typed}</span>\n</span>{body}</code></pre>'
    replace(site / "index.html", "<!-- doctor -->", "<!-- /doctor -->", fragment)
    replace(site / "index.html", "<!-- results -->", "<!-- /results -->", figures)
    replace(
        site / "style.css",
        "/* run: assets/demo/site.py writes the run's timing and the results meter here */\n",
        "/* /run */",
        timing(typed, len(run_steps), questions) + meter,
    )


if __name__ == "__main__":
    main()
