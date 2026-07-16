//! `/login` -- log in or re-authenticate with your account.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Log in to xAI or OpenAI Codex"
    }

    fn usage(&self) -> &str {
        "/login [codex]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("codex")
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(vec![ArgItem {
            display: "codex".to_string(),
            match_text: "codex openai chatgpt".to_string(),
            insert_text: "codex".to_string(),
            description: "Connect an OpenAI Codex account".to_string(),
        }])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim() {
            "" => CommandResult::Action(Action::Login),
            "codex" => CommandResult::Action(Action::LoginCodex),
            arg => CommandResult::Error(format!(
                "Unknown account: {arg}. Use /login or /login codex"
            )),
        }
    }
}
