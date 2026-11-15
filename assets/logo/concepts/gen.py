"""Draw the token-max logo concepts and their comparison sheet.

Run: python3 -I gen.py <out-dir>. Deleted with the concepts once one is picked.
"""
import math
import os
import sys

OUT = sys.argv[1]

THEMES = {
    "light": dict(bg="#ffffff", ink="#0f172a", accent="#7c3aed", green="#15803d", muted="#cbd5e1", knock="#ffffff"),
    "dark": dict(bg="#0d1117", ink="#f1f5f9", accent="#a78bfa", green="#4ade80", muted="#334155", knock="#0d1117"),
}

# Monoline lowercase wordmark: x-height 4..14, ascender 0.5, baseline 14.
GLYPHS = {
    "t": (6.5, "M2.5 0.5V11A3 3 0 0 0 5.5 14H6.5M0 4.5H6.5"),
    "o": (10, "M0 9A5 5 0 1 0 10 9A5 5 0 1 0 0 9"),
    "k": (8.5, "M0 0.5V14M8 4.5L0.5 10.5M3.6 8L8.5 14"),
    "e": (10, "M0 9H10A5 5 0 1 0 8.54 12.54"),
    "n": (9, "M0 4.5V14M0 9A4.5 4.5 0 0 1 9 9V14"),
    "-": (5, "M0 9.5H5"),
    "m": (14, "M0 4.5V14M0 8A3.5 3.5 0 0 1 7 8V14M7 8A3.5 3.5 0 0 1 14 8V14"),
    "a": (10, "M0 9A5 5 0 1 0 10 9A5 5 0 1 0 0 9M10 4.5V14"),
    "x": (9, "M0 4.5L9 14M9 4.5L0 14"),
}
GAP = 4.2
STROKE = 2.4


def shift(d, dx):
    """Translate an absolute path made of M/L/H/V/A commands by dx."""
    out, toks = [], []
    num = ""
    for ch in d:
        if ch.isalpha():
            if num:
                toks.append(num)
                num = ""
            toks.append(ch)
        elif ch in " ,":
            if num:
                toks.append(num)
                num = ""
        elif ch == "-" and num:
            toks.append(num)
            num = ch
        else:
            num += ch
    if num:
        toks.append(num)
    cmd = None
    args = []

    def fmt(v):
        return f"{v:.2f}".rstrip("0").rstrip(".")

    def flush():
        if cmd is None:
            return
        a = [float(x) for x in args]
        if cmd in "ML":
            a = [a[0] + dx, a[1]]
        elif cmd == "H":
            a = [a[0] + dx]
        elif cmd == "A":
            a[5] += dx
        out.append(cmd + " ".join(fmt(v) for v in a))

    for t in toks:
        if t.isalpha():
            flush()
            cmd, args = t, []
        else:
            args.append(t)
    flush()
    return "".join(out)


def wordmark(t, split_accent=True):
    """Return (svg group body, width) for 'token-max' in glyph units."""
    x = 0
    ink, acc = [], []
    for i, ch in enumerate("token-max"):
        w, d = GLYPHS[ch]
        (acc if split_accent and i >= 5 else ink).append(shift(d, x))
        x += w + GAP
    width = x - GAP
    attrs = f'fill="none" stroke-width="{STROKE}" stroke-linecap="round" stroke-linejoin="round"'
    body = f'<path d="{"".join(ink)}" stroke="{t["ink"]}" {attrs}/>'
    if acc:
        body += f'<path d="{"".join(acc)}" stroke="{t["accent"]}" {attrs}/>'
    return body, width


def arc(cx, cy, r, a0, a1):
    """SVG arc from angle a0 to a1 (degrees, clockwise from 3 o'clock)."""
    x0, y0 = cx + r * math.cos(math.radians(a0)), cy + r * math.sin(math.radians(a0))
    x1, y1 = cx + r * math.cos(math.radians(a1)), cy + r * math.sin(math.radians(a1))
    large = 1 if (a1 - a0) % 360 > 180 else 0
    return f"M{x0:.2f} {y0:.2f}A{r} {r} 0 {large} 1 {x1:.2f} {y1:.2f}"


# Each mark draws into a 64x64 box.
def stairs(t):
    s = []
    for col in range(5):
        for row in range(col + 1):
            x, y = 1 + col * 13, 52 - row * 13
            s.append(f'<rect x="{x}" y="{y}" width="11" height="11" rx="2.5" fill="{t["accent"]}"/>')
    return "".join(s)


def gauge(t):
    s = []
    n, start, sweep, gap = 6, 150, 240, 6
    seg = (sweep - gap * (n - 1)) / n
    for i in range(n):
        a0 = start + i * (seg + gap)
        col = t["accent"]
        op = 0.45 + 0.55 * i / (n - 1)
        s.append(f'<path d="{arc(32, 36, 25, a0, a0 + seg)}" fill="none" stroke="{col}" stroke-width="9" opacity="{op:.2f}"/>')
    ang = math.radians(start + sweep - seg / 2)
    nx, ny = 32 + 19 * math.cos(ang), 36 + 19 * math.sin(ang)
    s.append(f'<path d="M32 36L{nx:.2f} {ny:.2f}" stroke="{t["ink"]}" stroke-width="5" stroke-linecap="round"/>')
    s.append(f'<circle cx="32" cy="36" r="6" fill="{t["ink"]}"/>')
    return "".join(s)


def pulse(t):
    return (
        f'<circle cx="32" cy="32" r="27" fill="none" stroke="{t["accent"]}" stroke-width="6"/>'
        f'<path d="M12 34H22L27 18L36 48L41 30H52" fill="none" stroke="{t["ink"]}" stroke-width="5.5" stroke-linecap="round" stroke-linejoin="round"/>'
    )


def ceiling(t):
    return (
        f'<circle cx="32" cy="32" r="30" fill="{t["accent"]}"/>'
        f'<path d="M19 17H45M19 44L32 30L45 44" fill="none" stroke="{t["knock"]}" stroke-width="7" stroke-linecap="round" stroke-linejoin="round"/>'
    )


def slice_(t):
    # A coin with its top-left quarter pulled out: the part your settings save.
    r, c, off = 25, 35, 6
    a = arc(c, c, r, -90, 180)
    body = f'<path d="M{c} {c}V{c - r}{a[a.index("A"):]}Z" fill="{t["accent"]}"/>'
    sx = sy = c - off
    body += f'<path d="M{sx} {sy}H{sx - r}A{r} {r} 0 0 1 {sx} {sy - r}Z" fill="{t["green"]}"/>'
    return body


def receipt(t):
    zig = "".join(f"L{50 - 6 * i - 3} {58 - (i % 2 == 0) * 5}L{50 - 6 * (i + 1)} 58" for i in range(6))
    return (
        f'<path d="M14 6H50V58{zig}Z" fill="none" stroke="{t["ink"]}" stroke-width="4.5" stroke-linejoin="round"/>'
        f'<path d="M22 19H42M22 29H42" stroke="{t["muted"]}" stroke-width="4.5" stroke-linecap="round"/>'
        f'<path d="M22 39H34" stroke="{t["ink"]}" stroke-width="4.5" stroke-linecap="round"/>'
        f'<path d="M18 33L46 21" stroke="{t["accent"]}" stroke-width="5" stroke-linecap="round"/>'
    )


CONCEPTS = [
    ("stairs", "Stairs", "Tokens stacked to the max (today's T3 Code icon)", stairs),
    ("gauge", "Gauge", "A meter pinned at max", gauge),
    ("pulse", "Pulse", "A token with doctor's heartbeat", pulse),
    ("ceiling", "Ceiling", "A coin with a caret up to its cap", ceiling),
    ("slice", "Slice", "A coin with the saved slice pulled out", slice_),
    ("receipt", "Receipt", "A bill with a line struck off", receipt),
]


def svg(w, h, body, title="token-max"):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:g} {h:g}" width="{w:g}" height="{h:g}"><title>{title}</title>{body}</svg>\n'


def lockup(fn, t):
    """Mark at 64 px, wordmark scaled to sit on the mark's baseline."""
    scale = 2.6
    wm, ww = wordmark(t)
    x0 = 64 + 18
    body = fn(t) + f'<g transform="translate({x0} {13}) scale({scale})">{wm}</g>'
    return body, x0 + ww * scale + STROKE * scale / 2 + 2


def favicon(fn):
    """Mark on a dark tile so it reads on any tab strip."""
    t = THEMES["dark"]
    return f'<rect width="64" height="64" rx="14" fill="#0f172a"/><g transform="translate(8 8) scale(.75)">{fn(dict(t, knock="#0f172a"))}</g>'


def main():
    os.makedirs(OUT, exist_ok=True)
    rows = []
    for key, name, blurb, fn in CONCEPTS:
        for tn, t in THEMES.items():
            body, w = lockup(fn, t)
            open(f"{OUT}/{key}-{tn}.svg", "w").write(svg(w, 64, body))
        open(f"{OUT}/{key}-favicon.svg", "w").write(svg(64, 64, favicon(fn)))
        rows.append((key, name, blurb, fn))

    # Comparison sheet: light panel then dark panel, one row per concept.
    rh, pw, pad = 120, 760, 32
    H = pad * 2 + 40 + rh * len(rows)
    out = []
    for p, (tn, t) in enumerate(THEMES.items()):
        ox = p * pw
        out.append(f'<rect x="{ox}" width="{pw}" height="{H}" fill="{t["bg"]}"/>')
        out.append(f'<text x="{ox + pad}" y="{pad + 8}" fill="{t["ink"]}" opacity=".6" font-family="system-ui,sans-serif" font-size="15">{tn} background</text>')
        for i, (key, name, blurb, fn) in enumerate(rows):
            y = pad + 40 + i * rh
            out.append(f'<text x="{ox + pad}" y="{y + 12}" fill="{t["ink"]}" font-family="system-ui,sans-serif" font-size="14"><tspan font-weight="600">{chr(65 + i)}. {name}</tspan><tspan dx="8" opacity=".6">{blurb}</tspan></text>')
            body, w = lockup(fn, t)
            out.append(f'<g transform="translate({ox + pad} {y + 26}) scale(.95)">{body}</g>')
            fx = ox + pw - pad - 120
            out.append(f'<g transform="translate({fx} {y + 30}) scale(.5)">{fn(t)}</g>')
            out.append(f'<g transform="translate({fx + 50} {y + 30}) scale(.5)">{favicon(fn)}</g>')
            out.append(f'<g transform="translate({fx + 96} {y + 38}) scale(.25)">{favicon(fn)}</g>')
    sheet = "".join(out)
    open(f"{OUT}/concepts.svg", "w").write(svg(pw * 2, H, sheet, "token-max logo concepts"))


main()
