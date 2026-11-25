#!/usr/bin/env bash
# Regenerates the README's demo.gif, dashboard.png and review.png against a
# throwaway HOME, never your real one. Run from the repo root after
# `cargo build --release` (with the GUI). Needs asciinema 3, agg, Xvfb,
# ImageMagick and dbus-run-session:
#   nix shell nixpkgs#asciinema nixpkgs#asciinema-agg nixpkgs#imagemagick \
#     nixpkgs#xorg.xorgserver nixpkgs#dbus -c docs/demo/record.sh
set -euo pipefail

demo=/var/tmp/cleaner-demo
cast=$demo.cast
rm -rf "$demo" "$cast"
docs/demo/seed.sh "$demo"
export HOME=$demo PATH=$PWD/target/release:$PATH

# GUI screenshots first: apply in the recording deletes the seeded junk.
id=$(cleaner plan --json | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["id"])')
python3 -I docs/demo/screenshot.py docs/demo/dashboard.png
python3 -I docs/demo/screenshot.py docs/demo/review.png "$id"

asciinema rec --headless --window-size 124x26 \
  -c "python3 -I $PWD/docs/demo/drive.py" "$cast"
# Brand terminal theme: bg, fg, colours 0-15. Keep in step with docs/brand.md.
theme=0a1612,e3eee7,1b3029,f48c80,8fcc8e,f5b83d,7fafe6,e29bc8,6ccfc3,c4d4cb,67877c,ffaea4,b5e5ac,ffd580,a9ccf4,f2bfdf,9be7dc,f6fbf8
agg --font-size 16 --theme "$theme" --idle-time-limit 8 --last-frame-duration 4 \
  "$cast" docs/demo/demo.gif

rm -rf "$demo" "$cast"
