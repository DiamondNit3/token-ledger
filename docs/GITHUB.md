# GitHub Publication Checklist

The public project lives at `https://github.com/DiamondNit3/token-ledger`. Cargo, homepage, documentation, security-reporting, issue-template, CI, and release metadata use that canonical location.

## Public release gate

1. Complete the private CI and release workflows for the intended public version.
2. Download every Windows, Linux, and macOS release archive, verify `SHA256SUMS.txt`, and smoke-test the extracted binaries on their native platforms.
3. Confirm that the README demo, installation steps, unsigned-binary disclosures, and current limitations match those exact artifacts.
4. Run the public-content gate and inspect the Git tree for databases, configs, generated reports, environment files, user paths, credentials, and real client records.
5. Verify GitHub private vulnerability reporting, secret scanning, push protection, protected `main`, the `v*` tag ruleset, and the reviewer-gated `release` environment before sharing a release URL.
6. Keep `publish = false` unless the separate [crates.io assessment](CRATES_IO.md) is approved.
7. Do not announce a release merely because a tag or repository is public; wait for the protected release workflow and independently verify its checksums.

## Included GitHub configuration

- `.github/workflows/ci.yml` runs formatting, strict Clippy, tests on Windows, Linux, and macOS, a public-package inspection, and a Rust 1.88 minimum-version job.
- `.github/workflows/release.yml` builds four native archives, extraction-tests them, consolidates checksums, rejects mutable/lightweight release paths, and publishes only from an annotated tag push.
- Workflows default to read-only repository-content permission; only the final tag release job receives `contents: write`.
- Every third-party action is pinned to a full commit SHA and checkout credentials are not persisted.
- `.github/dependabot.yml` checks Cargo and GitHub Actions dependencies weekly.
- Structured issue forms require privacy confirmation for bugs, pricing corrections, and feature requests.
- The pull-request template requires accounting, compatibility, privacy, and verification review.

## Enabled repository settings

- `main` requires the Windows, Linux, macOS, and Rust 1.88 checks, a pull request, conversation resolution, and CODEOWNER review; force pushes and deletion are disabled.
- The `release` environment requires explicit approval before publication.
- An active tag ruleset restricts creation, update, and deletion of `v*` release tags.
- Private vulnerability reporting, secret scanning, and push protection are enabled.
- Default workflow permissions are read-only; only the final release job receives `contents: write`.
- Dependabot monitors Cargo and GitHub Actions dependencies weekly.

## Single-maintainer limitation

The repository currently has one administrator. `main` protection therefore allows the administrator to bypass required checks, and the administrator can approve a release deployment they initiated. These controls prevent accidents and constrain future collaborators, but they are not independent authorization. After adding a second trusted maintainer, enable administrator enforcement on `main`, add that maintainer as a required release reviewer, and prevent self-review. Do not enable those settings with only one reviewer: doing so would make ordinary maintenance and security releases impossible.

See [RELEASING.md](RELEASING.md) for artifact publication and [LAUNCH.md](LAUNCH.md) for the staged community and maintenance gate.
