# Changelog

Notable changes to cleaner. The release workflow publishes the section whose
heading matches the tag (`## v0.2.0 …`) as that release's notes, so add an
entry here before tagging.

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
