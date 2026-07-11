# GitHub Publication Checklist

This folder is prepared for GitHub but has not been initialized as a Git repository or uploaded anywhere.

## Before making it public

1. Create an empty repository named `token-ledger` through the GitHub interface. Do not auto-generate a README, license, or ignore file because this folder already contains them.
2. Choose the final owner and public URL.
3. Add that real URL to the `repository` field in `Cargo.toml`. Do not use a placeholder URL.
4. Keep `publish = false` unless a separate decision is made to publish on crates.io.
5. Replace the generic private-reporting language in `SECURITY.md` with the repository's private vulnerability-reporting route when one exists.
6. Review every file in the upload and confirm that `target/`, `dist/`, `open-source/`, databases, configs, generated reports, and environment files are absent.

## Included GitHub configuration

- `.github/workflows/ci.yml` runs formatting, strict Clippy, and tests on Windows, Linux, and macOS, plus a Rust 1.88 minimum-version job.
- The CI workflow grants the token read-only repository-content permission and does not publish, release, push, or use secrets.
- `.github/dependabot.yml` checks Cargo and GitHub Actions dependencies weekly.
- Structured issue forms require privacy confirmation for bugs, pricing corrections, and feature requests.
- The pull-request template requires accounting, compatibility, privacy, and verification review.

## Repository settings to enable manually

- Require the CI checks before merging to the default branch.
- Set default workflow token permissions to read-only.
- Enable private vulnerability reporting.
- Enable secret scanning and push protection.
- Enable Dependabot alerts and security updates.
- Add repository topics such as `rust`, `cli`, `token-usage`, `claude-code`, `codex`, and `cost-tracking`.

No automated release workflow is included. Release publication should remain a separate, explicit decision after the first public CI run succeeds.
