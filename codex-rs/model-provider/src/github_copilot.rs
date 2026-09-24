//! Experimental, explicitly configured GitHub Copilot Responses provider.

use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_api::AuthError;
use codex_api::AuthProvider;
use codex_api::AuthProviderFuture;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::Request;
use codex_http_client::RequestBody;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::ClientRedirectPolicy;
use codex_login::github_copilot;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::cache::ModelsCache;
use codex_models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use http::HeaderMap;
use http::HeaderValue;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use url::Url;

use crate::ResolvedResponsesProvider;
use crate::WorkspaceRoutingContext;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;
use crate::provider::ProviderCapabilities;
use crate::provider::ProviderUnauthorizedRecovery;
use crate::provider::RemoteCompactionSupport;

const MAX_MODELS_BYTES: usize = 1024 * 1024;
const MAX_MODELS: usize = 64;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(8);
const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub(crate) struct GithubCopilotProvider {
    info: ModelProviderInfo,
    home: Option<PathBuf>,
}

impl GithubCopilotProvider {
    pub(crate) fn new(info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
        Self {
            info,
            home: auth_manager.map(|manager| manager.runtime_config().codex_home),
        }
    }

    fn home(&self) -> codex_protocol::error::Result<&Path> {
        self.home.as_deref().ok_or_else(|| {
            CodexErr::UnsupportedOperation("GitHub Copilot requires an auth runtime".to_string())
        })
    }
}

struct CopilotAuth {
    home: PathBuf,
}

impl AuthProvider for CopilotAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}

    fn apply_auth(&self, request: Request) -> AuthProviderFuture<'_> {
        Box::pin(async move {
            let credential = github_copilot::current(&self.home)
                .await
                .map_err(|error| AuthError::Transient(error.to_string()))?;
            let url = Url::parse(&request.url)
                .map_err(|_| AuthError::Build("invalid Copilot request URL".to_string()))?;
            let expected = Url::parse(credential.base_url())
                .map_err(|_| AuthError::Build("invalid Copilot API endpoint".to_string()))?;
            if url.origin() != expected.origin() {
                return Err(AuthError::Build(
                    "Copilot request URL does not match its authenticated API origin".to_string(),
                ));
            }
            let mut request = request;
            let mut authorization =
                HeaderValue::from_str(&format!("Bearer {}", credential.api_token()))
                    .map_err(|_| AuthError::Build("invalid GitHub OAuth token".to_string()))?;
            authorization.set_sensitive(true);
            request
                .headers
                .insert(http::header::AUTHORIZATION, authorization);
            request.headers.insert(
                "x-initiator",
                HeaderValue::from_static(if is_user_turn(request.body.as_ref()) {
                    "user"
                } else {
                    "agent"
                }),
            );
            Ok(request)
        })
    }
}

fn is_user_turn(body: Option<&RequestBody>) -> bool {
    let value = match body {
        Some(RequestBody::Json(value)) => Some(Cow::Borrowed(value)),
        Some(RequestBody::EncodedJson(body)) if body.as_bytes().len() < 1024 * 1024 => {
            serde_json::from_slice(body.as_bytes()).ok().map(Cow::Owned)
        }
        Some(RequestBody::Raw(body)) if body.len() < 1024 * 1024 => {
            serde_json::from_slice(body).ok().map(Cow::Owned)
        }
        _ => None,
    };
    value
        .as_deref()
        .and_then(|body| body.get("input"))
        .and_then(serde_json::Value::as_array)
        .and_then(|input| input.last())
        .and_then(|last| last.get("role"))
        .and_then(serde_json::Value::as_str)
        == Some("user")
}

impl ModelProvider for GithubCopilotProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            namespace_tools: false,
            image_generation: false,
            web_search: false,
            external_web_access: false,
            remote_compaction: RemoteCompactionSupport::Unsupported,
        }
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        None
    }

    fn recover_from_unauthorized(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<ProviderUnauthorizedRecovery>> {
        Box::pin(async move {
            github_copilot::refresh(self.home()?)
                .await
                .map_err(|error| CodexErr::UnsupportedOperation(error.to_string()))?;
            Ok(ProviderUnauthorizedRecovery::Recovered)
        })
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async { None })
    }

    fn account_state(&self) -> ProviderAccountResult {
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        })
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, codex_protocol::error::Result<Provider>> {
        Box::pin(async move {
            let credential = github_copilot::current(self.home()?)
                .await
                .map_err(|error| CodexErr::UnsupportedOperation(error.to_string()))?;
            let mut provider = self.info.to_api_provider(/*auth_mode*/ None)?;
            provider.base_url = credential.base_url().to_string();
            provider.headers.insert(
                "Openai-Intent",
                HeaderValue::from_static("conversation-edits"),
            );
            provider
                .headers
                .insert("x-initiator", HeaderValue::from_static("agent"));
            provider.headers.insert(
                "X-GitHub-Api-Version",
                HeaderValue::from_static("2026-06-01"),
            );
            provider
                .headers
                .insert(http::header::USER_AGENT, HeaderValue::from_static("codex"));
            Ok(provider)
        })
    }

    fn responses_api_provider<'a>(
        &'a self,
        _routing_context: &'a WorkspaceRoutingContext,
    ) -> ModelProviderFuture<'a, codex_protocol::error::Result<ResolvedResponsesProvider>> {
        Box::pin(async move {
            Ok(ResolvedResponsesProvider {
                provider: self.api_provider().await?,
                redirect_policy: ClientRedirectPolicy::Reject,
            })
        })
    }

    fn runtime_base_url(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<Option<String>>> {
        Box::pin(async move { Ok(Some(self.api_provider().await?.base_url)) })
    }

    fn api_auth(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<SharedAuthProvider>> {
        Box::pin(async move {
            Ok(Arc::new(CopilotAuth {
                home: self.home()?.to_path_buf(),
            }) as SharedAuthProvider)
        })
    }

    fn models_manager(
        &self,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        match config_model_catalog {
            Some(catalog) => Arc::new(StaticModelsManager::new(None, catalog)),
            None => Arc::new(CopilotModelsManager::new(
                Some(codex_home.clone()),
                Some(codex_home),
            )),
        }
    }

    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        match config_model_catalog {
            Some(catalog) => Arc::new(StaticModelsManager::new(None, catalog)),
            None => Arc::new(CopilotModelsManager::new(self.home.clone(), None)),
        }
    }

    fn models_manager_with_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
        _cache: Arc<dyn ModelsCache>,
    ) -> SharedModelsManager {
        self.models_manager_without_cache(config_model_catalog)
    }
}

#[derive(Debug, Deserialize)]
struct CatalogResponse {
    data: Vec<CopilotModel>,
}

#[derive(Debug, Deserialize)]
struct CopilotModel {
    id: String,
    name: String,
    model_picker_enabled: bool,
    supported_endpoints: Option<Vec<String>>,
    policy: Option<ModelPolicy>,
    capabilities: ModelCapabilities,
}

#[derive(Debug, Deserialize)]
struct ModelPolicy {
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelCapabilities {
    limits: Option<ModelLimits>,
    supports: ModelSupports,
}

#[derive(Debug, Deserialize)]
struct ModelLimits {
    max_context_window_tokens: Option<i64>,
    max_prompt_tokens: Option<i64>,
    max_output_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ModelSupports {
    tool_calls: Option<bool>,
    vision: Option<bool>,
    reasoning_effort: Option<Vec<String>>,
}

fn convert_model(model: CopilotModel, priority: i32) -> Option<ModelInfo> {
    if model.id.is_empty()
        || model.id.len() > 128
        || !model
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:".contains(&byte))
        || model.name.is_empty()
        || model.name.len() > 128
        || !model.model_picker_enabled
        || model
            .policy
            .as_ref()
            .and_then(|policy| policy.state.as_deref())
            == Some("disabled")
        || !model
            .supported_endpoints
            .as_ref()
            .is_some_and(|endpoints| endpoints.iter().any(|endpoint| endpoint == "/responses"))
        || model.capabilities.supports.tool_calls != Some(true)
    {
        return None;
    }
    let limits = model.capabilities.limits?;
    if limits.max_output_tokens.unwrap_or_default() <= 0
        || limits.max_prompt_tokens.unwrap_or_default() <= 0
        || limits.max_prompt_tokens.unwrap_or_default() > 2_000_000
    {
        return None;
    }
    let mut info = model_info_from_slug(&model.id);
    info.used_fallback_model_metadata = false;
    info.slug = model.id;
    info.display_name = model.name;
    info.visibility = ModelVisibility::List;
    info.priority = priority;
    info.context_window = limits
        .max_context_window_tokens
        .zip(limits.max_prompt_tokens)
        .map(|(context, prompt)| context.min(prompt))
        .or(limits.max_prompt_tokens);
    info.max_context_window = info.context_window;
    info.input_modalities = vec![InputModality::Text];
    if model.capabilities.supports.vision == Some(true) {
        info.input_modalities.push(InputModality::Image);
    }
    info.supported_reasoning_levels = model
        .capabilities
        .supports
        .reasoning_effort
        .unwrap_or_default()
        .into_iter()
        .take(8)
        .filter_map(|effort| {
            if effort.len() > 32 {
                return None;
            }
            let effort = effort.parse::<ReasoningEffort>().ok()?;
            Some(ReasoningEffortPreset {
                description: effort.to_string(),
                effort,
            })
        })
        .collect();
    info.default_reasoning_level = info
        .supported_reasoning_levels
        .iter()
        .find(|preset| preset.effort == ReasoningEffort::Medium)
        .or_else(|| info.supported_reasoning_levels.first())
        .map(|preset| preset.effort.clone());
    info.supports_reasoning_summary_parameter = !info.supported_reasoning_levels.is_empty();
    Some(info)
}

fn convert_catalog(response: CatalogResponse) -> Vec<ModelInfo> {
    response
        .data
        .into_iter()
        .enumerate()
        .filter_map(|(index, model)| convert_model(model, index as i32))
        .take(MAX_MODELS)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogSnapshot {
    identity: String,
    fetched_at: u64,
    models: Vec<ModelInfo>,
}

impl CatalogSnapshot {
    fn is_fresh(&self) -> bool {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|elapsed| {
                elapsed.as_secs() >= self.fetched_at
                    && elapsed.as_secs() - self.fetched_at < CACHE_TTL.as_secs()
            })
    }
}

fn cached_catalog(home: &Path) -> std::io::Result<Option<CatalogSnapshot>> {
    github_copilot::load_models_cache(home)?
        .map(|value| serde_json::from_str(&value).map_err(std::io::Error::other))
        .transpose()
}

fn store_catalog(home: &Path, catalog: &CatalogSnapshot) -> std::io::Result<()> {
    let value = serde_json::to_string(catalog).map_err(std::io::Error::other)?;
    github_copilot::save_models_cache(home, &value)
}

#[derive(Debug)]
struct CopilotModelsManager {
    home: Option<PathBuf>,
    cache_home: Option<PathBuf>,
    snapshot: RwLock<Option<CatalogSnapshot>>,
    last_warning: RwLock<Option<String>>,
}

impl CopilotModelsManager {
    fn new(home: Option<PathBuf>, cache_home: Option<PathBuf>) -> Self {
        Self {
            home,
            cache_home,
            snapshot: RwLock::new(None),
            last_warning: RwLock::new(None),
        }
    }

    async fn verified_models(&self) -> Vec<ModelInfo> {
        let Some(home) = self.home.as_ref() else {
            return Vec::new();
        };
        let Ok(Some(credential)) = github_copilot::load(home) else {
            return Vec::new();
        };
        let identity = credential.account_identity();
        if let Some(snapshot) = self.snapshot.read().await.as_ref()
            && snapshot.identity == identity
        {
            return snapshot.models.clone();
        }
        if let Some(home) = self.cache_home.as_ref()
            && let Ok(Some(snapshot)) = cached_catalog(home)
            && snapshot.identity == identity
        {
            *self.snapshot.write().await = Some(snapshot.clone());
            return snapshot.models;
        }
        Vec::new()
    }

    async fn refresh(&self, factory: HttpClientFactory) -> std::io::Result<Vec<ModelInfo>> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Copilot auth runtime is unavailable"))?;
        let credential = github_copilot::current(home).await?;
        let origin = Url::parse(credential.base_url()).map_err(std::io::Error::other)?;
        let url = origin.join("models").map_err(std::io::Error::other)?;
        let client = RouteAwareClientPool::new_without_redirects_or_request_logging(
            factory,
            ClientRouteClass::Api,
        )
        .into_client();
        let response = tokio::time::timeout(REFRESH_TIMEOUT, async {
            let mut response = client
                .get(url)
                .bearer_auth(credential.api_token())
                .header("X-GitHub-Api-Version", "2026-06-01")
                .header("User-Agent", "codex")
                .send()
                .await
                .map_err(std::io::Error::other)?;
            if !response.status().is_success() {
                return Err(std::io::Error::other(format!(
                    "Copilot model catalog returned HTTP {}",
                    response.status()
                )));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(std::io::Error::other)? {
                if body.len().saturating_add(chunk.len()) > MAX_MODELS_BYTES {
                    return Err(std::io::Error::other("Copilot model catalog is too large"));
                }
                body.extend_from_slice(&chunk);
            }
            serde_json::from_slice::<CatalogResponse>(&body).map_err(std::io::Error::other)
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Copilot model refresh timed out",
            )
        })??;
        let models = convert_catalog(response);
        if models.is_empty() {
            return Err(std::io::Error::other(
                "Copilot returned no usable Responses models",
            ));
        }
        let snapshot = CatalogSnapshot {
            identity: credential.account_identity(),
            fetched_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(std::io::Error::other)?
                .as_secs(),
            models: models.clone(),
        };
        if let Some(home) = self.cache_home.as_ref()
            && let Err(error) = store_catalog(home, &snapshot)
        {
            eprintln!("Could not cache GitHub Copilot models: {error}");
        }
        *self.snapshot.write().await = Some(snapshot);
        Ok(models)
    }
}

impl ModelsManager for CopilotModelsManager {
    fn last_refresh_warning(&self) -> ModelsManagerFuture<'_, Option<String>> {
        Box::pin(async move { self.last_warning.read().await.clone() })
    }

    fn refresh_if_new_etag(
        &self,
        _etag: String,
        factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move {
            if let Err(error) = self.refresh(factory).await {
                eprintln!("GitHub Copilot model refresh failed: {error}");
            }
        })
    }

    fn raw_model_catalog(
        &self,
        strategy: RefreshStrategy,
        factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(async move {
            let cached = self.verified_models().await;
            let fresh = self
                .snapshot
                .read()
                .await
                .as_ref()
                .is_some_and(CatalogSnapshot::is_fresh);
            if strategy != RefreshStrategy::Offline
                && (strategy == RefreshStrategy::Online || cached.is_empty() || !fresh)
            {
                match self.refresh(factory).await {
                    Ok(models) => {
                        *self.last_warning.write().await = None;
                        return ModelsResponse { models };
                    }
                    Err(error) => {
                        let warning = format!(
                            "GitHub Copilot model refresh failed: {error}. {}",
                            if cached.is_empty() {
                                "No previously validated model catalog is available."
                            } else {
                                "Using the last validated model catalog."
                            }
                        );
                        eprintln!("{warning}");
                        *self.last_warning.write().await = Some(warning);
                    }
                }
            } else {
                *self.last_warning.write().await = None;
            }
            ModelsResponse { models: cached }
        })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(self.verified_models())
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        Ok(self
            .snapshot
            .try_read()?
            .as_ref()
            .map_or_else(Vec::new, |snapshot| snapshot.models.clone()))
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        None
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }
}

#[cfg(test)]
#[path = "github_copilot_tests.rs"]
mod tests;
