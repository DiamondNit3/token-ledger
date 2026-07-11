# Changelog

All notable user-facing changes to Token Ledger are recorded here. The project follows semantic versioning while it remains pre-1.0: minor releases may change human presentation, while machine-schema changes require an explicit new schema identifier.

## Unreleased

- Prepared a host-agnostic open-source source distribution.
- Added contributor, security, privacy, architecture, pricing, support, and release documentation.
- Declared and verified Rust 1.88 as the minimum supported Rust version.
- Restricted Cargo source packaging to intentional source, assets, tests, documentation, and verification scripts.
- Forbid unsafe Rust in the library and executable crates.

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
