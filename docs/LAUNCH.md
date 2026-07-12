# Public Launch and Maintenance Plan

Token Ledger should not be treated as established merely because one launch post performs well. Credibility comes from reproducible releases, public issue handling, and several maintenance cycles.

## Launch gate

Do not announce broadly until all of these are true:

- the repository is public and its security-reporting link works;
- CI passes on Windows, Linux, macOS Intel, macOS Apple Silicon, and Rust 1.88;
- a tagged release contains all four checksummed binary archives;
- the Windows, Linux, and both macOS archives have been downloaded and smoke-tested after publication;
- `token-ledger demo` runs without reading real local client data;
- the README GIF and 30-second quick start match the released binary;
- known limitations and unsigned macOS status are explicit; and
- at least two people other than the maintainer can complete installation from the README.

## Community sequence

Post separately and adapt the explanation to each audience. Verify each community's current rules immediately before posting; never cross-post identical promotional copy in a burst.

1. **Rust Users Forum** — ask for engineering feedback on local parsing, privacy boundaries, and release portability.
2. **OpenAI Developer Community project showcase** — focus on Codex accounting, synthetic demo, and the distinction between API-equivalent value and actual billing.
3. **r/rust or its current project-showcase thread** — focus on the Rust implementation, tests, and resource-bounded parsing.
4. **r/commandline** — focus on terminal UX, plain output, JSON/CSV stability, and local-first installation.
5. **Show HN** — use a neutral `Show HN:` title and lead with the working demo, source, exact limitations, and why local accounting is hard.

Optional later channels include a relevant Claude Code community, local Codex meetups, and release notes shared with users who already engaged. Participate in each community before and after posting; do not use it only as an announcement feed.

## Core launch message

> Token Ledger is a local-first CLI that turns Claude Code and Codex session counters into daily model usage and reproducible list-price estimates without storing transcript bodies. It distinguishes estimates, credits, and actual billing evidence; handles incomplete history conservatively; and ships a synthetic demo plus checksummed binaries for Windows, macOS, and Linux.

## Channel drafts

### Show HN

**Title:** `Show HN: Token Ledger – local token and cost accounting for Claude Code and Codex`

**Opening:**

> I built Token Ledger because subscription dashboards and local coding-agent logs answer different questions. It reads local Claude Code and Codex accounting records, never copies transcript bodies into its database, and reports exact, bounded, partial, or unpriced estimates instead of turning unknowns into zero. `token-ledger demo` shows the complete workflow using synthetic data.

Follow with the GIF, three commands, source link, binary checksums, accounting caveats, and two specific questions for reviewers.

### Rust community

> I would value review of three implementation boundaries: cumulative-counter reset handling, rewrite-safe incremental checkpoints, and deterministic pseudonymization that still permits deduplication. The project is Rust 2024, forbids unsafe code, tests its Rust 1.88 MSRV, and ships synthetic fixtures only.

### AI developer communities

> This is not a provider invoice tool. It compares locally persisted usage with effective-dated public rate rules and keeps API-equivalent USD, Codex credits, recorded cash, and provider reconciliation separate. Feedback on missing client record variants is welcome through synthetic fixtures only.

## Maintenance commitment

Treat the first public release as the start, not the finish.

### Release 1 — v0.4.0

- Harden completeness, rewrite detection, identifiers, and resource limits.
- Establish demo, GIF, multi-platform binaries, checksums, and public security reporting.
- Respond to valid bug and privacy reports promptly; document every known format gap.

### Release 2 — v0.4.1 or v0.5.0

- Incorporate real-world format feedback through synthetic reproductions.
- Publish a compatibility matrix and installation failure notes by platform.
- Report which launch claims held up and which were corrected.

### Release 3 — v0.4.2 or v0.6.0

- Close the most common onboarding and correctness issues.
- Decide on crates.io based on demonstrated maintenance capacity.
- Add signed/notarized artifacts if adoption justifies the operational cost.

## Operating cadence

- Triage new correctness, privacy, and security reports at least weekly.
- Verify official pricing sources at least monthly and before each release.
- Run the complete release matrix for every supported version.
- Publish changelog entries that distinguish fixes from changed estimates.
- Never request real transcripts in public support.
- Mark unsupported client-format changes clearly instead of guessing.

## Evidence before an acceptance claim

Do not describe program or showcase acceptance as likely until the project has at least three public releases, multiple external users, resolved public issues, a documented security-response path, and a maintenance history spanning roughly 60–90 days. A launch spike, stars, or downloads alone is not that evidence.

Track releases, unique external reporters, time-to-triage, confirmed platform installs, unresolved correctness issues, and catalog freshness. Avoid collecting invasive product analytics to obtain those metrics.
