# Security policy

cleaner deletes files for real, with no trash and no undo. A bug that makes it delete something you didn't approve is a security bug, and is treated as one.

## Supported versions

Only the [latest release](https://github.com/jj-scoll/cleaner/releases/latest) gets fixes. Fixes land on `main` first and ship in the next release. Please check that a problem still happens on the latest release (or on `main`) before reporting it.

| Version | Supported |
|---|---|
| latest release (currently 0.1.x) | yes |
| anything older | no |

## Reporting a vulnerability

**Please don't open a public issue, discussion or pull request for a security problem.**

Report it privately through GitHub's private vulnerability reporting: on the repository page, go to **Security → Report a vulnerability** ([direct link](https://github.com/jj-scoll/cleaner/security/advisories/new)). Only the maintainer can see the report.

Please include:

- the output of `cleaner --version`, your distro, and whether you installed from a release binary, Nix or source;
- the steps to reproduce, ideally against a throwaway directory or `HOME`, never your real one;
- what got deleted (or could be) that wasn't in the reviewed plan, and the relevant `cleaner show` output or plan file, with private paths redacted;
- if you have one, a failing test or a minimal proof of concept.

## Scope

In scope, roughly in order of how much they matter:

- **Deleting outside a reviewed plan.** Anything that removes, truncates or overwrites a file or directory that wasn't an approved item in the plan being applied, including something next to, above or inside an approved item.
- **Symlink and path traversal.** Ways to make a scan, a size measurement or a delete follow a symlink, cross a mount point, escape a scan root, or act on a different path than the one reviewed, for example by swapping a path between plan and apply.
- **Bypassing the safety rules** in the README's [Safety](README.md#safety) section: git-tracked files, protected paths (`.git`, `vendored/`, AI-tool state, your `protect` list), apply-time re-validation, or the running-app check for browser and Electron caches.
- **Timers and privileges.** The systemd user timers only plan and notify; they never delete. Anything that makes a timer, `cleaner plan --notify` or `cleaner plan --if-low` delete something, run as another user or as root, or that lets another local user influence what your timers or plans do (for example through files in `$XDG_RUNTIME_DIR` or the state directory) is in scope. So is anything that gets cleaner to do more than intended through its one `pkexec` prompt for Nix system generations.
- **Plan file tampering.** Ways to get `cleaner apply` to delete something that isn't in the plan you reviewed.

Out of scope:

- cleaner deleting something that **was** in the plan you approved. The review step is the safety net, and approved items are meant to be deleted. If a default pre-selects something it shouldn't, please open a regular bug report.
- Problems that need an attacker who already runs code as you or as root.
- Bugs in Docker, Nix, git, uv, GTK or other tools cleaner calls, unless cleaner is calling them unsafely.

## What to expect

cleaner is a hobby project maintained by one person in their spare time, so responses are best effort, not guaranteed. I aim to acknowledge a report within a week or two and to keep you updated as I look into it. Valid reports get a fix in a new release and a GitHub security advisory, with credit to you unless you'd rather not be named.

Please give a reasonable amount of time for a fix to ship before disclosing publicly.
