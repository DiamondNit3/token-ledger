# Release Process

Tagged GitHub releases build and test native Windows, Linux, macOS Intel, and macOS Apple Silicon archives with Rust 1.88.0. Each archive receives an individual SHA-256 file, and the release publishes one consolidated `SHA256SUMS.txt`. A manual workflow run produces private release candidates without publishing a release, even when it is dispatched from a tag.

## 1. Prepare the release

1. Choose the version according to semantic versioning.
2. Update `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and user-facing version references.
3. Verify that catalog timestamps and evidence remain appropriate for the release date.
4. Ensure no local database, config, environment file, report, or transcript fixture has entered the public tree.
5. Regenerate `THIRD-PARTY-NOTICES.html` with `cargo-about` 0.9.1 and verify it matches `Cargo.lock`.

## 2. Run the quality gate

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check-public.ps1
```

The gate formats, lints, tests, inspects Cargo package contents, and scans for common private-path and credential patterns. Review all output manually.

For the minimum supported Rust version:

```powershell
cargo +1.88.0 test --all-targets --locked
```

## 3. Build and inspect binaries

```powershell
cargo +1.88.0 build --release --locked
./target/release/token-ledger.exe --version
./target/release/token-ledger.exe prices verify --plain
```

Use an isolated synthetic configuration for mutation tests. A scan against real user data may be used for private smoke testing, but its database and outputs must never enter a release archive.

## 4. Build source artifacts

Run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/package-source.ps1
```

The script uses Cargo's explicit package allowlist, performs Cargo's packaged-source build verification, and places standard `.crate` and reviewer-friendly `.zip` source archives plus SHA-256 checksums in `open-source/`. It refuses to package when the public-content scan or packaged-source verification fails.

Inspect the archive listing. It must not contain `dist/`, `target/`, generated evidence HTML, databases, configuration, environment files, user paths, or release binaries.

## 5. Build binary bundles

Binary bundles should contain only:

- the platform executable;
- `README.md`;
- `LICENSE`;
- `THIRD-PARTY-NOTICES.html` with the locked four-target dependency notices;
- `RUST-1.88-STANDARD-LIBRARY-NOTICES.html` from the pinned Rust distribution;
- `MUSL-1.2.3-COPYRIGHT.txt` for the Rust 1.88 static Linux target (included in every bundle so the archive contract stays identical);
- the applicable price manifest; and
- checksums or provenance supplied alongside the archive.

Do not place old releases inside a new source or binary archive.

The release workflow builds these native targets:

- `x86_64-pc-windows-msvc` (`.zip`);
- `x86_64-unknown-linux-musl` (`.tar.gz`, static libc);
- `x86_64-apple-darwin` (`.tar.gz`); and
- `aarch64-apple-darwin` (`.tar.gz`).

macOS artifacts are currently unsigned and not notarized. Windows artifacts are also unsigned. The workflow prepends both disclosures and checksum-verification instructions to every generated GitHub release note until signing is implemented.

## 6. Verify from extraction

The workflow extracts every ZIP or TAR into a new empty directory before upload and automatically verifies:

- the documented file list;
- the third-party notice's embedded `Cargo.lock` digest;
- the fixed Rust standard-library and musl notice hashes;
- SHA-256 checksums;
- `token-ledger --version`;
- `token-ledger prices verify --plain`;
- no-argument help;
- the isolated synthetic demo; and
- executable permission on Unix.

The responsive narrow, compact, and wide layouts, JSON/CSV/HTML ANSI isolation, and absence of private source paths remain part of the pre-tag manual review. Release only the exact checksummed archive that passed these checks, not an unverified staging directory.

After downloading candidates or release assets, verify the consolidated checksums before extraction:

```bash
# Linux
sha256sum --check SHA256SUMS.txt

# macOS
shasum -a 256 --check SHA256SUMS.txt
```

On Windows, compute the selected archive's digest and compare it with its named entry in `SHA256SUMS.txt`:

```powershell
(Get-FileHash .\token-ledger-v0.4.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash.ToLowerInvariant()
```

## 7. Publish a tagged release

After every platform candidate has been verified, finalize a dated changelog heading such as `## 0.4.0 - 2026-07-12`, then create and push an annotated tag matching the Cargo version exactly:

```bash
git tag -a v0.4.0 -m "Token Ledger v0.4.0"
git push origin v0.4.0
```

A signed tag is also accepted because it is annotated. Lightweight tags are rejected. Before publication, the release job requires exactly one changelog heading for the tag version with an ISO `YYYY-MM-DD` date. The tag push starts publication; manual dispatch never publishes. The release job also refuses to update or replace an existing GitHub release for that tag.

Download the resulting release assets, verify `SHA256SUMS.txt` independently, and smoke-test the extracted executable on each platform before announcing it.

Do not reuse a version or move a published tag. Correct release defects with a new patch version.
