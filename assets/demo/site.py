"""Write max.kixel.ai's terminal: `token-max doctor` on synthetic transcripts, as HTML.

Usage, from the repository root after `cargo build --release`:
    python3 assets/demo/site.py site/public/index.html

Runs the release binary in a pseudo-terminal against home.py's synthetic home, answers
its questions as walk.tape does, and replaces what sits between the page's
<!-- doctor --> and <!-- /doctor --> markers. Each question becomes a .step the page's
script reveals in turn, so the text stays sharp, selectable and a few kilobytes.
"""

import html
import os
import pty
import re
import shutil
import subprocess
import sys
from pathlib import Path

# The same answers as walk.tape: usage credits, then accept, accept, decline, write.
ANSWERS = ["a", "y", "y", "n", "y"]
COLUMNS = 110
# Every question ends in its dim choices, then ": ".
PROMPT = re.compile(rb"\x1b\[0m: $")
SGR = re.compile(r"\x1b\[([0-9;]*)m")
CLASSES = {"1": "b", "2": "d", "31": "r", "32": "g", "33": "y"}


def run(home: Path) -> str:
    """Answer each question once its prompt shows, then read to exit."""
    root = Path.cwd()
    env = {
        "HOME": str(home),
        "PATH": f"{root / 'target/release'}:{os.environ['PATH']}",
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
        "COLUMNS": str(COLUMNS),
    }
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(home)
        os.execvpe("token-max", ["token-max", "doctor"], env)
    out, pending = b"", b""
    answers = iter(ANSWERS)
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
            answer = next(answers, None)
            if answer is None:
                sys.exit("doctor asked more questions than ANSWERS holds")
            os.write(fd, answer.encode() + b"\r")
            pending = b""
    _, status = os.waitpid(pid, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        sys.exit(f"token-max doctor exited with {os.waitstatus_to_exitcode(status)}")
    if next(answers, None) is not None:
        sys.exit("doctor asked fewer questions than ANSWERS holds")
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


def render(transcript: str) -> str:
    """Group lines into steps, each ending at a question whose answer shows later."""
    steps, lines = [], []
    for raw in transcript.replace("\r\n", "\n").split("\n"):
        line = line_html(raw)
        if re.search(r"\[0m: [a-z]$", raw.rstrip()):
            prompt, answer = line[:-1], line[-1]
            lines.append(f'{prompt}<span class="ans">{answer}</span><span class="cur"></span>')
            steps.append(lines)
            lines = []
        else:
            lines.append(line)
    while lines and not lines[-1]:
        lines.pop()
    steps.append(lines)
    body = "".join(f'<span class="step">{chr(10).join(s)}\n</span>' for s in steps)
    return f'<pre class="screen"><code><span class="cmdline">$ <span class="typed">token-max doctor</span>\n</span>{body}</code></pre>'


def main() -> None:
    page = Path(sys.argv[1])
    home = Path(".tmp/site-demo").resolve()
    shutil.rmtree(home, ignore_errors=True)
    subprocess.run([sys.executable, Path(__file__).with_name("home.py"), home], check=True)
    fragment = render(run(home))
    text = page.read_text()
    start, end = "<!-- doctor -->", "<!-- /doctor -->"
    if text.count(start) != 1 or text.count(end) != 1:
        sys.exit(f"{page} needs one {start} and one {end}")
    head, rest = text.split(start)
    _, tail = rest.split(end)
    page.write_text(f"{head}{start}{fragment}{end}{tail}")


if __name__ == "__main__":
    main()
