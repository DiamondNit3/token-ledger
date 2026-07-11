# Architecture

Token Ledger is a local accounting pipeline with deliberately separate ingestion, canonicalization, pricing, reporting, and rendering layers.

```text
Claude Code JSONL ─┐
                   ├─ adapters ─ scanner ─ canonical SQLite ledger
Codex JSONL/Zstd ──┘                         │
                                             ├─ reports ─ terminal/JSON/CSV/HTML
verified price catalog ─ pricing engine ─────┤
user billing evidence ─ billing engine ──────┤
provider exports ─ reconciliation ───────────┘
```

Client source files are inputs only. Token Ledger writes its own configuration, database, and explicitly installed catalog revisions.

## Modules

- `src/adapters/claude.rs` parses Claude Code main-session and subagent JSONL records.
- `src/adapters/codex.rs` parses active, archived, and compressed Codex rollouts and derives deltas from cumulative counters.
- `src/scanner.rs` discovers sources, manages incremental checkpoints, detects rewrites and concurrent changes, and coordinates scan leases.
- `src/model.rs` defines canonical accounting events, token classes, and deterministic identifiers.
- `src/db.rs` owns migrations, canonicalization, checkpoints, provenance, coverage, and purge behavior.
- `src/pricing.rs` validates immutable price catalogs and evaluates effective-dated rules and unresolved dimensions.
- `src/billing.rs` keeps user-attested charges and completeness evidence separate from API-equivalent estimates.
- `src/reconcile.rs` imports sanitized provider buckets and compares them with local canonical totals.
- `src/report.rs` builds day and range report envelopes.
- `src/cost.rs` builds multi-model cost envelopes and evidence summaries.
- `src/html.rs` renders share-oriented, escaped HTML.
- `src/terminal.rs` handles terminal capability detection, semantic styling, tables, and responsive layouts.
- `src/main.rs` defines commands and coordinates the layers.

## Accounting invariants

1. Source counters are never silently guessed.
2. Repeated observations are canonicalized deterministically.
3. Unknown or unavailable prices never become zero.
4. Independent unknown dimensions produce independent bounds.
5. API-equivalent value is not cash paid.
6. Provider reconciliation evidence never overwrites local totals.
7. A numeric actual-billed total requires complete bounded attestation for the selected providers and time window.
8. All report dates resolve through an explicit IANA timezone and half-open UTC bounds.

## Privacy invariants

1. Transcript bodies and tool payloads do not enter canonical events.
2. Database provenance contains sanitized locators and parser metadata, not raw paths or records.
3. Parser errors do not echo source fragments.
4. Provider export identifiers are discarded.
5. Normal identifiers are deterministic pseudonyms; raw IDs require explicit configuration.
6. Share-safe HTML excludes paths and source, event, session, and billing-evidence identifiers.

See [PRIVACY.md](PRIVACY.md) for limitations and user responsibilities.

## Compatibility boundaries

- `token-ledger.report.v2` is the usage-report envelope.
- `token-ledger.cost.v1` is the combined-cost envelope.
- CSV exports carry metadata records and repeat essential context.
- `--plain` is intended for stable line-oriented human automation.
- Decorated terminal output may change between minor versions.

Schema changes must use a new identifier rather than silently repurposing an existing field.

## Adding a client adapter

An adapter must:

1. Define source discovery precedence and capability boundaries.
2. Parse bounded records without storing bodies.
3. Convert provider counters into canonical token classes without double counting.
4. Produce deterministic deduplication and source identifiers.
5. Sanitize every warning.
6. Include synthetic fixtures for malformed, partial, repeated, reset, and evolving records.
7. Add coverage and privacy assertions at the CLI boundary.

## Maintenance boundaries

The largest modules are `pricing.rs`, `main.rs`, and `db.rs`. Future refactoring should split implementation detail without changing public schemas or accounting semantics. Suitable seams are command dispatch versus rendering, catalog storage versus rule evaluation, and migrations versus query repositories.
