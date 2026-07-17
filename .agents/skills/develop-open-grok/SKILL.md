---
name: develop-open-grok
description: Safely implement and verify scoped Open Grok changes in this Rust workspace. Use when setting up the checkout, modifying code, documentation, or shell helpers, investigating build or test failures, or preparing commits without disturbing an installed release or concurrent work.
---

# Develop Open Grok

## Establish the change boundary

1. Read `AGENTS.md` and the nearest `docs/agents/` page for the subsystem.
2. Run `git status --short --branch` before editing. Treat every pre-existing change as someone else's work unless proven otherwise.
3. Map the request to pager, shell, sampler, tools, workspace, configuration, or shared types. Read adjacent tests before adding a parallel abstraction.
4. Record a focused baseline when the target already fails.

## Implement a scoped unit

- Edit per-crate manifests; never hand-edit the generated root `Cargo.toml`.
- Preserve `$OPENGROK_HOME` / `~/.opengrok` isolation and Open Grok branding.
- Keep pager dispatch pure (`Action` to state plus `Effect`) and perform I/O in effects or ACP handlers.
- Add tests next to the behavior. Include negative or fail-closed cases for permission, auth, persistence, and routing changes.
- Recheck `git status` after long builds because this checkout may be shared.

## Verify proportionally

Run the smallest complete stack for the changed behavior:

```sh
cargo fmt --all -- --check
cargo clippy --locked -p <crate> --all-targets
cargo test --locked -p <crate> -- <focused-filter>
./bin/open-grok-dev --version
```

- Run `bash -n <script>` for every changed shell helper.
- Use an isolated `OPENGROK_HOME` for runtime tests.
- Do not mistake a long first compile or release link for a hang; inspect process activity first.
- If a broad suite fails, rerun the failure alone. Report global-state interference separately, but never hide a repeatable failure.
- If repo-wide formatting finds unrelated drift, leave it visible and format/check only owned files before committing.

## Commit and hand off

1. Review `git diff --check` and the exact owned diff.
2. Stage explicit paths only.
3. Commit each tested behavior or invariant with a descriptive subject; do not wait for an unrelated later phase.
4. Report the commit, focused checks, broader checks, and any baseline limitation.
