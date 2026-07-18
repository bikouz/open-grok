# Hooks, plugins, and skills

Implementation map of file-based hooks, plugin discovery/marketplace, and skill/command loading into agent sessions. Paths are relative to the repo root under `crates/codegen/` unless noted.

**End-user product docs (do not duplicate tutorials here):**

| Topic | User guide |
| --- | --- |
| Skills | `xai-grok-pager/docs/user-guide/08-skills.md` |
| Plugins | `…/09-plugins.md` |
| Hooks | `…/10-hooks.md` |

Pager overview (Action/Effect, extensions modal): [tui-and-config.md](tui-and-config.md). Permission order that includes PreToolUse: [agent-runtime.md](agent-runtime.md).

## Architecture snapshot

```text
Discovery (disk + config)
  ├── xai-grok-hooks          JSON / settings hooks
  ├── xai-grok-agent/plugins  plugin dirs + InstallRegistry
  └── xai-grok-agent/prompt/skills + xai-grok-tools/skills

Session startup (shell)
  ├── discover_hooks(git_root, compat, folder_trust)
  ├── discover_plugins → PluginRegistry
  ├── list_skills_with_plugins → skill list / slash
  ├── merge plugin hooks into HookRegistry
  └── agent builder injects skill listing into prompt

Turn path
  prepare_tool_call
    → plan_mode_edit_gate
    → PreToolUse file hooks (blocking deny) + client hooks (x.ai/hooks/run)
    → permissions
    → dispatch
  PostToolUse / failures / compact / session lifecycle → non-blocking hooks
```

| Concern | Path |
| --- | --- |
| Hooks crate | `xai-grok-hooks/` |
| Shell hook source policy | `xai-grok-shell/src/util/hooks.rs` |
| Session hook dispatch | `…/session/acp_session_impl/hook_dispatch.rs` |
| Client-registered hooks | `…/session/acp_session/hooks.rs` |
| Hooks/plugins modal actions | `…/session/acp_session_impl/hooks_plugins.rs` |
| Wire DTOs | `xai-hooks-plugins-types/` |
| Plugin discovery / install | `xai-grok-agent/src/plugins/` |
| Marketplace browse/install | `xai-grok-plugin-marketplace/` |
| Shell plugin CLI helpers | `xai-grok-shell/src/plugin.rs` |
| Skill parsing | `xai-grok-tools/src/implementations/skills/` |
| Skill orchestration | `xai-grok-agent/src/prompt/skills.rs` |
| Bundled skills | `xai-grok-shell/skills/*/SKILL.md` |
| Pager UI | `xai-grok-pager/src/views/extensions_modal.rs`, `plugin_cmd.rs` |

---

## Hooks

### Discovery

**Single entry for live sessions:** `shell::util::hooks::discover_hooks(git_root, compat, trusted)`.

Source construction: `discover_hook_source_paths` → global then project; project sources included only when **folder-trusted**.

| Scope | Typical paths |
| --- | --- |
| Global | `$OPENGROK_HOME/hooks/*.json`, optional `$OPENGROK_HOME/hooks-paths` list, compat `~/.claude/settings*.json`, `~/.cursor/hooks.json` |
| Project | `<git-root>/.opengrok/hooks/`, compat `.claude/settings*.json`, `.cursor/hooks.json` |
| Plugin | Bundled via plugin load (`hooks/hooks.json` or manifest) merged into registry with plugin name prefixes |

Compat vendor dirs are gated by `CompatConfig` (claude/cursor hooks toggles). After Claude import mark, raw `.claude/settings.json` may be skipped while native `.opengrok/hooks/` still loads.

Load API: `xai_grok_hooks::discovery::load_hooks_from_sources`. Global hooks are name-prefixed `global/…`; project `project/…`. Snapshot is **session-scoped** — disk edits need reload (`HooksAction::Reload` / session restart). Matcher recompile after deserialize is fail-closed for that hook (bad pattern → never-match); an intentionally absent matcher remains match-all.

### Events

`HookEventName` in `xai-grok-hooks/src/event.rs` (serde snake_case wire; accepts PascalCase / third-party aliases):

| Category | Events |
| --- | --- |
| Session | `SessionStart`, `SessionEnd`, `Stop`, `StopFailure` |
| Tools | `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied` |
| User | `UserPromptSubmit`, `Notification` |
| Subagent | `SubagentStart`, `SubagentStop` (`SubagentEnd` alias) |
| Compaction | `PreCompact`, `PostCompact` |

Third-party names like `beforeShellExecution` / `afterFileEdit` map onto generic Pre/Post tool events; scripts filter on tool name in the JSON envelope or via `matcher`.

### PreToolUse deny and fail-open

```text
dispatch_pre_tool_use (file hooks)
  for each matching enabled hook in config order:
    run command or HTTP handler
    explicit Deny → stop chain, tool blocked
    crash / timeout / malformed / missing binary → FAIL OPEN (continue)
  no deny → Allow
```

| Rule | Detail |
| --- | --- |
| Only **PreToolUse** can block | All other events are non-blocking / observe-only |
| Fail-open | Failures logged + scrollback; tool proceeds |
| Allow does not skip permissions | PreToolUse allow ≠ YOLO; permission manager still runs |
| Plan gate runs first | Plan-mode edit rejections happen before hooks |

Client (IDE) hooks: groups registered at `session/new` via `_meta["x.ai/hooks"]`. PreToolUse uses reverse RPC `x.ai/hooks/run` (default **30s**, cap 300s); other events fire-and-forget `x.ai/hooks/event`. Timeout / transport / unknown decision → **fail open**.

### Trust

- **Folder trust** (`$OPENGROK_HOME/trusted_folders.toml`) is the single authority for project hooks **and** project MCP/LSP.
- Legacy `trusted-hook-projects` migrates into folder trust (`xai-grok-hooks/src/trust.rs`).
- Untrusted project: project hook sources omitted; global + user still load.
- Trust/Untrust from extensions modal → `hooks_plugins.rs` → `folder_trust::grant/revoke` → reload hooks + reseed MCP output cap.
- Per-hook disable list: disabled-hooks file under user home (`trust::disable_hook` / `is_hook_disabled`).

### Handlers

| Type | Module |
| --- | --- |
| Command (subprocess) | `runner/command.rs` — stdin JSON envelope, env expansion |
| HTTP | `runner/http.rs` — POST payload |

Env expansion supports plugin roots (`CLAUDE_PLUGIN_ROOT`, etc.). Payload size caps: `MAX_PAYLOAD_SIZE` (128 KiB) for tool input/result fields.

**Security:** hooks are **not** a sole security boundary — combine with permission deny rules and sandbox. Documented in AGENTS.md.

### Examples and tests

- Examples: `xai-grok-hooks/examples/`
- Unit + integration: `xai-grok-hooks` tests, `shell/.../client_hooks_tests.rs`, inspect path in `shell/src/inspect/`

---

## Plugins

### Layout

A plugin directory (convention-based; optional `plugin.json`):

```text
my-plugin/
  ├── plugin.json              # optional; also .opengrok-plugin/ or .claude-plugin/
  ├── skills/…/SKILL.md
  ├── commands/*.md            # flat → slash commands
  ├── agents/                  # subagent definitions
  ├── hooks/hooks.json
  ├── .mcp.json
  └── .lsp.json
```

Manifest paths that escape the plugin root are rejected. Missing manifest: name from directory; components still discovered by convention.

### Discovery scopes

Priority (high → low) in `xai-grok-agent/src/plugins/discovery.rs`:

| Scope | Sources |
| --- | --- |
| `CliOverride` | `--plugin-dir` (always trusted) |
| `Project` | `.opengrok/plugins/*`, `.claude/plugins/*` (folder-trust gated) |
| `User` | `$OPENGROK_HOME/plugins/*`, `$OPENGROK_HOME/installed-plugins/*`, compat `~/.claude/plugins/*` |
| `ConfigPath` | `[plugins].paths` in config |

`PluginOrigin` records fine-grained provenance (marketplace install, Claude marketplace clone, etc.) for UI without encoding it into `PluginId` (`<scope>/<hex8>/<name>`).

### Marketplace and install

| Piece | Path |
| --- | --- |
| Marketplace crate | `xai-grok-plugin-marketplace/` |
| Install registry | `xai-grok-agent/src/plugins/install_registry.rs` |
| Git / local install | `…/plugins/git_install.rs` |
| Shell CLI | `shell/src/plugin.rs`, pager `plugin_cmd.rs` → `open-grok plugin …` |
| Official source | `OFFICIAL_SOURCE_GIT_URL` (`xai-org/plugin-marketplace`) |

Installs land under managed storage (`installed-plugins`) with registry metadata and optional marketplace provenance. Path traversal on marketplace relative paths is rejected (`MarketplaceRelativePath`).

ACP list/action: `x.ai/plugins/*` DTOs in `xai-hooks-plugins-types`.

### What plugins contribute at session load

1. **Skills / commands** — merged into skill list (plugin scope; bare-name native wins; qualified `plugin:skill` preserved).
2. **Hooks** — appended to `HookRegistry` (reload mid-session supported).
3. **MCP / LSP** — descriptors merged when enabled; project plugin MCP still subject to folder trust.
4. **Agents** — subagent discovery (`discovery::all_subagents_with_plugins`).

Disable/enable and trust UI live in the pager extensions modal; shell handles `PluginsAction` / `HooksAction`.

---

## Skills

### Discovery priority

Orchestrated by `xai-grok-agent/src/prompt/skills.rs` (`list_skills` / `list_skills_with_plugins`). Parsing primitives in `xai-grok-tools/.../skills/discovery.rs`.

**Scope order (lower enum = higher priority):**

| `SkillScope` | Typical roots |
| --- | --- |
| `Local` | `cwd/.opengrok/skills`, `cwd/.agents/skills`, vendor `cwd/.claude/skills` (compat) |
| `Repo` | same under git root; intermediate dirs between cwd and root |
| `User` | `$OPENGROK_HOME/skills`, `~/.agents/skills`, vendor user dirs (compat) |
| `Server` | launcher-injected `server_skill_dirs` |
| `Bundled` | `bundled_skill_dirs` + extracted platform skills under `$OPENGROK_HOME` |
| `Plugin` | lowest bare-name precedence; use `plugin-name:skill` |

Same-name: higher-priority scope wins. Plugin collisions keep **qualified** names. Config `[skills].paths` adds extra roots; `[skills].ignore` hides by path prefix; `[skills].disabled` keeps listing but excludes from prompt/invocation.

**Commands:** flat `commands/*.md` (no deep recursion) become slash commands. Skills: `SKILL.md` under skill dirs (walk depth ≤ 5) or a directory that itself is a skill (`find_skill_md_paths`).

**Not filtered by `.gitignore`.** Use ignore/disabled config instead.

Vendor defaults under `/.cursor/` and `/.claude/` denylists are dropped (path-scoped so user skills of the same name under `.opengrok` remain).

Built-in examples: `xai-grok-shell/skills/{help,code-review,create-skill,…}/SKILL.md`.

### `SKILL.md` frontmatter

Parsed fields (see `discovery.rs` / `types.rs`): `name`, `description`, `when_to_use`, `paths` (path-gated listing), `argument_hint`, `allowed_tools`, optional model override, metadata, license, compatibility. Limits: name/description size, frontmatter peek bytes.

Invocation packaging: XML skill envelopes (`skill.rs`) with path attributes; internal link resolution relative to skill dir.

### Slash surface

- Skills and command markdown appear as ACP-advertised slash commands (`CommandRegistry::set_acp_commands` can replace by name).
- Builtin pager commands and ACP skills coexist; ACP set can drop prior ACP-only entries (e.g. memory `/flush` when re-advertised).
- Skill hot-reload: config watcher uses `collect_skill_config_dirs` so discovery and watch roots cannot drift.

---

## Session load path (shell / agent / tools)

```text
Session setup
  1. Resolve cwd, git_root, CompatConfig, folder trust
  2. discover_hooks → HookRegistry on SessionActor
  3. discover_plugins → PluginRegistry; merge plugin hooks/MCP/LSP/agents
  4. list_skills_with_plugins → skill infos
  5. Agent builder (xai-grok-agent): system prompt skill listing when discover_skills
  6. Advertise slash commands (skills, commands, gated builtins)
  7. Tool bridge Resources: MemoryBackend, skill tracker, etc.

Tool call
  prepare_tool_call → file PreToolUse + client PreToolUse → permissions → Tool::call

Skill slash invoke
  load SKILL.md body → user/system message path with skill envelope
  (not a separate tool by default; opencode skill tool is compat-only)
```

Inspect / doctor surfaces: `shell/src/inspect/` and `mcp_doctor.rs` re-run discovery with the same trust gates for user-visible lists.

Mid-session:

- Hooks reload without full restart.
- Plugin hook registry refresh via session commands.
- Skill filesystem changes may require rebuild / watcher-driven refresh depending on path.
- Folder trust change must re-run project-gated discovery (hooks + project plugins + MCP caps).

---

## Test index

| Area | Where |
| --- | --- |
| Hook discovery / dispatch / fail-open | `xai-grok-hooks` unit + `tests/integration.rs` |
| Hook source policy | `shell/src/util/hooks.rs` tests (if present), inspect list |
| Client hooks | `acp_session_tests/client_hooks_tests.rs` |
| Plugin discovery / manifest | `xai-grok-agent/src/plugins/*` tests |
| Marketplace paths / install | `xai-grok-plugin-marketplace` tests |
| Skill discovery / parse / dedupe | `xai-grok-tools/.../skills/` tests; `agent/prompt/skills.rs` |
| Extensions modal | pager `views/extensions_modal.rs` tests |
| Wire DTOs | `xai-hooks-plugins-types` |

```sh
cargo test --locked -p xai-grok-hooks
cargo test --locked -p xai-grok-plugin-marketplace
cargo test --locked -p xai-grok-tools -- skills
cargo test --locked -p xai-grok-shell -- hooks
cargo test --locked -p xai-grok-shell -- plugin
```

---

## Gotchas

1. **Hooks fail open** — never treat PreToolUse as the only deny layer; pair with permissions + sandbox.
2. **Allow ≠ skip permissions** — PreToolUse allow still hits the permission manager.
3. **Folder trust is unified** — project hooks, MCP, LSP share one gate; untrust immediately reseeds MCP caps.
4. **Project hooks need a git worktree root** — trust helpers error outside a repo.
5. **Session snapshot of hooks** — editing JSON on disk does not affect the running session until reload.
6. **Client hook timeout is shorter than some hosts (30s)** — hung IDE hooks must not stall tools for minutes.
7. **Plugin path escape** — reject `..` in manifest and marketplace relative paths.
8. **Skill roots ignore `.gitignore`** — large vendor trees need `[skills] ignore` / compat toggles / denylists.
9. **Native skills beat plugins on bare names** — document qualified names for plugin skills.
10. **Do not scan `skills-cursor/`** — product-specific Cursor default trees are intentionally excluded.
11. **Matcher recompile is narrow-fail-closed** — an invalid matcher becomes never-match after deserialize, while an intentionally absent matcher remains match-all; keep the distinction covered by tests.
12. **HTTP hook URL display** — show pre-expansion `raw_url` in UI to avoid leaking `${TOKEN}` expansions.

---

## See also

- [tui-and-config.md](tui-and-config.md) — config merge, extensions modal, slash registration
- [agent-runtime.md](agent-runtime.md) — tool prepare order, permissions, subagents
- [architecture.md](architecture.md) — crate layering
- User guides 08 / 09 / 10 under `xai-grok-pager/docs/user-guide/`
