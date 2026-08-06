use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use oc_auth::Credential;
use oc_error::BoxSource;
use oc_llm::catalog::resolved::{ResolvedModel, ResolvedProvider};

/// Context passed to a provider model loader.
#[derive(Debug, Clone, Copy)]
pub struct ProviderHookContext<'a> {
    pub auth: Option<&'a Credential>,
}

/// Live callback behind `ProviderHook.models` (`index.ts:214-217`).
#[async_trait]
pub trait ProviderModelLoader: Send + Sync {
    async fn models(
        &self,
        provider: &ResolvedProvider,
        context: ProviderHookContext<'_>,
    ) -> Result<BTreeMap<String, ResolvedModel>, BoxSource>;
}

/// Custom provider resource registered by a plugin.
#[derive(Clone)]
pub struct ProviderHook {
    pub id: String,
    pub models: Option<Arc<dyn ProviderModelLoader>>,
}
