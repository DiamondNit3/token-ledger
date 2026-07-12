# Dependency Policy

Token Ledger commits `Cargo.lock` and builds with `--locked` so a release can be tied to an exact dependency graph.

The open-source readiness audit for version 0.4.0 resolved 277 packages across the complete Cargo metadata graph. Every resolved package declared a license expression or license file; no missing-license entry was found. The binary notice graph excludes development-only dependencies and contains 186 unique third-party package/version pairs used across the four release targets. This is an engineering inventory, not legal advice.

## Policy

- Prefer maintained crates with explicit licensing and a documented minimum Rust version.
- Avoid unnecessary dependencies in the transcript-parsing and privacy boundary.
- Keep default network features disabled when they are not needed.
- Use Rustls for the optional HTTPS catalog client.
- Bundle SQLite to reduce platform ambiguity.
- Review duplicate major versions and newly introduced native build dependencies.
- Re-run the license, advisory, test, and public-content audits whenever `Cargo.lock` changes.

The dependency graph currently contains permissive and weak-copyleft alternatives including MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, MPL-2.0, CDLA-Permissive-2.0, BSL-1.0, and Unlicense expressions. `about.toml` chooses accepted alternatives for distribution, and `THIRD-PARTY-NOTICES.html` reproduces the selected texts and attributions. It is generated offline with locked `cargo-about` 0.9.1 inputs for Windows x64, Linux x64 musl, Intel macOS, and Apple-silicon macOS, and is included in every binary archive. Its header binds it to the exact `Cargo.lock` SHA-256 digest.

The `zstd-sys` entry is explicitly clarified with checksums for its MIT Rust wrapper, BSD-licensed generated bindings, and the bundled Zstandard C library's BSD notice. This preserves the native component's required binary-distribution attribution instead of relying only on the crate manifest expression.

The `ring` 0.17.14 entry is also hash-pinned because its split license layout is newer than cargo-about's built-in workaround. The clarification includes its Apache files, Brian Smith ISC notice, and each distinct Google/BoringSSL-derived ISC header used by the supported target set. Globally dual-licensed crates prefer Apache-2.0 before MIT so a missing upstream MIT file can never degrade into an unfilled copyright template; no selected Apache package in the locked release graph contains a separate `NOTICE` file.

Cargo dependency analysis does not cover code linked from the Rust toolchain. Every archive therefore also carries Rust 1.88's generated `COPYRIGHT-library.html` from the official rustup distribution and [musl 1.2.3's complete COPYRIGHT text](https://git.musl-libc.org/cgit/musl/plain/COPYRIGHT?h=v1.2.3). [Rust 1.88's official musl build](https://raw.githubusercontent.com/rust-lang/rust/1.88.0/src/ci/docker/scripts/musl-toolchain.sh) is pinned to musl 1.2.3; the fixed asset hashes are checked before release. The musl notice is intentionally present in non-Linux archives too, keeping one auditable file contract across platforms.

Regenerate and verify it with:

```powershell
cargo install --locked --features cli cargo-about --version 0.9.1
cargo fetch --locked
./scripts/generate-third-party-notices.ps1
./scripts/generate-third-party-notices.ps1 -Check
```

CI and the release workflow compile the pinned generator and reproduce the notice byte-for-byte; `.gitattributes` and the generator pin notice/template line endings to LF for clean-checkout stability. Review newly selected licenses and text whenever `Cargo.lock`, the target matrix, `about.toml`, `about.hbs`, or the generator version changes. A distributor remains responsible for satisfying the selected license terms.

The bundled SQLite runtime is security-sensitive because Token Ledger uses WAL mode across processes. A runtime integration test queries `sqlite_version()` and rejects versions affected by SQLite's WAL-reset advisory; dependency updates must also pass the Rust 1.88 minimum-version job.

## Minimum Rust version

Rust 1.86 was rejected by a locked transitive build path in `comfy-table`. The complete suite passes on Rust 1.88, which is therefore the declared minimum supported Rust version for this source release.
