---
title: "Deleting files safely: how cleaner decides what to remove"
---

<!--
Reviewer notes (not part of the prose). Each claim and where it comes from, at v0.2.0:
- plan / review / apply split, entry kinds, fingerprints: src/plan.rs (Fingerprint, EntryKind, Entry::is_estimate)
- apply re-validation order (exists, not symlink, not protected, fingerprint, not git-tracked, delete): src/category/mod.rs apply_target
- missing/failed git counts as tracked: src/git.rs is_tracked
- deleter: src/fsutil/delete.rs (openat/unlinkat, O_NOFOLLOW|O_DIRECTORY, statat SYMLINK_NOFOLLOW,
  st_dev check per directory, CrossesMount, freed_bytes = st_blocks*512 when st_nlink == 1)
- parent of the top-level target is opened by path (O_NOFOLLOW covers its last component only): delete.rs delete_tree_on_dev
- measuring sizes: src/fsutil/measure.rs (lstat only, symlink root refused, other st_dev skipped, InodeSet dedup,
  newest_mtime includes directory mtimes)
- Freed = statvfs f_bavail*f_frsize after minus before, per filesystem: src/disk.rs usage, src/engine.rs apply_plan_with,
  printed by src/cli.rs ("Freed (filesystem free space after minus before)")
- run lock (flock): src/lock.rs; refuses euid 0: src/main.rs
- app caches skipped if the app is running at apply time: src/category/app_caches.rs apply
- store rules re-evaluated at apply time: src/category/store.rs
- protected paths (.git, vendored, AI-tool state, user list): src/protect.rs
- large-and-old is report-only; no undo/trash: README.md "What it finds" / "Safety"
- The ClamAV anecdote is the author's own experience (from the brief), not in the repo. Kept generic on purpose;
  author: confirm or adjust the details before publishing.
-->

# Deleting files safely

I wrote [cleaner](https://github.com/jj-scoll/cleaner) because my disk kept filling up with things I could regenerate in a minute: `target/` directories in projects I hadn't touched in months, `node_modules/` in a dozen worktrees, package-manager caches, old Nix generations, Docker build cache. Every few weeks I'd notice the disk at 95% and spend an evening running `du` and deleting by hand.

Automating that is easy. Automating it so I'd trust it on a timer is harder, and most of cleaner is about that. This post covers the deleting: what it checks, in what order, and what it still doesn't cover.

## Why approve-first

Every cleanup tool guesses. A `build/` directory next to a `CMakeLists.txt` is probably output, but sometimes it holds hand-written files. No heuristic gets that right every time, so cleaner doesn't make the last call: I do. It works in three separate steps:

1. **Plan.** A scan writes a plan file. Each entry has its size, why it's there, whether it's pre-selected, and a fingerprint of what it looked like. Anything the scan skipped is listed too, with the reason.
2. **Review.** I read the plan in the GUI or with `cleaner show`, and tick or untick entries.
3. **Apply.** Only the entries I approved are deleted, and each one is checked again first.

The timers only ever run step 1. A nightly run builds a plan and, if it can free more than a threshold, sends a notification. Nothing scheduled deletes anything.

Most entries are an exact list of paths. A few are rules over a churning cache ("files here older than 30 days"), which are re-evaluated at apply time and whose sizes are labelled as estimates.

## Re-validating at apply time

A plan can be hours old by the time I look at it. Before deleting a path, cleaner checks that:

- it still exists, and is not now a symlink (checked with `lstat`);
- it isn't protected (anything under `.git` or a `vendored/` dir, some AI-tool state, and anything I list in the config);
- its fingerprint still matches;
- git doesn't track anything at or under it.

The fingerprint is the path's device and inode numbers plus the newest mtime anywhere under it, directories included. If the directory was deleted and recreated, the inode is almost certainly different. If anything inside was written, or a file was added or removed, some mtime moves. If anything doesn't match, the entry is skipped with a reason and left for the next plan. cleaner never re-plans on the fly.

The git check exists because a commit changes no mtimes, so the fingerprint can't see it. It fails closed: if `git` is missing or errors, the path counts as tracked.

This is a heuristic, not a hash. A file rewritten in place with a back-dated mtime would get past it. There's also a window between the check and the delete; the deleter below is built so that what changes in that window makes the delete fail rather than go somewhere else.

## A deleter that doesn't follow symlinks

`std::fs::remove_dir_all` exists, and so does `rm -rf`. I wrote my own anyway. It's about 140 lines on top of `openat`, `unlinkat` and `O_NOFOLLOW`.

The everyday reason is symlinks inside the trees being deleted. `node_modules` is full of them: `npm link` and workspace setups put links there that point at real projects. A delete that follows a symlink into a directory removes the target's contents, which here means my source code.

The other reason is a race: time-of-check to time-of-use, or TOCTOU. A path-based recursive delete looks like this:

1. `lstat("target/debug")`: a real directory, so recurse.
2. `readdir("target/debug")`, which lists `foo`.
3. `unlink("target/debug/foo")`.

Every step resolves the path again from the start. If something replaces `target/debug` with a symlink to `~/Documents` between step 1 and step 3, step 3 resolves through the link and deletes `~/Documents/foo`. The check in step 1 was correct when it ran and is meaningless by step 3. On a desktop that "something" is rarely an attacker. Build tools, editors and sync clients rewrite these trees all the time.

cleaner's deleter works on directory file descriptors instead of paths. It opens each directory with `O_DIRECTORY | O_NOFOLLOW`, then lists, stats and unlinks the entries relative to that descriptor (`statat` without following links, `unlinkat`). The descriptor refers to the directory inode it opened, wherever that inode later moves. If an entry turns into a symlink between the `statat` and the `openat`, the open fails instead of following the link, and the delete stops with an error. A symlink found inside the tree is unlinked as a link; its target is never touched.

One honest caveat: the deleter opens the parent of the top-level target by path, so `O_NOFOLLOW` only covers the last part of that path. A symlink higher up would be followed. The apply-time checks above (and scan roots that refuse to be symlinks) are what guard that part.

## Mount boundaries

Each directory the deleter opens has its `st_dev` compared with the target's. If it differs (a bind mount, a FUSE mount, a tmpfs someone mounted inside a build dir), the delete stops with "crosses a mount point". The same check runs at the top level, so asking to delete a mount point is refused rather than emptying whatever is mounted there. Size measurement skips other filesystems too, so the plan never counts bytes it won't delete.

Stopping isn't rolling back, though: whatever it removed in that tree before reaching the mount stays removed, so a failed entry can be partly deleted.

## Measuring "Freed" instead of estimating it

The plan's sizes come from `lstat`: `st_blocks × 512` per file, hardlinks counted once across the plan. That's a good estimate, but what I care about is how much free space I got back.

So "Freed" is measured: cleaner calls `statvfs` on each filesystem involved before applying and again afterwards, and reports the difference in available bytes. The log also records per-entry bytes, but the headline number is the filesystem's own.

I learned why the difference matters on one of my own machines. I deleted a large pile of files and expected the space back, and the per-file accounting agreed with me. The free space barely moved. A long-running ClamAV daemon still had many of those files open. On Linux, unlinking a file removes its name, but the blocks aren't freed until the last open descriptor closes, so the space was held by a process that had no reason to let go. (`lsof +L1`, or `(deleted)` entries under `/proc/*/fd`, will show you this.) Restarting the daemon gave the space back. An estimate would have said "done"; the measured number showed that it wasn't.

Measuring has its own noise: anything else writing to the disk during the run shows up in the difference, in either direction. I'd still rather report a number that's sometimes noisy than one that's sometimes wrong.

## What it doesn't do

- **No undo.** There's no trash and no snapshots. Approved entries are deleted for real; the review step is the safety net.
- **Not a defence against a hostile local user.** The deleter handles races from my own processes. Someone who can already write to my home directory has easier ways to cause damage than racing cleaner.
- **No root.** It refuses to run as root and won't offer anything it couldn't delete as me.
- **Large old files are report-only.** It lists anything over 1 GiB untouched for 90 days, but can't delete from that list.
- **Linux only,** and so far tested mostly on one NixOS desktop.

The code is MIT-licensed at [github.com/jj-scoll/cleaner](https://github.com/jj-scoll/cleaner). If you think one of these checks is wrong, or you've hit a case it misses, I'd like to hear about it.
