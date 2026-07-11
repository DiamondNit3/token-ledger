## Summary

Describe the problem and the smallest useful change.

## Accounting and compatibility impact

- [ ] No accounting semantics changed.
- [ ] Unknown values still remain unknown rather than becoming zero.
- [ ] API estimates, provider units, cash evidence, and reconciliation remain separate.
- [ ] Machine schemas are unchanged, or a new schema identifier and migration note are included.

Explain any checked exceptions:

## Privacy review

- [ ] No real transcript, database, config, receipt, account identifier, credential, or user path is included.
- [ ] New fixtures are minimal and synthetic.
- [ ] Errors and diagnostics do not echo source content.
- [ ] Output changes were reviewed for identifiers and paths.

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --all-targets --locked`
- [ ] `scripts/check-public.ps1` or equivalent public-content review
- [ ] User-facing changes are recorded in `CHANGELOG.md` and relevant documentation.

## Additional evidence

Include sanitized output, exact arithmetic, or primary documentation links where useful. Never attach real session data.
