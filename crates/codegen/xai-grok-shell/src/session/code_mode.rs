//! Shell adapter for the persistent in-process Code Mode runtime.
//!
//! The embedded runtime is `Send + Sync`, while [`SessionActor`] intentionally
//! runs on a `LocalSet`. Runtime callbacks therefore cross an unbounded channel
//! and are dispatched by the single local task started with
//! [`CodeModeRuntime::start_dispatch_loop`].

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xai_grok_code_mode::InProcessCodeModeSessionProvider;
use xai_grok_code_mode_protocol::{
    CellId, CodeModeNestedToolCall, CodeModeSession, CodeModeSessionDelegate,
    CodeModeSessionProvider, CodeModeToolKind, ExecuteRequest, FunctionCallOutputContentItem,
    ImageDetail, NotificationFuture, RuntimeResponse, ToolDefinition as CodeModeToolDefinition,
    ToolInvocationFuture, ToolName, WaitOutcome, WaitRequest,
};
use xai_grok_tools::types::definition::ToolDefinition as GrokToolDefinition;
use xai_grok_tools::util::{ceil_char_boundary, truncate_str};

use super::acp_session::SessionActor;
use crate::sampling::rs::{CustomGrammarFormatParam, CustomToolParamFormat, GrammarSyntax};
use crate::sampling::{ClientTool, ContentPart, ToolSpec};

const CODE_MODE_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Notify {
        call_id: String,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
}

/// A session-scoped, lazily initialized Code Mode host.
///
/// Construct this once per [`SessionActor`], call [`Self::start_dispatch_loop`]
/// after the actor has been wrapped in an [`Arc`], and retain it for the full
/// actor lifetime. Calls to [`Self::exec`] share one persistent JavaScript
/// session, including values written through `store()`.
pub(crate) struct CodeModeRuntime {
    session: OnceCell<Arc<dyn CodeModeSession>>,
    dispatch_tx: mpsc::UnboundedSender<DispatchMessage>,
    dispatch_rx: Mutex<Option<mpsc::UnboundedReceiver<DispatchMessage>>>,
}

impl CodeModeRuntime {
    pub(crate) fn new() -> Arc<Self> {
        let (dispatch_tx, dispatch_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            session: OnceCell::new(),
            dispatch_tx,
            dispatch_rx: Mutex::new(Some(dispatch_rx)),
        })
    }

    /// Starts the sole local callback dispatcher.
    ///
    /// The receiver can only be taken once. A second call returns an error
    /// rather than silently creating a competing consumer.
    pub(crate) async fn start_dispatch_loop(
        self: &Arc<Self>,
        session_actor: Weak<SessionActor>,
    ) -> Result<(), String> {
        let mut receiver = self
            .dispatch_rx
            .lock()
            .take()
            .ok_or_else(|| "code mode dispatch loop already started".to_string())?;

        tokio::task::spawn_local(async move {
            while let Some(message) = receiver.recv().await {
                spawn_dispatch_message(session_actor.clone(), message);
            }
        });
        Ok(())
    }

    /// Executes raw custom-tool input in the persistent JavaScript session.
    ///
    /// `raw_input` is parsed with the pinned Codex first-line pragma parser;
    /// callers must not JSON-wrap the JavaScript source.
    pub(crate) async fn exec(
        self: &Arc<Self>,
        call_id: &str,
        raw_input: &str,
        enabled_tools: &[GrokToolDefinition],
    ) -> Result<CodeModeToolOutput, String> {
        let parsed = xai_grok_code_mode_protocol::parse_exec_source(raw_input)?;
        let enabled_tools = collect_code_mode_tool_definitions(enabled_tools);
        let max_output_tokens = parsed.max_output_tokens;
        let started_at = Instant::now();
        let started_cell = self
            .session()
            .await?
            .execute(ExecuteRequest {
                tool_call_id: call_id.to_string(),
                enabled_tools,
                source: parsed.code,
                yield_time_ms: parsed.yield_time_ms,
                max_output_tokens,
            })
            .await?;
        let response = started_cell.initial_response().await?;
        Ok(format_runtime_response(
            response,
            max_output_tokens,
            started_at.elapsed(),
        ))
    }

    /// Waits for, or terminates, a yielded cell using the function tool's raw
    /// JSON arguments.
    pub(crate) async fn wait(
        self: &Arc<Self>,
        raw_arguments: &str,
    ) -> Result<CodeModeToolOutput, String> {
        let arguments = parse_wait_arguments(raw_arguments)?;
        let started_at = Instant::now();
        let session = self.session().await?;
        let cell_id = CellId::new(arguments.cell_id);
        let response: RuntimeResponse = if arguments.terminate {
            session.terminate(cell_id).await?
        } else {
            session
                .wait(WaitRequest {
                    cell_id,
                    yield_time_ms: arguments.yield_time_ms,
                })
                .await?
        }
        .into();
        Ok(format_runtime_response(
            response,
            arguments.max_tokens,
            started_at.elapsed(),
        ))
    }

    /// Shuts down an initialized runtime without creating an otherwise unused
    /// session.
    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        match self.session.get() {
            Some(session) => session.shutdown().await,
            None => Ok(()),
        }
    }

    async fn session(self: &Arc<Self>) -> Result<Arc<dyn CodeModeSession>, String> {
        self.session
            .get_or_try_init(|| {
                let delegate: Arc<dyn CodeModeSessionDelegate> = self.clone();
                async move {
                    InProcessCodeModeSessionProvider
                        .create_session(delegate)
                        .await
                }
            })
            .await
            .map(Arc::clone)
    }
}

impl CodeModeSessionDelegate for CodeModeRuntime {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        _cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn spawn_dispatch_message(session_actor: Weak<SessionActor>, message: DispatchMessage) {
    tokio::task::spawn_local(async move {
        let Some(session_actor) = session_actor.upgrade() else {
            match message {
                DispatchMessage::InvokeTool { response_tx, .. } => {
                    let _ = response_tx.send(Err(
                        "code mode nested tool dispatcher is unavailable".to_string(),
                    ));
                }
                DispatchMessage::Notify { response_tx, .. } => {
                    let _ = response_tx.send(Err(
                        "code mode notification dispatcher is unavailable".to_string(),
                    ));
                }
            }
            return;
        };

        match message {
            DispatchMessage::InvokeTool {
                invocation,
                cancellation_token,
                response_tx,
            } => {
                let response = tokio::select! {
                    response = session_actor.dispatch_code_mode_nested_tool(
                        invocation,
                        cancellation_token.clone(),
                    ) => response,
                    _ = cancellation_token.cancelled() => {
                        Err("code mode nested tool call cancelled".to_string())
                    }
                };
                let _ = response_tx.send(response);
            }
            DispatchMessage::Notify {
                call_id,
                text,
                cancellation_token,
                response_tx,
            } => {
                let response = tokio::select! {
                    _ = session_actor.record_code_mode_notification(call_id, text) => Ok(()),
                    _ = cancellation_token.cancelled() => {
                        Err("code mode notification cancelled".to_string())
                    }
                };
                let _ = response_tx.send(response);
            }
        }
    });
}

/// Model-facing result from `exec` or `wait`.
///
/// `content` retains the runtime's text/image ordering. `image_details` has
/// one entry for every image in `content`, in image order. The sidecar is
/// necessary because the shell sampling [`ContentPart::Image`] currently only
/// carries a URL; missing runtime details are resolved to Codex's `high`
/// default before being stored here.
#[derive(Clone, Debug)]
pub(crate) struct CodeModeToolOutput {
    pub(crate) content: Vec<ContentPart>,
    pub(crate) image_details: Vec<ImageDetail>,
    pub(crate) cell_id: Option<String>,
    pub(crate) success: bool,
}

impl CodeModeToolOutput {
    /// Concatenates text parts without inventing separators. The Codex status
    /// header already ends in a newline before the first body part.
    pub(crate) fn text(&self) -> String {
        let mut text = String::new();
        for part in &self.content {
            if let ContentPart::Text { text: part_text } = part {
                text.push_str(part_text);
            }
        }
        text
    }

    /// Returns the sampling image parts in their original relative order.
    pub(crate) fn images(&self) -> Vec<ContentPart> {
        self.content
            .iter()
            .filter(|part| matches!(part, ContentPart::Image { .. }))
            .cloned()
            .collect()
    }
}

/// Converts a Grok function definition into the embedded runtime protocol.
///
/// The JavaScript-visible name is normalized, while `tool_name` retains the
/// original registry key used by [`SessionActor::dispatch_code_mode_nested_tool`].
pub(crate) fn to_code_mode_tool_definition(
    definition: &GrokToolDefinition,
) -> CodeModeToolDefinition {
    let raw_name = definition.function.name.clone();
    CodeModeToolDefinition {
        name: xai_grok_code_mode_protocol::normalize_code_mode_identifier(&raw_name),
        tool_name: ToolName::plain(raw_name),
        description: definition.function.description.clone().unwrap_or_default(),
        kind: CodeModeToolKind::Function,
        input_schema: Some(definition.function.parameters.clone()),
        output_schema: None,
    }
}

pub(crate) fn collect_code_mode_tool_definitions(
    definitions: &[GrokToolDefinition],
) -> Vec<CodeModeToolDefinition> {
    let mut definitions = definitions
        .iter()
        .map(to_code_mode_tool_definition)
        .filter(|definition| {
            xai_grok_code_mode_protocol::is_code_mode_nested_tool(&definition.name)
        })
        .collect::<Vec<_>>();
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions.dedup_by(|left, right| left.name == right.name);
    definitions
}

/// Creates the native Responses custom `exec` declaration.
pub(crate) fn create_exec_tool(
    enabled_tools: &[GrokToolDefinition],
    code_mode_only: bool,
) -> ClientTool {
    let enabled_tools = collect_code_mode_tool_definitions(enabled_tools);
    ClientTool::Custom {
        name: xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME.to_string(),
        description: Some(xai_grok_code_mode_protocol::build_exec_tool_description(
            &enabled_tools,
            &[],
            &BTreeMap::new(),
            code_mode_only,
        )),
        format: CustomToolParamFormat::Grammar(CustomGrammarFormatParam {
            definition: CODE_MODE_FREEFORM_GRAMMAR.to_string(),
            syntax: GrammarSyntax::Lark,
        }),
    }
}

/// Creates the ordinary function `wait` declaration with the pinned Codex
/// schema and description.
pub(crate) fn create_wait_tool() -> ToolSpec {
    ToolSpec {
        name: xai_grok_code_mode_protocol::WAIT_TOOL_NAME.to_string(),
        description: Some(format!(
            "Waits on a yielded `{}` cell and returns new output or completion.\n{}",
            xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME,
            xai_grok_code_mode_protocol::build_wait_tool_description().trim()
        )),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "cell_id": {
                    "type": "string",
                    "description": "Identifier of the running exec cell."
                },
                "max_tokens": {
                    "type": "number",
                    "description": "Output token budget for this wait call. Defaults to 10000 tokens."
                },
                "terminate": {
                    "type": "boolean",
                    "description": "True stops the running exec cell; false or omitted waits for output."
                },
                "yield_time_ms": {
                    "type": "number",
                    "description": "Wait before yielding more output. Defaults to 10000 ms."
                }
            },
            "required": ["cell_id"],
            "additionalProperties": false
        }),
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct WaitArguments {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn default_wait_yield_time_ms() -> u64 {
    xai_grok_code_mode_protocol::DEFAULT_WAIT_YIELD_TIME_MS
}

fn parse_wait_arguments(arguments: &str) -> Result<WaitArguments, String> {
    serde_json::from_str(arguments)
        .map_err(|error| format!("failed to parse function arguments: {error}"))
}

fn format_runtime_response(
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    wall_time: Duration,
) -> CodeModeToolOutput {
    let (status, cell_id, mut items, success) = match response {
        RuntimeResponse::Yielded {
            cell_id,
            content_items,
        } => (
            format!("Script running with cell ID {cell_id}"),
            cell_id,
            content_items,
            true,
        ),
        RuntimeResponse::Terminated {
            cell_id,
            content_items,
        } => (
            "Script terminated".to_string(),
            cell_id,
            content_items,
            true,
        ),
        RuntimeResponse::Result {
            cell_id,
            content_items,
            error_text,
        } => {
            let success = error_text.is_none();
            let status = if success {
                "Script completed"
            } else {
                "Script failed"
            };
            let mut content_items = content_items;
            if let Some(error_text) = error_text {
                content_items.push(FunctionCallOutputContentItem::InputText {
                    text: format!("Script error:\n{error_text}"),
                });
            }
            (status.to_string(), cell_id, content_items, success)
        }
    };

    items = truncate_code_mode_result(items, resolve_max_tokens(max_output_tokens));
    let wall_time_seconds = (wall_time.as_secs_f32() * 10.0).round() / 10.0;
    items.insert(
        0,
        FunctionCallOutputContentItem::InputText {
            text: format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n"),
        },
    );

    let mut content = Vec::with_capacity(items.len());
    let mut image_details = Vec::new();
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                content.push(ContentPart::Text { text: text.into() });
            }
            FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                content.push(ContentPart::Image {
                    url: image_url.into(),
                });
                image_details
                    .push(detail.unwrap_or(xai_grok_code_mode_protocol::DEFAULT_IMAGE_DETAIL));
            }
        }
    }

    CodeModeToolOutput {
        content,
        image_details,
        cell_id: Some(cell_id.to_string()),
        success,
    }
}

fn resolve_max_tokens(max_output_tokens: Option<usize>) -> usize {
    max_output_tokens
        .unwrap_or(xai_grok_code_mode_protocol::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
}

fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_tokens: usize,
) -> Vec<FunctionCallOutputContentItem> {
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        return formatted_truncate_text_items(items, max_tokens);
    }

    let mut output = Vec::with_capacity(items.len());
    let mut remaining_tokens = max_tokens;
    let mut omitted_text_items = 0usize;
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                if remaining_tokens == 0 {
                    omitted_text_items += 1;
                    continue;
                }
                let cost = approximate_token_count(&text);
                if cost <= remaining_tokens {
                    output.push(FunctionCallOutputContentItem::InputText { text });
                    remaining_tokens = remaining_tokens.saturating_sub(cost);
                } else {
                    let text = truncate_middle_with_token_budget(&text, remaining_tokens);
                    if text.is_empty() {
                        omitted_text_items += 1;
                    } else {
                        output.push(FunctionCallOutputContentItem::InputText { text });
                    }
                    remaining_tokens = 0;
                }
            }
            image @ FunctionCallOutputContentItem::InputImage { .. } => output.push(image),
        }
    }
    if omitted_text_items > 0 {
        output.push(FunctionCallOutputContentItem::InputText {
            text: format!("[omitted {omitted_text_items} text items ...]"),
        });
    }
    output
}

fn formatted_truncate_text_items(
    items: Vec<FunctionCallOutputContentItem>,
    max_tokens: usize,
) -> Vec<FunctionCallOutputContentItem> {
    let text_segments = items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            FunctionCallOutputContentItem::InputImage { .. } => None,
        })
        .collect::<Vec<_>>();
    if text_segments.is_empty() {
        return items;
    }
    let combined = text_segments.join("\n");
    if combined.len() <= approximate_bytes_for_tokens(max_tokens) {
        return items;
    }

    let original_token_count = approximate_token_count(&combined);
    let total_lines = combined.lines().count();
    let truncated = truncate_middle_with_token_budget(&combined, max_tokens);
    vec![FunctionCallOutputContentItem::InputText {
        text: format!(
            "Warning: truncated output (original token count: {original_token_count})\n\
             Total output lines: {total_lines}\n\n{truncated}"
        ),
    }]
}

fn approximate_token_count(text: &str) -> usize {
    approximate_tokens_from_byte_count(text.len())
}

fn approximate_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(4)
}

fn approximate_tokens_from_byte_count(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

fn truncate_middle_with_token_budget(text: &str, max_tokens: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let max_bytes = approximate_bytes_for_tokens(max_tokens);
    if max_tokens > 0 && text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes == 0 {
        return format!("…{} tokens truncated…", approximate_token_count(text));
    }

    let left_budget = max_bytes / 2;
    let right_budget = max_bytes - left_budget;
    let prefix = truncate_str(text, left_budget);
    let suffix_target = text.len().saturating_sub(right_budget);
    let suffix_start = ceil_char_boundary(text, suffix_target).max(prefix.len());
    let removed_tokens = approximate_tokens_from_byte_count(text.len().saturating_sub(max_bytes));
    format!(
        "{prefix}…{removed_tokens} tokens truncated…{}",
        &text[suffix_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_tool_uses_pinned_lark_grammar_and_description() {
        let tools = vec![GrokToolDefinition::function(
            "update-plan",
            Some("Update the plan"),
            json!({"type": "object"}),
        )];
        let tool = create_exec_tool(&tools, true);
        let ClientTool::Custom {
            name,
            description,
            format,
        } = tool
        else {
            panic!("exec must be a native custom tool");
        };

        assert_eq!(name, xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME);
        assert_eq!(
            description,
            Some(xai_grok_code_mode_protocol::build_exec_tool_description(
                &collect_code_mode_tool_definitions(&tools),
                &[],
                &BTreeMap::new(),
                true,
            ))
        );
        assert_eq!(
            format,
            CustomToolParamFormat::Grammar(CustomGrammarFormatParam {
                definition: CODE_MODE_FREEFORM_GRAMMAR.to_string(),
                syntax: GrammarSyntax::Lark,
            })
        );
    }

    #[test]
    fn wait_tool_matches_pinned_schema() {
        let tool = create_wait_tool();
        assert_eq!(tool.name, xai_grok_code_mode_protocol::WAIT_TOOL_NAME);
        assert_eq!(
            tool.description,
            Some(format!(
                "Waits on a yielded `exec` cell and returns new output or completion.\n{}",
                xai_grok_code_mode_protocol::build_wait_tool_description().trim()
            ))
        );
        assert_eq!(
            tool.parameters,
            json!({
                "type": "object",
                "properties": {
                    "cell_id": {
                        "type": "string",
                        "description": "Identifier of the running exec cell."
                    },
                    "max_tokens": {
                        "type": "number",
                        "description": "Output token budget for this wait call. Defaults to 10000 tokens."
                    },
                    "terminate": {
                        "type": "boolean",
                        "description": "True stops the running exec cell; false or omitted waits for output."
                    },
                    "yield_time_ms": {
                        "type": "number",
                        "description": "Wait before yielding more output. Defaults to 10000 ms."
                    }
                },
                "required": ["cell_id"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn grok_tool_conversion_normalizes_only_javascript_name() {
        let tool = GrokToolDefinition::function(
            "mcp/server.tool",
            Some("Call it"),
            json!({"type": "object", "properties": {"value": {"type": "string"}}}),
        );
        let converted = to_code_mode_tool_definition(&tool);
        assert_eq!(converted.name, "mcp_server_tool");
        assert_eq!(converted.tool_name, ToolName::plain("mcp/server.tool"));
        assert_eq!(converted.kind, CodeModeToolKind::Function);
        assert_eq!(converted.description, "Call it");
        assert_eq!(converted.input_schema, Some(tool.function.parameters));
    }

    #[test]
    fn wait_arguments_apply_codex_defaults() {
        assert_eq!(
            parse_wait_arguments(r#"{"cell_id":"7"}"#).unwrap(),
            WaitArguments {
                cell_id: "7".to_string(),
                yield_time_ms: 10_000,
                max_tokens: None,
                terminate: false,
            }
        );
        assert_eq!(
            resolve_max_tokens(
                parse_wait_arguments(r#"{"cell_id":"7"}"#)
                    .unwrap()
                    .max_tokens
            ),
            10_000
        );
        assert!(parse_wait_arguments(r#"{"yield_time_ms":1}"#).is_err());
    }

    #[test]
    fn yielded_response_has_exact_status_and_preserves_image_detail() {
        let output = format_runtime_response(
            RuntimeResponse::Yielded {
                cell_id: CellId::new("12".to_string()),
                content_items: vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "hello".to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AA==".to_string(),
                        detail: Some(ImageDetail::Original),
                    },
                ],
            },
            None,
            Duration::from_millis(149),
        );

        assert_eq!(
            output.text(),
            "Script running with cell ID 12\nWall time 0.1 seconds\nOutput:\nhello"
        );
        assert_eq!(output.cell_id.as_deref(), Some("12"));
        assert!(output.success);
        assert_eq!(output.image_details, vec![ImageDetail::Original]);
        assert!(matches!(
            output.images().as_slice(),
            [ContentPart::Image { url }] if url.as_ref() == "data:image/png;base64,AA=="
        ));
    }

    #[test]
    fn failed_response_uses_exact_status_and_error_body() {
        let output = format_runtime_response(
            RuntimeResponse::Result {
                cell_id: CellId::new("3".to_string()),
                content_items: Vec::new(),
                error_text: Some("boom".to_string()),
            },
            None,
            Duration::ZERO,
        );
        assert_eq!(
            output.text(),
            "Script failed\nWall time 0.0 seconds\nOutput:\nScript error:\nboom"
        );
        assert!(!output.success);
    }

    #[test]
    fn text_truncation_matches_codex_warning_and_is_utf8_safe() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "0123456789012345678901234567890123456789".to_string(),
        }];
        assert_eq!(
            truncate_code_mode_result(items, 5),
            vec![FunctionCallOutputContentItem::InputText {
                text: concat!(
                    "Warning: truncated output (original token count: 10)\n",
                    "Total output lines: 1\n\n",
                    "0123456789…5 tokens truncated…0123456789"
                )
                .to_string(),
            }]
        );

        // 15 UTF-8 bytes with a 4-byte budget makes the removed-byte count 11,
        // deliberately landing inside a three-byte character.
        let unicode = truncate_middle_with_token_budget("日本語日本", 1);
        assert!(unicode.contains("tokens truncated"));
        assert!(std::str::from_utf8(unicode.as_bytes()).is_ok());
    }
}
