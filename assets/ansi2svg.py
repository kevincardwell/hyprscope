#!/usr/bin/env python3
"""Render `tmux capture-pane -e` output (ANSI SGR) to a crisp SVG screenshot."""
import re, sys, html

PALETTE = {
    30: "#3b4252", 31: "#e06c75", 32: "#98c379", 33: "#e5c07b", 34: "#61afef", 35: "#c678dd", 36: "#56b6c2", 37: "#c8ccd4",
    90: "#5c6370", 91: "#e06c75", 92: "#98c379", 93: "#e5c07b", 94: "#61afef", 95: "#c678dd", 96: "#56b6c2", 97: "#ffffff",
}
FG_DEFAULT = "#c8ccd4"
BG_DEFAULT = "#1e2127"
CW, LH = 8.4, 18
SGR = re.compile(r"\x1b\[([0-9;]*)m")

def idx256(n):
    """Map a 256-colour index onto the 16-colour palette keys."""
    return 30 + n if n < 8 else 90 + (n - 8) if n < 16 else 37

def parse(text):
    rows = []
    for line in text.split("\n"):
        fg, bg, bold, dim, strike = FG_DEFAULT, None, False, False, False
        cells = []
        pos = 0
        for m in SGR.finditer(line):
            seg = line[pos:m.start()]
            if seg:
                cells.append((seg, fg, bg, bold, dim, strike))
            pos = m.end()
            codes = [int(c) for c in m.group(1).split(";") if c != ""] or [0]
            i = 0
            while i < len(codes):
                c = codes[i]
                if c == 0: fg, bg, bold, dim, strike = FG_DEFAULT, None, False, False, False
                elif c == 1: bold = True
                elif c == 2: dim = True
                elif c == 9: strike = True
                elif c == 22: bold = dim = False
                elif c == 29: strike = False
                elif c == 39: fg = FG_DEFAULT
                elif c == 49: bg = None
                elif c in PALETTE: fg = PALETTE[c]
                elif 40 <= c <= 47: bg = PALETTE[c - 10]
                elif 100 <= c <= 107: bg = PALETTE[c - 60]
                elif c == 38 and i + 1 < len(codes) and codes[i + 1] == 2:
                    fg = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5]); i += 4
                elif c == 48 and i + 1 < len(codes) and codes[i + 1] == 2:
                    bg = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5]); i += 4
                elif c == 38 and i + 1 < len(codes) and codes[i + 1] == 5:
                    fg = PALETTE.get(idx256(codes[i + 2]), FG_DEFAULT); i += 2
                elif c == 48 and i + 1 < len(codes) and codes[i + 1] == 5:
                    bg = PALETTE.get(idx256(codes[i + 2]), None); i += 2
                i += 1
        seg = line[pos:]
        if seg:
            cells.append((seg, fg, bg, bold, dim, strike))
        rows.append(cells)
    return rows

def width(s):
    import unicodedata
    return sum(2 if unicodedata.east_asian_width(ch) in "WF" else 1 for ch in s)

def render(rows, cols):
    h = len(rows) * LH + 16
    w = cols * CW + 16
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" viewBox="0 0 {w:.0f} {h:.0f}">',
           f'<rect width="100%" height="100%" rx="8" fill="{BG_DEFAULT}"/>',
           '<style>text{font-family:"JetBrains Mono","Fira Code",ui-monospace,monospace;font-size:13px;white-space:pre}</style>']
    for r, cells in enumerate(rows):
        x = 8
        y = 8 + r * LH
        for seg, fg, bg, bold, dim, strike in cells:
            wdt = width(seg) * CW
            if bg:
                out.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{wdt:.1f}" height="{LH}" fill="{bg}"/>')
            style = []
            if bold: style.append("font-weight:600")
            if dim: style.append("opacity:0.55")
            if strike: style.append("text-decoration:line-through")
            out.append(f'<text x="{x:.1f}" y="{y + 13.5:.1f}" fill="{fg}" style="{";".join(style)}">{html.escape(seg)}</text>')
            x += wdt
    out.append("</svg>")
    return "\n".join(out)

if __name__ == "__main__":
    cols = int(sys.argv[1]) if len(sys.argv) > 1 else 160
    text = sys.stdin.read().rstrip("\n")
    sys.stdout.write(render(parse(text), cols))
