//! Experimental Claude Code provider and shared protocol metadata.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
use linux::create;
#[cfg(target_os = "linux")]
pub use linux::{Error, configure_plugin, validation};
#[cfg(not(target_os = "linux"))]
use unsupported::create;
#[cfg(not(target_os = "linux"))]
pub use unsupported::{Error, configure_plugin};

use crate::model::ModelFamily;
use crate::providers::anthropic;
use crate::spec::{
    AuthDoc, Build, CatalogDoc, GeneratedDocs, NO_CURATED_MODELS, Native, ProviderSpec, api_decides,
};

pub(crate) const SLUG: &str = "claude-code";

pub(crate) const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

const DISPLAY_NAME: &str = "Claude Code (experimental)";

pub const PLUGIN: &str = "claude_code";

const NO_KEY_ENV: &str = "";

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
    supports_deferred_tools: false,
    adaptive_effort: api_decides,
    thinking_fields_fallback: |_| None,
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
