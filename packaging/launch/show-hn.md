# Show HN draft

**Title:** Show HN: Cleaner – approve-first disk cleanup for Linux desktops

**URL:** https://github.com/jj-scoll/cleaner

**Text:**

I kept running out of disk to stale `target/` and `node_modules/` dirs, package caches, old Nix generations and Docker build cache, and cleaning up by hand every few weeks. Existing tools each cover one slice, and the ones that cover everything want to delete on their own.

cleaner scans on a timer and writes a plan: every candidate with its size and the reason it's there. Nothing is deleted until you approve entries in a GTK app or with `cleaner apply`. At apply time each entry is checked again (still exists, not a symlink, fingerprint unchanged, not tracked by git) and skipped if anything moved.

The deleter is built on openat/unlinkat with O_NOFOLLOW, so it never follows a symlink and stops at mount points. "Freed" is measured with statvfs before and after rather than added up from file sizes. I wrote up why that matters, with a story about a virus scanner holding deleted files open: https://jj-scoll.github.io/cleaner/blog/safe-deletes

Rust, MIT. Static CLI binaries for x86_64/aarch64, plus a Nix flake with the GUI. Linux only, and so far tested mainly on one NixOS desktop, so reports from other distros are very welcome.
