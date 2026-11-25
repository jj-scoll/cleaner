# cleaner brand

For anyone making something with cleaner's name on it: the site, the README, a terminal recording, a talk slide. Out of scope: the GTK app's own colours, which follow libadwaita and the user's system theme.

## The idea

cleaner's one promise is that **nothing is deleted until you've approved it**. The brand is built from the product's own vocabulary: the plan, the checkbox, the tick. The mark is an open box (the plan) with a tick (your approval) leaving through its open side (what you let go). It also reads as a lowercase *c*.

The palette is a dark conifer forest rather than the usual near-black: a cleared, quiet place. Marigold is the colour of *approved*. Rosehip is the colour of *permanent* and *skipped*: things that deserve a second look. Every accent has a job; none is decoration.

## Palette

| Name | Hex | Role |
|---|---|---|
| Night | `#0A1612` | Deepest surface: terminal and code backgrounds, the icon's drop edge |
| Spruce | `#0F201A` | Page background |
| Fir | `#16302A` | Raised surfaces: cards, the plan, screenshots' frames |
| Needle | `#24443B` | Borders and dividers only. Never text (1.6:1) |
| Lichen | `#94B3A6` | Secondary text, captions, unticked checkboxes |
| Frost | `#E3EEE7` | Primary text and the wordmark on dark |
| Marigold | `#F5B83D` | Accent 1: approved, selected, primary buttons, focus rings |
| Rosehip | `#F48C80` | Accent 2: permanent deletions, skipped items, warnings |

On light backgrounds (GitHub light, print), the wordmark is Spruce `#0F201A`. The icon keeps its own Spruce tile, so it never changes.

### Contrast (WCAG 2.x, measured)

Computed with the WCAG relative-luminance formula over the hex values above.

| Foreground | on Spruce | on Fir | on Night | Use |
|---|---|---|---|---|
| Frost | 14.2 | 11.8 | 15.5 | body text: AAA everywhere |
| Lichen | 7.5 | 6.2 | 8.2 | secondary text: AA (AAA on Spruce/Night) |
| Marigold | 9.5 | 7.9 | 10.4 | accent text and focus rings: AAA |
| Rosehip | 7.2 | 6.0 | 7.8 | warning text: AA, AAA on Spruce/Night |
| Spruce on Marigold | 9.5 | | | text on primary buttons: AAA |

## Terminal theme

A full 16-colour ANSI set derived from the palette, for terminal recordings and screenshots. Background is Night, foreground is Frost; yellow is Marigold and red is Rosehip, so a recording's highlights match the site. Every colour except 0 (black, used as a background tint) clears 4.5:1 on Night.

| # | Name | Hex | # | Name | Hex |
|---|---|---|---|---|---|
| bg | background | `#0A1612` | fg | foreground | `#E3EEE7` |
| 0 | black | `#1B3029` | 8 | bright black | `#67877C` |
| 1 | red | `#F48C80` | 9 | bright red | `#FFAEA4` |
| 2 | green | `#8FCC8E` | 10 | bright green | `#B5E5AC` |
| 3 | yellow | `#F5B83D` | 11 | bright yellow | `#FFD580` |
| 4 | blue | `#7FAFE6` | 12 | bright blue | `#A9CCF4` |
| 5 | magenta | `#E29BC8` | 13 | bright magenta | `#F2BFDF` |
| 6 | cyan | `#6CCFC3` | 14 | bright cyan | `#9BE7DC` |
| 7 | white | `#C4D4CB` | 15 | bright white | `#F6FBF8` |

### agg

`agg --theme` takes a custom theme as comma-separated hex without `#`: background, foreground, then colours 0–15 (18 values). Verified with agg 1.9.0: a test cast rendered with this string has background pixels of exactly `rgb(10,22,18)`.

```sh
agg --theme 0a1612,e3eee7,1b3029,f48c80,8fcc8e,f5b83d,7fafe6,e29bc8,6ccfc3,c4d4cb,67877c,ffaea4,b5e5ac,ffd580,a9ccf4,f2bfdf,9be7dc,f6fbf8 \
  demo.cast demo.gif
```

## Typography

**[Recursive](https://www.recursive.design)** (SIL OFL 1.1), one variable family for everything. cleaner lives in two places, the terminal and the desktop, and Recursive spans both: its `MONO` axis goes from a proportional sans to a true monospace without changing family.

| Use | Settings |
|---|---|
| Wordmark, headlines | weight 800, `CASL` 1 (casual: soft, brush-written terminals) |
| Body, UI text | weight 400–450, `CASL` 0 (linear) |
| Labels, buttons | weight 600–700, `CASL` 0 |
| Code, commands, sizes | `MONO` 1, weight 400–500 |

The site self-hosts a Latin subset (`docs/assets/fonts/recursive.woff2`, about 136 KiB, with the `wght`, `MONO` and `CASL` axes; slant and cursive are pinned off). Headlines are sentence case. No all-caps labels.

## Logo

| File | Use |
|---|---|
| `assets/logo-dark.svg` | Mark + wordmark for dark backgrounds (Frost wordmark) |
| `assets/logo-light.svg` | Mark + wordmark for light backgrounds (Spruce wordmark) |
| `assets/logo.svg` | Self-contained lockup on a Spruce plate: safe on any background, for places that can't switch by colour scheme |
| `assets/icon/cleaner.svg` | Square app icon, no text (freedesktop hicolor style). PNGs at 128 and 256 beside it |
| `docs/assets/social-preview.png` | 1280×640 Open Graph / GitHub social preview |

In HTML that can switch (GitHub READMEs), use a `<picture>`:

```html
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
  <img src="assets/logo-light.svg" alt="cleaner" width="300">
</picture>
```

Rules:

- Write the name in lowercase: **cleaner**, also mid-sentence. It's a command first.
- Keep clear space around the lockup of at least a quarter of the icon's height.
- Don't recolour the tick: it is always Marigold. Don't put the mark on a busy photo; use `logo.svg`'s plate instead.
- Below 24 px, use the icon alone, never the lockup.

## Regenerating

Every SVG is generated by `assets/build.py` with text converted to outlines, so the files render identically without the font installed (GitHub's image proxy, `rsvg-convert`). It needs the full Recursive variable TTF from [google/fonts](https://github.com/google/fonts/tree/main/ofl/recursive):

```sh
nix shell --impure --expr 'with import <nixpkgs> {}; python3.withPackages (p: [p.fonttools p.uharfbuzz])' \
  -c python3 assets/build.py 'Recursive[CASL,CRSV,MONO,slnt,wght].ttf'
nix shell nixpkgs#librsvg -c sh -c '
  rsvg-convert -w 128 -h 128 assets/icon/cleaner.svg -o assets/icon/cleaner-128.png
  rsvg-convert -w 256 -h 256 assets/icon/cleaner.svg -o assets/icon/cleaner-256.png
  rsvg-convert -w 180 -h 180 assets/icon/cleaner.svg -o docs/assets/apple-touch-icon.png
  rsvg-convert -w 32 -h 32 assets/icon/cleaner.svg -o docs/assets/favicon-32.png
  rsvg-convert -w 1280 -h 640 assets/social-preview.svg -o docs/assets/social-preview.png
  cp assets/icon/cleaner.svg docs/assets/icon.svg
  cp assets/logo-dark.svg docs/assets/logo-dark.svg'
```

The site lives in `docs/` (the GitHub Pages root), so anything it shows must be copied into `docs/assets/`; files outside `docs/` aren't served.
