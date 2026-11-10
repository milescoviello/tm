#!/usr/bin/env python3
"""Render a tmux pane (with colours) to a PNG, to look at a TUI without a
display:

    tools/shot.py SOCKET TARGET OUT.png

SOCKET is a tmux -L name. Reads `capture-pane -peN` (escape sequences on,
trailing cells kept) and draws it cell by cell. ASCII uses DejaVu Sans Mono;
everything else (braille spinners, box drawing, arrows) uses DejaVu Sans,
which has those glyphs. Needs Pillow.
"""
import re
import subprocess
import sys

from PIL import Image, ImageDraw, ImageFont

MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
MONO_B = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
SANS = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
CW, CH, FS = 10, 20, 16
FG, BG = (192, 202, 245), (26, 27, 38)
BASIC = [
    (21, 22, 30), (247, 118, 142), (158, 206, 106), (224, 175, 104),
    (122, 162, 247), (187, 154, 247), (125, 207, 255), (169, 177, 214),
    (65, 72, 104), (247, 118, 142), (158, 206, 106), (224, 175, 104),
    (122, 162, 247), (187, 154, 247), (125, 207, 255), (192, 202, 245),
]


def xterm256(n):
    if n < 16:
        return BASIC[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return (v, v, v)


def sgr(params, st):
    p = [int(x) if x else 0 for x in params.split(";")] if params else [0]
    i = 0
    while i < len(p):
        c = p[i]
        if c == 0:
            st.update(fg=None, bg=None, bold=False, rev=False, dim=False)
        elif c == 1:
            st["bold"] = True
        elif c == 2:
            st["dim"] = True
        elif c == 22:
            st["bold"] = st["dim"] = False
        elif c == 7:
            st["rev"] = True
        elif c == 27:
            st["rev"] = False
        elif 30 <= c <= 37:
            st["fg"] = BASIC[c - 30]
        elif 90 <= c <= 97:
            st["fg"] = BASIC[c - 90 + 8]
        elif 40 <= c <= 47:
            st["bg"] = BASIC[c - 40]
        elif 100 <= c <= 107:
            st["bg"] = BASIC[c - 100 + 8]
        elif c == 39:
            st["fg"] = None
        elif c == 49:
            st["bg"] = None
        elif c in (38, 48) and i + 1 < len(p):
            key = "fg" if c == 38 else "bg"
            if p[i + 1] == 5 and i + 2 < len(p):
                st[key] = xterm256(p[i + 2])
                i += 2
            elif p[i + 1] == 2 and i + 4 < len(p):
                st[key] = (p[i + 2], p[i + 3], p[i + 4])
                i += 4
        i += 1


def main():
    sock, target, out = sys.argv[1:4]
    raw = subprocess.run(
        ["tmux", "-L", sock, "capture-pane", "-peN", "-t", target],
        capture_output=True, check=True,
    ).stdout.decode("utf-8", "replace")
    width = int(subprocess.run(
        ["tmux", "-L", sock, "display", "-p", "-t", target, "#{pane_width}"],
        capture_output=True, check=True, text=True,
    ).stdout.strip())
    lines = raw.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    img = Image.new("RGB", (width * CW, len(lines) * CH), BG)
    d = ImageDraw.Draw(img)
    mono = ImageFont.truetype(MONO, FS)
    mono_b = ImageFont.truetype(MONO_B, FS)
    sans = ImageFont.truetype(SANS, FS)
    st = dict(fg=None, bg=None, bold=False, rev=False, dim=False)
    tok = re.compile(r"\x1b\[([0-9;]*)m|\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|(.)", re.S)
    for y, line in enumerate(lines):
        x = 0
        for m in tok.finditer(line):
            if m.group(1) is not None:
                sgr(m.group(1), st)
                continue
            ch = m.group(2)
            if ch is None:
                continue
            fg = st["fg"] or FG
            bg = st["bg"] or BG
            if st["rev"]:
                fg, bg = bg, fg
            if st["dim"]:
                fg = tuple(int(v * 0.6) for v in fg)
            wide = 2 if ord(ch) >= 0x1100 and (
                0x1100 <= ord(ch) <= 0x115F or 0x2E80 <= ord(ch) <= 0xA4CF
                or 0xAC00 <= ord(ch) <= 0xD7A3 or 0xF900 <= ord(ch) <= 0xFAFF
                or 0xFF00 <= ord(ch) <= 0xFF60 or 0x1F300 <= ord(ch) <= 0x1FAFF
            ) else 1
            d.rectangle([x * CW, y * CH, (x + wide) * CW - 1, (y + 1) * CH - 1], fill=bg)
            if ch != " ":
                font = (mono_b if st["bold"] else mono) if ord(ch) < 128 else sans
                d.text((x * CW, y * CH + 1), ch, font=font, fill=fg)
            x += wide
    img.save(out)
    print(out)


if __name__ == "__main__":
    main()
