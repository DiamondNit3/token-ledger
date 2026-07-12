# Changelog

All notable user-facing changes to Token Ledger are recorded here. The project follows semantic versioning while it remains pre-1.0: minor releases may change human presentation, while machine-schema changes require an explicit new schema identifier.

## Unreleased

No changes yet.

## 0.4.4 - 2026-07-12

### Security

- Upgraded `rusqlite` to 0.39.0 and bundled `libsqlite3-sys` to 0.37.0, moving the shipped SQLite runtime from vulnerable 3.50.2 to fixed 3.51.3.
- Made a missing `schema_version` fail closed whenever any ledger or privacy state exists; neither ordinary open nor migration consent can bless damaged metadata as a fresh database.

### Fixed

- Added a runtime regression that rejects bundled SQLite versions affected by the WAL-reset advisory.
- Added database regressions for damaged metadata with a retained raw path and for genuinely empty database initialization.
- Added end-to-end migration CLI tests covering missing consent, `--db` targeting, authorized migration, idempotency, fresh databases, and WAL cleanup failure.
- Added help text for destructive confirmations, `--no-scan`, grouping, format, and export destination options.
- Clarified that v0.4.1 report exports must be created with the old binary, cannot restore the ledger, and are distinct from a SQLite-safe backup that captures WAL state.
- Narrowed the reproducibility claim to accounting decisions and price evidence; complete export bytes include current scan and generation timestamps.

## 0.4.3 - 2026-07-12

### Launch safety

- Made the destructive schema-v6 privacy barrier fail closed during ordinary commands and added `token-ledger migrate --accept-history-loss` as a deliberate authorization step.
- Added prominent warnings to retain original session files and export or back up v0.4.1 data because history cannot be rebuilt when those source files have expired or been deleted.
- Added a regression proving that an unapproved migration leaves a v6 observation cache intact even when its original source is absent.

### Scanner hardening

- Parse each admitted source from a bounded private snapshot whose exact digest is compared with the post-parse live source, closing the hash-before-parse mutation gap.
- Carry each trusted discovery boundary through scanning and reject detected symbolic-link and Windows reparse-point ancestors below it, while preserving trusted operating-system aliases such as macOS `/var` and documenting that the CLI is not a sandbox for hostile concurrently controlled source trees.
- Charge every stability retry against the 2 GiB aggregate work budget instead of reusing the first attempt's reservation.

## 0.4.2 - 2026-07-12

### Fixed

- Added a schema-v7 privacy barrier that invalidates pre-barrier schema-v6 accounting caches instead of trusting potentially contaminated rows or double-hashing legitimate pseudonyms.
- Acquired one exclusive migration transaction before reading the schema version, advanced every version with compare-and-swap checks, and rejected schema-marker downgrades or deletion at the database boundary.
- Made every incomplete compressed parse all-or-nothing, including oversized-line, truncated-tail, adapter-gap, record, observation, warning, byte, and decompression failures.
- Revalidated aggregate work from the safely opened source handle and lowered the explicit physical source ceiling to 128 MiB so every admitted source fits the 2 GiB invocation budget.
- Opened sources without following final-component Unix symlinks or Windows reparse points and retained the verified handle through fingerprinting, checkpoint validation, and parsing.

### Repository

- Made CODEOWNERS own itself, corrected public-repository documentation, and reclassified v0.4.1 as a prelaunch security patch.
- Closed the Rust-1.88-incompatible rusqlite dependency update without merging it.

### Upgrade note

Before upgrading from any earlier Token Ledger version, including v0.4.0 and v0.4.1, terminate every older `token-ledger` process. The first v0.4.2 open invalidates schema-v6 source, observation, warning, and reconciliation caches so the next scan can rebuild identifier-bearing accounting from authoritative local sources. Re-import any provider reconciliation exports afterward.

## 0.4.1 - 2026-07-12

### Fixed

- Prevented older Token Ledger processes from reintroducing raw identifiers after the schema-v6 privacy migration.
- Made oversized compressed session archives fail with an explicit unsupported-limit warning instead of promising progress that could never reach the suffix.
- Strengthened large-file rewrite detection with a complete digest accumulated during parsing.
- Bounded catalog files and downloads to 8 MiB, added connection and overall timeouts, limited redirects, and rejected every non-HTTPS network hop.
- Corrected the minimum-Rust CI job so it explicitly installs Rust 1.88.0.
- Hardened private application directories and sensitive database/configuration files on Unix.

### Security

- Required release tags to point to commits reachable from `main` and routed release publication through the protected `release` environment.
- Added code ownership for release, CI, dependency-update, and security-policy changes.

### Upgrade note

Before upgrading from any earlier Token Ledger version, including v0.4.0, terminate every older `token-ledger` process. Version 0.4.1 rejects new unsafe mixed-version writes, but an already-running older binary cannot protect a database until the fixed version has opened and upgraded it.

## 0.4.0 - 2026-07-12

### Added

- Added `token-ledger demo`, a deterministic synthetic walkthrough that returns before loading configuration, a database, or session roots.
- Added a reproducible 14.7-second terminal GIF generated from the real demo output.
- Added native Windows x64, static Linux x64, macOS Intel, and macOS Apple Silicon release jobs with per-archive and consolidated SHA-256 checksums.
- Added contributor, security, privacy, architecture, pricing, support, release, crates.io assessment, and public-maintenance documentation.

### Changed

- Made `token-ledger` the primary executable name.
- Reduced the README opening to the outcome, three differentiators, installation, and a 30-second quick start.
- Declared Rust 1.88 as the minimum supported version and pinned the locked source-package boundary.
- Filled Cargo repository, homepage, and documentation metadata and configured GitHub private vulnerability reporting.

### Fixed

- Marked time-limited scans, discovery failures, unreadable or malformed records, incomplete tails, volatile sources, and resource-limited parsing as provisional instead of implying complete local history.
- Strengthened append and rewrite validation so unchanged metadata alone cannot bypass checkpoint content checks.
- Bounded source discovery, source and decompression reads, JSONL record size, records, observations, warnings, and total scan work.
- Preserved useful discovery results at candidate, directory-entry, and I/O boundaries; completed candidate sets page across scans while unprovable traversal suffixes remain provisional.
- Bounded retained scan diagnostics to the latest 256 completed runs, with older warning rows removed by cascade.
- Preserved retryable checkpoints when parsing stops early and prevented partial sources from becoming successful coverage boundaries.
- Scheduled unseen, deferred, and known sources in bounded rotating windows so unchanged prefixes cannot starve unvisited history.
- Treated explicitly selected missing or invalid Claude roots as sanitized discovery failures instead of complete zero-source scans.
- Preserved migrated Codex fork/copy event identity across partial replays, source-local rebuilds, and copies first discovered after migration; ambiguous or unanchored legacy matches now roll back rather than guess or double-count.

### Security

- Replaced persisted event, session, provider-message, provider-request, import, adapter-state, and source identifiers with deterministic domain-separated pseudonyms.
- Added schema-v6 migrations that securely delete, checkpoint, and vacuum legacy identifier values on a best-effort basis while retaining only privacy-safe Codex rebuild aliases.
- Redacted warning details at the SQLite storage boundary and pinned every GitHub Action to an immutable revision.
- Restricted Cargo packages to intentional source, assets, tests, documentation, and verification scripts and continued to forbid unsafe Rust.

## 0.3.0 - 2026-07-11

### Added

- Responsive Audit Console layouts for wide, compact, and narrow terminals.
- Semantic color and Unicode controls with plain ASCII output.
- Redesigned cost, usage, doctor, scan, price-catalog, and reconciliation views.
- TTY-only scan progress and progressive `--details` evidence.
- Width, color, plain-output, help, and machine-output regression tests.

### Verified

- 130 automated tests.
- Stable ANSI-free JSON, CSV, and HTML output paths.
- Real-ledger smoke tests at 50, 80, and 120 columns.

## Earlier local builds

Versions 0.1.0 and 0.2.0 were development releases produced before this changelog was established. Their release archives are not part of the open-source source package.
