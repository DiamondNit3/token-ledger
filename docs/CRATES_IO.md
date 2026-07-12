# crates.io Publication Assessment

The `token-ledger` package name was unclaimed on crates.io when checked on 2026-07-11. Availability can change at any time.

The package is technically suitable for `cargo install token-ledger`: it has a locked dependency graph, MIT licensing, a declared Rust 1.88 minimum, repository/homepage/documentation metadata, an explicit source-package allowlist, and a primary binary named `token-ledger`.

Publication is intentionally disabled with `publish = false` while the project is private. Before changing that setting:

1. Publish the source repository and security-reporting route.
2. Complete at least one tagged multi-platform release and verify every downloaded archive.
3. Confirm the README's installation and quick-start paths from a clean machine.
4. Run `cargo package --locked` and inspect every included file.
5. Run `cargo publish --dry-run --locked`.
6. Decide whether crates.io releases will be maintained for the same versions and support window as binary releases.

If publication is approved, replace `publish = false` with `publish = ["crates-io"]`. Never place a crates.io API token in the repository, workflow source, command history, or issue tracker. Prefer crates.io trusted publishing when it is configured for the public repository; otherwise use a narrowly scoped repository secret and a separately reviewed release workflow.

Publishing to crates.io is irreversible for a version. Yank can discourage new installs but does not delete published source.
