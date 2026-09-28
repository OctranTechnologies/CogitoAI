//! Unified, provider-neutral model catalog with durable offline fallback.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    provider_from_config, ModelCapabilities, ModelConfig, ModelDescriptor, ModelProvider,
    ProviderKind, ReasoningEffort,
};

const CACHE_SCHEMA: u32 = 1;
const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    #[default]
    Unknown,
    Discovered,
    Cached,
    ManuallyConfigured,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKnowledge {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    TextInput,
    Vision,
    Streaming,
    ToolCalling,
    ParallelToolCalls,
    Reasoning,
    ConfigurableReasoningEffort,
    StructuredOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub capability: ModelCapability,
    pub support: CapabilityKnowledge,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistryFilter {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub requirements: Vec<CapabilityRequirement>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Decimal USD per million input tokens, kept as text to avoid rounding.
    #[serde(default)]
    pub input_usd_per_million_tokens: Option<String>,
    /// Decimal USD per million output tokens, kept as text to avoid rounding.
    #[serde(default)]
    pub output_usd_per_million_tokens: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    #[serde(default)]
    pub source: ModelMetadataSource,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub refreshed_at_unix: Option<u64>,
    #[serde(default)]
    pub capabilities: ModelCapabilityKnowledgeMap,
    #[serde(default)]
    pub reasoning_levels: Option<Vec<ReasoningEffort>>,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilityKnowledgeMap {
    #[serde(default)]
    pub text_input: CapabilityKnowledge,
    #[serde(default)]
    pub vision: CapabilityKnowledge,
    #[serde(default)]
    pub streaming: CapabilityKnowledge,
    #[serde(default)]
    pub tool_calling: CapabilityKnowledge,
    #[serde(default)]
    pub parallel_tool_calls: CapabilityKnowledge,
    #[serde(default)]
    pub reasoning: CapabilityKnowledge,
    #[serde(default)]
    pub configurable_reasoning_effort: CapabilityKnowledge,
    #[serde(default)]
    pub structured_output: CapabilityKnowledge,
}

impl ModelCapabilityKnowledgeMap {
    pub fn get(&self, capability: ModelCapability) -> CapabilityKnowledge {
        match capability {
            ModelCapability::TextInput => self.text_input,
            ModelCapability::Vision => self.vision,
            ModelCapability::Streaming => self.streaming,
            ModelCapability::ToolCalling => self.tool_calling,
            ModelCapability::ParallelToolCalls => self.parallel_tool_calls,
            ModelCapability::Reasoning => self.reasoning,
            ModelCapability::ConfigurableReasoningEffort => self.configurable_reasoning_effort,
            ModelCapability::StructuredOutput => self.structured_output,
        }
    }

    fn record_known_support(&mut self, capabilities: &ModelCapabilities) {
        let record = |known: &mut CapabilityKnowledge, supported: bool| {
            if *known == CapabilityKnowledge::Unknown && supported {
                *known = CapabilityKnowledge::Supported;
            }
        };
        record(&mut self.text_input, capabilities.text_input);
        record(&mut self.vision, capabilities.image_input);
        record(&mut self.streaming, capabilities.streaming);
        record(&mut self.tool_calling, capabilities.tool_calling);
        record(
            &mut self.parallel_tool_calls,
            capabilities.parallel_tool_calls,
        );
        record(&mut self.reasoning, capabilities.reasoning);
        record(
            &mut self.configurable_reasoning_effort,
            capabilities.configurable_reasoning_effort,
        );
        record(&mut self.structured_output, capabilities.structured_output);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogRefreshResult {
    pub provider_id: String,
    pub model_count: usize,
    /// Provider failures are isolated so other catalogs can still refresh.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogRefreshReport {
    pub providers: Vec<CatalogRefreshResult>,
    pub available_model_count: usize,
}

impl CatalogRefreshReport {
    pub fn failed_provider_count(&self) -> usize {
        self.providers
            .iter()
            .filter(|provider| provider.error.is_some())
            .count()
    }

    pub fn succeeded_provider_count(&self) -> usize {
        self.providers
            .iter()
            .filter(|provider| provider.error.is_none())
            .count()
    }
}

#[derive(Debug, Error)]
pub enum ModelRegistryError {
    #[error("model provider '{0}' is not registered")]
    ProviderNotRegistered(String),
    #[error("provider catalog refresh failed: {0}")]
    Provider(String),
    #[error("model catalog cache could not be read or written")]
    Cache,
    #[error("model provider configuration is invalid")]
    Configuration,
}

struct ProviderEntry {
    provider: Arc<dyn ModelProvider>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CatalogRecord {
    refreshed_at_unix: u64,
    models: Vec<ModelDescriptor>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RegistryState {
    catalogs: BTreeMap<String, CatalogRecord>,
    manual_models: BTreeMap<String, ModelDescriptor>,
    defaults: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RegistryCacheFile {
    schema: u32,
    #[serde(default)]
    state: RegistryState,
}

/// Combines model catalogs while keeping provider transports behind
/// [`ModelProvider`]. Refresh failures preserve the last successful data.
pub struct ModelRegistry {
    providers: RwLock<BTreeMap<String, ProviderEntry>>,
    state: Mutex<RegistryState>,
    cache_path: RwLock<Option<PathBuf>>,
    persist_lock: Mutex<()>,
    cache_ttl: Duration,
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRegistry")
            .field("providers", &self.provider_ids())
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new(None, DEFAULT_TTL)
    }
}

impl ModelRegistry {
    pub fn new(cache_path: Option<PathBuf>, cache_ttl: Duration) -> Self {
        let mut state = cache_path
            .as_deref()
            .and_then(load_cache)
            .unwrap_or_default();
        mark_cached_records(&mut state, cache_ttl, unix_now());
        Self {
            providers: RwLock::new(BTreeMap::new()),
            state: Mutex::new(state),
            cache_path: RwLock::new(cache_path),
            persist_lock: Mutex::new(()),
            cache_ttl,
        }
    }

    /// Builds a registry for all providers compiled into this runtime. It
    /// performs no network access; the selected model remains manually present
    /// even when its provider cannot be reached.
    pub fn with_builtins(active: &ModelConfig, cache_path: Option<PathBuf>) -> Self {
        Self::with_builtins_using(active, cache_path, None)
    }

    /// Builds the built-in catalogs using the runtime's environment/keychain
    /// credential source. This only constructs adapters and performs no HTTP.
    pub fn with_builtins_and_credentials(
        active: &ModelConfig,
        cache_path: Option<PathBuf>,
        store: &dyn super::CredentialStore,
    ) -> Self {
        Self::with_builtins_using(active, cache_path, Some(store))
    }

    fn with_builtins_using(
        active: &ModelConfig,
        cache_path: Option<PathBuf>,
        store: Option<&dyn super::CredentialStore>,
    ) -> Self {
        let registry = Self::new(cache_path, DEFAULT_TTL);
        for kind in [
            ProviderKind::Mock,
            ProviderKind::OpenAi,
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
            ProviderKind::OpenCodeZen,
            ProviderKind::OpenCodeGo,
        ] {
            let config = if active.provider == kind {
                active.clone()
            } else {
                ModelConfig::for_provider(kind)
            };
            let provider_id = provider_id(kind);
            let provider = store.map_or_else(
                || provider_from_config(&config),
                |store| super::provider_from_config_with_store(&config, store),
            );
            if let Ok(provider) = provider {
                registry.register_provider(provider_id, Arc::from(provider));
            }
            registry.set_default_model(provider_id, config.model.clone());
            if active.provider == kind {
                registry.register_manual_model(provider_id, config.model, None);
            }
        }
        registry
    }

    /// Registers another catalog source, allowing future providers to join the
    /// registry without changing its aggregation or RPC interfaces.
    pub fn register_provider(
        &self,
        provider_id: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
    ) {
        self.providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider_id.into(), ProviderEntry { provider });
    }

    /// Makes an explicitly configured provider/model available even when no
    /// discovery record contains it. This never validates against a catalog.
    pub fn register_configured_model(
        &self,
        config: &ModelConfig,
    ) -> Result<(), ModelRegistryError> {
        let provider =
            provider_from_config(config).map_err(|_| ModelRegistryError::Configuration)?;
        let provider_id = provider_id(config.provider);
        self.providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                provider_id.to_owned(),
                ProviderEntry {
                    provider: Arc::from(provider),
                },
            );
        self.set_default_model(provider_id, config.model.clone());
        self.register_manual_model(provider_id, config.model.clone(), None);
        Ok(())
    }

    /// Rebuilds the configured provider adapter using the runtime credential
    /// store. Called after a credential is connected or disconnected so the
    /// registry's next refresh uses the new authentication state.
    pub fn register_configured_model_with_credentials(
        &self,
        config: &ModelConfig,
        store: &dyn super::CredentialStore,
    ) -> Result<(), ModelRegistryError> {
        let provider = super::provider_from_config_with_store(config, store)
            .map_err(|_| ModelRegistryError::Configuration)?;
        let provider_id = provider_id(config.provider);
        self.providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                provider_id.to_owned(),
                ProviderEntry {
                    provider: Arc::from(provider),
                },
            );
        self.set_default_model(provider_id, config.model.clone());
        self.register_manual_model(provider_id, config.model.clone(), None);
        Ok(())
    }

    pub fn set_default_model(&self, provider_id: impl Into<String>, model_id: impl Into<String>) {
        let provider_id = provider_id.into();
        let model_id = model_id.into();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.defaults.insert(provider_id.clone(), model_id.clone());
        let key = canonical_key(&provider_id, &model_id);
        state
            .manual_models
            .entry(key)
            .or_insert_with(|| manual_descriptor(&provider_id, &model_id, None));
        drop(state);
        let _ = self.persist();
    }

    pub fn default_model(&self, provider_id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .defaults
            .get(provider_id)
            .cloned()
    }

    pub fn defaults(&self) -> BTreeMap<String, String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .defaults
            .clone()
    }

    pub fn register_manual_model(
        &self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
        display_name: Option<String>,
    ) {
        let provider_id = provider_id.into();
        let model_id = model_id.into();
        let key = canonical_key(&provider_id, &model_id);
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .manual_models
            .entry(key)
            .or_insert_with(|| manual_descriptor(&provider_id, &model_id, display_name));
        let _ = self.persist();
    }

    /// Changes the durable cache location, loading any existing workspace
    /// cache. This is called when the runtime binds to its workspace.
    pub fn set_cache_path(&self, cache_path: PathBuf) {
        let cached = load_cache(&cache_path);
        *self
            .cache_path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cache_path);
        if let Some(mut cached) = cached {
            mark_cached_records(&mut cached, self.cache_ttl, unix_now());
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (provider, record) in cached.catalogs.iter_mut() {
                state
                    .catalogs
                    .entry(provider.clone())
                    .or_insert_with(|| record.clone());
            }
            for (key, model) in cached.manual_models {
                state.manual_models.entry(key).or_insert(model);
            }
            for (provider, model) in cached.defaults {
                state.defaults.entry(provider).or_insert(model);
            }
        }
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    pub fn models(&self, filter: &ModelRegistryFilter) -> Vec<ModelDescriptor> {
        let now = unix_now();
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut models = BTreeMap::new();
        for (provider_id, record) in &state.catalogs {
            let stale = self.is_stale_at(record.refreshed_at_unix, now);
            for descriptor in &record.models {
                let mut descriptor = descriptor.clone();
                descriptor.provider.clone_from(provider_id);
                descriptor.metadata.source = if stale {
                    ModelMetadataSource::Cached
                } else {
                    descriptor.metadata.source
                };
                descriptor.metadata.stale = stale;
                descriptor.metadata.refreshed_at_unix = Some(record.refreshed_at_unix);
                record_capability_knowledge(&mut descriptor);
                models.insert(descriptor.canonical_id(), descriptor);
            }
        }
        for descriptor in state.manual_models.values() {
            let key = descriptor.canonical_id();
            models.entry(key).or_insert_with(|| descriptor.clone());
        }
        models
            .into_values()
            .filter(|descriptor| matches_filter(descriptor, filter))
            .collect()
    }

    pub fn refresh_provider(
        &self,
        provider_id: &str,
    ) -> Result<Vec<ModelDescriptor>, ModelRegistryError> {
        let provider = self
            .providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(provider_id)
            .map(|entry| Arc::clone(&entry.provider))
            .ok_or_else(|| ModelRegistryError::ProviderNotRegistered(provider_id.to_owned()))?;
        let mut models = provider
            .refresh_models()
            .map_err(|error| ModelRegistryError::Provider(error.to_string()))?;
        for model in &mut models {
            model.provider = provider_id.to_owned();
            record_capability_knowledge(model);
            model.metadata.source = ModelMetadataSource::Discovered;
            model.metadata.stale = false;
            model.metadata.refreshed_at_unix = Some(unix_now());
        }
        models.sort_by(|left, right| left.id.cmp(&right.id));
        let refreshed_at_unix = unix_now();
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .catalogs
            .insert(
                provider_id.to_owned(),
                CatalogRecord {
                    refreshed_at_unix,
                    models: models.clone(),
                },
            );
        self.persist()?;
        Ok(models)
    }

    /// Refreshes every registered provider independently. Failed providers
    /// retain their prior successful cache and are reported alongside success.
    pub fn refresh_all(&self) -> CatalogRefreshReport {
        let provider_ids = self.provider_ids();
        let mut results = Vec::with_capacity(provider_ids.len());
        for provider_id in provider_ids {
            match self.refresh_provider(&provider_id) {
                Ok(models) => results.push(CatalogRefreshResult {
                    provider_id,
                    model_count: models.len(),
                    error: None,
                }),
                Err(error) => results.push(CatalogRefreshResult {
                    provider_id,
                    model_count: 0,
                    error: Some(error.to_string()),
                }),
            }
        }
        CatalogRefreshReport {
            providers: results,
            available_model_count: self.models(&ModelRegistryFilter::default()).len(),
        }
    }

    pub fn refresh_provider_report(&self, provider_id: &str) -> CatalogRefreshReport {
        let result = match self.refresh_provider(provider_id) {
            Ok(models) => CatalogRefreshResult {
                provider_id: provider_id.to_owned(),
                model_count: models.len(),
                error: None,
            },
            Err(error) => CatalogRefreshResult {
                provider_id: provider_id.to_owned(),
                model_count: 0,
                error: Some(error.to_string()),
            },
        };
        CatalogRefreshReport {
            providers: vec![result],
            available_model_count: self.models(&ModelRegistryFilter::default()).len(),
        }
    }

    fn is_stale_at(&self, refreshed_at_unix: u64, now: u64) -> bool {
        now.saturating_sub(refreshed_at_unix) > self.cache_ttl.as_secs()
    }

    fn persist(&self) -> Result<(), ModelRegistryError> {
        let _write_guard = self
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cache_path = self
            .cache_path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(cache_path) = cache_path else {
            return Ok(());
        };
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let data = serde_json::to_vec_pretty(&RegistryCacheFile {
            schema: CACHE_SCHEMA,
            state,
        })
        .map_err(|_| ModelRegistryError::Cache)?;
        let parent = cache_path.parent().ok_or(ModelRegistryError::Cache)?;
        fs::create_dir_all(parent).map_err(|_| ModelRegistryError::Cache)?;
        let temporary = cache_path.with_extension("json.tmp");
        fs::write(&temporary, data).map_err(|_| ModelRegistryError::Cache)?;
        fs::rename(&temporary, cache_path).map_err(|_| ModelRegistryError::Cache)
    }
}

fn load_cache(path: &std::path::Path) -> Option<RegistryState> {
    let bytes = fs::read(path).ok()?;
    let file: RegistryCacheFile = serde_json::from_slice(&bytes).ok()?;
    if file.schema != CACHE_SCHEMA {
        return None;
    }
    Some(file.state)
}

fn mark_cached_records(state: &mut RegistryState, ttl: Duration, now: u64) {
    for record in state.catalogs.values_mut() {
        for model in &mut record.models {
            model.metadata.source = ModelMetadataSource::Cached;
            model.metadata.refreshed_at_unix = Some(record.refreshed_at_unix);
            model.metadata.stale = now.saturating_sub(record.refreshed_at_unix) > ttl.as_secs();
        }
    }
    for descriptor in state.manual_models.values_mut() {
        descriptor.metadata.source = ModelMetadataSource::ManuallyConfigured;
    }
}

fn provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::OpenCodeGo => "opencode-go",
    }
}

fn canonical_key(provider_id: &str, model_id: &str) -> String {
    let model_id = model_id
        .strip_prefix(provider_id)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(model_id);
    format!("{provider_id}/{model_id}")
}

fn manual_descriptor(
    provider_id: &str,
    model_id: &str,
    display_name: Option<String>,
) -> ModelDescriptor {
    let mut descriptor = ModelDescriptor {
        provider: provider_id.to_owned(),
        id: model_id.to_owned(),
        display_name: display_name.unwrap_or_else(|| model_id.to_owned()),
        capabilities: ModelCapabilities {
            text_input: false,
            ..ModelCapabilities::default()
        },
        metadata: ModelMetadata {
            source: ModelMetadataSource::ManuallyConfigured,
            ..ModelMetadata::default()
        },
    };
    descriptor.metadata.stale = false;
    descriptor
}

fn record_capability_knowledge(descriptor: &mut ModelDescriptor) {
    descriptor
        .metadata
        .capabilities
        .record_known_support(&descriptor.capabilities);
}

fn matches_filter(descriptor: &ModelDescriptor, filter: &ModelRegistryFilter) -> bool {
    if filter
        .provider_id
        .as_ref()
        .is_some_and(|provider_id| descriptor.provider != *provider_id)
    {
        return false;
    }
    filter.requirements.iter().all(|requirement| {
        descriptor.metadata.capabilities.get(requirement.capability) == requirement.support
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContentBlock, FinishReason, ModelRequest, ModelResponse, ModelStreamEvent, ProviderError,
        ToolCall, Usage,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CatalogProvider {
        provider_id: &'static str,
        models: Vec<ModelDescriptor>,
        fail: AtomicBool,
    }

    impl CatalogProvider {
        fn new(provider_id: &'static str, models: Vec<ModelDescriptor>) -> Self {
            Self {
                provider_id,
                models,
                fail: AtomicBool::new(false),
            }
        }

        fn fail(&self) {
            self.fail.store(true, Ordering::SeqCst);
        }
    }

    impl ModelProvider for CatalogProvider {
        fn descriptor(&self) -> ModelDescriptor {
            self.models
                .first()
                .cloned()
                .unwrap_or_else(|| descriptor(self.provider_id, "default", false))
        }

        fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
            if self.fail.load(Ordering::SeqCst) {
                Err(ProviderError::Transport {
                    provider: "test-provider",
                })
            } else {
                Ok(self.models.clone())
            }
        }

        fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
            self.discover_models()
        }

        fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
            Ok(ModelResponse {
                id: "test".to_owned(),
                model: request.model.clone(),
                content: vec![ContentBlock::Text {
                    text: "ok".to_owned(),
                }],
                tool_calls: vec![ToolCall {
                    id: "call".to_owned(),
                    name: "noop".to_owned(),
                    arguments: json!({}),
                }],
                finish_reason: FinishReason::Stop,
                usage: Some(Usage::new(1, 1)),
            })
        }

        fn stream(
            &self,
            _request: &ModelRequest,
            _on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::Transport {
                provider: "test-provider",
            })
        }
    }

    fn descriptor(provider: &str, id: &str, tool_calling: bool) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.to_owned(),
            id: id.to_owned(),
            display_name: format!("{provider} {id}"),
            capabilities: ModelCapabilities {
                text_input: true,
                streaming: true,
                tool_calling,
                ..ModelCapabilities::default()
            },
            metadata: ModelMetadata::default(),
        }
    }

    #[test]
    fn duplicate_model_ids_remain_distinct_by_provider() {
        let registry = ModelRegistry::default();
        registry.register_provider(
            "openai",
            Arc::new(CatalogProvider::new(
                "openai",
                vec![descriptor("openai", "shared", true)],
            )),
        );
        registry.register_provider(
            "anthropic",
            Arc::new(CatalogProvider::new(
                "anthropic",
                vec![descriptor("anthropic", "shared", false)],
            )),
        );
        let report = registry.refresh_all();
        assert_eq!(report.succeeded_provider_count(), 2);
        let models = registry.models(&ModelRegistryFilter::default());
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].canonical_id(), "anthropic/shared");
        assert_eq!(models[1].canonical_id(), "openai/shared");
    }

    #[test]
    fn partial_provider_failure_preserves_success_and_reports_unavailable_source() {
        let registry = ModelRegistry::default();
        let ready = Arc::new(CatalogProvider::new(
            "openai",
            vec![descriptor("openai", "ready", true)],
        ));
        let unavailable = Arc::new(CatalogProvider::new(
            "gemini",
            vec![descriptor("gemini", "known", false)],
        ));
        unavailable.fail();
        registry.register_provider("openai", ready);
        registry.register_provider("gemini", unavailable);
        let report = registry.refresh_all();
        assert_eq!(report.succeeded_provider_count(), 1);
        assert_eq!(report.failed_provider_count(), 1);
        assert_eq!(registry.models(&ModelRegistryFilter::default()).len(), 1);
        assert!(report.providers.iter().any(|result| {
            result.provider_id == "gemini"
                && result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("provider test-provider transport failed"))
        }));
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_successful_provider_catalog() {
        let registry = ModelRegistry::default();
        let provider = Arc::new(CatalogProvider::new(
            "openai",
            vec![descriptor("openai", "last-known-good", true)],
        ));
        let provider_source: Arc<dyn ModelProvider> = provider.clone();
        registry.register_provider("openai", provider_source);
        assert_eq!(registry.refresh_provider("openai").unwrap().len(), 1);
        provider.fail();

        let report = registry.refresh_provider_report("openai");
        assert_eq!(report.failed_provider_count(), 1);
        let available = registry.models(&ModelRegistryFilter::default());
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "last-known-good");
        assert_eq!(
            available[0].metadata.source,
            ModelMetadataSource::Discovered
        );
    }

    #[test]
    fn stale_cache_is_retained_and_marked_after_expiration() {
        let path = temp_cache_path("stale");
        let mut state = RegistryState::default();
        state.catalogs.insert(
            "openai".to_owned(),
            CatalogRecord {
                refreshed_at_unix: 1,
                models: vec![descriptor("openai", "cached", true)],
            },
        );
        write_cache(&path, state);
        let registry = ModelRegistry::new(Some(path.clone()), Duration::from_secs(1));
        let models = registry.models(&ModelRegistryFilter::default());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].metadata.source, ModelMetadataSource::Cached);
        assert!(models[0].metadata.stale);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn manual_models_survive_catalog_misses_and_capability_filtering_is_conservative() {
        let registry = ModelRegistry::default();
        let known = Arc::new(CatalogProvider::new(
            "openai",
            vec![descriptor("openai", "tool-model", true)],
        ));
        registry.register_provider("openai", known);
        registry.register_manual_model("openai", "custom-valid-id", None);
        registry.refresh_provider("openai").unwrap();

        let all = registry.models(&ModelRegistryFilter::default());
        assert_eq!(all.len(), 2);
        let manual = all
            .iter()
            .find(|model| model.id == "custom-valid-id")
            .unwrap();
        assert_eq!(
            manual.metadata.source,
            ModelMetadataSource::ManuallyConfigured
        );
        assert_eq!(
            manual.metadata.capabilities.tool_calling,
            CapabilityKnowledge::Unknown
        );
        let tools = registry.models(&ModelRegistryFilter {
            provider_id: None,
            requirements: vec![CapabilityRequirement {
                capability: ModelCapability::ToolCalling,
                support: CapabilityKnowledge::Supported,
            }],
        });
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "tool-model");
    }

    #[test]
    fn refresh_updates_catalog_and_persists_for_offline_startup() {
        let path = temp_cache_path("offline");
        let registry = ModelRegistry::new(Some(path.clone()), Duration::from_secs(60));
        registry.register_provider(
            "openai",
            Arc::new(CatalogProvider::new(
                "openai",
                vec![descriptor("openai", "cached-model", true)],
            )),
        );
        let report = registry.refresh_provider_report("openai");
        assert_eq!(report.succeeded_provider_count(), 1);
        assert_eq!(registry.models(&ModelRegistryFilter::default()).len(), 1);

        let offline = ModelRegistry::new(Some(path.clone()), Duration::from_secs(60));
        let models = offline.models(&ModelRegistryFilter::default());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "cached-model");
        assert_eq!(models[0].metadata.source, ModelMetadataSource::Cached);
        assert!(!models[0].metadata.stale);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn builtins_load_offline_defaults_without_requesting_provider_networks() {
        let config = ModelConfig::for_provider(ProviderKind::OpenCodeGo);
        let registry = ModelRegistry::with_builtins(&config, None);
        assert_eq!(
            registry.default_model("opencode-go").as_deref(),
            Some("opencode-go/glm-5.3")
        );
        let models = registry.models(&ModelRegistryFilter::default());
        assert!(models
            .iter()
            .any(|model| model.canonical_id() == "opencode-go/glm-5.3"));
        assert!(models
            .iter()
            .find(|model| model.canonical_id() == "opencode-go/glm-5.3")
            .is_some_and(|model| model.metadata.source == ModelMetadataSource::ManuallyConfigured));
    }

    fn temp_cache_path(label: &str) -> PathBuf {
        let unique = format!(
            "cogito-model-registry-{label}-{}-{}.json",
            std::process::id(),
            unix_now()
        );
        std::env::temp_dir().join(unique)
    }

    fn write_cache(path: &std::path::Path, state: RegistryState) {
        fs::write(
            path,
            serde_json::to_vec(&RegistryCacheFile {
                schema: CACHE_SCHEMA,
                state,
            })
            .unwrap(),
        )
        .unwrap();
    }
}
