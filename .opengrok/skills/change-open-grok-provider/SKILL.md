---
name: change-open-grok-provider
description: Implement or review Open Grok provider, model, API backend, authentication, auxiliary-model, or live-switch behavior. Use for xAI, Codex, Kimi Platform, Kimi Code, Responses, Chat Completions, Messages, OAuth, API-key, catalog, hosted-tool, compaction, or provider-isolation changes.
---

# Change an Open Grok Provider

## Start from the contracts

Read `docs/provider-architecture.md`, `docs/agents/providers.md`, and the relevant Codex or Code Mode port document. Classify the change across three independent axes:

1. `ApiBackend`: HTTP request and stream protocol.
2. `ProviderProfile`: dialect, tools, private metadata, session-auth kind, and export policy.
3. `AuthScheme` / `BearerResolver`: API-key or OAuth credentials and refresh.

Provider identity must come from model metadata. An endpoint, model slug, or Responses backend is not a credential decision.

## Preserve isolation

- Keep adapters in `xai-grok-sampler` credential-free.
- Keep xAI, Codex, Kimi Platform, and Kimi Code keys, OAuth refresh, logout, caches, catalogs, trusted hosts, headers, and 401 retries separate.
- Let an explicit model API key win over built-in OAuth.
- Scope hosted tools and opaque response history to the declared dialect.
- Close the monotonic xAI export boundary for every denying profile, including subagent trees.
- Route recap, memory, titles, embeddings, and other auxiliary sampling explicitly; do not leak the active chat provider's credentials or policy.

## Handle live mutations fail-closed

When Settings or login changes provider state while sessions exist:

1. Assign mutation order before spawning async work; preserve FIFO execution.
2. Hold prompts while provider identity, endpoint, catalog, or credentials are unresolved.
3. Rebuild/rebind the session before releasing queued work.
4. Cancel or revoke stale subagent provider credentials.
5. Cover send-now, queued send, interject, server-queue force-send, late session load, retry, and rollback paths.

## Verify the matrix

Add focused registry, request, stream, tool, structured-output, credential-isolation, refresh, logout, export-boundary, compaction, and live-switch tests as applicable. Start with:

```sh
cargo test --locked -p xai-grok-sampling-types
cargo test --locked -p xai-grok-sampler --test test_actor
cargo test --locked -p xai-grok-shell --test codex_auth_contract
cargo test --locked -p xai-grok-shell --test auxiliary_provider_routing
cargo test --locked -p xai-grok-code-mode
cargo test --locked -p xai-grok-code-mode-protocol
```

Also run the focused pager Settings/login/model tests and session tests for the changed provider. Commit types/adapters, auth/runtime, and UI/live-rebind units separately when that makes rollback and review clearer.
