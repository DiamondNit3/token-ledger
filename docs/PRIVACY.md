# Privacy Model

Token Ledger is designed to calculate local usage without building a second transcript store.

## What it reads

The scanner opens configured Claude Code and OpenAI Codex session files read-only. Those files may contain highly sensitive prompts, responses, code, tool calls, reasoning, paths, and terminal output. Parsers extract only accounting fields needed to identify and price usage.

Provider reconciliation is an explicit offline import. Imported files may contain provider identifiers; the importer retains only canonical accounting buckets and content digests required for idempotence.

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

Normal identifiers are deterministic so repeated scans can deduplicate events. They can therefore be linkable across reports and may be correlated by someone who possesses the source data. Treat the ledger and detailed exports as private accounting data.

`show_raw_ids = true` and equivalent raw-ID output are explicit privacy reductions intended only for local diagnosis.

## Outputs

- Human terminal output is pseudonymous by default.
- JSON and CSV retain detailed accounting evidence and timestamps; review before sharing.
- HTML reports are designed to omit prompts, paths, event/session/source IDs, and billing-evidence IDs, but should still be reviewed before distribution.
- `--details` and `ledger explain` intentionally disclose more local accounting evidence.

Terminal color and progress never affect machine serializers. JSON, CSV, and HTML modes are ANSI-free.

## Network behavior

Scanning and reporting do not send session data to a provider or analytics service. Network access occurs only when the user explicitly checks or updates a price catalog over HTTPS. Catalog downloads contain no session records.

No catalog update is installed solely because it arrived over HTTPS. Remote installation requires separately trusted checksum evidence, and official-feed mode requires a pinned manifest checksum that binds the candidate catalog.

## Deletion and retention

`ledger purge --yes` removes Token Ledger's accounting rows, truncates its SQLite WAL, and vacuums the database on a best-effort basis. It never modifies Claude Code or Codex source sessions.

Purge cannot erase external backups, filesystem snapshots, crash dumps, exported reports, or storage-device history. Delete those separately according to the operating environment.

## Threat model limitations

Token Ledger does not protect data from an attacker who already has equivalent access to the user's account or filesystem. It does not encrypt its database. Local filesystem permissions, device encryption, backup policy, and operating-system security remain the user's responsibility.

Model names, timestamps, token totals, price dimensions, and billing evidence can reveal work patterns even without transcript content.

## Public fixture policy

Only synthetic fixtures belong in the source distribution. A fixture must contain no real transcript, user path, email, account identifier, receipt, database row, or credential. Sanitized warning tests should construct fictional sensitive-looking input rather than copying a real record.
