# Privacy Model

Token Ledger is designed to calculate local usage without building a second transcript store.

## What it reads

The scanner opens configured Claude Code and OpenAI Codex session files read-only. Those files may contain highly sensitive prompts, responses, code, tool calls, reasoning, paths, and terminal output. Parsers extract only accounting fields needed to identify and price usage.

Provider reconciliation is an explicit offline import. Imported files may contain provider identifiers; the importer retains only canonical accounting buckets and a domain-separated digest required for idempotence.

## What it stores

The SQLite ledger contains accounting envelopes such as:

- UTC timestamps and local report bounds;
- client and model names;
- provider-reported token classes and request counts;
- deterministic pseudonymous event, session, and source identities;
- parser and checkpoint provenance;
- bounded sanitized warning codes;
- catalog revisions, price evidence, and reconciliation buckets; and
- user-entered billing evidence when configured.

It is not intended to store prompts, responses, reasoning, code, tool bodies, terminal output, raw JSON records, source paths, provider credentials, or provider account identifiers.

## Pseudonymization is not anonymity

Stored event, session, message, request, import, and source identifiers are deterministic, domain-separated pseudonyms so repeated scans can deduplicate without retaining provider-native values. They can still be linkable across reports and may be correlated by someone who possesses the source data. Treat the ledger and detailed exports as private accounting data.

`show_raw_ids = true` shows complete stored pseudonyms instead of shortened display references. It cannot recover raw provider identifiers because those values are not retained.

Schema v7 is the current privacy boundary. It acquires exclusive migration access before reading the prior version, advances each version with a compare-and-swap check, and installs database triggers that reject schema downgrades and legacy writes. A pre-barrier schema-v6 completion marker cannot prove that an older process did not write raw identifiers after the earlier scrub, so the barrier invalidates schema-v6 source, observation, warning, and reconciliation caches instead of trusting or double-hashing mixed rows. This invalidation is irreversible when authoritative local sources no longer exist. Ordinary commands therefore fail closed on a v0.4.1-era database until the user retains source files, exports or backs up the database, and deliberately runs `token-ledger migrate --accept-history-loss`. The next scan rebuilds accounting from retained sources; provider reconciliation exports must be re-imported.

Upgrades from older schemas reapply the required identifier storage boundaries and use SQLite secure deletion, checked WAL truncation, and a post-migration vacuum so removed identifier values are physically scrubbed on a best-effort basis. Cleanup remains durably marked as pending until those physical steps succeed, so an interrupted or busy cleanup is retried without transforming retained logical rows twice.

The schema-v6-to-v7 invalidation intentionally removes the migration-only Codex identity bridge along with its source cache. Fresh scans recreate canonical identity from the current authoritative records. Backups, snapshots, and storage history remain outside the privacy boundary.

## Outputs

- Human terminal output is pseudonymous by default.
- JSON and CSV retain detailed accounting evidence and timestamps; review before sharing.
- HTML reports are designed to omit prompts, paths, event/session/source IDs, and billing-evidence IDs, but should still be reviewed before distribution.
- `--details` and `token-ledger explain` intentionally disclose more local accounting evidence.

Terminal color and progress never affect machine serializers. JSON, CSV, and HTML modes are ANSI-free.

## Network behavior

Scanning and reporting do not send session data to a provider or analytics service. Network access occurs only when the user explicitly checks or updates a price catalog over HTTPS. Catalog downloads contain no session records.

No catalog update is installed solely because it arrived over HTTPS. Remote installation requires separately trusted checksum evidence, and official-feed mode requires a pinned manifest checksum that binds the candidate catalog.

## Deletion and retention

Token Ledger retains the latest 256 completed scan diagnostics; older scan runs and their warning rows are pruned automatically. Health and coverage warning summaries describe only the latest scan, so a resolved warning does not remain a current alert. Canonical usage observations, source checkpoints, catalog history, reconciliation imports, and explicit billing evidence follow their functional retention rules rather than this diagnostic limit.

`token-ledger purge --yes` removes Token Ledger's accounting rows, truncates its SQLite WAL, and vacuums the database on a best-effort basis. It never modifies Claude Code or Codex source sessions.

Purge cannot erase external backups, filesystem snapshots, crash dumps, exported reports, or storage-device history. Delete those separately according to the operating environment.

## Threat model limitations

Token Ledger does not protect data from an attacker who already has equivalent access to the user's account or filesystem. It does not encrypt its database. Local filesystem permissions, device encryption, backup policy, and operating-system security remain the user's responsibility.

The scanner rejects detected symbolic-link and Windows reparse-point source components below each trusted discovery root, parses a bounded private snapshot, and verifies the live source before commit. Trusted operating-system path aliases above the discovery boundary remain compatible. It is not a sandbox for session roots that a hostile process can concurrently replace or manipulate. Configure only roots controlled by the same trusted account.

Model names, timestamps, token totals, price dimensions, and billing evidence can reveal work patterns even without transcript content.

## Public fixture policy

Only synthetic fixtures belong in the source distribution. A fixture must contain no real transcript, user path, email, account identifier, receipt, database row, or credential. Sanitized warning tests should construct fictional sensitive-looking input rather than copying a real record.
