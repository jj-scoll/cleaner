"""Regenerates cleaner's brand SVGs (icon, logo lockups, social preview).

All text is converted to outlines, so the SVGs render the same everywhere,
including GitHub's image proxy and rsvg-convert, with no font installed.
The typeface is Recursive (SIL OFL 1.1, https://www.recursive.design).

    nix shell --impure --expr 'with import <nixpkgs> {}; python3.withPackages (p: [p.fonttools p.uharfbuzz])' \
      -c python3 assets/build.py path/to/Recursive[CASL,CRSV,MONO,slnt,wght].ttf

Then rasterise from the repo root (see docs/brand.md for the full list):

    rsvg-convert -w 1280 -h 640 assets/social-preview.svg -o docs/assets/social-preview.png
"""

import sys
from pathlib import Path

import uharfbuzz as hb
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parent.parent

# Brand palette (docs/brand.md is the source of truth for roles).
NIGHT = "#0A1612"
SPRUCE = "#0F201A"
FIR = "#16302A"
NEEDLE = "#24443B"
LICHEN = "#94B3A6"
FROST = "#E3EEE7"
MARIGOLD = "#F5B83D"
ROSEHIP = "#F48C80"

FONT_PATH = sys.argv[1]
TT = TTFont(FONT_PATH)
UPEM = TT["head"].unitsPerEm
BLOB = hb.Blob.from_file_path(FONT_PATH)
FACE = hb.Face(BLOB)
XHEIGHT = TT["OS/2"].sxHeight / UPEM  # at the default location; close enough for layout

# Named styles: Recursive axis locations.
DISPLAY = {"wght": 800, "CASL": 1, "MONO": 0, "slnt": 0, "CRSV": 0}
BODY = {"wght": 450, "CASL": 0, "MONO": 0, "slnt": 0, "CRSV": 0}
LABEL = {"wght": 600, "CASL": 0, "MONO": 0, "slnt": 0, "CRSV": 0}
MONO = {"wght": 500, "CASL": 0, "MONO": 1, "slnt": 0, "CRSV": 0}


def num(v):
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return "0" if s == "-0" else s


def text(s, size, style, x, y, fill, anchor="start", tracking=0.0):
    """Returns (svg <path>, advance width in px) for `s` set at baseline y."""
    font = hb.Font(FACE)
    font.set_variations(style)
    buf = hb.Buffer()
    buf.add_str(s)
    buf.guess_segment_properties()
    hb.shape(font, buf, {"kern": True, "liga": True})
    scale = size / UPEM
    track = tracking * UPEM
    width = sum(p.x_advance + track for p in buf.glyph_positions) - track
    if anchor == "end":
        x -= width * scale
    elif anchor == "middle":
        x -= width * scale / 2
    gs = TT.getGlyphSet(location=style)
    order = TT.getGlyphOrder()
    pen = SVGPathPen(gs, ntos=num)
    cx = 0.0
    for info, pos in zip(buf.glyph_infos, buf.glyph_positions):
        name = order[info.codepoint]
        t = (scale, 0, 0, -scale, x + (cx + pos.x_offset) * scale, y - pos.y_offset * scale)
        gs[name].draw(TransformPen(pen, t))
        cx += pos.x_advance + track
    return f'<path fill="{fill}" d="{pen.getCommands()}"/>', width * scale


def icon_body(tile=True):
    """The mark on a 128-unit grid: an open box (the plan) with the approval
    tick leaving through its open side (what you let go)."""
    parts = []
    if tile:
        parts.append(
            "<defs><linearGradient id='t' x1='0' y1='0' x2='0' y2='1'>"
            f"<stop offset='0' stop-color='#1B3D33'/><stop offset='1' stop-color='{SPRUCE}'/>"
            "</linearGradient></defs>"
            f'<rect x="8" y="11" width="112" height="112" rx="26" fill="{NIGHT}"/>'
            '<rect x="8" y="6" width="112" height="112" rx="26" fill="url(#t)"/>'
            f'<rect x="8.75" y="6.75" width="110.5" height="110.5" rx="25.25" fill="none" stroke="#2E5548" stroke-width="1.5"/>'
        )
    parts.append(
        f'<path d="M84 32H47Q33 32 33 46V78Q33 92 47 92H84" fill="none" stroke="{FROST}" '
        'stroke-width="12" stroke-linecap="round" stroke-linejoin="round"/>'
        f'<path d="M50 61L64 75L98 37" fill="none" stroke="{MARIGOLD}" '
        'stroke-width="14" stroke-linecap="round" stroke-linejoin="round"/>'
    )
    return "".join(parts)


def svg(w, h, body, title):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" '
        f'role="img" aria-label="{title}"><title>{title}</title>{body}</svg>\n'
    )


def write(rel, content):
    p = ROOT / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)
    print(f"wrote {rel}")


def lockup(word_fill, background=None):
    """Icon + wordmark, x-height of the wordmark centred on the mark."""
    size = 112
    gap = 22
    baseline = 62 + XHEIGHT * size / 2
    word, ww = text("cleaner", size, DISPLAY, 128 + gap, baseline, word_fill, tracking=-0.01)
    w = round(128 + gap + ww + 8)
    pad = 0
    body = ""
    if background:
        pad = 20
        body += f'<rect width="{w + 2 * pad}" height="{128 + 2 * pad}" rx="32" fill="{background}"/>'
    body += f'<g transform="translate({pad} {pad})">' + icon_body() + word + "</g>"
    return svg(w + 2 * pad, 128 + 2 * pad, body, "cleaner")


def social():
    W, H = 1280, 640
    out = [f'<rect width="{W}" height="{H}" fill="{SPRUCE}"/>']
    # Lockup, top left.
    out.append(f'<g transform="translate(88 72) scale(0.75)">{icon_body()}</g>')
    word, _ = text("cleaner", 64, DISPLAY, 88 + 96 + 18, 72 + 48 + XHEIGHT * 64 / 2, FROST, tracking=-0.01)
    out.append(word)
    # Headline and subline.
    for i, line in enumerate(["Delete only", "what you", "approve."]):
        p, _ = text(line, 88, DISPLAY, 88, 300 + i * 92, FROST, tracking=-0.015)
        out.append(p)
    sub, _ = text("Approve-first disk cleanup for Linux desktops.", 26, BODY, 88, 560, LICHEN)
    out.append(sub)
    # A plan card, right: the product's own vocabulary.
    cx, cy, cw, ch = 740, 72, 452, 496
    out.append(f'<rect x="{cx}" y="{cy}" width="{cw}" height="{ch}" rx="22" fill="{FIR}" stroke="{NEEDLE}" stroke-width="2"/>')
    t, _ = text("New plan ready", 26, LABEL, cx + 32, cy + 56, FROST)
    out.append(t)
    t, _ = text("nothing has been touched yet", 18, BODY, cx + 32, cy + 84, LICHEN)
    out.append(t)
    rows = [
        ("Project artifacts", "untouched for 60 days", "4.4 GiB", True),
        ("Tool caches", "untouched for 30+ days", "2.4 GiB", True),
        ("Trash", "deleted over 30 days ago", "1.2 GiB", True),
        ("projects/cleaner-fork", "active 0 days ago", "900 MiB", False),
    ]
    ry = cy + 112
    for label, why, size, on in rows:
        out.append(f'<line x1="{cx + 24}" y1="{ry}" x2="{cx + cw - 24}" y2="{ry}" stroke="{NEEDLE}" stroke-width="2"/>')
        bx, by = cx + 32, ry + 18
        if on:
            out.append(f'<rect x="{bx}" y="{by}" width="28" height="28" rx="7" fill="{MARIGOLD}"/>')
            out.append(
                f'<path d="M{bx + 7} {by + 14.5}L{bx + 12} {by + 19.5}L{bx + 21.5} {by + 8.5}" fill="none" '
                f'stroke="{SPRUCE}" stroke-width="3.6" stroke-linecap="round" stroke-linejoin="round"/>'
            )
        else:
            out.append(f'<rect x="{bx + 1.5}" y="{by + 1.5}" width="25" height="25" rx="6" fill="none" stroke="{LICHEN}" stroke-width="3"/>')
        fill = FROST if on else LICHEN
        t, _ = text(label, 22, LABEL, bx + 46, ry + 34, fill)
        out.append(t)
        t, _ = text(why, 16, BODY, bx + 46, ry + 56, LICHEN)
        out.append(t)
        t, _ = text(size, 20, MONO, cx + cw - 32, ry + 42, fill, anchor="end")
        out.append(t)
        ry += 68
    # The button.
    by = cy + ch - 32 - 52
    out.append(f'<rect x="{cx + 32}" y="{by}" width="{cw - 64}" height="52" rx="26" fill="{MARIGOLD}"/>')
    t, _ = text("Clean selected (8.0 GiB)", 21, LABEL, cx + cw / 2, by + 33, SPRUCE, anchor="middle")
    out.append(t)
    return svg(W, H, "".join(out), "cleaner: delete only what you approve")


write("assets/icon/cleaner.svg", svg(128, 128, icon_body(), "cleaner"))
write("assets/logo-dark.svg", lockup(FROST))
write("assets/logo-light.svg", lockup(SPRUCE))
write("assets/logo.svg", lockup(FROST, background=SPRUCE))
write("assets/social-preview.svg", social())
