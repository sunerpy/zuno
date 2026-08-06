use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use oc_auth::{Credential, Secret};
use oc_error::BoxSource;
use oc_llm::catalog::resolved::{JsonMap, ResolvedProvider};

/// Values collected from an auth method's prompts.
pub type AuthInputs = BTreeMap<String, String>;

/// Comparison used by a prompt's `when` condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRuleOperator {
    Eq,
    NotEq,
}

/// Declarative prompt visibility rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRule {
    pub key: String,
    pub operator: AuthRuleOperator,
    pub value: String,
}

/// Synchronous validation callback retained by the resident plugin host.
pub type AuthTextValidator = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Deprecated imperative prompt condition retained for compatibility.
pub type AuthPromptCondition = Arc<dyn Fn(&AuthInputs) -> bool + Send + Sync>;

/// One item in a select prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSelectOption {
    pub label: String,
    pub value: String,
    pub hint: Option<String>,
}

/// OAuth/API prompt union from `index.ts:95-149`.
#[derive(Clone)]
pub enum AuthPrompt {
    Text {
        key: String,
        message: String,
        placeholder: Option<String>,
        validate: Option<AuthTextValidator>,
        condition: Option<AuthPromptCondition>,
        when: Option<AuthRule>,
    },
    Select {
        key: String,
        message: String,
        options: Vec<AuthSelectOption>,
        condition: Option<AuthPromptCondition>,
        when: Option<AuthRule>,
    },
}

/// Deferred credential lookup passed to an auth loader.
#[async_trait]
pub trait AuthCredentialResolver: Send + Sync {
    async fn resolve(&self) -> Result<Option<Credential>, BoxSource>;
}

/// Optional auth provider-options loader (`index.ts:90`).
#[async_trait]
pub trait AuthLoader: Send + Sync {
    async fn load(
        &self,
        auth: &dyn AuthCredentialResolver,
        provider: &ResolvedProvider,
    ) -> Result<JsonMap, BoxSource>;
}

/// Result of an API-key authorization callback.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthApiResult {
    Success {
        key: Secret,
        provider: Option<String>,
        metadata: Option<BTreeMap<String, Secret>>,
    },
    Failed,
}

/// Successful result of an OAuth callback.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthSuccess {
    OAuth {
        provider: Option<String>,
        refresh: Secret,
        access: Secret,
        expires: u64,
        account_id: Option<String>,
        enterprise_url: Option<String>,
    },
    ApiKey {
        provider: Option<String>,
        key: Secret,
        metadata: Option<BTreeMap<String, Secret>>,
    },
}

/// OAuth callback success/failure union.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthCallbackResult {
    Success(AuthSuccess),
    Failed,
}

/// Callback for an automatic OAuth flow.
#[async_trait]
pub trait AuthAutoCallback: Send + Sync {
    async fn callback(&self) -> Result<AuthCallbackResult, BoxSource>;
}

/// Callback for a code-entry OAuth flow.
#[async_trait]
pub trait AuthCodeCallback: Send + Sync {
    async fn callback(&self, code: &str) -> Result<AuthCallbackResult, BoxSource>;
}

/// The callback arm selected by an OAuth authorization start.
#[derive(Clone)]
pub enum AuthOAuthCallback {
    Auto(Arc<dyn AuthAutoCallback>),
    Code(Arc<dyn AuthCodeCallback>),
}

/// OAuth authorization start plus its live callback handle.
#[derive(Clone)]
pub struct AuthOAuthResult {
    pub url: String,
    pub instructions: String,
    pub callback: AuthOAuthCallback,
}

/// Starts an OAuth authorization flow.
#[async_trait]
pub trait AuthOAuthAuthorizer: Send + Sync {
    async fn authorize(&self, inputs: Option<&AuthInputs>) -> Result<AuthOAuthResult, BoxSource>;
}

/// Runs an optional API-key authorization flow.
#[async_trait]
pub trait AuthApiAuthorizer: Send + Sync {
    async fn authorize(&self, inputs: Option<&AuthInputs>) -> Result<AuthApiResult, BoxSource>;
}

/// One authentication method advertised by a plugin.
#[derive(Clone)]
pub enum AuthMethod {
    OAuth {
        label: String,
        prompts: Vec<AuthPrompt>,
        authorize: Arc<dyn AuthOAuthAuthorizer>,
    },
    Api {
        label: String,
        prompts: Vec<AuthPrompt>,
        authorize: Option<Arc<dyn AuthApiAuthorizer>>,
    },
}

/// Provider authentication resource registered by a plugin.
#[derive(Clone)]
pub struct AuthHook {
    pub provider: String,
    pub loader: Option<Arc<dyn AuthLoader>>,
    pub methods: Vec<AuthMethod>,
}
