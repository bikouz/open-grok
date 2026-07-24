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
/// Antigravity model (e.g. `antigravity:gemini-3.6-flash`).
pub const MODEL_PREFIX: &str = "antigravity:";
/// The blessed reference / default Antigravity model. It is a base id (so it
/// accepts `--effort low|medium|high`), is advertised first in the roster, and
/// is suggested when a requested model is unavailable.
pub const REFERENCE_MODEL: &str = "gemini-3.6-flash";
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

    /// Roster as task-tool slugs (`antigravity:<model>`), with the reference
    /// model surfaced first (then reference effort-variants, then the rest in
    /// probe order) so guidance leads with the blessed default.
    pub fn prefixed_models(&self) -> Vec<String> {
        let mut ordered: Vec<&String> = self.models.iter().collect();
        ordered.sort_by_key(|m| reference_rank(m.as_str()));
        ordered
            .into_iter()
            .map(|m| format!("{MODEL_PREFIX}{m}"))
            .collect()
    }
}

/// Ordering key that floats the reference model to the front: the exact
/// reference id first (0), its effort-variants next (1), everything else last
/// (2). Stable within each rank, preserving the CLI's probe order.
fn reference_rank(model: &str) -> u8 {
    if model == REFERENCE_MODEL {
        0
    } else if base_model_id(model) == REFERENCE_MODEL {
        1
    } else {
        2
    }
}

/// Strip a trailing effort suffix (`-low|-medium|-high`) to recover the base
/// model id; returns the input unchanged when there is no such suffix.
pub fn base_model_id(model: &str) -> &str {
    EFFORT_SUFFIXES
        .iter()
        .find_map(|s| model.strip_suffix(s))
        .unwrap_or(model)
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
    *QUOTA_CACHE.lock().unwrap() = None;
    *MODELS_CACHE.lock().unwrap() = None;
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

/// Clamp a fork `ReasoningEffort` string onto agy's `low|medium|high`. The
/// fork's ladder (`minimal < low < medium < high < xhigh < max < ultra`) is
/// collapsed onto agy's three levels: anything above `medium` — including
/// `max` and `ultra` — maps to `high`. `none`, absent, and unknown values map
/// to `None` (omit the flag entirely).
pub fn normalize_effort(effort: &str) -> Option<&'static str> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" | "ultra" => Some("high"),
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

// ── LanguageServer quota probe (best-effort) ────────────────────────────────
//
// Every `agy --print` run spawns a LanguageServer that binds a random HTTP
// port and logs it to our `--log-file` within ~1s. While the run is live, that
// port answers a small Connect-protocol JSON RPC surface — including
// `RetrieveUserQuotaSummary`, which reports the Gemini/model quota buckets. The
// runner fires one such request per run and caches the parsed result
// process-globally (mirroring [`STATUS_CACHE`]) so `/usage` can surface it. The
// port dies with the run, so the cache is the only cross-run view of quota.

/// Timeout for the single best-effort quota request fired during a run.
const QUOTA_FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// One quota bucket from `RetrieveUserQuotaSummary`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaBucket {
    /// Group `displayName`, e.g. `Gemini Models`.
    pub group: String,
    /// Bucket `displayName`, e.g. `Weekly Limit`.
    pub display_name: String,
    /// Bucket id, e.g. `gemini-weekly`.
    pub bucket_id: Option<String>,
    /// Window, e.g. `weekly`.
    pub window: Option<String>,
    /// Fraction of quota remaining in `[0, 1]`; `0` means exhausted.
    pub remaining_fraction: f64,
    /// RFC3339 reset time as reported by agy.
    pub reset_time: Option<String>,
}

impl QuotaBucket {
    /// Best display name for the bucket (falls back to the group name).
    pub fn label(&self) -> &str {
        if self.display_name.trim().is_empty() {
            self.group.trim()
        } else {
            self.display_name.trim()
        }
    }

    /// Whether this bucket reports no remaining quota.
    pub fn is_exhausted(&self) -> bool {
        self.remaining_fraction <= 0.0
    }
}

/// Cached quota summary plus when it was captured.
#[derive(Debug, Clone)]
pub struct QuotaSummary {
    pub buckets: Vec<QuotaBucket>,
    pub fetched_at: Instant,
}

impl QuotaSummary {
    /// Wall-clock age of the cached summary.
    pub fn age(&self) -> Duration {
        self.fetched_at.elapsed()
    }

    /// First fully-exhausted bucket, if any.
    pub fn first_exhausted(&self) -> Option<&QuotaBucket> {
        self.buckets.iter().find(|b| b.is_exhausted())
    }
}

/// The agy LanguageServer's HTTP port, parsed from its `--log-file`.
///
/// The log emits `Language server listening on random port at <PORT> for HTTP`
/// within ~1s of a `--print` run starting. A sibling HTTPS/gRPC port line is
/// deliberately ignored — only the line ending in `for HTTP` is the plain-JSON
/// Connect endpoint we can POST to.
pub fn parse_http_port(log: &str) -> Option<u16> {
    const MARKER: &str = "listening on random port at ";
    for line in log.lines() {
        let trimmed = line.trim_end();
        if !trimmed.ends_with("for HTTP") {
            continue;
        }
        let Some(idx) = trimmed.find(MARKER) else {
            continue;
        };
        let after = &trimmed[idx + MARKER.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(port) = digits.parse::<u16>()
            && port != 0
        {
            return Some(port);
        }
    }
    None
}

/// Parse a `RetrieveUserQuotaSummary` (or `GetAvailableModels`-shaped) response
/// into a flat bucket list. Returns `None` for a Connect error envelope
/// (`{"code":...,"message":...}`) or any body without a usable `response`.
pub fn parse_quota_summary(json: &str) -> Option<Vec<QuotaBucket>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    // Connect error envelope carries a top-level `code`/`message` and no
    // `response` — never treat that as quota data.
    if value.get("response").is_none() {
        return None;
    }
    let groups = value.pointer("/response/groups")?.as_array()?;
    let mut buckets = Vec::new();
    for group in groups {
        let group_name = group
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(bucket_values) = group.get("buckets").and_then(|v| v.as_array()) else {
            continue;
        };
        for bucket in bucket_values {
            buckets.push(QuotaBucket {
                group: group_name.clone(),
                display_name: bucket
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                bucket_id: bucket
                    .get("bucketId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                window: bucket
                    .get("window")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                remaining_fraction: bucket
                    .get("remainingFraction")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(1.0),
                reset_time: bucket
                    .get("resetTime")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            });
        }
    }
    (!buckets.is_empty()).then_some(buckets)
}

/// POST `RetrieveUserQuotaSummary` to the live LanguageServer on `port`.
/// Best-effort: any transport/parse failure returns `Err` for the caller to
/// log at debug. The body is a literal `{}` and the response is parsed as
/// Connect JSON, so no reqwest `json` feature is required.
pub async fn fetch_quota_summary(port: u16) -> Result<Vec<QuotaBucket>, String> {
    let url = format!(
        "http://localhost:{port}/exa.language_server_pb.LanguageServerService/\
         RetrieveUserQuotaSummary"
    );
    let response = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body("{}")
        .timeout(QUOTA_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("agy quota request failed: {e}"))?;
    let body = response
        .text()
        .await
        .map_err(|e| format!("agy quota body read failed: {e}"))?;
    parse_quota_summary(&body).ok_or_else(|| {
        let excerpt: String = body.chars().take(200).collect();
        format!("agy quota response not parseable: {excerpt}")
    })
}

static QUOTA_CACHE: Mutex<Option<QuotaSummary>> = Mutex::new(None);

/// Store a freshly fetched quota summary, stamped now.
pub fn cache_quota_summary(buckets: Vec<QuotaBucket>) {
    *QUOTA_CACHE.lock().unwrap() = Some(QuotaSummary {
        buckets,
        fetched_at: Instant::now(),
    });
}

/// Read the cached quota summary (populated whenever an agy subagent runs).
pub fn cached_quota_summary() -> Option<QuotaSummary> {
    QUOTA_CACHE.lock().unwrap().clone()
}

/// Spawn-failure annotation: when the cached quota shows any bucket fully
/// exhausted, a short `Antigravity quota: <bucket> exhausted, resets <time>`.
/// `None` when no cache exists or nothing is exhausted — never blocks a spawn.
pub fn exhausted_quota_note() -> Option<String> {
    let summary = cached_quota_summary()?;
    let bucket = summary.first_exhausted()?;
    Some(match bucket.reset_time.as_deref() {
        Some(reset) if !reset.trim().is_empty() => {
            format!(
                "Antigravity quota: {} exhausted, resets {reset}",
                bucket.label()
            )
        }
        _ => format!("Antigravity quota: {} exhausted", bucket.label()),
    })
}

// ── GetAvailableModels availability probe ───────────────────────────────────
//
// The same live LanguageServer port that answers `RetrieveUserQuotaSummary`
// also answers `GetAvailableModels`, whose payload carries a per-model
// `quotaInfo.remainingFraction` (plus whatever effort/reasoning-capability
// fields agy exposes). We cache the full per-model JSON so a spawn can fail
// fast when the requested model is out of quota, instead of eating a full run.

/// One model entry from `GetAvailableModels`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelAvailability {
    /// Model key as reported by agy (e.g. `gemini-3.6-flash`).
    pub key: String,
    /// `quotaInfo.remainingFraction`; `0` means exhausted.
    pub remaining_fraction: f64,
    /// Full per-model JSON, preserved so any effort/reasoning-capability fields
    /// agy adds remain inspectable without a parser change.
    pub raw: serde_json::Value,
}

impl ModelAvailability {
    pub fn is_exhausted(&self) -> bool {
        self.remaining_fraction <= 0.0
    }
}

/// A pre-spawn availability problem discovered from the cached `GetAvailableModels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityIssue {
    /// The model is listed but has no remaining quota.
    Exhausted,
    /// The model is absent from a payload whose key format matches the roster.
    Absent,
}

/// Parse a `GetAvailableModels` response into per-model availability. Returns
/// `None` for a Connect error envelope or any body without a usable `response`.
pub fn parse_available_models(json: &str) -> Option<Vec<ModelAvailability>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("response").is_none() {
        return None;
    }
    let models = value.pointer("/response/models")?.as_object()?;
    let mut out = Vec::with_capacity(models.len());
    for (key, entry) in models {
        out.push(ModelAvailability {
            key: key.clone(),
            remaining_fraction: entry
                .pointer("/quotaInfo/remainingFraction")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(1.0),
            raw: entry.clone(),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// POST `GetAvailableModels` to the live LanguageServer on `port`. Best-effort;
/// mirrors [`fetch_quota_summary`].
pub async fn fetch_available_models(port: u16) -> Result<Vec<ModelAvailability>, String> {
    let url = format!(
        "http://localhost:{port}/exa.language_server_pb.LanguageServerService/\
         GetAvailableModels"
    );
    let response = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body("{}")
        .timeout(QUOTA_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("agy models request failed: {e}"))?;
    let body = response
        .text()
        .await
        .map_err(|e| format!("agy models body read failed: {e}"))?;
    parse_available_models(&body).ok_or_else(|| {
        let excerpt: String = body.chars().take(200).collect();
        format!("agy models response not parseable: {excerpt}")
    })
}

static MODELS_CACHE: Mutex<Option<Vec<ModelAvailability>>> = Mutex::new(None);

/// Store a freshly fetched per-model availability payload.
pub fn cache_available_models(models: Vec<ModelAvailability>) {
    *MODELS_CACHE.lock().unwrap() = Some(models);
}

/// Read the cached per-model availability payload (populated when an agy
/// subagent runs).
pub fn cached_available_models() -> Option<Vec<ModelAvailability>> {
    MODELS_CACHE.lock().unwrap().clone()
}

fn key_matches(key: &str, model: &str, base: &str) -> bool {
    key.eq_ignore_ascii_case(model)
        || key.eq_ignore_ascii_case(base)
        || base_model_id(key).eq_ignore_ascii_case(base)
}

/// Pre-spawn availability check against the cached `GetAvailableModels`.
///
/// - Returns `Exhausted` when the model is present with no remaining quota.
/// - Returns `Absent` only when the model is missing AND the payload's key
///   format is provably compatible with `roster` (some roster id maps to some
///   cached key by base id). This guard prevents a differently-named payload
///   from false-flagging every model as absent and blocking valid spawns.
/// - Returns `None` on an empty/missing cache (never blocks on staleness).
pub fn model_availability_issue(model: &str, roster: &[String]) -> Option<AvailabilityIssue> {
    let models = cached_available_models()?;
    if models.is_empty() {
        return None;
    }
    let base = base_model_id(model);
    if let Some(entry) = models.iter().find(|m| key_matches(&m.key, model, base)) {
        return entry.is_exhausted().then_some(AvailabilityIssue::Exhausted);
    }
    // Not matched: only trust an "absent" verdict when the payload's naming
    // scheme lines up with the roster; otherwise stay silent.
    let key_format_compatible = roster.iter().any(|roster_id| {
        let roster_base = base_model_id(roster_id);
        models
            .iter()
            .any(|m| key_matches(&m.key, roster_id, roster_base))
    });
    key_format_compatible.then_some(AvailabilityIssue::Absent)
}

/// Build the spawn-failure message for an unavailable model: the availability
/// verdict, a reference-model suggestion (unless the model already is the
/// reference), and the exhausted-bucket note when the quota cache has one.
pub fn unavailable_model_message(model: &str, issue: AvailabilityIssue) -> String {
    let mut msg = match issue {
        AvailabilityIssue::Exhausted => {
            format!("Antigravity model \"{model}\" is out of quota.")
        }
        AvailabilityIssue::Absent => {
            format!("Antigravity model \"{model}\" is not currently available.")
        }
    };
    if base_model_id(model) != REFERENCE_MODEL {
        msg.push_str(&format!(
            " Try the reference model `{MODEL_PREFIX}{REFERENCE_MODEL}`."
        ));
    }
    if let Some(note) = exhausted_quota_note() {
        msg.push('\n');
        msg.push_str(&note);
    }
    msg
}

/// Map the current agy `--log-file` contents to a short heartbeat phase for the
/// subagent card. Best-effort and driven by log markers: the `for HTTP` port
/// line and the `shutting down` line are empirically verified; the activity /
/// conversation markers are coarse heuristics. Returns a stable `&'static str`
/// so the runner can dedupe emissions.
pub fn phase_from_log(log: &str) -> &'static str {
    if log.contains("Language server shutting down") {
        return "Wrapping up";
    }
    if log.contains("cascade")
        || log.contains("Cascade")
        || log.contains("tool_call")
        || log.contains("ExecuteCommand")
        || log.contains("RunCommand")
    {
        return "Working";
    }
    if log.contains("for HTTP") && log.contains("listening on random port at") {
        return "Language server ready";
    }
    if log.contains("Print mode: conversation=") || log.contains("Created conversation ") {
        return "Model resolved";
    }
    "Starting"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the process-global antigravity caches
    /// (quota / available-models), which cargo would otherwise run in parallel.
    static CACHE_TEST_GUARD: Mutex<()> = Mutex::new(());

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

    #[test]
    fn effort_compat_first_attempt_suffixed_vs_base() {
        let roster = vec![
            "gemini-3.6-flash".to_string(),
            "gemini-3.6-flash-high".to_string(),
            "gemini-3.6-pro".to_string(),
        ];
        // Base id + any effort: --effort flows on the FIRST attempt (base ids
        // accept it), never a suffix swap.
        assert_eq!(
            plan_model_args("gemini-3.6-flash", Some("ultra"), &roster),
            ("gemini-3.6-flash".to_string(), Some("high".to_string()))
        );
        assert_eq!(
            plan_model_args("gemini-3.6-flash", Some("max"), &roster),
            ("gemini-3.6-flash".to_string(), Some("high".to_string()))
        );
        assert_eq!(
            plan_model_args("gemini-3.6-pro", Some("medium"), &roster),
            ("gemini-3.6-pro".to_string(), Some("medium".to_string()))
        );
        // Base id, no/none effort: omit the flag entirely.
        assert_eq!(
            plan_model_args("gemini-3.6-flash", Some("none"), &roster),
            ("gemini-3.6-flash".to_string(), None)
        );
        assert_eq!(
            plan_model_args("gemini-3.6-flash", None, &roster),
            ("gemini-3.6-flash".to_string(), None)
        );
        // Suffixed id: NEVER pass --effort on the first attempt (agy rejects it
        // for concrete variants); with no roster sibling the id is kept as-is.
        assert_eq!(
            plan_model_args("gemini-3.6-flash-high", Some("ultra"), &roster),
            ("gemini-3.6-flash-high".to_string(), None)
        );
        assert_eq!(normalize_effort("ultra"), Some("high"));
        assert_eq!(normalize_effort("none"), None);
        assert_eq!(base_model_id("gemini-3.6-flash-high"), "gemini-3.6-flash");
        assert_eq!(base_model_id("gemini-3.6-flash"), "gemini-3.6-flash");
    }

    #[test]
    fn roster_advertises_reference_model_first() {
        let status = parse_models_output(
            "claude-sonnet-4-6\ngemini-3.6-flash-high\ngemini-3.6-flash\ngemini-3.6-pro\n",
            "",
        );
        let slugs = status.prefixed_models();
        assert_eq!(slugs[0], "antigravity:gemini-3.6-flash");
        // Effort-variants of the reference follow it, ahead of other models.
        assert_eq!(slugs[1], "antigravity:gemini-3.6-flash-high");
        assert!(slugs.contains(&"antigravity:claude-sonnet-4-6".to_string()));
        // Non-reference models keep their probe order after the reference block.
        let pro = slugs.iter().position(|s| s == "antigravity:gemini-3.6-pro");
        let claude = slugs
            .iter()
            .position(|s| s == "antigravity:claude-sonnet-4-6");
        assert!(claude < pro, "probe order preserved within the tail");
    }

    #[test]
    fn available_models_parse_and_reject_errors() {
        let json = r#"{"response":{"models":{
            "gemini-3.6-flash":{"quotaInfo":{"remainingFraction":1},"supportsEffort":true},
            "gemini-3.6-pro":{"quotaInfo":{"remainingFraction":0}}
        }}}"#;
        let models = parse_available_models(json).expect("models");
        assert_eq!(models.len(), 2);
        let flash = models.iter().find(|m| m.key == "gemini-3.6-flash").unwrap();
        assert_eq!(flash.remaining_fraction, 1.0);
        assert!(!flash.is_exhausted());
        // Unknown per-model fields are preserved in `raw`.
        assert_eq!(
            flash.raw.get("supportsEffort").and_then(|v| v.as_bool()),
            Some(true)
        );
        let pro = models.iter().find(|m| m.key == "gemini-3.6-pro").unwrap();
        assert!(pro.is_exhausted());

        assert_eq!(
            parse_available_models(r#"{"code":"failed_precondition","message":"x"}"#),
            None
        );
        assert_eq!(parse_available_models("not json"), None);
    }

    #[test]
    fn availability_issue_flags_exhausted_and_absent_with_key_guard() {
        let _guard = CACHE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        invalidate_status_cache();
        let roster = vec!["gemini-3.6-flash".to_string(), "gemini-3.6-pro".to_string()];
        // Cold cache never blocks.
        assert_eq!(model_availability_issue("gemini-3.6-flash", &roster), None);

        cache_available_models(vec![
            ModelAvailability {
                key: "gemini-3.6-flash".to_string(),
                remaining_fraction: 1.0,
                raw: serde_json::json!({}),
            },
            ModelAvailability {
                key: "gemini-3.6-pro".to_string(),
                remaining_fraction: 0.0,
                raw: serde_json::json!({}),
            },
        ]);
        // Present + available → None; a suffixed request matches its base key.
        assert_eq!(model_availability_issue("gemini-3.6-flash", &roster), None);
        assert_eq!(
            model_availability_issue("gemini-3.6-flash-high", &roster),
            None
        );
        // Present + exhausted → Exhausted.
        assert_eq!(
            model_availability_issue("gemini-3.6-pro", &roster),
            Some(AvailabilityIssue::Exhausted)
        );
        // Absent, key format compatible with the roster → Absent.
        assert_eq!(
            model_availability_issue("gemini-9.9-ghost", &roster),
            Some(AvailabilityIssue::Absent)
        );
        // Absent BUT key format incompatible (roster is entirely foreign) →
        // stay silent rather than block a valid spawn.
        let foreign_roster = vec!["totally-different-scheme".to_string()];
        assert_eq!(
            model_availability_issue("gemini-9.9-ghost", &foreign_roster),
            None
        );
        invalidate_status_cache();
    }

    #[test]
    fn unavailable_message_suggests_reference_and_appends_note() {
        let _guard = CACHE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        invalidate_status_cache();
        cache_quota_summary(vec![QuotaBucket {
            group: "Gemini Models".to_string(),
            display_name: "Daily Limit".to_string(),
            bucket_id: Some("gemini-daily".to_string()),
            window: Some("daily".to_string()),
            remaining_fraction: 0.0,
            reset_time: Some("2026-07-24T00:00:00Z".to_string()),
        }]);
        let msg = unavailable_model_message("gemini-3.6-pro", AvailabilityIssue::Exhausted);
        assert!(msg.contains("out of quota"), "got: {msg}");
        assert!(
            msg.contains("reference model `antigravity:gemini-3.6-flash`"),
            "got: {msg}"
        );
        assert!(
            msg.contains("Antigravity quota: Daily Limit exhausted, resets 2026-07-24T00:00:00Z"),
            "got: {msg}"
        );
        // The reference model itself is never told to switch to itself.
        let ref_msg = unavailable_model_message("gemini-3.6-flash", AvailabilityIssue::Absent);
        assert!(
            !ref_msg.contains("Try the reference model"),
            "got: {ref_msg}"
        );
        invalidate_status_cache();
    }

    #[test]
    fn http_port_parsed_from_the_http_line_only() {
        let log = "\
I0722 lsp.go:88] Language server listening on random port at 51037 for HTTPS/gRPC\n\
I0722 lsp.go:90] Language server listening on random port at 51042 for HTTP\n";
        assert_eq!(parse_http_port(log), Some(51042));
    }

    #[test]
    fn http_port_absent_when_no_http_line() {
        let log =
            "I0722 lsp.go:88] Language server listening on random port at 51037 for HTTPS/gRPC";
        assert_eq!(parse_http_port(log), None);
        assert_eq!(parse_http_port("nothing here"), None);
    }

    #[test]
    fn quota_summary_parses_gemini_buckets() {
        let json = r#"{"response":{"groups":[{"displayName":"Gemini Models","buckets":[
            {"bucketId":"gemini-weekly","displayName":"Weekly Limit","window":"weekly","remainingFraction":1,"resetTime":"2026-07-30T16:00:30Z"},
            {"bucketId":"gemini-daily","displayName":"Daily Limit","window":"daily","remainingFraction":0.25,"resetTime":"2026-07-24T00:00:00Z"}
        ]}]}}"#;
        let buckets = parse_quota_summary(json).expect("buckets");
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].group, "Gemini Models");
        assert_eq!(buckets[0].display_name, "Weekly Limit");
        assert_eq!(buckets[0].bucket_id.as_deref(), Some("gemini-weekly"));
        assert_eq!(buckets[0].window.as_deref(), Some("weekly"));
        assert_eq!(buckets[0].remaining_fraction, 1.0);
        assert_eq!(
            buckets[0].reset_time.as_deref(),
            Some("2026-07-30T16:00:30Z")
        );
        assert_eq!(buckets[1].remaining_fraction, 0.25);
        assert!(!buckets[0].is_exhausted());
    }

    #[test]
    fn quota_summary_rejects_connect_error_envelope() {
        let err = r#"{"code":"failed_precondition","message":"auth RPC unavailable in cli mode"}"#;
        assert_eq!(parse_quota_summary(err), None);
        assert_eq!(parse_quota_summary("not json"), None);
        assert_eq!(parse_quota_summary(r#"{"response":{"groups":[]}}"#), None);
    }

    #[test]
    fn quota_summary_defaults_missing_remaining_fraction_to_full() {
        let json =
            r#"{"response":{"groups":[{"displayName":"G","buckets":[{"displayName":"B"}]}]}}"#;
        let buckets = parse_quota_summary(json).expect("buckets");
        assert_eq!(buckets[0].remaining_fraction, 1.0);
        assert_eq!(buckets[0].label(), "B");
    }

    #[test]
    fn bucket_label_falls_back_to_group_name() {
        let bucket = QuotaBucket {
            group: "Gemini Models".to_string(),
            display_name: String::new(),
            bucket_id: None,
            window: None,
            remaining_fraction: 0.0,
            reset_time: None,
        };
        assert_eq!(bucket.label(), "Gemini Models");
        assert!(bucket.is_exhausted());
    }

    #[test]
    fn exhausted_quota_note_reads_the_cache() {
        let _guard = CACHE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        invalidate_status_cache();
        assert_eq!(exhausted_quota_note(), None);
        cache_quota_summary(vec![
            QuotaBucket {
                group: "Gemini Models".to_string(),
                display_name: "Weekly Limit".to_string(),
                bucket_id: Some("gemini-weekly".to_string()),
                window: Some("weekly".to_string()),
                remaining_fraction: 1.0,
                reset_time: Some("2026-07-30T16:00:30Z".to_string()),
            },
            QuotaBucket {
                group: "Gemini Models".to_string(),
                display_name: "Daily Limit".to_string(),
                bucket_id: Some("gemini-daily".to_string()),
                window: Some("daily".to_string()),
                remaining_fraction: 0.0,
                reset_time: Some("2026-07-24T00:00:00Z".to_string()),
            },
        ]);
        let note = exhausted_quota_note().expect("note");
        assert_eq!(
            note,
            "Antigravity quota: Daily Limit exhausted, resets 2026-07-24T00:00:00Z"
        );
        let cached = cached_quota_summary().expect("cache");
        assert_eq!(cached.buckets.len(), 2);
        assert_eq!(
            cached.first_exhausted().unwrap().display_name,
            "Daily Limit"
        );
        invalidate_status_cache();
        assert_eq!(cached_quota_summary().is_none(), true);
    }

    #[test]
    fn phase_from_log_maps_milestones() {
        assert_eq!(phase_from_log(""), "Starting");
        assert_eq!(
            phase_from_log("I0722 printmode.go:216] Print mode: conversation=abc"),
            "Model resolved"
        );
        assert_eq!(
            phase_from_log(
                "I0722 lsp.go:90] Language server listening on random port at 51042 for HTTP"
            ),
            "Language server ready"
        );
        assert_eq!(
            phase_from_log("I0722 cascade.go:12] cascade step running"),
            "Working"
        );
        assert_eq!(
            phase_from_log("I0722 server.go:1] Language server shutting down"),
            "Wrapping up"
        );
    }
}
