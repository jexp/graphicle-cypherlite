# Contributing to CypherLite

How changes move through review here. The short version: **draft early, iterate cheap, flip to review when a human says so.**

## The draft-PR workflow

1. **Open every PR as a draft.** A draft runs only the *fast gate*: formatting, clippy on the light crates, the light unit lane, and the three automated reviews (code/stellar, security, rust). It skips the full build/test matrix — OS sweeps, Bolt driver smokes, corpus (TCK/CIP/cookbook), feature bundles, loadtest gates.
2. **Catch findings locally first** — the local gates are cheaper and faster than any CI round: run `cargo fmt --all --check`, `cargo clippy`, the touched crates' tests, and the local stellar pipeline before the first push.
3. **Iterate in ~10-minute loops.** Watch the fast gate's review findings, fix them in the same squashed commit, re-push. One squashed commit per PR, always — re-squash fixes into it with `scripts/re-squash.sh` (the revert-proof recipe; a bare `git reset --soft <base>` is banned — it keeps the stale index and can silently revert main's newer merges).
4. **A clean fast gate means asking a maintainer to mark the PR ready for review.** Never flip the draft flag yourself. Marking ready re-runs the *full* matrix before merge.
5. **Merging cancels the PR's in-flight runs automatically** — nothing burns after the PR is gone.

## Review feedback and triage

- Read review comments **newest first** — each round's summary supersedes earlier rounds; only the latest round is current.
- Every finding gets a disposition, posted as a reply on its inline thread: `fixed in <sha>`, `deferred → LABS-N`, `refuted — <evidence>`, or `won't fix — <reason>`. Then resolve the thread. Nits never open inline threads (ADR-0354) — they ride a collapsed appendix.
- **Deferred findings go to Linear issues tagged `review-tasks`** — that label is the nightly queue.

## The night-shift (deferred fixes)

The stellar night-shift (03:00-07:00 Europe/Berlin) drains the deferred queue automatically: it pulls `review-tasks` issues and each PR's carried review ledger, clusters the findings (one cluster = one small PR), has a model apply the remedies without running cargo, verifies with the fast tier (fmt, clippy, touched-crate tests), and publishes a PR per cluster. A patch that fails the tier is discarded softly — the cluster stays open for a retry. At 07:00-09:00 Berlin a recurrence report histograms recurring finding patterns and posts promotion candidates (≥5 findings across ≥3 PRs) to the distillation tracking issue.

## Conventions that matter

- **ADRs** (`docs/adr/`) record every architecture/storage/protocol/API decision, minted late, once (`scripts/adr_mint.py`).
- **Rebase, never merge.** Keep branches linear; squash to one commit.
- Full agent contract: [`AGENTS.md`](AGENTS.md). Decision log: [`docs/adr/`](docs/adr/).
