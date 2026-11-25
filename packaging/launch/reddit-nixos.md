# r/NixOS draft

**Title:** cleaner: approve-first disk cleanup, with a flake and a home-manager module

**Body:**

I built cleaner on and for my NixOS desktop. It scans for regenerable junk (build artifacts, tool caches, stale worktrees, Docker leftovers) and writes a plan you review before anything is deleted.

The Nix side:

- Old profile generations are kept by **count**, never by age, so a machine left off for a month doesn't lose its rollbacks.
- Dangling `result` links and idle devbox roots are listed, and a store GC is an explicit entry. Closure sizes are shown as estimates, since nothing is freed until the GC runs.
- System generations go through one `pkexec` prompt. cleaner itself refuses to run as root.

Try it with `nix run github:jj-scoll/cleaner`. With home-manager:

```nix
imports = [ inputs.cleaner.homeManagerModules.default ];
services.cleaner.enable = true;   # nightly plan + low-space check, declared as user timers
```

The timers only ever plan and notify; deleting always needs your approval.

How the deleting stays safe (symlinks, mounts, measuring freed space): https://jj-scoll.github.io/cleaner/blog/safe-deletes

Repo: https://github.com/jj-scoll/cleaner. Feedback on the flake and the module is welcome.
