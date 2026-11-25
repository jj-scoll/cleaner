# Draft nixpkgs PR: `cleaner: init at 0.2.0`

## Read first: nixpkgs automation/AI policy

nixpkgs' CONTRIBUTING.md ("Automation/AI policy", checked 2026-11-25) says:

- PR descriptions, comments and commit message bodies **must not contain LLM output**. Write the summary below yourself. The "Facts" list is there for you to work from; don't paste it.
- A commit made with LLM help **must** carry an `Assisted-by:` trailer naming the tool and the model version, for example `Assisted-by: Claude Code (Claude Opus 5.5)`. `Co-authored-by:` doesn't count.
- The PR summary needs its own disclosure, separate from the commit's. The template has a "Follows the automation/AI policy" checkbox.
- You must review and understand `package.nix` yourself, and be able to answer reviewers without handing their questions to a tool.

## Commits

1. `maintainers: add <handle>`: your entry in `maintainers/maintainer-list.nix`.
2. `cleaner: init at 0.2.0`: `pkgs/by-name/cl/cleaner/package.nix` (a copy of `packaging/nixpkgs/package.nix`), with `meta.maintainers` filled in and the `Assisted-by:` trailer.

## Facts for your summary (don't paste these)

- Homepage https://jj-scoll.github.io/cleaner/ (the template asks new packages to describe the package or link its homepage).
- What it is: approve-first disk cleanup for Linux desktops. It plans, you review, and only approved entries are deleted, each re-validated first. Freed space is measured with statvfs. There's a CLI and a GTK4/libadwaita GUI.
- `wrapGAppsHook4`. `CLEANER_EXE` is set to the wrapper so the systemd user units from `cleaner timer install`, and the GUI launched from its notification, run the wrapped binary.
- The desktop entry and icon match the upstream flake (`io.github.jj_scoll.Cleaner`).
- The tests run in the sandbox with `nativeCheckInputs = [ git ]`: 418 passed, 0 failed locally. The GTK smoke test prints "skipped (no display)".
- `versionCheckHook` and `nix-update-script` are included.
- Linux only (`/proc`, statvfs, systemd user session).
- The name `cleaner` is generic. Expect a reviewer to ask about it. Nothing in nixpkgs or the Arch repos/AUR uses it as of 2026-11-25.

## PR body (template current as of 2026-11-25; fill the top yourself)

```markdown
<YOUR SUMMARY HERE, written by you, plus a line disclosing the AI assistance>

## Things done

- [ ] Fits [CONTRIBUTING.md], [pkgs/README.md], [maintainers/README.md] and other READMEs.
- [ ] Follows the [automation/AI policy].
- Built on platform:
  - [x] x86_64-linux
  - [ ] aarch64-linux
  - [ ] aarch64-darwin
- Tested, as applicable:
  - [ ] Basic functionality of all binary files, usually in `./result/bin/`.
  - [ ] Ran `nixpkgs-review` on this PR. See [nixpkgs-review usage].
  - [ ] [Package tests] at `passthru.tests`.
  - [ ] [NixOS tests] in [nixos/tests].
  - [ ] Tests in [lib/tests] or [pkgs/test] for functions and "core" functionality.
- [Nixpkgs Release Notes]
  - [ ] Package update: when the change is major or breaking.
- [NixOS Release Notes]
  - [ ] Module addition: when adding a new NixOS module.
  - [ ] Module update: when the change is significant.

[Package tests]: https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md#package-tests
[NixOS tests]: https://nixos.org/manual/nixos/unstable/index.html#sec-nixos-tests
[nixpkgs-review usage]: https://github.com/Mic92/nixpkgs-review#usage

[CONTRIBUTING.md]: https://github.com/NixOS/nixpkgs/blob/master/CONTRIBUTING.md
[automation/AI policy]: https://github.com/NixOS/nixpkgs/blob/master/CONTRIBUTING.md#automationai-policy
[lib/tests]: https://github.com/NixOS/nixpkgs/blob/master/lib/tests
[maintainers/README.md]: https://github.com/NixOS/nixpkgs/blob/master/maintainers/README.md
[nixos/tests]: https://github.com/NixOS/nixpkgs/blob/master/nixos/tests
[pkgs/README.md]: https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md
[pkgs/test]: https://github.com/NixOS/nixpkgs/blob/master/pkgs/test
[Nixpkgs Release Notes]: https://github.com/NixOS/nixpkgs/blob/master/doc/release-notes
[NixOS Release Notes]: https://github.com/NixOS/nixpkgs/blob/master/nixos/doc/manual/release-notes
```

Before ticking each box:

- **x86_64-linux:** ticked because it built locally against nixpkgs 2786a2a with `callPackage`. Rebuild in your nixpkgs checkout with `nix-build -A cleaner`.
- **Basic functionality:** checked so far are `cleaner --version` and `cleaner plan` in a throwaway HOME. Open the GUI once (`./result/bin/cleaner gui`) before ticking.
- **nixpkgs-review:** run `nixpkgs-review rev HEAD` in the checkout, or `nixpkgs-review pr <N>` once the PR is open.
- **Fits CONTRIBUTING / AI policy:** tick only after the disclosure steps above are done.
