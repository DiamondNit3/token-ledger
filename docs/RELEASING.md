# Host-Agnostic Release Process

This process produces source and binary artifacts locally. It does not require or perform any version-control or hosting operation.

## 1. Prepare the release

1. Choose the version according to semantic versioning.
2. Update `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and user-facing version references.
3. Verify that catalog timestamps and evidence remain appropriate for the release date.
4. Ensure no local database, config, environment file, report, or transcript fixture has entered the public tree.

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
cargo build --release --locked
./target/release/ledger.exe --version
./target/release/ledger.exe prices verify --plain
```

Use an isolated synthetic configuration for mutation tests. A scan against real user data may be used for private smoke testing, but its database and outputs must never enter a release archive.

## 4. Build source artifacts

Run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/package-source.ps1
```

The script uses Cargo's explicit package allowlist and places standard `.crate` and reviewer-friendly `.zip` source archives plus SHA-256 checksums in `open-source/`. It refuses to package when the public-content scan fails.

Inspect the archive listing. It must not contain `dist/`, `target/`, generated evidence HTML, databases, configuration, environment files, user paths, or release binaries.

## 5. Build binary bundles

Binary bundles should contain only:

- the platform executable;
- `README.md`;
- `LICENSE`;
- the applicable price manifest; and
- checksums or provenance supplied alongside the archive.

Do not place old releases inside a new source or binary archive.

## 6. Verify from extraction

Extract each artifact into a new empty directory and verify:

- the documented file list;
- SHA-256 checksums;
- `ledger --version`;
- `ledger prices verify --plain`;
- no-argument help;
- narrow, compact, and wide output;
- JSON/CSV/HTML ANSI isolation; and
- absence of private source paths.

Release only the extracted artifact that passed these checks, not an unverified staging directory.
