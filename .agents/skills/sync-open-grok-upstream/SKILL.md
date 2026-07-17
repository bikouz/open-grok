---
name: sync-open-grok-upstream
description: Compare xai-org/grok-build with the Open Grok fork and selectively import compatible upstream changes. Use when fetching upstream, reviewing a new snapshot, deciding what the fork can adopt, replaying patches without a merge base, or validating that upstream ports preserve fork-specific behavior.
---

# Sync Open Grok with Upstream

## Establish ancestry and baselines

1. Read `AGENTS.md`, `docs/provider-architecture.md`, and the subsystem docs touched by the upstream range.
2. Run `git status --short --branch`; preserve all concurrent edits.
3. Fetch `upstream/main`, record both heads, and run `git merge-base HEAD upstream/main` before choosing a strategy.
4. Capture focused baseline tests for the likely import areas.

If there is no merge base, treat upstream as a republished snapshot. Do not merge or rebase wholesale. Use a temporary replay worktree/directory and patch or reimplement compatible deltas against the fork.

## Inventory before importing

Group upstream changes by subsystem and risk:

- security, permissions, managed policy, and sandbox
- sessions, persistence, worktrees, and headless runtime
- tools, hooks, plugins, and skills
- pager/settings/PTY behavior
- providers, auth, catalogs, compaction, and Code Mode
- release/update, branding, and paths

For each candidate, identify its upstream tests, fork conflicts, dependencies, and user-visible behavior. Defer changes that require importing an incompatible provider/settings architecture or weaken a fork invariant.

## Replay in reviewable batches

- Port the smallest coherent subsystem with its tests.
- Preserve `open-grok`, `$OPENGROK_HOME`, `.opengrok`, updater/release behavior, provider metadata, credential isolation, Code Mode, export boundaries, and Open Grok branding.
- Keep upstream security hardening unless it conflicts with a stronger fork rule; document intentional omissions such as diagnostic upload or trust broadening.
- Format, test, review, and commit each batch before starting the next.

## Validate and report

Run focused upstream tests plus fork regression suites for every affected boundary. Run combined shell/pager compilation when cross-layer behavior moves. For broad-suite failures, compare with baseline and rerun isolated failures without concealing repeatable regressions.

Report:

1. upstream and fork heads plus merge-base result;
2. imported commits grouped by behavior;
3. intentionally deferred/rejected changes and why;
4. focused and broad test results;
5. push status and any remaining local commits.
