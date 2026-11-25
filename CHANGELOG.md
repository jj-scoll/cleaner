# Changelog

Notable changes to cleaner. The release workflow publishes the section whose
heading matches the tag (`## v0.2.0 …`) as that release's notes, so add an
entry here before tagging.

## v0.3.1 - 2026-11-25

### Added
- Scan feedback in the window: under the progress bar, a live "1,240 folders
  checked · 3.1 GiB found so far" line, and when the scan finishes, a "Scan
  done: … in N items, … pre-selected" notice with a Review button.

## v0.3.0 - 2026-11-25

### Added
- **AI models**: Hugging Face hub repos, Ollama models and LM Studio models,
  one row each with its size, never pre-selected. Honours `HF_HUB_CACHE`,
  `HF_HOME` and `OLLAMA_MODELS` (for timer runs, set them in
  `~/.config/environment.d/`). Ollama blobs shared with another model are
  always kept.
- **Old IDE versions**: JetBrains cache and data dirs for versions older than
  the newest you have (settings in `~/.config/JetBrains` are never touched),
  and VS Code / VSCodium extension versions older than the newest that no
  `extensions.json` uses.
- **Toolchains and runtimes**: rustup toolchains, nvm Node versions and pyenv
  Pythons other than the default, the newest and those pinned by a project;
  the conda package cache; Flatpak runtimes no installed app needs. rustup,
  Flatpak and conda removals go through the tool, and what a tool still uses
  is checked again at apply time.
- `[ai_models]`, `[ide_versions]` and `[toolchains]` config sections, with a
  switch per source, and matching Settings groups.
- **`cleaner review [PLAN]`**: review a plan in the terminal, for servers and
  SSH sessions. Tick entries with space (`a` toggles a category), sort with
  `s`, filter by title with `/`, and press Enter, then `y`, to delete. With a
  filter on, only the ticked entries it shows are applied. Leaving without
  deleting (`q`, Esc) exits 1, like answering no to `cleaner apply`. It
  applies through the same checks as `cleaner apply` and the window;
  report-only and skipped rows can't be ticked. Needs a TTY.
- **`cleaner explain <PATH> [--plan ID] [--json]`**: says why a path is or
  isn't in the plan: the entry, category, size, reason, pre-selection and the
  rule that matched; the item that contains it; its skip reason; or, checked
  live without changing anything, the first reason it isn't planned
  (protected, tracked in git, another filesystem, missing project marker, too
  young, category disabled, no rule matches). Never follows a final symlink.
- **`cleaner stats [--json] [--since YYYY-MM-DD]`**: space freed all time,
  this year and in the last 30 days, run count, biggest run and top
  categories. Each run keeps both its measured figure (filesystem free space
  before and after) and its estimate (sum of deleted item sizes); the text
  headline is measured and shows the estimate too when they are more than
  20% apart, and `--json` carries both per run (`per_run`) and in each total.
- Dashboard card: "Freed: … all time · … this year".
- **Holding area (opt-in).** With `[hold] enabled = true` (or Settings →
  Holding area), apply moves files and folders into
  `~/.local/share/cleaner/hold` instead of deleting them, by a
  same-filesystem rename only (never a copy, never over anything).
  `cleaner hold list`, `cleaner hold restore <id|path>` and
  `cleaner hold purge [--all]` manage it. Held items free no space until
  purged, `days` (default 7) after holding, at the start of a later apply
  (never by the timer). Docker objects, Nix generations and tool commands are
  never held, and the confirmation says which items will be held, skipped or
  deleted for good. Items on another filesystem than the holding area are
  skipped, not deleted. There's no GUI page for held items yet, and
  `status` and the history don't count them. Off by default.

### Changed
- The run log gains one line per run that deleted something:
  `{"kind":"run","time":…,"run_id":…,"freed_by_fs":[{"fs":…,"bytes":…}]}`,
  the measured free-space change per filesystem. Older versions and
  `cleaner log` skip it. Runs logged before this release count as estimates.
- Held and purged entries are logged too (`held`, `restored`, and `deleted`
  under the `hold` category for purges).
- The holding area is a protected path: no scan offers it, and
  `cleaner explain` reports it as protected.

### Fixed
- `cleaner explain` knows the rustup, Docker and Flatpak objects, and the
  review's confirmation (window and terminal) says what holding will do
  instead of claiming nothing can be undone.

## v0.2.0 - 2026-11-25

### Added
- **home-manager module.** The flake exports `homeManagerModules.default`:
  `services.cleaner = { enable = true; nightlyAt = "03:00"; lowSpaceEvery = "10min"; }`
  installs the package and declares the nightly and low-space timers, so they
  follow every rebuild (`lowSpaceEvery = null` turns the low-space check off).
- **Sort the review.** Rows in each category now sort largest first. The GUI
  review header has a Sort by picker (Size / Name / Age), remembered between
  runs, and `cleaner show` takes `--sort size|name|age`. Age sorts least
  recently used first, using the project's last edit or commit for artifacts.
- **An icon.** `cleaner timer install` writes the app icon to
  `~/.local/share/icons/hicolor/scalable/apps/` next to the `.desktop` file
  (`uninstall` removes it), the Nix package ships both, and the GUI window
  uses it.

### Changed
- Paths under your home show as `~/…` everywhere in `cleaner show`, `apply`
  and the GUI, including cache and Trash entries. The plan file still stores
  absolute paths.
- Counts read correctly in the singular: "1 item", "1 day", "1 cache dir".
- When the nightly timer units are managed by Nix (symlinks into the store),
  `cleaner timer install` and `uninstall` refuse to touch them,
  `cleaner timer status` says "managed by Nix", settings saves write no units,
  and the GUI greys out the timer rows.
- The Nix package's wrapper sets `CLEANER_EXE`, so timer units and the
  notification's GUI launch keep the wrapper's GTK environment.
- Older plan files still load (the new `last_active` field is optional).

### Docs and project
- A new look: logo, palette and a rebuilt site at
  <https://jj-scoll.github.io/cleaner/>.
- README: recorded demo, GUI screenshots, CI badge and a flow diagram that
  renders in the GitHub app.
- CONTRIBUTING, a security policy with private vulnerability reporting, issue
  forms and a pull request template.

## v0.1.0 - 2026-11-11

The first public release. See the
[release notes](https://github.com/jj-scoll/cleaner/releases/tag/v0.1.0).
