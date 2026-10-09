use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use flume::Sender;
use serde_json::Value;
use tracing::{debug, warn};

use maki_config::ModelPolicy;
use maki_storage::id::SessionRef;

use crate::model::{Model, ModelInfo};
use crate::model_registry::set_known_models;
use crate::providers::catalog::{
    available_if_warm, catalog_providers, catalog_providers_if_available, try_create,
};
use crate::providers::{KeyRotation, Timeouts, custom, plugin};
use crate::spec::{Owner, ProviderRegistry, ProviderSpec};
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a request runs: its session, if any, and that session's working
/// directory. A provider shared by all sessions cannot know this, and one that
/// starts a process per request needs the directory. Cancelling a request
/// drops its future, which stops it.
#[derive(Clone, Debug)]
pub struct RequestScope<'a> {
    pub session_id: Option<&'a SessionRef>,
    pub cwd: &'a Path,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ModelListing {
    #[default]
    Cached,
    Refresh,
}

pub trait Provider: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        scope: RequestScope<'a>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>>;

    fn list_models(
        &self,
        listing: ModelListing,
    ) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>>;

    /// Fetch provider-side usage quota (remaining percentage / reset times).
    /// `Ok(None)` means the provider does not expose a programmatic usage endpoint.
    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    /// The keys this provider rotates through, and where the current one lives.
    /// `None` is one fixed credential that never changes. This is the only hook
    /// for rotation: both the count and the swap come off it, so they can never
    /// describe different pools, which is what a per-provider `rotate_key` let
    /// happen before.
    fn keys(&self) -> Option<KeyRotation<'_>> {
        None
    }

    fn adjust_model(&self, _model: &mut Model) {}
}

pub fn provider_for_slug(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    match Owner::of(slug) {
        Owner::Builtin(new) => new(timeouts),
        Owner::Plugin => plugin::create(slug, timeouts),
        Owner::Custom => custom::create(slug, timeouts),
        Owner::Catalog | Owner::Unknown => try_create(slug, timeouts).unwrap_or_else(|| {
            Err(AgentError::Config {
                message: format!("unknown provider '{slug}'"),
            })
        }),
    }
}

pub fn provider_available(slug: &str) -> bool {
    provider_for_slug(slug, Timeouts::default()).is_ok()
}

/// Non-blocking variant of [`provider_available`] for offline model discovery:
/// catalog-backed slugs consult only the already-warm catalog, so a cold cache
/// reports them unavailable instead of blocking on a network fetch.
fn provider_available_offline(slug: &str) -> bool {
    match Owner::of(slug) {
        Owner::Builtin(_) | Owner::Plugin | Owner::Custom => provider_available(slug),
        Owner::Catalog | Owner::Unknown => available_if_warm(slug),
    }
}

pub fn from_model(model: &mut Model, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let provider = provider_for_slug(&model.provider, timeouts)?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %model.id, "provider created");
    Ok(provider)
}

/// Adjust a model against its provider's static table without retaining the
/// provider. Used to reconcile a resumed model so it matches one started
/// fresh (e.g. inherited thinking support for a routed Aperture model).
pub fn adjust_model(model: &mut Model, timeouts: Timeouts) -> Result<(), AgentError> {
    provider_for_slug(&model.provider, timeouts)?.adjust_model(model);
    Ok(())
}

pub fn from_model_fallback(model: &mut Model, timeouts: Timeouts) -> Box<dyn Provider> {
    match from_model(model, timeouts) {
        Ok(provider) => provider,
        Err(e) => {
            warn!(error = %e, "provider creation failed, using unconfigured provider");
            Box::new(UnconfiguredProvider)
        }
    }
}

struct UnconfiguredProvider;

const STATIC_FALLBACK: &str = "using static fallback";
const NO_FALLBACK: &str = "there is no model list";

/// A provider that uses another provider's model table has no curated rows of its own.
fn failed_listing(spec: &ProviderSpec, error: &AgentError) -> ModelBatch {
    let models: Vec<String> = spec
        .listed_rows()
        .iter()
        .flat_map(|entry| entry.prefixes.iter())
        .map(|prefix| format!("{}/{prefix}", spec.slug))
        .collect();
    let fallback = if models.is_empty() {
        NO_FALLBACK
    } else {
        STATIC_FALLBACK
    };
    warn!(provider = spec.slug, %error, "maki cannot get the model list, {fallback}");
    ModelBatch {
        models,
        warnings: vec![format!("{}: {error} ({fallback})", spec.display_name)],
    }
}

const NOT_CONFIGURED: &str = "no provider configured — run /login or `maki auth login`";

impl Provider for UnconfiguredProvider {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        _tools: &'a Value,
        _event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _scope: RequestScope<'a>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async {
            Err(AgentError::Config {
                message: NOT_CONFIGURED.to_string(),
            })
        })
    }

    fn list_models(
        &self,
        _listing: ModelListing,
    ) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async {
            Err(AgentError::Config {
                message: NOT_CONFIGURED.to_string(),
            })
        })
    }
}

pub async fn from_model_async(
    model: &mut Model,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let slug = Arc::clone(&model.provider);
    let id = model.id.clone();
    let provider = smol::unblock(move || provider_for_slug(&slug, timeouts)).await?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %id, "provider created");
    Ok(provider)
}

pub struct ModelBatch {
    pub models: Vec<String>,
    pub warnings: Vec<String>,
}

/// The offline twin of [`fetch_all_models`]. It never waits for the catalog
/// download, so catalog-backed providers only show up once the catalog has
/// warmed in the background.
pub fn available_model_specs(policy: &ModelPolicy) -> Vec<String> {
    let mut specs: Vec<String> = ProviderRegistry::all()
        .into_iter()
        .filter(|m| provider_available_offline(m.slug))
        .flat_map(|m| {
            m.listed_rows()
                .iter()
                .flat_map(|entry| entry.prefixes.iter())
                .map(move |p| format!("{}/{}", m.slug, p))
        })
        .collect();
    for spec in custom::declared_model_specs() {
        if !specs.contains(&spec) {
            specs.push(spec);
        }
    }
    if let Some(catalog) = catalog_providers_if_available() {
        for cat in catalog {
            // Any slug the registry knows was listed above; see
            // `spec::tests::builtins_are_the_native_and_catalog_backed_slugs`.
            if ProviderRegistry::get(&cat.slug).is_some()
                || matches!(Owner::of(&cat.slug), Owner::Plugin | Owner::Custom)
            {
                continue;
            }
            if !provider_available(&cat.slug) {
                continue;
            }
            for model_id in cat.models.keys() {
                let spec = format!("{}/{}", cat.slug, model_id);
                if !specs.contains(&spec) {
                    specs.push(spec);
                }
            }
        }
    }
    specs.retain(|spec| policy.allows(spec));
    specs
}

pub async fn fetch_all_models(
    policy: &ModelPolicy,
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn FnOnce() + Send>>,
    listing: ModelListing,
) {
    let (tx, rx) = flume::unbounded();
    let timeouts = Timeouts::default();

    for spec in ProviderRegistry::all() {
        let slug = spec.slug;
        let Ok(provider) = smol::unblock(move || provider_for_slug(slug, timeouts)).await else {
            warn!(provider = slug, "failed to create provider, skipping");
            continue;
        };
        let tx = tx.clone();
        let listing = listing.clone();
        smol::spawn(async move {
            let listed = provider.list_models(listing).await;
            let batch = match listed {
                Ok(models) => {
                    let mut specs: Vec<String> =
                        models.iter().map(|m| format!("{slug}/{}", m.id)).collect();
                    set_known_models(slug, models);
                    for entry in spec.listed_rows() {
                        for prefix in &entry.prefixes {
                            let spec = format!("{slug}/{prefix}");
                            if !specs.contains(&spec) {
                                specs.push(spec);
                            }
                        }
                    }
                    ModelBatch {
                        models: specs,
                        warnings: Vec::new(),
                    }
                }
                Err(e) => failed_listing(spec, &e),
            };
            let _ = tx.send_async(batch).await;
        })
        .detach();
    }

    let tx_catalog = tx.clone();
    smol::spawn(async move {
        let catalog = smol::unblock(catalog_providers).await;
        for cat in catalog {
            // No `Owner::Custom` here, unlike `available_model_specs` above:
            // a long-standing asymmetry, changing it is a behaviour change.
            if ProviderRegistry::get(&cat.slug).is_some()
                || matches!(Owner::of(&cat.slug), Owner::Plugin)
            {
                continue;
            }
            if !provider_available(&cat.slug) {
                continue;
            }
            let slug = cat.slug;
            let models: Vec<String> = cat.models.keys().map(|id| format!("{slug}/{id}")).collect();
            let _ = tx_catalog
                .send_async(ModelBatch {
                    models,
                    warnings: Vec::new(),
                })
                .await;
        }
    })
    .detach();

    let tx_custom = tx.clone();
    smol::spawn(async move {
        let declared = custom::declared_model_specs();
        if !declared.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    models: declared,
                    warnings: Vec::new(),
                })
                .await;
        }
        let custom_specs = smol::unblock(move || custom::discover_models(timeouts)).await;
        if !custom_specs.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    models: custom_specs,
                    warnings: Vec::new(),
                })
                .await;
        }
    })
    .detach();

    drop(tx);

    while let Ok(mut batch) = rx.recv_async().await {
        batch.models.retain(|spec| policy.allows(spec));
        on_ready(batch);
    }
    if let Some(done) = on_done {
        done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{anthropic, claude_code};

    /// Only a provider with its own rows falls back to them after an error.
    #[test_case::test_case(&anthropic::SPEC => (false, STATIC_FALLBACK) ; "a_provider_with_its_own_rows")]
    #[test_case::test_case(&claude_code::SPEC => (true, NO_FALLBACK) ; "one_that_runs_anothers_models")]
    fn a_failed_listing_says_whether_it_fell_back(spec: &ProviderSpec) -> (bool, &'static str) {
        let batch = failed_listing(
            spec,
            &AgentError::Config {
                message: "down".into(),
            },
        );
        let [warning] = batch.warnings.as_slice() else {
            panic!("{:?}", batch.warnings);
        };
        let said = [STATIC_FALLBACK, NO_FALLBACK]
            .into_iter()
            .find(|fallback| warning.contains(fallback))
            .unwrap();
        (batch.models.is_empty(), said)
    }

    fn policy(allowed: &[&str], excluded: &[&str]) -> ModelPolicy {
        ModelPolicy::new(
            &allowed
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
            &excluded
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn available_specs_apply_model_policy() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-model-policy") };
        let policy = policy(&["openai/*"], &["*/gpt-5.6-terra"]);

        let specs = available_model_specs(&policy);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        assert!(!specs.is_empty());
        assert!(specs.iter().all(|spec| spec.starts_with("openai/")));
        assert!(!specs.iter().any(|spec| spec == "openai/gpt-5.6-terra"));
    }

    #[test]
    fn provider_for_slug_unknown_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        crate::providers::catalog::warm_empty_catalog_for_tests(maki_storage::StateDir::from_path(
            tmp.path().to_path_buf(),
        ));
        let result = provider_for_slug("nonexistent-provider-xyz", Timeouts::default());
        match result {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("unknown provider"),
                    "expected 'unknown provider' message, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for unknown provider"),
        }
    }
}
