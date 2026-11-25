# Contributing to cleaner

Thanks for taking a look. cleaner deletes files for real, so the bar for changes is mostly about safety: every behaviour change comes with tests, and anything that touches what gets deleted comes with tests proving it stays inside the reviewed plan.

Bug reports from distros other than NixOS are especially welcome. Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Dev setup

### With Nix

The flake's default dev shell provides `pkg-config`, GTK 4 and libadwaita. It uses the Rust toolchain already on your `PATH`.

```sh
mkdir -p .gcroots
nix develop --profile .gcroots/devshell
```

`--profile` keeps a GC root on the shell, so a Nix garbage collection can't remove the GTK libraries your build links against. `.gcroots/` is gitignored.

CI uses a second shell, `nix develop .#ci`, which also pins `cargo`, `rustc` and `clippy` from nixpkgs and adds `xvfb-run` and `git` for the GUI tests.

### Without Nix

- Linux with a systemd user session and a freedesktop notification daemon.
- A recent stable Rust toolchain (edition 2024), with `clippy`.
- `git` (the test suite builds real repos and worktrees).
- For the GUI: GTK >= 4.10, libadwaita >= 1.6 and `pkg-config`:

  ```sh
  # Fedora
  sudo dnf install gtk4-devel libadwaita-devel pkgconf-pkg-config
  # Debian 13 / Ubuntu 24.10+
  sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config
  # Arch
  sudo pacman -S gtk4 libadwaita pkgconf
  ```

You don't need Docker, Nix, uv or Go installed to run the tests: they're faked behind a `Runner` trait.

## Building

The GUI is the default `gui` Cargo feature.

```sh
cargo build --release                         # CLI + GUI (needs GTK and libadwaita)
cargo build --release --no-default-features   # CLI only, no GTK needed
```

With Nix, run the first one inside the dev shell: `nix develop --profile .gcroots/devshell -c cargo build --release`. `nix build` builds the flake package (GUI included) the way users get it.

## Tests and lints, as CI runs them

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs two jobs on every push to `main` and every pull request. Both treat clippy warnings as errors. Run the same commands before you open a PR.

**CLI build (no GTK)**, on stable Rust:

```sh
cargo test --locked --no-default-features
cargo clippy --locked --no-default-features --all-targets -- -D warnings
```

**Full build (GTK, via Nix)**, with the GUI smoke tests required to run under Xvfb:

```sh
export CLEANER_REQUIRE_GUI_TESTS=1 GDK_BACKEND=x11 GSK_RENDERER=cairo
nix develop .#ci -c xvfb-run -a cargo test --locked
nix develop .#ci -c cargo clippy --locked --all-targets -- -D warnings
nix build -L --no-link
```

Without `CLEANER_REQUIRE_GUI_TESTS`, the GUI smoke test skips itself when there's no display. With it set, a missing display fails the run instead, which is what CI wants. On a desktop session you can drop `xvfb-run` and the environment variables.

**Formatting.** Run `cargo fmt --all` before you commit. CI fails on unformatted code (`cargo fmt --all --check`).

### The test suite is hermetic

- Every test runs in a temporary home, never your real one.
- Docker, Nix, uv and Go are faked behind the `Runner` trait (`FakeRunner` in [`tests/common/mod.rs`](tests/common/mod.rs)).
- systemd and D-Bus are faked too.
- Ownership by another user is simulated rather than created with `chown`.

Keep it that way. A test that reads or writes the real `$HOME`, talks to the real Docker daemon or systemd, or needs root won't be merged.

## Tests first

For any change in behaviour, write the test first, run it, and check it fails for the reason you expect (not a compile error or a missing fixture). Then make it pass. Say in the PR which test covers the change. Refactors that shouldn't change behaviour need the existing tests passing before and after.

### The safety bar for anything that deletes

Changes to what gets planned, re-validated or deleted (a new category or rule, a new delete path, changes to the walker, the deleter, protection or apply-time checks) need tests proving the change **never touches anything outside the plan**. In practice, show that the new path respects each of the rules in the README's [Safety](README.md#safety) section that applies to it:

- it never follows a symlink, including a symlink swapped in between plan and apply;
- it never crosses a mount point;
- it never deletes git-tracked files;
- it stays out of protected paths (`.git`, `vendored/`, AI-tool state, user `protect` entries);
- anything that changed since the plan was made is skipped at apply time, not re-planned;
- files next to, above and inside the target that weren't in the plan are still there afterwards;
- read-only views (disk map, large-and-old, low-space check) still have no path to deletion.

The existing fixtures recreate these hazards (tracked `node_modules`, symlinked artifacts, hardlinked venvs, a `package.json` nested inside an artifact, worktrees in every state). Reuse them, and add a fixture when your change creates a new hazard.

## Commit style

Commit subjects are short, lowercase and imperative-ish, with a prefix naming the area:

| Prefix | For |
|---|---|
| `fix:` | bug fixes |
| `review:` | the review screen and `cleaner show` |
| `nix:` | the flake, package and home-manager module |
| `ci:` | GitHub Actions workflows |
| `release:` | release workflow and binaries |
| `docs:` | the GitHub Pages site and other docs |
| `readme:` | README changes |
| `chore:` | repo housekeeping |

For a new area, use the area's name (a category or module, for example `docker:` or `timer:`). Use the body to say what changed and why, wrapped at about 72 columns. See `git log` for examples.

## Demo assets

The README's `docs/demo/demo.gif`, `dashboard.png` and `review.png` are generated by [`docs/demo/record.sh`](docs/demo/record.sh). If your change alters the CLI output or the dashboard or review screens, regenerate them in the same PR.

It runs against a throwaway `HOME` at `/var/tmp/cleaner-demo` seeded with fake junk by `seed.sh`, never your real one. From the repo root, after a release build with the GUI:

```sh
cargo build --release
nix shell nixpkgs#asciinema nixpkgs#asciinema-agg nixpkgs#imagemagick \
  nixpkgs#xorg.xorgserver nixpkgs#dbus -c docs/demo/record.sh
```

Without Nix you need asciinema 3, agg, Xvfb, ImageMagick and `dbus-run-session` on your `PATH`. Check the new GIF and screenshots before committing them.

## Pull requests

- Keep each PR to one change. Unrelated cleanups go in their own PR.
- Fill in the checklist in the PR template.
- CI must be green before merge.

By contributing you agree your work is released under the [MIT License](LICENSE).
