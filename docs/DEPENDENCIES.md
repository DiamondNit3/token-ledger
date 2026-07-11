# Dependency Policy

Token Ledger commits `Cargo.lock` and builds with `--locked` so a release can be tied to an exact dependency graph.

The open-source readiness audit for version 0.3.0 resolved 277 packages. Every resolved package declared a license expression or license file in Cargo metadata; no missing-license entry was found. This is an engineering inventory, not legal advice or a substitute for reviewing the license text shipped by each dependency.

## Policy

- Prefer maintained crates with explicit licensing and a documented minimum Rust version.
- Avoid unnecessary dependencies in the transcript-parsing and privacy boundary.
- Keep default network features disabled when they are not needed.
- Use Rustls for the optional HTTPS catalog client.
- Bundle SQLite to reduce platform ambiguity.
- Review duplicate major versions and newly introduced native build dependencies.
- Re-run the license, advisory, test, and public-content audits whenever `Cargo.lock` changes.

The dependency graph currently contains permissive and weak-copyleft alternatives including MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, MPL-2.0, CDLA-Permissive-2.0, BSL-1.0, and Unlicense expressions. A distributor remains responsible for satisfying the selected license terms and including notices required by the actual release contents.

## Minimum Rust version

Rust 1.86 was rejected by a locked transitive build path in `comfy-table`. The complete 130-test suite passes on Rust 1.88, which is therefore the declared minimum supported Rust version for this source release.
