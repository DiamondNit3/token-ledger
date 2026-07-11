# Token Ledger

> Unofficial community project. Token Ledger is not affiliated with, endorsed by, or sponsored by OpenAI or Anthropic.

Token Ledger is a local-first CLI that opens locally persisted Claude Code and OpenAI Codex sessions read-only, groups provider-reported token usage by model and calendar day, and calculates reproducible API list-price equivalents. It writes only its own configuration, accounting database, and explicitly installed price catalogs.

It never stores transcript bodies. The ledger contains only accounting envelopes: timestamps, pseudonymous session identity, model names, token counters, pricing dimensions, parser provenance, and bounded warnings.

## Status

The project is available under the MIT license and is prepared as a host-agnostic open-source source distribution. Generated binaries, databases, local configuration, and evidence reports are intentionally excluded from the source package.

Version `0.3.0` implements the local-first ledger, its evidence-aware cost workflow, and the responsive Audit Console terminal interface:

- Claude Code main-session and subagent JSONL ingestion
- OpenAI Codex active, archived, and Zstandard-compressed rollout ingestion
- Incremental checkpoints with append/rewrite detection
- Claude response deduplication and Codex cumulative-counter deltas/reset epochs
- UTC storage with IANA-timezone day and range queries
- Effective-dated, source-backed pricing with exact decimal arithmetic
- First-class multi-model `ledger cost` totals for today, yesterday, month, explicit ranges, or all history
- Independent exact, bounded, partial, unpriced, and unavailable states for API USD and provider units
- Scenario-aware estimates for unresolved dimensions, including Claude global versus US-only routing
- Separate API-equivalent value, Codex credits, recorded cash, attested actual billing, and provider reconciliation
- Coverage-aware JSON/CSV and privacy-safe, self-contained HTML reports
- Privacy-safe SQLite provenance, diagnostics, and event explanation
- Single-writer scan leases, abandoned-run recovery, and provisional active-session snapshots
- Atomic, checksum-pinned catalog updates with immutable revision selection, diff, activation, and rollback
- Idempotent offline reconciliation for canonical, OpenAI organization, and Anthropic Admin exports
- Responsive wide, compact, and stacked terminal layouts with grouped human-readable numbers
- Semantic exact/range/partial/unpriced status badges, adaptive ANSI color, and ASCII fallbacks
- TTY-only scan progress, structured health diagnostics, styled help/errors, and progressive `--details`
- Stable `--plain` output plus `--color auto|always|never` and `--unicode auto|always|never`

## Install from source

Install Rust 1.88 or newer, then install the locked source tree:

```text
cargo install --path . --locked
```

This installs the `ledger` executable. It embeds the verified price catalog and needs no API key for local session accounting.

Prebuilt executables are separate release artifacts and are not committed to or included in the source package. If you received a Windows bundle, extract it and use `./ledger.exe` in place of `ledger` in the examples below.

## Build

Build the locked dependency graph:

```powershell
cargo build --release --locked
```

The Windows binary is created at:

```text
target\release\ledger.exe
```

Windows x86_64 is the directly tested release target. Other Rust-supported desktop targets are source-supported on a best-effort basis until they are continuously validated there.

## First run

Initialize a config and database:

```powershell
ledger init --tz America/New_York
ledger doctor
ledger scan
ledger today
ledger cost --all
```

Use `--config` and `--db` to keep test or alternate ledgers isolated:

```powershell
ledger --config .\ledger.toml --db .\ledger.sqlite3 scan --dry-run
```

## Commands

```text
ledger [--config <PATH>] [--db <PATH>] [--claude-root <PATH>] [--codex-home <PATH>] [--catalog-revision <REVISION>] [--color auto|always|never] [--unicode auto|always|never] [--plain] [--details] <COMMAND>
ledger init [--tz <IANA_ZONE>] [--force]
ledger doctor
ledger scan [--client claude|codex] [--since <DATE_OR_TIMESTAMP>] [--rebuild|--full] [--dry-run]
ledger day <YYYY-MM-DD|today|yesterday> [--tz <IANA_ZONE>] [--no-scan] [--client ...] [--model ...] [--json|--html [PATH]]
ledger today [--tz <IANA_ZONE>] [--no-scan] [--client ...] [--model ...] [--json|--html [PATH]]
ledger cost [--today|--yesterday|--month|--all|--start <DATE> --end <DATE>] [--client claude|codex] [--model <MODEL>] [--no-scan] [--json|--html [PATH]]
ledger range <START> <END> [--group-by day,client,model] [--no-scan] [--client ...] [--model ...] [--json|--html [PATH]]
ledger sessions --date <YYYY-MM-DD> [--no-scan] [--json]
ledger models [--json]
ledger explain --event <EVENT_ID> [--json]
ledger prices status|list [--json]|verify|history|check|diff|activate|rollback
ledger prices update [--from <HTTPS_URL_OR_FILE> --sha256 <DIGEST>|--official]
ledger reconcile import|status|report
ledger export --start <DATE> --end <DATE> --format json|csv|html [--output <PATH>]
ledger purge --yes
```

Day/range/session reports refresh the ledger first unless `--no-scan` is supplied.

Running `ledger` without a subcommand shows a concise quick start. Human output detects terminal width and selects a wide table, compact two-line rows, or stacked records. Currency and headline counts are rounded only for display; JSON/CSV retain exact accounting values. Use `--details` for catalog digests, coverage windows, event drilldowns, and billing/reconciliation notes.

Color and animation are enabled only for attended terminals by default. `NO_COLOR`, `TERM=dumb`, redirected output, JSON/CSV/HTML modes, and `--plain` suppress terminal decoration. `--plain` is stable, line-oriented, and ASCII-safe; explicit `--color` and `--unicode` flags can override automatic capability selection for normal human output. Progress is written to stderr and is hidden outside an interactive terminal.

`ledger cost` directly answers per-model and combined cost questions without manual arithmetic. Model and client filters are repeatable, and `today` is the default period. Dates use the configured IANA timezone and accept `today`, `yesterday`, or `YYYY-MM-DD`:

```powershell
ledger cost --all --model gpt-5.6-sol --model claude-fable-5
ledger cost --month --client codex --json
ledger cost --start 2026-07-01 --end today --html cost-report.html
```

Its stable JSON schema is `token-ledger.cost.v1`. Aggregate pricing evidence compacts repeated per-event arithmetic into bounded categories and discloses observed and omitted explanation counts; use `ledger explain` when complete math for one event is needed. The self-contained HTML report is share-safe by construction: it includes aggregate accounting, catalog, billing, reconciliation, and coverage evidence without prompts, paths, event/session/source IDs, or billing evidence IDs. `--all` reports the first and last matching local event dates; an empty all-history result keeps its bounds null instead of inventing a current-day interval.

An HTTPS catalog update requires `--sha256` from a trusted, separately obtained source. A configured official feed requires both `official_price_manifest` and a separately trusted `official_price_manifest_sha256`; the manifest then binds the candidate catalog's own digest and evidence. Valid updates are installed atomically and both old and new revisions are retained. No catalog network request happens implicitly.

Use `--catalog-revision <REVISION>` to reproduce a report with a verified retained catalog without changing the active revision. `prices activate` and `prices rollback` are explicit mutations; `prices check` and historical selection are read-only.

`scan --dry-run` uses an in-memory ledger and creates or changes no database. A persisted `scan --since` stores only matching observations but deliberately leaves a deferred checkpoint, so the next unrestricted scan can backfill older local history.

## What the numbers mean

Token Ledger keeps four concepts separate:

1. **Provider-reported tokens** — exact counters or exact deltas derived from cumulative counters.
2. **API-equivalent estimate** — what the usage would cost at a matching public list-price rule.
3. **Provider units** — currently Codex credits when a published credit rate matches.
4. **Cash billing evidence** — user-attested charges/refunds and bounded completeness attestations from configuration.

An API-equivalent estimate is not an invoice. Subscription inclusion, prepaid credits, taxes, discounts, negotiated agreements, and fixed monthly fees are outside the calculation.

Cost reports always show `recorded_cash_usd` separately. They expose a numeric `actual_billed_usd` only when completeness attestations cover every selected provider for the full half-open UTC report window. Model filters cannot allocate account-level or subscription cash to an individual model. Imported provider reconciliation evidence is summarized independently and never overwrites local totals.

Codex credits apply only to ChatGPT-authenticated usage. API-key sessions report the API-equivalent USD estimate without credits. When a rollout omits its auth route, Token Ledger reads only the non-secret `auth_mode` field from the current Codex profile, marks that inference partial, and never retains credentials.

JSON usage reports use the versioned `token-ledger.report.v2` envelope; combined cost reports use `token-ledger.cost.v1`. They include local and UTC query bounds, catalog revision and SHA-256, independent price-measure status and bounds, freshness, provisional/as-of coverage, warnings, and grouped rows. CSV exports begin with a metadata record and repeat essential query/catalog fields on each data record. Pass an event ID to `ledger explain --event <ID>` for sanitized source/parser observations and complete price-rule evidence.

Unknown price data is never treated as `$0`. A partial row shows a known lower-bound subtotal with `≥`; a fully unpriced row shows `—`.

## Coverage limits

“All sessions” means all readable sessions still persisted in the configured local roots. Coverage can be incomplete when:

- a client deleted or expired history;
- persistence was disabled or a run was ephemeral;
- work occurred on another machine or only in a cloud task;
- a local format changed beyond the adapter's capability profile.

Claude and Codex transcript schemas are version-sensitive. Unknown records are skipped with sanitized warnings; known accounting records continue processing.

Every day/range report includes a conservative per-client coverage assessment. An empty report distinguishes no sources, no observations, a date outside the observed window, and no matching events inside the broader observed window. None of these states is presented as proof of zero provider usage.

Reports include an `as_of` timestamp and become `provisional` when active or volatile source files changed during scanning. Concurrent writers are serialized with a SQLite-backed lease, and abandoned scan runs are recovered conservatively.

The bundled catalog was verified on 2026-07-10. API-equivalent rules begin at documented model release dates; Codex-credit rules without a dated publication boundary begin at catalog verification. Published gaps remain partial/unpriced rather than guessed. Run `ledger prices status` and `ledger prices verify` to inspect it.

## Default source discovery

- Claude Code: `CLAUDE_CONFIG_DIR`, otherwise `~/.claude/projects/**/*.jsonl`
- OpenAI Codex: `CODEX_HOME`, otherwise `~/.codex/sessions` and `~/.codex/archived_sessions`

Custom roots can be set in `config.toml`:

```toml
timezone = "America/New_York"
claude_root = "D:/profiles/claude"
codex_home = "D:/profiles/codex"
show_raw_ids = false
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

Resolution precedence is command line, environment, config file, then platform default. Use global `--claude-root <PATH>` and `--codex-home <PATH>` overrides when needed; `ledger doctor` shows the effective roots and their origin.

## Privacy design

- Source files are opened read-only.
- JSONL is processed in bounded batches.
- Prompts, responses, reasoning, code, terminal output, and tool bodies are not copied into SQLite.
- Stored source identities are pseudonymous; normal reports never expose source paths.
- Parser errors never echo raw JSON or serde error fragments that could contain content.
- Session IDs are pseudonymized in normal session reports unless `show_raw_ids = true`.
- No analytics or automatic network requests are made.
- Network access is used only for an explicit price-catalog check or update.

`ledger purge --yes` performs a best-effort local scrub of Token Ledger's accounting index, truncates its WAL, and vacuums the database. It never changes client session files, but it cannot erase external backups, filesystem snapshots, or SSD history.

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

The underlying checks are:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/check-public.ps1
```

The 130-test suite covers client record variants, deduplication, cumulative resets, cache token semantics, DST boundaries, independent price bounds, billing completeness, catalog tamper/rollback behavior, scan concurrency, reconciliation, privacy, compact aggregate evidence, responsive terminal widths, color/plain behavior, machine-output ANSI safety, and CLI idempotency.

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
- [Host-agnostic release process](docs/RELEASING.md)
- [GitHub publication checklist](docs/GITHUB.md)

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
