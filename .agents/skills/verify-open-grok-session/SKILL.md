---
name: verify-open-grok-session
description: Inspect persisted Open Grok sessions and prove resume, compaction, rewind, recap, memory, or subagent-history behavior from on-disk evidence. Use when a user asks whether compaction worked, whether a child run was preserved, why a session cannot resume, or what happened in a specific session id.
---

# Verify an Open Grok Session

## Anchor on the exact session id

Read `docs/agents/sessions.md`. Resolve the active home from `$OPENGROK_HOME` or `~/.opengrok`, then find the exact id below `sessions/`. Child sessions normally live at `<parent-session>/subagents/<subagent-id>/`.

Do not declare a directory resumable merely because it exists: `summary.json` is required. Do not copy user session dumps into the repository or expose credentials in the report.

## Inspect the evidence chain

For the parent and relevant child, inspect:

1. `summary.json` for identity, timestamps, model/provider, sticky metadata, and child linkage.
2. `updates.jsonl` for persisted conversation updates, compaction checkpoint references, rewind markers, and prompt ordering.
3. `events.jsonl` for lifecycle and telemetry events when present.
4. `compaction_checkpoints/<id>.json` for every referenced checkpoint.
5. `chat_history.jsonl`, `prompt_context.json`, or `system_prompt.txt` only when the question requires their content.

Use `rg`/`jq` against explicit resolved paths. Preserve event order rather than counting strings alone.

## Prove the requested behavior

- Auto-compaction: show `auto_compact_started` → persisted checkpoint/reference → `auto_compact_completed` for the same attempt.
- Resume: prove `summary.json` exists under the requested encoded cwd; an images-only stub is not resumable.
- Subagent history: prove spawn linkage and inspect the child directory independently; do not assume the parent update stream contains the child's full trace.
- Rewind/replay: use checkpoint-aware replay semantics from `helpers/replay.rs`; prompt indexes alone are unreliable after compaction.
- Cross-provider history: confirm opaque Codex/xAI items are projected only to their matching dialect and plaintext fallback is used where required.

## Report a verdict

State the exact session id and directory, the evidence sequence, checkpoint ids, child ids, and any missing or contradictory artifact. Distinguish “not found,” “started but incomplete,” “completed and persisted,” and “persisted but not resumable.”
