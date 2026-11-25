## What and why

<!-- One change per PR. Link the issue it fixes, if any. -->

## Checklist

- [ ] Tests written first for any behaviour change; they failed before the change and pass after.
- [ ] If this changes what gets planned or deleted: tests show nothing outside the plan is touched (see [CONTRIBUTING.md](https://github.com/jj-scoll/cleaner/blob/main/CONTRIBUTING.md#the-safety-bar-for-anything-that-deletes)).
- [ ] `cargo test --locked --no-default-features` and `cargo clippy --locked --no-default-features --all-targets -- -D warnings` pass.
- [ ] If the GUI is touched: the full build passes too (`cargo test --locked` and `cargo clippy --locked --all-targets -- -D warnings`, in `nix develop .#ci` or with GTK installed).
- [ ] `cargo fmt --all --check` passes.
- [ ] If CLI output or the dashboard/review screens changed: demo assets regenerated with `docs/demo/record.sh`.
- [ ] README updated if user-facing behaviour or config changed.
