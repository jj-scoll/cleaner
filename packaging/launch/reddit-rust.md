# r/rust draft

**Title:** cleaner: an approve-first disk cleaner for Linux, with a symlink-safe deleter on rustix

**Body:**

I've been building cleaner, a disk cleanup tool for Linux desktops (CLI plus a GTK4/libadwaita GUI via gtk4-rs). It finds regenerable junk (`target/`, `node_modules/`, tool caches, stale git worktrees, Docker leftovers, old Nix generations), writes a plan, and deletes only what you approve.

A few parts that might interest Rust folks:

- **The deleter** is ~140 lines on rustix: `openat` with `O_DIRECTORY | O_NOFOLLOW`, `statat` without following links, `unlinkat` relative to the directory fd, and an `st_dev` check per directory. A symlink is unlinked, never followed; a mount point stops the delete. It's there because `remove_dir_all`-style path walks are open to symlink races.
- **Apply-time re-validation**: each entry carries a `(dev, ino, newest mtime)` fingerprint and is skipped if it drifted.
- **Tests**: about 400, hermetic. Temp homes, with Docker/Nix/uv/Go behind a `Runner` trait and systemd/D-Bus faked.

Edition 2024, MIT. Static musl binaries for the CLI. Write-up on the delete path: https://jj-scoll.github.io/cleaner/blog/safe-deletes

Repo: https://github.com/jj-scoll/cleaner. Code review on `src/fsutil/delete.rs` especially welcome.
