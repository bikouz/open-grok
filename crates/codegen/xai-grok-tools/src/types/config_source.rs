use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where a piece of configuration was loaded from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ConfigSource {
    /// Built-in / bundled with the binary.
    Builtin,
    /// Bundled skill shipped with the binary (extracted to ~/.opengrok/skills/
    /// or injected via bundled skill dirs).
    Bundled { path: PathBuf },
    /// Server-synced (e.g. ~/.opengrok/server-skills from the skill store).
    Server { path: PathBuf },
    /// Project-scoped: cwd/.opengrok/ or cwd/.claude/.
    Project { path: PathBuf },
    /// User-scoped: ~/.opengrok/ or ~/.claude/.
    User { path: PathBuf },
    /// Plugin-provided component.
    Plugin { plugin_name: String, path: PathBuf },
    /// config.toml `[mcp_servers.*]`, `[skills]`, etc. `path` is
    /// domain-specific: the declaring config.toml for MCP servers, the
    /// skill's own SKILL.md for `[skills].paths` skills.
    ConfigToml { path: PathBuf },
    /// `~/.claude.json` MCP servers.
    ClaudeJson { path: PathBuf },
    /// `.mcp.json` project-level MCP config.
    McpJson { path: PathBuf },
    /// CLI override (`--plugin-dir`, `--mcp-server`).
    Cli { path: PathBuf },
    /// Managed (server-managed / IT-deployed).
    Managed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
}

impl ConfigSource {
    /// Compact label for columnar terminal display (no paths).
    pub fn display_label(&self) -> String {
        match self {
            Self::Builtin => "builtin".into(),
            Self::Bundled { .. } => "bundled".into(),
            Self::Server { .. } => "server".into(),
            Self::Project { .. } => "project".into(),
            Self::User { .. } => "user".into(),
            Self::Plugin { plugin_name, .. } => format!("plugin: {plugin_name}"),
            Self::ConfigToml { .. } => "config".into(),
            Self::ClaudeJson { .. } => "~/.claude.json".into(),
            Self::McpJson { .. } => ".mcp.json".into(),
            Self::Cli { .. } => "cli".into(),
            Self::Managed { .. } => "managed".into(),
        }
    }
}
