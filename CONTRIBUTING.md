# Contributing to Token Ledger

Thank you for improving Token Ledger. Contributions should preserve its accounting conservatism, local-first operation, and privacy guarantees.

By contributing, you agree that your contribution may be distributed under the project's MIT license.

## Before changing code

Token Ledger reads evolving local formats produced by Claude Code and OpenAI Codex. A parser change can silently alter totals, so observable behavior must be backed by synthetic fixtures and tests.

The project follows these rules:

- Unknown values remain unknown; they are never converted to zero.
- API-equivalent estimates, provider units, recorded cash, and attested billed amounts remain separate.
- Client session files are read-only inputs.
- Transcript bodies, source paths, credentials, and provider account identifiers must not enter the ledger or normal reports.
- Parser failures must be sanitized and must not echo source content.
- Pricing changes require dated primary-source evidence and exact decimal arithmetic.
- Machine-readable schemas remain stable within their declared schema version.

Read [Architecture](docs/ARCHITECTURE.md), [Privacy](docs/PRIVACY.md), and [Pricing](docs/PRICING.md) before changing those areas.

## Development setup

Requirements:

- Rust 1.88 or newer
- A C/C++ build toolchain supported by Rust for the target platform
- PowerShell 5.1+ or a POSIX-compatible shell for the convenience scripts

Build and test with the locked dependency graph:

```text
cargo build --locked
cargo test --all-targets --locked
```

Run the full local quality gate on Windows or anywhere PowerShell is available:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check.ps1
```

The portable POSIX script runs the formatting, lint, test, and source-package-boundary subset:

```sh
sh ./scripts/check.sh
```

Release CI additionally runs the PowerShell private-content/dependency metadata audit and reproduces `THIRD-PARTY-NOTICES.html` with pinned `cargo-about` 0.9.1. Run those checks before publishing artifacts when PowerShell is available.

## Fixtures and privacy

Never contribute real session logs, database files, configuration files, user paths, receipts, account identifiers, or exported provider data.

Fixtures must be minimal and synthetic. Use obviously fictional identifiers and content, remove absolute paths, and include only the fields needed by the test. A malformed-record fixture must not contain copied private text merely to reproduce a parser error.

Before sharing a source archive, run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check-public.ps1
```

This is a defense-in-depth scan, not proof that a file is safe to publish. Review the archive manually as well.

## Pricing changes

Every catalog change must include:

1. A primary provider documentation URL.
2. The documented effective date or an explicit statement that no earlier boundary was published.
3. Exact decimal rates and units.
4. A new immutable catalog revision and matching manifest digest.
5. Tests for boundary dates, aliases, incomplete dimensions, and status classification.

Do not infer an unknown historical price, geography, authentication route, subscription inclusion, or discount.

## Change checklist

- Keep the change focused and explain the accounting or privacy effect.
- Add or update tests for behavior changes.
- Update `CHANGELOG.md` for user-visible changes.
- Update public documentation when commands, schemas, pricing, or privacy behavior changes.
- Run formatting, Clippy, all tests, package inspection, and the public-content scan.
- Do not include generated binaries, databases, HTML evidence files, or release archives.

## Compatibility

Human terminal formatting may evolve between minor releases. The `--plain`, JSON, and CSV surfaces are compatibility-sensitive. A breaking machine-schema change requires a new schema identifier and migration notes.

The canonical executable is `token-ledger`. Treat future executable renames as user-facing breaking changes that require a dedicated release.
