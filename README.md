# Token Ledger

Know which models consumed your locally recorded Claude Code and OpenAI Codex tokens each day—and their reproducible public list-price equivalent.

![Token Ledger synthetic demo showing daily model usage, bounded price estimates, credits, and coverage](docs/images/token-ledger-demo.gif)

- **Local by design.** Session files are opened read-only; transcript bodies, credentials, raw provider identifiers, and source paths are not copied into the ledger.
- **Honest about uncertainty.** Exact, bounded, partial, unpriced, credits, recorded cash, and attested billing remain distinct instead of turning missing evidence into zero.
- **Built for auditability.** Effective-dated price rules, rewrite-safe checkpoints, stable machine exports, and checksummed native releases make every result inspectable.

> Unofficial community project. Token Ledger is not affiliated with, endorsed by, or sponsored by OpenAI or Anthropic.

## Installation

Download the archive for Windows x64, Linux x64, macOS Intel, or macOS Apple Silicon from [GitHub Releases](https://github.com/DiamondNit3/token-ledger/releases/latest). Verify the archive against `SHA256SUMS.txt`, extract it, and place `token-ledger` (or `token-ledger.exe`) on your `PATH`.

```text
token-ledger --version
token-ledger prices verify --plain
```

The macOS archives are currently unsigned and not notarized. The Windows binary is also unsigned and may trigger Microsoft Defender SmartScreen. To build from source instead, install Rust 1.88 or newer and run this from the repository:

```text
cargo install --path . --locked
```

Both paths install `token-ledger` as the primary executable. It embeds the verified price catalog and needs no provider API key for local session accounting.

## 30-second quick start

```text
token-ledger demo
token-ledger init --tz America/New_York
token-ledger today
token-ledger cost --month
```

`demo` is deterministic synthetic data and returns before loading configuration, a database, or session roots. `today` and `cost` refresh readable local sources automatically; add `--no-scan` to query only the current ledger.

## Command reference

```text
token-ledger [--config <PATH>] [--db <PATH>] [--claude-root <PATH>] [--codex-home <PATH>] [--catalog-revision <REVISION>] [--color auto|always|never] [--unicode auto|always|never] [--plain] [--details] <COMMAND>
token-ledger demo
token-ledger init [--tz <IANA_ZONE>] [--force]
token-ledger doctor
token-ledger scan [--client claude|codex] [--since <DATE_OR_TIMESTAMP>] [--rebuild|--full] [--dry-run]
token-ledger day <YYYY-MM-DD|today|yesterday> [--tz <IANA_ZONE>] [--no-scan] [--client ...] [--model ...] [--json|--html [PATH]]
token-ledger today [--tz <IANA_ZONE>] [--no-scan] [--client ...] [--model ...] [--json|--html [PATH]]
token-ledger cost [--today|--yesterday|--month|--all|--start <DATE> --end <DATE>] [--client claude|codex] [--model <MODEL>] [--no-scan] [--json|--html [PATH]]
token-ledger range <START> <END> [--group-by day,client,model] [--no-scan] [--client ...] [--model ...] [--json|--html [PATH]]
token-ledger sessions --date <YYYY-MM-DD> [--no-scan] [--json]
token-ledger models [--json]
token-ledger explain --event <EVENT_ID> [--json]
token-ledger prices status|list [--json]|verify|history|check|diff|activate|rollback
token-ledger prices update [--from <HTTPS_URL_OR_FILE> --sha256 <DIGEST>|--official]
token-ledger reconcile import|status|report
token-ledger export --start <DATE> --end <DATE> --format json|csv|html [--output <PATH>]
token-ledger purge --yes
```

Running `token-ledger` without a subcommand shows a concise quick start. Human output adapts to terminal width; JSON, CSV, and HTML retain exact accounting values and never contain terminal escape codes. `--plain` is stable, line-oriented, and ASCII-safe. Use `--details` for catalog digests, coverage windows, event drilldowns, and billing/reconciliation notes.

`token-ledger cost` reports per-model and combined totals for today, yesterday, month-to-date, an explicit range, or all readable history:

```powershell
token-ledger cost --all --model gpt-5.6-sol --model claude-fable-5
token-ledger cost --month --client codex --json
token-ledger cost --start 2026-07-01 --end today --html cost-report.html
```

Its stable JSON schema is `token-ledger.cost.v1`. Aggregate evidence is bounded; use `token-ledger explain --event <ID>` for complete event-level math. The self-contained HTML report omits prompts, paths, event/session/source IDs, and billing-evidence IDs.

An HTTPS catalog update requires a separately trusted `--sha256`. No catalog network request occurs implicitly. `--catalog-revision <REVISION>` reproduces a report with a retained catalog without changing the active revision; catalog activation and rollback are explicit mutations.

`scan --dry-run` uses an in-memory ledger and creates or changes no database. A persisted `scan --since` is intentionally provisional and leaves a deferred checkpoint so the next unrestricted scan can backfill older local history.

## What the numbers mean

Token Ledger keeps four concepts separate:

1. **Provider-reported tokens** — exact counters or exact deltas derived from cumulative counters.
2. **API-equivalent estimate** — what the usage would cost at a matching public list-price rule.
3. **Provider units** — currently Codex credits when a published credit rate matches.
4. **Cash billing evidence** — user-attested charges/refunds and bounded completeness attestations from configuration.

An API-equivalent estimate is not an invoice. Subscription inclusion, prepaid credits, taxes, discounts, negotiated agreements, and fixed monthly fees are outside the calculation.

Cost reports always show `recorded_cash_usd` separately. They expose a numeric `actual_billed_usd` only when completeness attestations cover every selected provider for the full half-open UTC report window. Model filters cannot allocate account-level or subscription cash to an individual model. Imported provider reconciliation evidence is summarized independently and never overwrites local totals.

Codex credits apply only to ChatGPT-authenticated usage. API-key sessions report the API-equivalent USD estimate without credits. When a rollout omits its auth route, Token Ledger reads only the non-secret `auth_mode` field from the current Codex profile, marks that inference partial, and never retains credentials.

JSON usage reports use the versioned `token-ledger.report.v2` envelope; combined cost reports use `token-ledger.cost.v1`. They include local and UTC query bounds, catalog revision and SHA-256, independent price-measure status and bounds, freshness, provisional/as-of coverage, warnings, and grouped rows. CSV exports begin with a metadata record and repeat essential query/catalog fields on each data record. Pass an event ID to `token-ledger explain --event <ID>` for sanitized source/parser observations and complete price-rule evidence.

Unknown price data is never treated as `$0`. A partial row shows a known lower-bound subtotal with `≥`; a fully unpriced row shows `—`.

## Coverage limits

“All sessions” means all readable sessions still persisted in the configured local roots. Coverage can be incomplete when:

- a client deleted or expired history;
- persistence was disabled or a run was ephemeral;
- work occurred on another machine or only in a cloud task;
- a local format changed beyond the adapter's capability profile.

Claude and Codex transcript schemas are version-sensitive. Unknown records are skipped with sanitized warnings; known accounting records continue processing.

Parsing is bounded per record, source, decompression stream, and scan invocation. When a run reaches an aggregate bound it rotates the source window on later runs and remains provisional. Completed discovery traversals page bounded candidate sets across later scans; if the hard directory-entry ceiling itself is reached, safe partial candidates are retained but an arbitrary flat or deep suffix remains unprovable and the snapshot stays provisional. Sources above the exact fingerprint threshold use bounded sampled rewrite checks and remain provisional because sampled equality is not proof that every byte is unchanged.

Every day/range report includes a conservative per-client coverage assessment. An empty report distinguishes no sources, no observations, a date outside the observed window, and no matching events inside the broader observed window. None of these states is presented as proof of zero provider usage.

Reports include an `as_of` timestamp and become `provisional` when discovery fails, a record cannot be accounted for, a resource boundary is reached, a time-limited scan defers history, or a source changes during scanning. Concurrent writers are serialized with a SQLite-backed lease, and abandoned scan runs are recovered conservatively.

The bundled catalog was verified on 2026-07-10. API-equivalent rules begin at documented model release dates; Codex-credit rules without a dated publication boundary begin at catalog verification. Published gaps remain partial/unpriced rather than guessed. Run `token-ledger prices status` and `token-ledger prices verify` to inspect it.

## Default source discovery

- Claude Code: `CLAUDE_CONFIG_DIR`, otherwise `~/.claude/projects/**/*.jsonl`
- OpenAI Codex: `CODEX_HOME`, otherwise `~/.codex/sessions` and `~/.codex/archived_sessions`

Custom roots can be set in `config.toml`:

```toml
timezone = "America/New_York"
claude_root = "D:/profiles/claude"
codex_home = "D:/profiles/codex"
show_raw_ids = false # true shows full stored pseudonyms, never raw provider IDs
catalog_revision = "2026-07-10.3" # optional, selects immutable history

# Optional explicit official feed. HTTPS use remains opt-in.
official_price_manifest = "https://example.invalid/token-ledger/prices.manifest.json"
official_price_manifest_sha256 = "replace-with-a-separately-trusted-sha256"
```

Unknown pricing dimensions can be resolved only with bounded, user-attested overrides. The interval is half-open and the attestation is embedded in price evidence:

```toml
[[pricing_dimension_overrides]]
id = "fable-global-july"
provider = "anthropic"
canonical_model = "claude-fable-5"
dimension = "inference_geo"
value = "global"
effective_from = "2026-07-01T00:00:00Z"
effective_to = "2026-08-01T00:00:00Z"
attested_at = "2026-08-01T00:00:00Z"
note = "Workspace routing was verified for this bounded interval."
```

Cash evidence is also explicit. Recorded cash is never called actual billed unless completeness attestations cover every selected provider for the full report window:

```toml
[[billing_evidence.one_time_charges]]
id = "chatgpt-credit-purchase-2026-07"
provider = "openai"
category = "credit_purchase"
amount_usd = "20.00"
charged_at = "2026-07-01T12:00:00Z"
attested_at = "2026-07-02T12:00:00Z"
source_note = "User-entered receipt total; no credential or account ID stored."
```

Resolution precedence is command line, environment, config file, then platform default. An absent platform default is a normal no-client state; an explicitly selected missing or invalid root is a provisional discovery failure. Use global `--claude-root <PATH>` and `--codex-home <PATH>` overrides when needed; `token-ledger doctor` shows the effective roots and their origin.

## Privacy design

- Source files are opened read-only.
- JSONL is processed in bounded batches.
- Prompts, responses, reasoning, code, terminal output, and tool bodies are not copied into SQLite.
- Stored event, session, message, request, import, and source identities are deterministic domain-separated pseudonyms; source paths are not retained.
- Parser errors never echo raw JSON or serde error fragments that could contain content.
- `show_raw_ids = true` reveals complete stored pseudonyms for local diagnosis; it cannot reveal raw provider identifiers because they are not stored.
- No analytics or automatic network requests are made.
- Network access is used only for an explicit price-catalog check or update.

`token-ledger purge --yes` performs a best-effort local scrub of Token Ledger's accounting index, truncates its WAL, and vacuums the database. It never changes client session files, but it cannot erase external backups, filesystem snapshots, or SSD history.

The full threat model, output sensitivity guidance, and public fixture policy are in [docs/PRIVACY.md](docs/PRIVACY.md).

## Development checks

Run the complete local gate on Windows:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check.ps1
```

or on a POSIX shell:

```sh
sh ./scripts/check.sh
```

The POSIX script is the portable code/package subset. Release CI also runs the PowerShell private-content and dependency metadata audit, then reproduces `THIRD-PARTY-NOTICES.html` byte-for-byte with pinned `cargo-about` 0.9.1.

The underlying checks are:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check-public.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/generate-third-party-notices.ps1 -Check
```

The test suite covers client record variants, deduplication, cumulative resets, cache token semantics, rewrite detection, resource ceilings, privacy migrations, DST boundaries, independent price bounds, billing completeness, catalog tamper/rollback behavior, scan concurrency, reconciliation, responsive terminal widths, machine-output ANSI safety, and CLI idempotency.

Build the standard open-source source archive locally with:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/package-source.ps1
```

This uses Cargo's explicit package allowlist and writes checksummed standard `.crate` and reviewer-friendly `.zip` source archives to `open-source/`. It performs no version-control or hosting operation.

## Project documentation

- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)
- [Support](SUPPORT.md)
- [Code of conduct](CODE_OF_CONDUCT.md)
- [Changelog](CHANGELOG.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Privacy model](docs/PRIVACY.md)
- [Pricing model and maintenance](docs/PRICING.md)
- [Dependency policy](docs/DEPENDENCIES.md)
- [Release process](docs/RELEASING.md)
- [GitHub publication checklist](docs/GITHUB.md)
- [crates.io publication assessment](docs/CRATES_IO.md)
- [Public launch and maintenance plan](docs/LAUNCH.md)

## License and trademarks

Token Ledger is distributed under the [MIT License](LICENSE). Contributions are distributed under the same license.

OpenAI, ChatGPT, Codex, Anthropic, and Claude are trademarks or product names of their respective owners. Their names are used only to describe compatible local formats and documented pricing rules. No affiliation or endorsement is implied.

## Primary documentation

- [Claude Code sessions](https://code.claude.com/docs/en/sessions)
- [Claude Code monitoring](https://code.claude.com/docs/en/monitoring-usage)
- [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- [Codex observability](https://learn.chatgpt.com/docs/config-file/config-advanced#observability-and-telemetry)
- [Codex pricing](https://learn.chatgpt.com/docs/pricing#what-are-tokens-and-credits)
- [OpenAI API pricing](https://developers.openai.com/api/docs/pricing)
- [OpenAI Usage and Costs API](https://developers.openai.com/cookbook/examples/completions_usage_api)
- [Anthropic Usage & Cost API](https://platform.claude.com/docs/en/manage-claude/usage-cost-api)

Exact source URLs and retrieval timestamps for each bundled rate are embedded in `assets/prices.json`.
