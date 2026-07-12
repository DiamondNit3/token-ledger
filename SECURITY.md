# Security Policy

## Supported versions

Security fixes are made against the latest released minor version. Older local builds may receive no separate patch.

## Reporting a vulnerability

Do not publish a report that contains session logs, prompts, source paths, account identifiers, database contents, configuration files, or credentials.

For the public project, use [GitHub private vulnerability reporting](https://github.com/DiamondNit3/token-ledger/security/advisories/new). GitHub makes this reporter-facing feature available only after the repository is public and a maintainer enables it. During private staging, disclose through the private channel by which you received access. Do not open a public issue for a suspected vulnerability. Send the smallest synthetic reproduction possible.

A useful report includes:

- affected Token Ledger version and operating system;
- impact and required attacker access;
- a synthetic reproduction or test;
- whether the issue exposes source content, identifiers, credentials, or unsafe network behavior; and
- any known mitigation.

There is no guaranteed response SLA. Maintainers should acknowledge credible reports, reproduce them privately, prepare a fix, and disclose details only after affected users can update.

## Security boundaries

Token Ledger:

- opens configured Claude Code and Codex session sources read-only;
- writes its own configuration, SQLite accounting index, and explicitly installed price catalogs;
- parses local files as untrusted input and skips malformed records conservatively;
- does not intentionally persist prompts, responses, reasoning, code, tool bodies, or terminal output;
- does not intentionally persist provider credentials or provider account identifiers;
- makes no implicit catalog network request; HTTPS checks and updates are explicit;
- requires checksum-pinned catalog installation paths; and
- escapes generated HTML.

Pseudonymous identifiers are deterministic accounting identifiers, not anonymous data. Someone with the original source data may be able to correlate them.

## User responsibilities

- Protect the ledger database and configuration with normal local filesystem permissions.
- Treat JSON/CSV exports and full-pseudonym diagnostic output as potentially sensitive.
- Obtain catalog checksums through a separately trusted channel.
- Review HTML before sharing even though the exporter is designed to omit paths, source IDs, and transcript content.
- Remember that purge cannot erase filesystem snapshots, backups, or storage-device history.

Pricing disagreement without a confidentiality, integrity, or code-execution impact is normally a correctness issue rather than a security vulnerability.
