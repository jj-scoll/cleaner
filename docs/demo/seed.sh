#!/usr/bin/env bash
# Seeds a throwaway HOME with regenerable junk for the README demo.
# Usage: seed.sh DIR   (DIR must not exist; it becomes the fake HOME)
# Files are allocated with fallocate so their sizes are real on disk.
set -euo pipefail

home=${1:?usage: seed.sh DIR}
[[ ! -e $home ]] || { echo "$home already exists" >&2; exit 1; }
mkdir -p "$home"
cd "$home"

blob() { mkdir -p "$(dirname "$1")"; fallocate -l "$2" "$1"; }
age() { find "$1" -exec touch -h -d "$2 days ago" {} +; }

# Rust project, untouched for two months.
mkdir -p projects/raytracer/src
echo '[package]
name = "raytracer"' > projects/raytracer/Cargo.toml
echo 'fn main() {}' > projects/raytracer/src/main.rs
blob projects/raytracer/target/debug/deps/libraytracer.rlib 1400M
blob projects/raytracer/target/release/raytracer 380M
age projects/raytracer 60

# Node app, untouched for six weeks.
mkdir -p projects/dashboard
echo '{"name": "dashboard"}' > projects/dashboard/package.json
blob projects/dashboard/node_modules/.pnpm/esbuild/bin/esbuild 610M
blob projects/dashboard/node_modules/typescript/lib/typescript.js 240M
age projects/dashboard 45

# Python tool, untouched for three months.
mkdir -p projects/scraper
echo '[project]
name = "scraper"' > projects/scraper/pyproject.toml
blob projects/scraper/.venv/lib/python3.13/site-packages/torch/lib/libtorch.so 1900M
age projects/scraper 90

# A project you're working on today: found, but not pre-selected.
mkdir -p projects/cleaner-fork/src
echo '[package]
name = "cleaner-fork"' > projects/cleaner-fork/Cargo.toml
echo 'fn main() {}' > projects/cleaner-fork/src/main.rs
blob projects/cleaner-fork/target/debug/cleaner 900M

# Tool caches.
blob .cache/go-build/3f/3f9a1c-d 700M
blob .cache/go-build/a1/a1e2b4-d 500M
age .cache/go-build 45
blob .cache/pip/http-v2/7/c/9/wheel-cache 820M
age .cache/pip 120
blob .npm/_cacache/content-v2/sha512/ab/cd/tarball 460M
age .npm 60

# Electron app cache (Slack isn't running in the demo).
blob ".config/Slack/Cache/Cache_Data/data_1" 540M
blob ".config/Slack/Code Cache/js/index" 120M
age .config/Slack 2

# Trash emptied 45 days ago.
blob .local/share/Trash/files/old-iso.img 1200M
mkdir -p .local/share/Trash/info
printf '[Trash Info]\nPath=%s/Downloads/old-iso.img\nDeletionDate=%s\n' \
  "$home" "$(date -d '45 days ago' +%Y-%m-%dT%H:%M:%S)" \
  > .local/share/Trash/info/old-iso.img.trashinfo

# Config: no Docker or Nix, so the demo never touches the real system.
mkdir -p .config/cleaner
cat > .config/cleaner/config.toml <<'EOF'
[caches]
uv_prune = false
go_clean = false

[docker]
enabled = false

[nix]
enabled = false
EOF
