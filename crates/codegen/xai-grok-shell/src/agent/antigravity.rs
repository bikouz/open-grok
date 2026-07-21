//! Antigravity CLI (`agy`) subagent integration.
//!
//! Antigravity is an external agentic CLI with its own models, login, and tool
//! loop. It cannot be wired in as a regular HTTP model provider, so instead we
//! run it as an out-of-process subagent runner: the task/agent_swarm/workflow
//! tools may target `antigravity:<model>` slugs, and the subagent coordinator
//! shells out to `agy --print` instead of spawning an in-process child session
//! (see `agent::subagent::antigravity_runner`).
//!
//! Empirically verified CLI contract (agy 1.1.5):
//! - `agy models` prints one model id per line when signed in; when signed
//!   out it prints `Error: Please sign in ...` — **with exit code 0**, so
//!   output text, not the exit status, is the source of truth.
//! - `agy --print <prompt>` runs one headless prompt and prints only the
//!   final response on stdout. Errors are printed as `Error: ...` lines or a
//!   `jetski: no output produced ...` diagnostic, again with exit code 0.
//! - `--model` accepts either effort-suffixed ids (`gemini-3.6-flash-high`)
//!   or base ids plus `--effort low|medium|high`; models without effort
//!   variants (e.g. `claude-sonnet-4-6`) reject `--effort`.
//! - `--log-file` captures a log containing
//!   `Print mode: conversation=<uuid>`; that uuid can be fed back through
//!   `--conversation <uuid>` to continue the conversation in a later run.
//! - Headless runs auto-deny permissioned tools (file writes, commands)
//!   unless `--dangerously-skip-permissions` is passed; read-only tools work
//!   without it. `--add-dir` grants the workspace directory.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// Namespace prefix that marks a task/swarm/workflow `model` argument as an
/// Antigravity model (e.g. `antigravity:gemini-3.1-pro`).
pub const MODEL_PREFIX: &str = "antigravity:";
/// Default binary name; `[antigravity].binary` overrides.
pub const DEFAULT_BINARY: &str = "agy";
/// `agy models` probe timeout. The command answers in ~1s when healthy.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Cache TTL for a signed-in probe result.
const STATUS_TTL_OK: Duration = Duration::from_secs(300);
/// Cache TTL for a signed-out/failed probe, kept short so a fresh `agy`
/// login is picked up without restarting.
const STATUS_TTL_ERR: Duration = Duration::from_secs(30);
/// Defensive cap on the prompt we pass via argv (macOS ARG_MAX is ~1MB).
const MAX_PROMPT_BYTES: usize = 512 * 1024;
/// Grace period past `--print-timeout` before we hard-kill the process.
const KILL_GRACE: Duration = Duration::from_secs(30);

/// `Some(model)` when `slug` is `antigravity:<model>` with a non-empty model.
pub fn strip_model_prefix(slug: &str) -> Option<&str> {
    slug.strip_prefix(MODEL_PREFIX)
        .map(str::trim)
        .filter(|m| !m.is_empty())
}

/// Whether a task/swarm/workflow model slug targets Antigravity.
pub fn is_antigravity_slug(slug: &str) -> bool {
    strip_model_prefix(slug).is_some()
}

/// Effective binary name/path from `[antigravity].binary` (default `agy`).
pub fn binary_name(config: &crate::agent::config::AntigravityConfig) -> String {
    config
        .binary
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(DEFAULT_BINARY)
        .to_string()
}

/// Whether the Antigravity CLI is installed (PATH lookup, or direct file
/// check for an explicit path override).
pub fn cli_installed(config: &crate::agent::config::AntigravityConfig) -> bool {
    xai_grok_config::shell::is_command_available(&binary_name(config))
}

/// Whether the user has switched the feature on (`[ui].antigravity_subagents`).
pub fn ui_enabled(ui: &crate::agent::config::UiConfig) -> bool {
    ui.antigravity_subagents.unwrap_or(false)
}

/// Result of probing `agy models`: the signed-in state and model roster.
#[derive(Debug, Clone)]
pub struct AntigravityStatus {
    pub signed_in: bool,
    /// Native model ids as reported by `agy models` (no `antigravity:` prefix).
    pub models: Vec<String>,
    /// Human-readable reason when the CLI is unusable (sign-in prompt, probe
    /// failure). `None` when `signed_in` with a non-empty roster.
    pub detail: Option<String>,
}

impl AntigravityStatus {
    fn unavailable(detail: String) -> Self {
        Self {
            signed_in: false,
            models: Vec::new(),
            detail: Some(detail),
        }
    }

    /// Roster as task-tool slugs (`antigravity:<model>`).
    pub fn prefixed_models(&self) -> Vec<String> {
        self.models
            .iter()
            .map(|m| format!("{MODEL_PREFIX}{m}"))
            .collect()
    }
}

/// Parse `agy models` output. Exit status is deliberately ignored: agy
/// reports sign-in errors on stdout with exit code 0.
fn parse_models_output(stdout: &str, stderr: &str) -> AntigravityStatus {
    let combined_error_line = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|l| l.starts_with("Error:") || l.to_ascii_lowercase().contains("sign in"));
    if let Some(line) = combined_error_line {
        return AntigravityStatus::unavailable(line.to_string());
    }
    let models: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' ') && !l.starts_with('-'))
        .map(str::to_string)
        .collect();
    if models.is_empty() {
        return AntigravityStatus {
            signed_in: true,
            models,
            detail: Some("`agy models` reported no models".to_string()),
        };
    }
    AntigravityStatus {
        signed_in: true,
        models,
        detail: None,
    }
}

/// Run `agy models` and classify the outcome. Never panics on CLI weirdness;
/// worst case is an `unavailable` status with a diagnostic string.
pub async fn probe_models(binary: &str) -> AntigravityStatus {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("models")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return AntigravityStatus::unavailable(format!("failed to run `{binary} models`: {e}"));
        }
        Err(_) => {
            return AntigravityStatus::unavailable(format!(
                "`{binary} models` timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            ));
        }
    };
    parse_models_output(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

struct StatusCacheEntry {
    binary: String,
    fetched_at: Instant,
    status: AntigravityStatus,
}

static STATUS_CACHE: Mutex<Option<StatusCacheEntry>> = Mutex::new(None);

/// `probe_models` behind a process-wide TTL cache (5m signed-in, 30s
/// otherwise so a fresh login is noticed quickly).
pub async fn cached_status(binary: &str) -> AntigravityStatus {
    if let Some(entry) = STATUS_CACHE.lock().unwrap().as_ref() {
        let ttl = if entry.status.signed_in {
            STATUS_TTL_OK
        } else {
            STATUS_TTL_ERR
        };
        if entry.binary == binary && entry.fetched_at.elapsed() < ttl {
            return entry.status.clone();
        }
    }
    let status = probe_models(binary).await;
    *STATUS_CACHE.lock().unwrap() = Some(StatusCacheEntry {
        binary: binary.to_string(),
        fetched_at: Instant::now(),
        status: status.clone(),
    });
    status
}

/// Drop the cached probe (tests / explicit refresh).
pub fn invalidate_status_cache() {
    *STATUS_CACHE.lock().unwrap() = None;
    *FEATURE_CACHE.lock().unwrap() = None;
}

/// Snapshot of the feature gates: `[ui].antigravity_subagents`, binary
/// presence, and the `[antigravity]` operator knobs.
///
/// Backed by a short-TTL disk read of the user config rather than a field
/// threaded through the session spawn plumbing: the toggle, binary, and agy
/// login are all process-global, so every consumer (agent rebuild, task
/// validators, the subagent coordinator) reads one consistent source.
#[derive(Debug, Clone)]
pub struct FeatureState {
    /// `[ui].antigravity_subagents` is on.
    pub enabled: bool,
    /// The CLI binary resolves.
    pub installed: bool,
    pub config: crate::agent::config::AntigravityConfig,
}

impl FeatureState {
    pub fn active(&self) -> bool {
        self.enabled && self.installed
    }
}

static FEATURE_CACHE: Mutex<Option<(Instant, FeatureState)>> = Mutex::new(None);
const FEATURE_TTL: Duration = Duration::from_secs(30);

/// Current feature state (config disk read behind a 30s cache).
pub async fn feature_state() -> FeatureState {
    if let Some((at, state)) = FEATURE_CACHE.lock().unwrap().clone()
        && at.elapsed() < FEATURE_TTL
    {
        return state;
    }
    let cfg = crate::util::config::load_config().await;
    let state = FeatureState {
        enabled: ui_enabled(&cfg.ui),
        installed: cli_installed(&cfg.antigravity),
        config: cfg.antigravity.clone(),
    };
    *FEATURE_CACHE.lock().unwrap() = Some((Instant::now(), state.clone()));
    state
}

/// Cache-only read of [`feature_state`]; spawns a background refresh when the
/// cache is cold or stale. For sync contexts (validator closures, builders)
/// that must not block — callers treat `None` as "unknown, be permissive"
/// and rely on the coordinator's authoritative async gate.
pub fn feature_state_nonblocking() -> Option<FeatureState> {
    let cached = FEATURE_CACHE.lock().unwrap().clone();
    match cached {
        Some((at, state)) if at.elapsed() < FEATURE_TTL => Some(state),
        stale => {
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(async {
                    let _ = feature_state().await;
                });
            }
            stale.map(|(_, state)| state)
        }
    }
}

/// Cache-only read of the model-roster probe; spawns a refresh when missing
/// or stale (mirrors [`feature_state_nonblocking`]).
pub fn status_nonblocking(binary: &str) -> Option<AntigravityStatus> {
    let cached = STATUS_CACHE.lock().unwrap().as_ref().and_then(|entry| {
        let ttl = if entry.status.signed_in {
            STATUS_TTL_OK
        } else {
            STATUS_TTL_ERR
        };
        (entry.binary == binary).then(|| (entry.fetched_at.elapsed() < ttl, entry.status.clone()))
    });
    match cached {
        Some((fresh, status)) => {
            if !fresh && tokio::runtime::Handle::try_current().is_ok() {
                let binary = binary.to_string();
                tokio::spawn(async move {
                    let _ = cached_status(&binary).await;
                });
            }
            Some(status)
        }
        None => {
            if tokio::runtime::Handle::try_current().is_ok() {
                let binary = binary.to_string();
                tokio::spawn(async move {
                    let _ = cached_status(&binary).await;
                });
            }
            None
        }
    }
}

/// `antigravity:<model>` slugs to advertise in the task tool's model
/// guidance. Empty while the feature is off, the CLI is signed out, or the
/// probes haven't landed yet (spawn-side validation stays authoritative, so
/// a lagging advertisement never blocks a valid spawn).
pub fn advertised_slugs_nonblocking() -> Vec<String> {
    let Some(state) = feature_state_nonblocking() else {
        return Vec::new();
    };
    if !state.active() {
        return Vec::new();
    }
    status_nonblocking(&binary_name(&state.config))
        .filter(|status| status.signed_in)
        .map(|status| status.prefixed_models())
        .unwrap_or_default()
}

/// Sync validator for a model-facing `antigravity:*` task slug. `None` means
/// allowed (including "gates not probed yet" — the coordinator re-checks
/// authoritatively at spawn).
pub fn task_slug_error_nonblocking(slug: &str) -> Option<String> {
    let model = strip_model_prefix(slug)?;
    let state = feature_state_nonblocking()?;
    if !state.enabled {
        return Some(
            "Antigravity subagents are disabled. Enable the \"Antigravity subagents\" \
             setting (`[ui].antigravity_subagents`) to use antigravity:* models."
                .to_string(),
        );
    }
    if !state.installed {
        return Some(format!(
            "Antigravity CLI (`{}`) was not found on this system, so antigravity:* \
             models are unavailable.",
            binary_name(&state.config)
        ));
    }
    let status = status_nonblocking(&binary_name(&state.config))?;
    if !status.signed_in {
        return Some(format!(
            "Antigravity CLI is not signed in ({}). Run `agy` once in a terminal to \
             sign in, then retry.",
            status.detail.as_deref().unwrap_or("sign-in required")
        ));
    }
    if !status.models.is_empty() && !status.models.iter().any(|m| m == model) {
        return Some(format!(
            "Unknown antigravity model \"{model}\". Available: {}",
            status
                .models
                .iter()
                .map(|m| format!("{MODEL_PREFIX}{m}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    None
}

/// Clamp a subagent `reasoning_effort` string onto agy's `low|medium|high`.
/// Unknown values map to `None` (omit the flag).
pub fn normalize_effort(effort: &str) -> Option<&'static str> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

const EFFORT_SUFFIXES: [&str; 3] = ["-low", "-medium", "-high"];

/// Decide the `--model` / `--effort` pair for a run.
///
/// - Effort-suffixed ids (`gemini-3.6-flash-high`): swap the suffix to the
///   requested effort when that sibling exists in the roster; never pass
///   `--effort` (agy rejects it for concrete variants).
/// - Plain ids: pass `--effort` and rely on the caller's retry-without-effort
///   fallback for models that don't support it (`claude-*`).
pub fn plan_model_args(
    model: &str,
    effort: Option<&str>,
    roster: &[String],
) -> (String, Option<String>) {
    let normalized = effort.and_then(normalize_effort);
    if let Some(base) = EFFORT_SUFFIXES.iter().find_map(|s| model.strip_suffix(s)) {
        if let Some(eff) = normalized {
            let candidate = format!("{base}-{eff}");
            if candidate != model && roster.iter().any(|m| m == &candidate) {
                return (candidate, None);
            }
        }
        return (model.to_string(), None);
    }
    (model.to_string(), normalized.map(str::to_string))
}

/// One headless `agy --print` invocation.
#[derive(Debug, Clone)]
pub struct AgyRun {
    /// Binary name or path (see [`binary_name`]).
    pub binary: String,
    /// Native agy model id (already stripped of `antigravity:`).
    pub model: String,
    /// Requested reasoning effort (raw subagent string; mapped internally).
    pub effort: Option<String>,
    pub prompt: String,
    /// Working directory for the process; also granted via `--add-dir`.
    pub workspace_dir: PathBuf,
    /// Where agy writes its log (conversation id is recovered from here).
    pub log_file: PathBuf,
    /// Passed as `--print-timeout`; the process is hard-killed 30s later.
    pub timeout: Duration,
    /// Pass agy's auto-approve flag (`[antigravity].skip_permissions`).
    pub skip_permissions: bool,
    /// Continue a previous conversation (`--conversation <uuid>`).
    pub conversation_id: Option<String>,
}

/// Successful run: agy's stdout plus the conversation id for later resumes.
#[derive(Debug)]
pub struct AgyOutcome {
    pub output: String,
    pub conversation_id: Option<String>,
}

#[derive(Debug)]
pub enum AgyRunError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for AgyRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "Antigravity subagent was cancelled"),
            Self::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

/// Classify a finished `agy --print` process. agy exits 0 even on errors, so
/// stdout/stderr text is inspected first. Only the FIRST non-empty stdout
/// line is checked for the `Error:` prefix — agy prints its own errors as
/// the sole output, and a subagent's legitimate report may well contain
/// "Error:" lines further down. The `jetski: no output produced` diagnostic
/// (which replaces output entirely) is matched anywhere.
fn classify_output(stdout: &str, stderr: &str, exit_ok: bool) -> Result<String, String> {
    let trimmed = stdout.trim();
    let first_line_error = trimmed
        .lines()
        .map(str::trim)
        .next()
        .filter(|l| l.starts_with("Error:"));
    let jetski_line = trimmed
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("jetski: no output produced"));
    if let Some(line) = first_line_error.or(jetski_line) {
        let mut msg = line.to_string();
        if line.to_ascii_lowercase().contains("sign in") {
            msg.push_str(" (run `agy` once in a terminal to sign in)");
        }
        return Err(msg);
    }
    if trimmed.is_empty()
        && let Some(line) = stderr
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("Error:") || l.starts_with("jetski: no output produced"))
    {
        return Err(line.to_string());
    }
    if !exit_ok {
        let tail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            trimmed
        };
        let mut tail = tail.chars().rev().take(500).collect::<Vec<_>>();
        tail.reverse();
        return Err(format!(
            "Antigravity CLI exited with an error: {}",
            tail.into_iter().collect::<String>()
        ));
    }
    if trimmed.is_empty() {
        return Err("Antigravity CLI produced no output".to_string());
    }
    Ok(trimmed.to_string())
}

/// Recover the conversation uuid from an agy `--log-file`.
fn extract_conversation_id(log: &str) -> Option<String> {
    for marker in ["Print mode: conversation=", "Created conversation "] {
        for line in log.lines() {
            if let Some(idx) = line.find(marker) {
                let rest = &line[idx + marker.len()..];
                let id: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
                    .collect();
                // UUIDs are 36 chars; require enough shape to avoid grabbing
                // stray tokens from unrelated log lines.
                if id.len() >= 32 {
                    return Some(id);
                }
            }
        }
    }
    None
}

fn build_command(run: &AgyRun, model: &str, effort: Option<&str>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&run.binary);
    cmd.arg("--print")
        .arg(&run.prompt)
        .arg("--model")
        .arg(model)
        .arg("--add-dir")
        .arg(&run.workspace_dir)
        .arg("--log-file")
        .arg(&run.log_file)
        .arg("--print-timeout")
        .arg(format!("{}s", run.timeout.as_secs().max(30)));
    if let Some(eff) = effort {
        cmd.arg("--effort").arg(eff);
    }
    if let Some(ref conversation) = run.conversation_id {
        cmd.arg("--conversation").arg(conversation);
    }
    if run.skip_permissions {
        cmd.arg("--dangerously-skip-permissions");
    }
    cmd.current_dir(&run.workspace_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn run_once(
    run: &AgyRun,
    model: &str,
    effort: Option<&str>,
    cancel: &CancellationToken,
) -> Result<String, AgyRunError> {
    let mut child = build_command(run, model, effort)
        .spawn()
        .map_err(|e| AgyRunError::Failed(format!("failed to launch `{}`: {e}", run.binary)))?;
    // Both pipes are drained CONCURRENTLY: reading them one after the other
    // can deadlock if the process fills the un-read pipe's buffer.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let hard_deadline = run.timeout + KILL_GRACE;
    let wait = async {
        use tokio::io::AsyncReadExt as _;
        let drain_out = async {
            let mut out = String::new();
            if let Some(mut pipe) = stdout {
                let _ = pipe.read_to_string(&mut out).await;
            }
            out
        };
        let drain_err = async {
            let mut err = String::new();
            if let Some(mut pipe) = stderr {
                let _ = pipe.read_to_string(&mut err).await;
            }
            err
        };
        let (out, err) = tokio::join!(drain_out, drain_err);
        let status = child.wait().await;
        (out, err, status)
    };
    tokio::select! {
        (out, err, status) = wait => {
            let exit_ok = status.map(|s| s.success()).unwrap_or(false);
            classify_output(&out, &err, exit_ok).map_err(AgyRunError::Failed)
        }
        _ = cancel.cancelled() => Err(AgyRunError::Cancelled),
        _ = tokio::time::sleep(hard_deadline) => Err(AgyRunError::Failed(format!(
            "Antigravity run exceeded {}s and was terminated",
            hard_deadline.as_secs()
        ))),
    }
}

/// Execute one headless run. Maps effort onto the model (suffix swap or
/// `--effort`), retrying once without `--effort` when agy rejects it for the
/// model. On success the conversation id is read back from the log file.
pub async fn run_print(
    run: &AgyRun,
    roster: &[String],
    cancel: &CancellationToken,
) -> Result<AgyOutcome, AgyRunError> {
    if run.prompt.len() > MAX_PROMPT_BYTES {
        return Err(AgyRunError::Failed(format!(
            "prompt too large for the Antigravity CLI ({} bytes > {MAX_PROMPT_BYTES})",
            run.prompt.len()
        )));
    }
    let (model, effort) = plan_model_args(&run.model, run.effort.as_deref(), roster);
    let first = run_once(run, &model, effort.as_deref(), cancel).await;
    let output = match first {
        Ok(output) => output,
        Err(AgyRunError::Failed(msg))
            if effort.is_some() && msg.contains("--effort is not supported") =>
        {
            run_once(run, &model, None, cancel).await?
        }
        Err(e) => return Err(e),
    };
    let conversation_id = std::fs::read_to_string(&run.log_file)
        .ok()
        .as_deref()
        .and_then(extract_conversation_id)
        .or_else(|| run.conversation_id.clone());
    Ok(AgyOutcome {
        output,
        conversation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_prefix_strips_and_rejects_empty() {
        assert_eq!(
            strip_model_prefix("antigravity:gemini-3.1-pro"),
            Some("gemini-3.1-pro")
        );
        assert_eq!(strip_model_prefix("antigravity:"), None);
        assert_eq!(strip_model_prefix("grok-4"), None);
        assert!(is_antigravity_slug("antigravity:claude-sonnet-4-6"));
        assert!(!is_antigravity_slug("gemini-3.1-pro"));
    }

    #[test]
    fn parse_models_signed_in_roster() {
        let status = parse_models_output(
            "gemini-3.6-flash-high\ngemini-3.1-pro-low\nclaude-sonnet-4-6\n",
            "",
        );
        assert!(status.signed_in);
        assert_eq!(status.models.len(), 3);
        assert_eq!(
            status.prefixed_models()[0],
            "antigravity:gemini-3.6-flash-high"
        );
        assert_eq!(status.detail, None);
    }

    #[test]
    fn parse_models_signed_out_error() {
        let status = parse_models_output(
            "Error: Please sign in to view available models. Launch the CLI without arguments to sign in.\n",
            "",
        );
        assert!(!status.signed_in);
        assert!(status.models.is_empty());
        assert!(status.detail.unwrap().contains("sign in"));
    }

    #[test]
    fn classify_output_paths() {
        assert_eq!(classify_output("PONG\n", "", true).unwrap(), "PONG");
        let denied = classify_output(
            "jetski: no output produced — a tool required the \"command\" permission that headless mode cannot prompt for, so it was auto-denied.",
            "",
            true,
        )
        .unwrap_err();
        assert!(denied.contains("jetski"));
        let sign_in = classify_output("Error: Please sign in to continue", "", true).unwrap_err();
        assert!(sign_in.contains("run `agy`"));
        assert!(
            classify_output("", "", true)
                .unwrap_err()
                .contains("no output")
        );
        assert!(
            classify_output("", "boom", false)
                .unwrap_err()
                .contains("boom")
        );
    }

    #[test]
    fn conversation_id_extraction() {
        let log = "I0721 printmode.go:216] Print mode: conversation=3605d5fa-fbdd-442c-83f4-90325fbc9186, sending message";
        assert_eq!(
            extract_conversation_id(log).as_deref(),
            Some("3605d5fa-fbdd-442c-83f4-90325fbc9186")
        );
        let created =
            "I0721 server.go:917] Created conversation 3605d5fa-fbdd-442c-83f4-90325fbc9186";
        assert_eq!(
            extract_conversation_id(created).as_deref(),
            Some("3605d5fa-fbdd-442c-83f4-90325fbc9186")
        );
        assert_eq!(extract_conversation_id("no ids here"), None);
    }

    #[test]
    fn effort_mapping_rules() {
        let roster = vec![
            "gemini-3.6-flash-high".to_string(),
            "gemini-3.6-flash-low".to_string(),
            "claude-sonnet-4-6".to_string(),
        ];
        // Suffixed id + effort with a roster sibling: swap, no flag.
        assert_eq!(
            plan_model_args("gemini-3.6-flash-high", Some("low"), &roster),
            ("gemini-3.6-flash-low".to_string(), None)
        );
        // Suffixed id, sibling missing from roster: keep as-is.
        assert_eq!(
            plan_model_args("gemini-3.6-flash-high", Some("medium"), &roster),
            ("gemini-3.6-flash-high".to_string(), None)
        );
        // Plain id: pass --effort (clamped).
        assert_eq!(
            plan_model_args("gemini-3.1-pro", Some("xhigh"), &roster),
            ("gemini-3.1-pro".to_string(), Some("high".to_string()))
        );
        // No effort requested: pass through untouched.
        assert_eq!(
            plan_model_args("claude-sonnet-4-6", None, &roster),
            ("claude-sonnet-4-6".to_string(), None)
        );
        assert_eq!(normalize_effort("minimal"), Some("low"));
        assert_eq!(normalize_effort("bogus"), None);
    }
}
