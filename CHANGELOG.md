# Changelog

All notable user-facing changes to Token Ledger are recorded here. The project follows semantic versioning while it remains pre-1.0: minor releases may change human presentation, while machine-schema changes require an explicit new schema identifier.

## Unreleased

No changes yet.

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

Before upgrading from 0.3.x or earlier, terminate every older `token-ledger` process. Version 0.4.1 rejects unsafe mixed-version writes, but an already-running older binary cannot protect a database until the fixed version has opened and upgraded it.

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
