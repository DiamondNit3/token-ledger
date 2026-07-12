# GitHub Publication Checklist

The local project is initialized on `main` with intentional history and a private staging remote at `https://github.com/DiamondNit3/token-ledger`. Cargo, homepage, documentation, security-reporting, issue-template, CI, and release metadata use that canonical location.

## Before making it public

1. Complete the private CI and release workflows for the intended public version.
2. Download every Windows, Linux, and macOS release archive, verify `SHA256SUMS.txt`, and smoke-test the extracted binaries on their native platforms.
3. Confirm that the README demo, installation steps, unsigned-binary disclosures, and current limitations match those exact artifacts.
4. Run the public-content gate and inspect the Git tree for databases, configs, generated reports, environment files, user paths, credentials, and real client records.
5. Change visibility without announcing the repository, immediately enable GitHub private vulnerability reporting, and test the reporting route before sharing the public URL. GitHub exposes this setting only for public repositories.
6. Keep `publish = false` unless the separate [crates.io assessment](CRATES_IO.md) is approved.
7. Change visibility only after branch protection and repository security settings are ready; do not announce the project merely because the repository became public.

## Included GitHub configuration

- `.github/workflows/ci.yml` runs formatting, strict Clippy, tests on Windows, Linux, and macOS, a public-package inspection, and a Rust 1.88 minimum-version job.
- `.github/workflows/release.yml` builds four native archives, extraction-tests them, consolidates checksums, rejects mutable/lightweight release paths, and publishes only from an annotated tag push.
- Workflows default to read-only repository-content permission; only the final tag release job receives `contents: write`.
- Every third-party action is pinned to a full commit SHA and checkout credentials are not persisted.
- `.github/dependabot.yml` checks Cargo and GitHub Actions dependencies weekly.
- Structured issue forms require privacy confirmation for bugs, pricing corrections, and feature requests.
- The pull-request template requires accounting, compatibility, privacy, and verification review.

## Repository settings to enable manually

- Require the CI checks before merging to the default branch.
- Set default workflow token permissions to read-only.
- Immediately after public visibility is enabled, turn on private vulnerability reporting and verify the `security/advisories/new` route before announcing the repository.
- Enable secret scanning and push protection.
- Enable Dependabot alerts and security updates.
- Add repository topics such as `rust`, `cli`, `token-usage`, `claude-code`, `codex`, and `cost-tracking`.

See [RELEASING.md](RELEASING.md) for artifact publication and [LAUNCH.md](LAUNCH.md) for the staged community and maintenance gate.
