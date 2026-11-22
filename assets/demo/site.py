"""Write max.kixel.ai's terminal: the maintainer's `token-max doctor --defaults`, as HTML.

Usage, from the repository root after `cargo build --release`, on the maintainer's host:
    python3 assets/demo/site.py site/public ~/.claude=p ~/.claude-api-credits=a ...

Runs the release binary in a pseudo-terminal against this host's own transcripts, answering
each "How is PROFILE paid for?" question with the PROFILE=ANSWER given for it, and replaces
what sits between index.html's <!-- doctor --> and <!-- /doctor --> markers. Each question
becomes a .step the page reveals in turn, then each section of the report; the run's timing
goes between style.css's steps markers, since the page's CSP allows no inline styles. The
EVERYTHING ELSE section is cut to one line: it is hundreds of lines long, and it names
project files, which can belong to private repositories. Read the result before committing it.
"""

import html
import os
import pty
import re
import sys
from pathlib import Path

COMMAND = "token-max doctor --defaults"
COLUMNS = 100
# Every question ends in its dim choices, then ": ".
PROMPT = re.compile(rb"\x1b\[0m: $")
SGR = re.compile(r"\x1b\[([0-9;]*)m")
CLASSES = {"1": "b", "2": "d", "31": "r", "32": "g", "33": "y"}
# A report section opens with its bold uppercase heading.
HEADING = re.compile(r"^  [A-Z][A-Z ]+[A-Z]  ")
CUT = "EVERYTHING ELSE"


def run(answers: dict[str, str]) -> str:
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
        os.execvpe("token-max", COMMAND.split(), env)
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
        sys.exit(f"{COMMAND} exited with {os.waitstatus_to_exitcode(status)}")
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
    out, lines, questions = [], [], 0
    raws = transcript.replace("\r\n", "\n").split("\n")
    i = 0
    while i < len(raws):
        raw = raws[i]
        plain = SGR.sub("", raw.split("\r")[-1])
        if re.search(r"\[0m: [a-z]$", raw.rstrip()):
            line = line_html(raw)
            prompt, answer = line[:-1], line[-1]
            lines.append(f'{prompt}<span class="ans">{answer}</span><span class="cur"></span>')
            out.append(lines)
            lines, questions = [], questions + 1
        elif HEADING.match(plain):
            while lines and not lines[-1]:
                lines.pop()
            if lines:
                out.append(lines + [""])
            lines = [line_html(raw)]
            if plain.strip().startswith(CUT):
                # Skip to the next section, keeping only a count of what was cut.
                j = i + 1
                while j < len(raws) and not HEADING.match(SGR.sub("", raws[j])):
                    j += 1
                cut = sum(1 for r in raws[i + 1 : j] if r.strip())
                lines.append(
                    f'<span class="elided">  … {cut} lines on every other setting and feature, cut from this page</span>'
                )
                i = j
                continue
        else:
            lines.append(line_html(raw))
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
    return tidy, questions


def timing(count: int, questions: int) -> str:
    """CSS for when each step and its answer show: questions wait for a reader, the report streams."""
    rules = [
        f".term.play .typed {{ animation: type 0.9s steps({len(COMMAND)}) 0.3s both; }}",
        f"@keyframes type {{ from {{ width: 0; }} to {{ width: {len(COMMAND)}ch; }} }}",
    ]
    t = 1.6
    for n in range(count):
        # The first step is the command line's sibling at nth-child(2).
        gap = 1.4 if n < questions else 0
        rules.append(f".step:nth-child({n + 2}) {{ --t: {t:.1f}s; --gap: {gap:.1f}s; }}")
        t += gap + (1.5 if n == questions - 1 else 0.6)
    return "\n".join(rules) + "\n"


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
    answers = {}
    for arg in sys.argv[2:]:
        profile, sep, answer = arg.rpartition("=")
        if not sep or answer not in ("p", "a", "s"):
            sys.exit(f"{arg!r} is not PROFILE=p, PROFILE=a or PROFILE=s")
        home = os.environ["HOME"]
        answers["~" + profile[len(home) :] if profile.startswith(home) else profile] = answer
    run_steps, questions = steps(run(answers))
    body = "".join(f'<span class="step">{chr(10).join(s)}\n</span>' for s in run_steps)
    fragment = f'<pre class="screen"><code><span class="cmdline">$ <span class="typed">{COMMAND}</span>\n</span>{body}</code></pre>'
    replace(site / "index.html", "<!-- doctor -->", "<!-- /doctor -->", fragment)
    replace(
        site / "style.css",
        "/* steps: assets/demo/site.py writes the run's timing here */\n",
        "/* /steps */",
        timing(len(run_steps), questions),
    )


if __name__ == "__main__":
    main()
