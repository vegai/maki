//! Experimental Claude Code provider for maki's agent loop. The handoff server holds tool
//! calls until maki stops Claude Code.
//!
//! maki executes the calls with its own tools and permissions. The plugin setting enables
//! both the plugin and the provider.

mod checks;
mod error;
#[cfg(all(test, target_os = "linux"))]
mod fake;
#[cfg(target_os = "linux")]
mod guard;
#[cfg(all(test, target_os = "linux"))]
mod live;
mod mcp;
mod run;
mod stream;
mod transcript;

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use flume::Sender;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use smol::lock::{Mutex as AsyncMutex, Semaphore};
use tracing::{info, warn};

use crate::model::{Model, ModelFamily, ModelInfo, is_same_model};
use crate::process::find_program;
use crate::provider::{BoxFuture, Provider, RequestScope};
use crate::providers::anthropic::shared::{
    LONG_CONTEXT_SUFFIX, LONG_CONTEXT_WINDOW, long_context_window,
};
use crate::providers::{Timeouts, anthropic, catalog};
use crate::spec::{
    AuthDoc, Build, CatalogDoc, GeneratedDocs, NO_CURATED_MODELS, Native, ProviderSpec,
};
use crate::types::dialect;
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
    ThinkingConfig,
};
use error::Error;
use maki_storage::atomic_write;
use maki_storage::id::SessionRef;
use run::{Limits, Listed, Thinking};

pub(crate) const SLUG: &str = "claude-code";
const DISPLAY_NAME: &str = "Claude Code (experimental)";
const PLUGIN: &str = "claude_code";
const PLUGIN_OFF: &str =
    "enable the claude_code plugin in init.lua: `plugins = { claude_code = { enabled = true } }`";
// The provider reads these plugin options too, so the plugin and the
// provider run the same `claude` with the same login.
const EXECUTABLE_OPTION: &str = "executable";
const CONFIG_DIR_OPTION: &str = "config_dir";
const MAX_CONCURRENT_OPTION: &str = "max_concurrent";
const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
/// The login lives in Claude Code's config directory, so there is no key
/// variable.
const NO_KEY_ENV: &str = "";
const DEFAULT_EXECUTABLE: &str = "claude";
const HOME_ENV: &str = "HOME";
const TMPDIR_ENV: &str = "TMPDIR";
const DEFAULT_MAX_CONCURRENT: usize = 2;
const MODELS_CACHE_FILE: &str = "claude-code-models.json";
/// Same as the models.dev catalog, because a listing starts several `claude`
/// processes.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(86_400);
/// A failed listing answers for this long, so a logged-out `claude` does not
/// start its processes on each fetch.
const FAILED_LISTING_TTL: Duration = Duration::from_secs(300);
const ERROR_PREFIX: &str = "claude-code: ";
/// Each turn runs on the subscription login, so maki counts it as `$0` and
/// shows the list price only as a reference.
const SUBSIDY: &str = "Claude subscription";
const BAD_REQUEST: u16 = 400;
const FEATURES: &str = "Experimental. Runs maki's agent loop on the Claude models of a Claude \
    subscription, through the [`claude` CLI](https://code.claude.com/docs/en/cli-reference). maki runs \
    every tool call itself.";
const AUTH_NOTE: &str = "None. The provider is on only while the `claude_code` plugin is enabled in \
    `init.lua`, and it shares some of the plugin's options. Log in with `claude auth login` on a claude.ai \
    subscription.";
const NOTES: &str = "The [Claude Code guide](/docs/claude-code/#experimental-provider) covers setup, \
    limits and billing.";
const MODELS_NOTE: &str = "The models Claude Code offers your account, each with the context window \
    Claude Code opens for it. Tiers and list prices come from the anthropic provider, or from models.dev \
    for a release not yet in its table.";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: NO_KEY_ENV,
    family: ModelFamily::Claude,
    supports_thinking: true,
    accepts_images: false,
    supports_deferred_tools: false,
    accepts_arbitrary_models: false,
    fallback_max_output: None,
    fallback_context_window: 200_000,
    models_toml: NO_CURATED_MODELS,
    models_of: Some(anthropic::SLUG),
    pricing_schedule: None,
    build: Build::Native(Native {
        new: create,
        with_auth: None,
    }),
    aperture: None,
    login: None,
    docs: GeneratedDocs {
        api_urls: &[],
        features: Some(FEATURES),
        auth: AuthDoc::Custom(AUTH_NOTE),
        catalog: CatalogDoc::Discovered(MODELS_NOTE),
        trailing_notes: &[NOTES],
    },
};

/// Shared by all sessions, because each request is a process. The plugin
/// limits its calls separately.
static SLOTS: Mutex<Option<(usize, Arc<Semaphore>)>> = Mutex::new(None);
/// All sessions share one login, so each sees the latest usage.
static PLAN_USAGE: Mutex<Option<ProviderUsage>> = Mutex::new(None);
/// `None` when the provider is off.
static PLUGIN_OPTIONS: Mutex<Option<PluginOptions>> = Mutex::new(None);
/// Failed listings, keyed by `claude` and login like the saved list. The
/// lock lets one listing run at a time, so a second fetch reads what the
/// first one found.
static FAILED_LISTINGS: AsyncMutex<BTreeMap<(PathBuf, PathBuf), (Instant, String)>> =
    AsyncMutex::new(BTreeMap::new());
/// Set by a refresh in the TUI, and taken by the next listing.
static REFRESH_LISTING: AtomicBool = AtomicBool::new(false);

struct Exchange<'a> {
    system: &'a str,
    messages: &'a [Message],
    tools: &'a Value,
    events: &'a Sender<ProviderEvent>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct PluginOptions {
    executable: Option<String>,
    config_dir: Option<PathBuf>,
    max_concurrent: Option<usize>,
}

impl PluginOptions {
    /// A missing, empty or invalid option is `None`.
    fn from_table(table: Option<&Map<String, Value>>) -> Self {
        let value = |key: &str| table.and_then(|table| table.get(key));
        let text = |key: &str| {
            value(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        };
        Self {
            executable: text(EXECUTABLE_OPTION).map(str::to_owned),
            config_dir: text(CONFIG_DIR_OPTION).map(PathBuf::from),
            max_concurrent: value(MAX_CONCURRENT_OPTION)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| n > 0),
        }
    }
}

/// Load plugin options before provider construction so both use the same configuration.
pub fn follow_plugins(enabled_plugins: &[String], options: &HashMap<String, Map<String, Value>>) {
    let enabled = enabled_plugins.iter().any(|name| name == PLUGIN);
    *PLUGIN_OPTIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner) =
        enabled.then(|| PluginOptions::from_table(options.get(PLUGIN)));
}

/// New limits need new slots. Active requests retain their old slots until completion.
fn slots() -> Arc<Semaphore> {
    let limit = plugin_options()
        .and_then(|options| options.max_concurrent)
        .unwrap_or(DEFAULT_MAX_CONCURRENT);
    let mut slots = SLOTS.lock().unwrap_or_else(PoisonError::into_inner);
    match &*slots {
        Some((size, semaphore)) if *size == limit => Arc::clone(semaphore),
        _ => {
            let semaphore = Arc::new(Semaphore::new(limit));
            *slots = Some((limit, Arc::clone(&semaphore)));
            semaphore
        }
    }
}

fn plugin_options() -> Option<PluginOptions> {
    PLUGIN_OPTIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn config_error(message: impl Display) -> AgentError {
    AgentError::Config {
        message: format!("{ERROR_PREFIX}{message}"),
    }
}

/// Temporary API errors and stalled replies enter maki's retry loop. Oversized
/// conversations remain overflow errors so maki can compact them.
///
/// Other failures are configuration errors. A retry can repeat a startup or handoff
/// failure, or send a request twice.
fn agent_error(error: Error) -> AgentError {
    if let Some(secs) = error.stalled() {
        warn!(%error, "claude-code: the reply stalled");
        return AgentError::Timeout { secs };
    }
    if let Some((status, retry_after)) = error.temporary() {
        return AgentError::Api {
            status,
            message: format!("{ERROR_PREFIX}{error}"),
            retry_after,
        };
    }
    match error.invalid_request() {
        Some(text) => AgentError::Api {
            status: BAD_REQUEST,
            message: format!("{ERROR_PREFIX}{text}"),
            retry_after: None,
        },
        None => config_error(error),
    }
}

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    create_with(plugin_options(), timeouts)
}

fn create_with(
    options: Option<PluginOptions>,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let options = options.ok_or_else(|| config_error(PLUGIN_OFF))?;
    Ok(Box::new(ClaudeCode::new(&options, timeouts)?))
}

fn models_cache_path() -> Option<PathBuf> {
    maki_storage::paths::cache_dir()
        .ok()
        .map(|dir| dir.join(MODELS_CACHE_FILE))
}

/// For `maki models --refresh`. After an error, a saved list stays in use
/// until it is a day old.
/// This function blocks, so call it only out of the executor.
pub fn refresh_models(timeouts: Timeouts) -> Option<Result<usize, AgentError>> {
    let options = plugin_options()?;
    Some(smol::block_on(async {
        let provider = ClaudeCode::new(&options, timeouts)?;
        let cwd = working_dir()?;
        let models = provider.list(&cwd, true).await.map_err(config_error)?;
        Ok(models.len())
    }))
}

/// For a refresh in the TUI: the next listing asks Claude Code even if the
/// saved list is fresh or a listing just failed.
pub fn refresh_on_next_listing() {
    REFRESH_LISTING.store(true, Ordering::Relaxed);
}

/// maki's working directory, as `getcwd` gives it. A model listing has no
/// session, so its checks run here.
fn working_dir() -> Result<PathBuf, AgentError> {
    env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .map_err(config_error)
}

/// A model list, saved with its `claude` and config dir, because another
/// `claude` or login can offer different models. A new login in the same
/// config dir keeps the list until it expires, while each request probes
/// the account again.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct SavedModels {
    executable: PathBuf,
    config_dir: PathBuf,
    models: Vec<Listed>,
}

fn saved_models(
    path: &Path,
    max_age: Duration,
    executable: &Path,
    config_dir: &Path,
) -> Option<Vec<Listed>> {
    let age = fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > max_age {
        return None;
    }
    let saved: SavedModels = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    (saved.executable == executable && saved.config_dir == config_dir).then_some(saved.models)
}

fn save_models(path: &Path, saved: &SavedModels) {
    let saved = serde_json::to_vec(saved)
        .map_err(|error| error.to_string())
        .and_then(|bytes| atomic_write(path, &bytes).map_err(|error| error.to_string()));
    if let Err(error) = saved {
        warn!(path = %path.display(), %error, "claude-code: maki cannot save the model list");
    }
}

/// Returns true if models.dev gives the model a 1M window.
fn takes_a_million(id: &str) -> bool {
    catalog::model_meta_if_available(SPEC.models_slug(), id)
        .and_then(|meta| meta.context)
        .is_some_and(|window| window >= LONG_CONTEXT_WINDOW)
}

/// Returns each model with the window Claude Code opens for it. When that
/// window is below the 1M that `can_open` allows, the `-1m` id comes first,
/// as in the anthropic provider.
fn with_long_context(models: Vec<Listed>, can_open: impl Fn(&str) -> bool) -> Vec<ModelInfo> {
    models
        .into_iter()
        .flat_map(|Listed { id, window }| {
            let short = window.is_none_or(|window| window < LONG_CONTEXT_WINDOW);
            let wide = (short && can_open(&id)).then(|| ModelInfo {
                id: format!("{id}{LONG_CONTEXT_SUFFIX}"),
                context_window: Some(LONG_CONTEXT_WINDOW),
                ..Default::default()
            });
            wide.into_iter().chain([ModelInfo {
                id,
                context_window: window,
                ..Default::default()
            }])
        })
        .collect()
}

/// Models with adaptive thinking run at an effort level, the rest on a
/// budget.
fn thinking_for(thinking: ThinkingConfig, model: &Model) -> Thinking {
    match thinking {
        ThinkingConfig::Off => Thinking::Off,
        _ if ThinkingConfig::requires_adaptive(&model.id) => thinking
            .effort_str(&dialect::CLAUDE_CODE, model)
            .map_or(Thinking::Default, Thinking::Effort),
        _ => thinking
            .request_thinking(model)
            .map_or(Thinking::Default, Thinking::Budget),
    }
}

/// Returns the variables whose name and value are UTF-8, the same set that
/// `maki.uv.os_environ` gives the plugin.
fn utf8_vars(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(String, String)> {
    vars.filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

struct Prepared {
    env: Vec<(String, String)>,
    temp_dir: PathBuf,
    config_dir: PathBuf,
}

struct ClaudeCode {
    executable: PathBuf,
    limits: Limits,
    /// maki's environment, read afresh for each request.
    environment: Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    models_cache: Option<PathBuf>,
    /// Takes precedence over `$CLAUDE_CONFIG_DIR`.
    config_dir: Option<PathBuf>,
}

impl ClaudeCode {
    fn new(options: &PluginOptions, timeouts: Timeouts) -> Result<Self, AgentError> {
        let name = options.executable.as_deref().unwrap_or(DEFAULT_EXECUTABLE);
        let cwd = env::current_dir().map_err(config_error)?;
        let executable = find_program(name, &cwd).ok_or_else(|| {
            config_error(format!(
                "Claude Code ({name}) is not installed, or it is not on PATH"
            ))
        })?;
        Ok(Self {
            executable,
            limits: Limits::new(timeouts.stream),
            environment: Box::new(|| utf8_vars(env::vars_os())),
            models_cache: models_cache_path(),
            config_dir: options.config_dir.clone(),
        })
    }

    fn login_dir(&self, vars: &[(String, String)]) -> Result<PathBuf, Error> {
        let var = |name: &str| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| OsString::from(value))
        };
        let configured = self
            .config_dir
            .clone()
            .map(OsString::from)
            .or_else(|| var(CONFIG_DIR_ENV));
        checks::config_dir(configured, var(HOME_ENV))
    }

    fn prepare(&self, cwd: &Path) -> Result<Prepared, Error> {
        let vars = (self.environment)();
        let config_dir = self.login_dir(&vars)?;
        let temp_dir = vars
            .iter()
            .find(|(key, _)| key == TMPDIR_ENV)
            .map_or_else(env::temp_dir, |(_, value)| PathBuf::from(value));
        let (mut env, withheld) = checks::child_env(vars);
        // Claude Code treats an empty value as unset. Remove it so the child and route
        // checks agree.
        env.retain(|(key, value)| key != CONFIG_DIR_ENV || !value.is_empty());
        if self.config_dir.is_some() {
            env.retain(|(key, _)| key != CONFIG_DIR_ENV);
            env.push((CONFIG_DIR_ENV.to_owned(), config_dir.display().to_string()));
        }
        if !withheld.is_empty() {
            info!(
                ?withheld,
                "claude-code: maki did not give the route variables to Claude Code"
            );
        }
        let local_dirs = checks::local_settings_dirs(cwd)?;
        let conflicts =
            checks::file_conflicts(&checks::skipped_settings(&config_dir, cwd, &local_dirs));
        if !conflicts.is_empty() {
            return Err(Error::SkippedSettings(conflicts));
        }
        Ok(Prepared {
            env,
            temp_dir,
            config_dir,
        })
    }

    async fn models(&self, cwd: &Path) -> Result<Vec<Listed>, Error> {
        self.list(cwd, REFRESH_LISTING.swap(false, Ordering::Relaxed))
            .await
    }

    /// Model discovery starts up to six processes. Cache successful lists for 24 hours
    /// and failures for five minutes. `fresh` bypasses both caches.
    async fn list(&self, cwd: &Path, fresh: bool) -> Result<Vec<Listed>, Error> {
        let key = (
            self.executable.clone(),
            self.login_dir(&(self.environment)())?,
        );
        let saved = || {
            self.models_cache
                .as_deref()
                .and_then(|path| saved_models(path, MODELS_CACHE_TTL, &key.0, &key.1))
        };
        // The lock waits for any listing in progress, which can wait for a
        // slot, so a saved list answers without it.
        if !fresh && let Some(models) = saved() {
            return Ok(models);
        }
        let mut failures = FAILED_LISTINGS.lock().await;
        if !fresh {
            if let Some(models) = saved() {
                return Ok(models);
            }
            if let Some((at, message)) = failures.get(&key)
                && at.elapsed() < FAILED_LISTING_TTL
            {
                return Err(Error::ListedRecently {
                    message: message.clone(),
                    wait: FAILED_LISTING_TTL,
                });
            }
        }
        let listed = self.ask_models(cwd).await;
        match &listed {
            Ok(_) => failures.remove(&key),
            Err(error) => failures.insert(key, (Instant::now(), error.to_string())),
        };
        listed
    }

    /// Returns the window from the last list, however old it is. Windows
    /// change far less often than the list.
    fn window_of(&self, id: &str) -> Option<u32> {
        let config_dir = self.login_dir(&(self.environment)()).ok()?;
        let path = self.models_cache.as_deref()?;
        saved_models(path, Duration::MAX, &self.executable, &config_dir)?
            .into_iter()
            .find(|listed| is_same_model(&listed.id, id))?
            .window
    }

    /// The listing takes one slot, like a request.
    async fn ask_models(&self, cwd: &Path) -> Result<Vec<Listed>, Error> {
        let slots = slots();
        let _slot = slots.acquire().await;
        let prepared = self.prepare(cwd)?;
        let models = run::models(
            &self.executable,
            &prepared.env,
            cwd,
            &prepared.temp_dir,
            &PLAN_USAGE,
            self.limits.startup,
        )
        .await?;
        if let Some(path) = &self.models_cache {
            let saved = SavedModels {
                executable: self.executable.clone(),
                config_dir: prepared.config_dir,
                models,
            };
            save_models(path, &saved);
            return Ok(saved.models);
        }
        Ok(models)
    }

    async fn request(
        &self,
        model: &Model,
        thinking: Thinking,
        exchange: Exchange<'_>,
        cwd: &Path,
    ) -> Result<StreamResponse, Error> {
        // Claude Code reports its directory as `getcwd` does.
        let cwd = cwd.canonicalize().map_err(|source| Error::Path {
            what: "resolve the directory of the session",
            path: cwd.to_owned(),
            source,
        })?;
        let slots = slots();
        let waiting = Instant::now();
        let _slot = match slots.try_acquire() {
            Some(slot) => slot,
            None => {
                info!("claude-code: every request slot is taken, so the request waits");
                slots.acquire().await
            }
        };
        let slot_wait = waiting.elapsed();
        let prepared = self.prepare(&cwd)?;
        run::request(run::Request {
            executable: &self.executable,
            env: &prepared.env,
            model: &model.id,
            cwd: &cwd,
            system: exchange.system,
            messages: exchange.messages,
            tools: exchange.tools,
            events: exchange.events,
            plan_usage: &PLAN_USAGE,
            temp_dir: &prepared.temp_dir,
            max_output: model.output_tokens(),
            thinking,
            limits: &self.limits,
            slot_wait,
        })
        .await
    }
}

impl Provider for ClaudeCode {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        _tools: &'a Value,
        _event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async { Err(config_error(Error::NoWorkingDir)) })
    }

    fn stream_message_in<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        scope: RequestScope<'a>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let thinking = thinking_for(opts.thinking, model);
        Box::pin(async move {
            let exchange = Exchange {
                system,
                messages,
                tools,
                events: event_tx,
            };
            self.request(model, thinking, exchange, scope.cwd)
                .await
                .map_err(agent_error)
        })
    }

    /// Asks Claude Code which models the account can use, so a new model
    /// shows up without a maki update.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let models = self.models(&working_dir()?).await.map_err(config_error)?;
            Ok(with_long_context(models, takes_a_million))
        })
    }

    /// The last request's usage, because Claude Code has no endpoint maki can
    /// ask.
    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async {
            Ok(PLAN_USAGE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone())
        })
    }

    /// The window is the one Claude Code opens for the name, and a model keeps the standard
    /// window until a listing reports it. models.dev gives the 1M a model can
    /// open, but Claude Code opens it only for `[1m]`.
    fn adjust_model(&self, model: &mut Model) {
        model.subsidised_by = Some(Arc::from(SUBSIDY));
        if long_context_window(&model.id).is_none() {
            model.context_window = self
                .window_of(&model.id)
                .unwrap_or(model.context_window.min(SPEC.fallback_context_window));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::env;
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::path::Path;
    use std::sync::PoisonError;

    use serde_json::{Value, json};
    use tempfile::tempdir;
    use test_case::test_case;

    use super::error::Error;
    use super::run::Thinking;
    #[cfg(unix)]
    use super::utf8_vars;
    use super::{
        ClaudeCode, LONG_CONTEXT_WINDOW, Listed, MODELS_CACHE_FILE, PLUGIN, PLUGIN_OFF,
        PluginOptions, SLOTS, SLUG, SUBSIDY, SavedModels, agent_error, create_with, follow_plugins,
        save_models, slots, thinking_for, with_long_context,
    };
    use crate::provider::Provider;
    use crate::providers::Timeouts;
    use crate::retry::RetryKind;
    use crate::types::Effort::{High, Max, Minimal};
    use crate::{Model, TokenUsage};
    use crate::{RequestOptions, ThinkingConfig};

    const ANTHROPIC: &str = "anthropic";
    /// A row with vision in the anthropic table.
    const VISION_ROW: &str = "claude-opus-5-5";
    /// A release with no row in the anthropic table.
    const RELEASE_WITHOUT_ROW: &str = "claude-sonnet-5-5";
    const LONG_CONTEXT_ID: &str = "claude-sonnet-5-5-1m";
    /// Anthropic's strong default, which Claude Code opens at 1M.
    const DEFAULT_ROW: &str = "claude-opus-5";
    /// Claude Code opens it at 200K, and at 1M only with `[1m]`.
    const OLDER_RELEASE: &str = "claude-opus-4-6";
    const OLDER_RELEASE_1M: &str = "claude-opus-4-6-1m";
    const STANDARD_ONLY: &str = "claude-haiku-4-5";
    /// Anthropic's medium default, and a dated snapshot of it.
    const MEDIUM_ROW: &str = "claude-sonnet-5";
    const DATED_MEDIUM_ROW: &str = "claude-sonnet-5-20261001";
    const STANDARD_WINDOW: u32 = 200_000;
    const MAX_CONCURRENT: &str = "max_concurrent";
    const INVALID: &str = "invalid_request";
    const TOO_LONG: &str = "Prompt is too long";
    const RATE_LIMITED: &str = "rate_limit";
    const OVERLOADED_KIND: &str = "overloaded";
    const UNKNOWN_KIND: &str = "unknown";
    const LOGIN_FAILED: &str = "authentication_failed";
    const REFUSAL_TEXT: &str = "Refused";
    /// Uses adaptive thinking, at an effort level.
    const ADAPTIVE: &str = "claude-code/claude-sonnet-5";
    /// Thinks on a token budget.
    const BUDGETED: &str = "claude-code/claude-haiku-4-5";
    #[cfg(unix)]
    const PATH: (&str, &str) = ("PATH", "/usr/bin");
    /// Not UTF-8, which a variable maki never uses can still hold.
    #[cfg(unix)]
    const LATIN1: &[u8] = b"caf\xe9";

    /// Each effort level maps to the nearest one Claude Code accepts.
    /// Adaptive keeps Claude Code's default.
    #[test_case(ADAPTIVE, ThinkingConfig::Off => Thinking::Off ; "adaptive_off")]
    #[test_case(ADAPTIVE, ThinkingConfig::Adaptive => Thinking::Default ; "adaptive_default")]
    #[test_case(ADAPTIVE, ThinkingConfig::Effort(High) => Thinking::Effort("high") ; "adaptive_high")]
    #[test_case(ADAPTIVE, ThinkingConfig::Effort(Minimal) => Thinking::Effort("low") ; "adaptive_minimal_snaps_to_low")]
    #[test_case(ADAPTIVE, ThinkingConfig::Effort(Max) => Thinking::Effort("max") ; "adaptive_max")]
    #[test_case(BUDGETED, ThinkingConfig::Off => Thinking::Off ; "budgeted_off")]
    #[test_case(BUDGETED, ThinkingConfig::Adaptive => Thinking::Default ; "budgeted_default")]
    #[test_case(BUDGETED, ThinkingConfig::Effort(High) => matches Thinking::Budget(_) ; "budgeted_effort_becomes_a_budget")]
    fn thinking_maps_to_what_claude_code_takes(spec: &str, thinking: ThinkingConfig) -> Thinking {
        thinking_for(thinking, &Model::from_spec(spec).unwrap())
    }

    fn refused(kind: &str, status: Option<u16>) -> Error {
        let text = if kind == INVALID {
            TOO_LONG
        } else {
            REFUSAL_TEXT
        };
        Error::ApiRefused {
            kind: kind.into(),
            text: text.into(),
            status,
        }
    }

    /// Preserve overflow errors regardless of stderr. Retry temporary API errors and
    /// empty tool-call turns. Login errors must stop the request.
    #[test_case(refused(INVALID, None) => (true, false) ; "an_overflow")]
    #[test_case(Error::WithStderr { error: Box::new(refused(INVALID, None)), stderr: "noise".into() } => (true, false) ; "an_overflow_with_stderr")]
    #[test_case(refused(RATE_LIMITED, None) => (false, true) ; "a_rate_limit")]
    #[test_case(refused(OVERLOADED_KIND, Some(529)) => (false, true) ; "an_overload_with_its_status")]
    #[test_case(refused(UNKNOWN_KIND, Some(503)) => (false, true) ; "an_unknown_kind_with_a_server_status")]
    #[test_case(refused(LOGIN_FAILED, Some(401)) => (false, false) ; "a_login_error")]
    #[test_case(refused(UNKNOWN_KIND, None) => (false, false) ; "an_unknown_kind_without_a_status")]
    #[test_case(Error::CliRetry { kind: OVERLOADED_KIND.into(), status: None, delay: None } => (false, true) ; "a_retry_that_claude_code_wanted")]
    #[test_case(Error::NoCallsToRun => (false, true) ; "a_tool_stop_without_calls")]
    fn a_refused_request_reads_as_the_anthropic_provider_reports_it(error: Error) -> (bool, bool) {
        let error = agent_error(error);
        (error.is_context_overflow(), error.is_retryable())
    }

    /// Stalled replies use the normal stream timeout. Startup and handoff failures must
    /// stop without a retry.
    #[test_case(Error::Stalled(300) => Some(RetryKind::Timeout) ; "a_stalled_reply")]
    #[test_case(Error::WithStderr { error: Box::new(Error::Stalled(300)), stderr: "noise".into() } => Some(RetryKind::Timeout) ; "a_stalled_reply_with_stderr")]
    #[test_case(Error::StartupLate(60) => None ; "a_late_startup")]
    #[test_case(Error::HandoffLate(30) => None ; "a_late_handoff")]
    fn only_a_stalled_reply_is_retried_as_a_timeout(error: Error) -> Option<RetryKind> {
        agent_error(error).retry_kind()
    }

    #[test_case(json!({ "executable": "/opt/claude", "config_dir": "/cfg", "max_concurrent": 3 }) => PluginOptions { executable: Some("/opt/claude".into()), config_dir: Some("/cfg".into()), max_concurrent: Some(3) } ; "all_given")]
    #[test_case(json!({ "executable": "", "config_dir": "", "max_concurrent": 0 }) => PluginOptions::default() ; "empty_or_zero")]
    #[test_case(json!({ "model": "sonnet" }) => PluginOptions::default() ; "none_it_shares")]
    fn the_provider_shares_the_plugins_options(table: Value) -> PluginOptions {
        PluginOptions::from_table(table.as_object())
    }

    /// An unloaded plugin must disable the provider before executable discovery.
    #[test]
    fn it_is_off_without_the_plugin() {
        let err = create_with(None, Timeouts::default()).err().unwrap();
        assert!(err.to_string().contains(PLUGIN_OFF), "got: {err}");
    }

    /// Resolve models and prices through the anthropic provider so dated snapshots share
    /// their source model metadata.
    #[test_case("claude-opus-5-5" ; "a_release")]
    #[test_case("claude-haiku-4-5-20251001" ; "a_dated_snapshot")]
    fn a_model_reads_as_the_anthropic_provider_reads_it(id: &str) {
        let described = |slug: &str| {
            let model = Model::from_spec(&format!("{slug}/{id}")).unwrap();
            let rates = &model.pricing;
            (
                model.tier,
                model.context_window,
                model.max_output_tokens,
                [
                    rates.input,
                    rates.output,
                    rates.cache_write,
                    rates.cache_read,
                ],
            )
        };
        assert_eq!(described(SLUG), described(ANTHROPIC));
    }

    /// Returns the provider as maki builds it, with an executable no test
    /// runs. The login and the saved model list live in `dir`, and the saved
    /// list is `listed`.
    fn provider_in(dir: &Path, listed: Vec<Listed>) -> ClaudeCode {
        let options = PluginOptions {
            executable: Some(env::current_exe().unwrap().to_string_lossy().into_owned()),
            config_dir: Some(dir.to_owned()),
            ..PluginOptions::default()
        };
        let mut provider = ClaudeCode::new(&options, Timeouts::default()).unwrap();
        let cache = dir.join(MODELS_CACHE_FILE);
        let saved = SavedModels {
            executable: provider.executable.clone(),
            config_dir: dir.to_owned(),
            models: listed,
        };
        save_models(&cache, &saved);
        provider.models_cache = Some(cache);
        provider
    }

    fn listed(id: &str, window: Option<u32>) -> Listed {
        Listed {
            id: id.to_owned(),
            window,
        }
    }

    /// Claude Code runs in the session's directory, so a request without one
    /// stops before any process starts.
    #[test]
    fn a_request_without_a_working_dir_is_refused() {
        let dir = tempdir().unwrap();
        let provider = provider_in(dir.path(), Vec::new());
        let model = Model::from_spec(&format!("{SLUG}/{DEFAULT_ROW}")).unwrap();
        let (events, _received) = flume::unbounded();
        let opts = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: false,
        };
        let error = smol::block_on(provider.stream_message(
            &model,
            &[],
            "",
            &json!([]),
            &events,
            opts,
            None,
        ))
        .unwrap_err();
        let expected = Error::NoWorkingDir.to_string();
        assert!(error.to_string().contains(&expected), "{error}");
    }

    /// maki counts a turn on the subscription login as `$0`, and the list
    /// price stays as a reference next to it.
    #[test]
    fn a_turn_counts_as_nothing_and_keeps_the_list_price() {
        let dir = tempdir().unwrap();
        let mut model = Model::from_spec(&format!("{SLUG}/{DEFAULT_ROW}")).unwrap();
        provider_in(dir.path(), Vec::new()).adjust_model(&mut model);
        let usage = TokenUsage {
            input: 1_000,
            output: 1_000,
            ..Default::default()
        };

        assert_eq!(model.billed_cost(&usage, false), Some(0.0));
        assert!(
            model
                .subsidised_list_cost(&usage, false)
                .is_some_and(|cost| cost > 0.0)
        );
        assert_eq!(model.subsidy_source(), Some(SUBSIDY));
    }

    #[test]
    fn a_model_it_runs_takes_no_images() {
        let dir = tempdir().unwrap();
        let mut model = Model::from_spec(&format!("{SLUG}/{VISION_ROW}")).unwrap();
        assert!(!model.supports_vision());
        model.supports_vision_override = Some(true);
        assert!(!model.supports_vision());
        provider_in(dir.path(), Vec::new()).adjust_model(&mut model);
        assert!(!model.supports_vision());
    }

    /// A model gets the window Claude Code listed for its name, dated
    /// snapshots too. An unlisted model keeps the standard window even when
    /// models.dev says 1M, so maki compacts before Claude Code rejects the
    /// conversation. A `-1m` id keeps its 1M.
    #[test_case(DEFAULT_ROW => LONG_CONTEXT_WINDOW ; "a_model_claude_code_opens_at_1m")]
    #[test_case(OLDER_RELEASE => STANDARD_WINDOW ; "a_model_it_opens_at_200k")]
    #[test_case(MEDIUM_ROW => LONG_CONTEXT_WINDOW ; "a_model_listed_by_its_dated_snapshot")]
    #[test_case(RELEASE_WITHOUT_ROW => STANDARD_WINDOW ; "a_model_no_listing_named")]
    #[test_case(LONG_CONTEXT_ID => LONG_CONTEXT_WINDOW ; "a_1m_id")]
    fn a_model_takes_the_window_claude_code_opens(id: &str) -> u32 {
        let dir = tempdir().unwrap();
        let provider = provider_in(
            dir.path(),
            vec![
                listed(DEFAULT_ROW, Some(LONG_CONTEXT_WINDOW)),
                listed(OLDER_RELEASE, Some(STANDARD_WINDOW)),
                listed(DATED_MEDIUM_ROW, Some(LONG_CONTEXT_WINDOW)),
            ],
        );
        let mut model = Model::from_spec(&format!("{SLUG}/{id}")).unwrap();
        model.context_window = LONG_CONTEXT_WINDOW;
        provider.adjust_model(&mut model);
        model.context_window
    }

    /// Each model shows with the window Claude Code opens for it. Below the
    /// 1M the model can open, the `-1m` id comes first, as in the anthropic
    /// provider. A model already at 1M has no second id.
    #[test]
    fn a_model_short_of_its_1m_window_also_lists_its_1m_id() {
        let listed = with_long_context(
            vec![
                listed(RELEASE_WITHOUT_ROW, Some(LONG_CONTEXT_WINDOW)),
                listed(OLDER_RELEASE, Some(STANDARD_WINDOW)),
                listed(STANDARD_ONLY, Some(STANDARD_WINDOW)),
            ],
            |id| id != STANDARD_ONLY,
        );
        let windows: Vec<(&str, Option<u32>)> = listed
            .iter()
            .map(|info| (info.id.as_str(), info.context_window))
            .collect();
        assert_eq!(
            windows,
            [
                (RELEASE_WITHOUT_ROW, Some(LONG_CONTEXT_WINDOW)),
                (OLDER_RELEASE_1M, Some(LONG_CONTEXT_WINDOW)),
                (OLDER_RELEASE, Some(STANDARD_WINDOW)),
                (STANDARD_ONLY, Some(STANDARD_WINDOW)),
            ]
        );
    }

    /// A variable whose name or value is not UTF-8 is left out rather than
    /// failing every request.
    #[cfg(unix)]
    #[test]
    fn a_variable_that_is_not_utf8_is_left_out() {
        let vars = [
            (OsString::from(PATH.0), OsString::from(PATH.1)),
            (
                OsString::from("ODD_VALUE"),
                OsString::from_vec(LATIN1.to_vec()),
            ),
            (
                OsString::from_vec(LATIN1.to_vec()),
                OsString::from("odd name"),
            ),
        ];
        assert_eq!(
            utf8_vars(vars.into_iter()),
            [(PATH.0.to_owned(), PATH.1.to_owned())]
        );
    }

    /// Turns the provider off again even if the test panics, so no later
    /// test in the process finds it on.
    struct ProviderOff;

    impl Drop for ProviderOff {
        fn drop(&mut self) {
            follow_plugins(&[], &HashMap::new());
            *SLOTS.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    #[test]
    fn slots_follow_a_new_max_concurrent() {
        let _off = ProviderOff;
        let enabled = [PLUGIN.to_owned()];
        let options = |max: usize| {
            let table = json!({ MAX_CONCURRENT: max }).as_object().unwrap().clone();
            HashMap::from([(PLUGIN.to_owned(), table)])
        };
        follow_plugins(&enabled, &options(1));
        let _running = slots().try_acquire_arc().expect("the one slot");

        follow_plugins(&enabled, &options(2));
        let wider = slots();
        let taken = [wider.try_acquire_arc(), wider.try_acquire_arc()];
        assert!(
            taken.iter().all(Option::is_some),
            "the new limit did not apply"
        );
    }
}

/// The provider as maki builds it, on the fake `claude`.
#[cfg(all(test, target_os = "linux"))]
mod on_the_fake {
    use std::env;
    use std::fs::{self, File};
    use std::iter;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use futures_lite::{FutureExt, future};
    use serde_json::{Value, json};
    use smol::Timer;
    use tempfile::tempdir;
    use test_case::test_case;

    use super::error::Error;
    use super::fake::{Fake, HAIKU, IDLE, MARKER, OPUS, SYSTEM, VERSION_ERROR, WAIT, tools};
    use super::run::{Limits, Listed, Thinking};
    use super::{ClaudeCode, Exchange, slots};
    use crate::{Message, Model, StreamResponse};

    const HAIKU_SPEC: &str = "claude-code/claude-haiku-4-5";
    /// Haiku's output limit in the model table.
    const HAIKU_MAX_OUTPUT: &str = "64000";
    const COMPACTION_BUDGET: u32 = 16_384;
    const FITTED_BUDGET: u32 = 500;
    const INSIDE_TEMP: &str = "tmp";
    const PLUGIN_LOGIN: &str = "/plugin/claude-config";
    const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";
    const CLAUDE_DIR: &str = ".claude";
    /// A model only a saved list has. The list is older than 24 hours.
    const SAVED_MODEL: &str = "claude-saved-1";
    const SAVED_LIST: &str = "models.json";
    const OTHER_LOGIN: &str = "/elsewhere/.claude";
    const STALE_LIST: Duration = Duration::from_secs(2 * 86_400);
    const PATH: &str = "PATH";
    const HOME: &str = "HOME";
    const TMPDIR: &str = "TMPDIR";

    /// maki's output budget for a request reaches Claude Code as its limit,
    /// through the provider as maki calls it, wherever the budget came from.
    #[test_case(None => HAIKU_MAX_OUTPUT.to_owned() ; "the_model_limit")]
    #[test_case(Some(COMPACTION_BUDGET) => COMPACTION_BUDGET.to_string() ; "a_compaction_budget")]
    #[test_case(Some(FITTED_BUDGET) => FITTED_BUDGET.to_string() ; "a_budget_cut_to_fit")]
    fn the_output_budget_reaches_claude_code(turn_budget: Option<u32>) -> String {
        let fake = Fake::new("text");
        let spec = Model::from_spec(HAIKU_SPEC).unwrap();
        let model =
            turn_budget.map_or_else(|| spec.clone(), |budget| spec.with_turn_output(budget));
        through_the_provider(&fake, &model, &fake.project(), &env::temp_dir()).unwrap();
        fake.log("max_output")
    }

    /// An ACP client can pass the directory through a link, while Claude Code
    /// reports the resolved directory it runs in.
    #[test]
    fn a_session_dir_named_through_a_link_is_accepted() {
        let fake = Fake::new("text");
        let links = tempdir().unwrap();
        let linked = links.path().join("project");
        symlink(fake.project(), &linked).unwrap();
        let model = Model::from_spec(HAIKU_SPEC).unwrap();

        through_the_provider(&fake, &model, &linked, &env::temp_dir()).unwrap();
    }

    /// `TMPDIR` can resolve into the project through a symlink. Refuse that path before a
    /// policy hook can run there.
    #[test]
    fn a_temp_dir_that_leads_into_the_project_is_refused() {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join("policy_hook"), "").unwrap();
        let inside = fake.project().join(INSIDE_TEMP);
        fs::create_dir(&inside).unwrap();
        let links = tempdir().unwrap();
        let temp = links.path().join(INSIDE_TEMP);
        symlink(&inside, &temp).unwrap();
        let model = Model::from_spec(HAIKU_SPEC).unwrap();

        let err = through_the_provider(&fake, &model, &fake.project(), &temp).unwrap_err();
        assert!(matches!(err, Error::TempInProject(_)), "{err}");
        assert!(!fake.dir.path().join("pid").exists(), "Claude Code started");
        assert_eq!(fake.log("hook_ran_in"), "", "a hook ran");
    }

    /// Returns the provider as maki builds it, on the fake, with `home` and
    /// `temp` as `$HOME` and `$TMPDIR`, keeping its model list at
    /// `models_cache`.
    fn provider_for(
        fake: &Fake,
        home: &Path,
        temp: &Path,
        models_cache: Option<PathBuf>,
    ) -> ClaudeCode {
        let vars = vec![
            (PATH.to_owned(), env::var(PATH).unwrap()),
            (HOME.to_owned(), home.display().to_string()),
            (TMPDIR.to_owned(), temp.display().to_string()),
        ];
        ClaudeCode {
            executable: fake.executable(),
            limits: Limits::new(IDLE),
            environment: Box::new(move || vars.clone()),
            models_cache,
            config_dir: None,
        }
    }

    /// The plugin's `config_dir` picks the login, and Claude Code gets it as
    /// its only `CLAUDE_CONFIG_DIR`.
    #[test]
    fn the_plugins_config_dir_names_the_login() {
        let fake = Fake::new("text");
        let home = tempdir().unwrap();
        let mut provider = provider_for(&fake, home.path(), &env::temp_dir(), None);
        provider.config_dir = Some(PathBuf::from(PLUGIN_LOGIN));

        let prepared = provider.prepare(&fake.project()).unwrap();
        let given: Vec<&str> = prepared
            .env
            .iter()
            .filter(|(name, _)| name == CONFIG_DIR_VAR)
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(given, [PLUGIN_LOGIN]);
    }

    /// Claude Code and the checks treat an empty `CLAUDE_CONFIG_DIR` as unset,
    /// so the child never gets it.
    #[test]
    fn an_empty_config_dir_stays_out_of_the_child() {
        let fake = Fake::new("text");
        let home = tempdir().unwrap();
        let mut provider = provider_for(&fake, home.path(), &env::temp_dir(), None);
        let mut vars = (provider.environment)();
        vars.push((CONFIG_DIR_VAR.to_owned(), String::new()));
        provider.environment = Box::new(move || vars.clone());

        let prepared = provider.prepare(&fake.project()).unwrap();
        assert!(
            prepared.env.iter().all(|(name, _)| name != CONFIG_DIR_VAR),
            "{:?}",
            prepared.env
        );
        assert_eq!(prepared.config_dir, home.path().join(CLAUDE_DIR));
    }

    /// Cache reuse must depend on age, executable and login. A cache hit must start no
    /// Claude Code process.
    #[test_case(Duration::ZERO, None, false ; "a_fresh_list")]
    #[test_case(STALE_LIST, None, true ; "a_stale_list")]
    #[test_case(Duration::ZERO, Some(OTHER_LOGIN), true ; "a_list_of_another_login")]
    fn a_listing_answers_from_a_list_saved_within_a_day(
        age: Duration,
        login: Option<&str>,
        asks: bool,
    ) {
        let fake = Fake::new("text");
        let home = tempdir().unwrap();
        let cache = home.path().join(SAVED_LIST);
        let login = login.map_or_else(|| home.path().join(CLAUDE_DIR), PathBuf::from);
        let saved = json!({ "executable": fake.executable(), "config_dir": login, "models": [{ "id": SAVED_MODEL, "window": null }] });
        fs::write(&cache, saved.to_string()).unwrap();
        File::options()
            .write(true)
            .open(&cache)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
        let provider = provider_for(&fake, home.path(), &env::temp_dir(), Some(cache.clone()));

        let listed = smol::block_on(provider.models(&fake.project())).unwrap();
        let want = if asks {
            vec![OPUS, HAIKU]
        } else {
            vec![SAVED_MODEL]
        };
        let ids: Vec<&str> = listed.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, want);
        assert_eq!(
            fake.dir.path().join("pid").exists(),
            asks,
            "Claude Code started"
        );
        let saved: Value = serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
        let saved_ids: Vec<&str> = saved["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str())
            .collect();
        assert_eq!(saved_ids, want);
    }

    /// A refresh waiting for a slot does not hold up a listing that its saved
    /// list answers.
    #[test]
    fn a_saved_list_answers_while_a_refresh_waits_for_a_slot() {
        let fake = Fake::new("text");
        let home = tempdir().unwrap();
        let cache = home.path().join(SAVED_LIST);
        let login = home.path().join(CLAUDE_DIR);
        let saved = json!({ "executable": fake.executable(), "config_dir": login, "models": [{ "id": SAVED_MODEL, "window": null }] });
        fs::write(&cache, saved.to_string()).unwrap();
        let provider = provider_for(&fake, home.path(), &env::temp_dir(), Some(cache));
        let cwd = fake.project();
        let slots = slots();
        let _held: Vec<_> = iter::from_fn(|| slots.try_acquire_arc()).collect();

        let listed = smol::block_on(
            async {
                let _ = provider.list(&cwd, true).await;
                None
            }
            .or(async { Some(provider.list(&cwd, false).await) })
            .or(async {
                Timer::after(WAIT).await;
                None
            }),
        );
        let ids: Vec<String> = listed
            .expect("the saved list must answer while the refresh waits")
            .unwrap()
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, [SAVED_MODEL]);
    }

    /// Two listings at once start Claude Code once, and the second reads what
    /// the first saved.
    #[test]
    fn two_listings_at_once_ask_claude_code_once() {
        let fake = Fake::new("text");
        let home = tempdir().unwrap();
        let cache = Some(home.path().join(SAVED_LIST));
        let provider = provider_for(&fake, home.path(), &env::temp_dir(), cache);
        let cwd = fake.project();

        let (first, second) =
            smol::block_on(future::zip(provider.models(&cwd), provider.models(&cwd)));
        let ids = |listed: Vec<Listed>| -> Vec<String> {
            listed.into_iter().map(|model| model.id).collect()
        };
        assert_eq!(ids(first.unwrap()), [OPUS, HAIKU]);
        assert_eq!(ids(second.unwrap()), [OPUS, HAIKU]);
        assert_eq!(fake.log("versions").lines().count(), 1);
    }

    /// A failed discovery must suppress repeat CLI launches until expiry or an explicit
    /// refresh.
    #[test]
    fn a_failed_listing_is_remembered_until_a_refresh() {
        let fake = Fake::new("text");
        fs::write(fake.dir.path().join("version_error"), VERSION_ERROR).unwrap();
        let home = tempdir().unwrap();
        let cache = Some(home.path().join(SAVED_LIST));
        let provider = provider_for(&fake, home.path(), &env::temp_dir(), cache);
        let cwd = fake.project();

        let first = smol::block_on(provider.models(&cwd)).unwrap_err();
        let again = smol::block_on(provider.models(&cwd)).unwrap_err();
        assert!(!matches!(first, Error::ListedRecently { .. }), "{first}");
        assert!(matches!(again, Error::ListedRecently { .. }), "{again}");
        assert_eq!(fake.log("versions").lines().count(), 1);

        let _ = smol::block_on(provider.list(&cwd, true));
        assert_eq!(fake.log("versions").lines().count(), 2);
    }

    /// Sends one request through the provider as maki calls it, from `cwd`,
    /// with a separate home directory and `temp` as `$TMPDIR`.
    fn through_the_provider(
        fake: &Fake,
        model: &Model,
        cwd: &Path,
        temp: &Path,
    ) -> Result<StreamResponse, Error> {
        let home = tempdir().unwrap();
        let provider = provider_for(fake, home.path(), temp, None);
        let tools = tools();
        let messages = [Message::user(format!("find {MARKER}"))];
        let (events, _received) = flume::unbounded();
        let exchange = Exchange {
            system: SYSTEM,
            messages: &messages,
            tools: &tools,
            events: &events,
        };
        smol::block_on(provider.request(model, Thinking::Default, exchange, cwd))
    }
}
