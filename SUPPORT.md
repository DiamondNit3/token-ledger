# Support

Token Ledger is a community-maintained accounting tool, not an official OpenAI or Anthropic product.

## Before requesting help

Run these commands with the latest build:

```text
ledger --version
ledger doctor
ledger scan --dry-run
ledger prices status
ledger prices verify
```

For terminal-formatting problems, also capture:

```text
ledger --plain doctor
```

Share the command, exit code, operating system, and sanitized warning codes. Do not share real JSONL sessions, database files, configuration files, source paths, raw IDs, receipts, or credentials.

## Support scope

- Windows release binaries are directly tested.
- Source builds on other Rust-supported platforms are best effort until they are continuously validated there.
- Upstream client formats can change without notice; sanitized fixtures are the preferred way to report compatibility problems.
- Price estimates are reproductions of documented list-price rules, not provider bills or subscription statements.

Use `ledger explain` and `--details` for local diagnosis, but review their output before sharing it. Machine exports can contain detailed timestamps and pseudonymous accounting identifiers.

Security and privacy reports should follow `SECURITY.md`, not a public support channel.
