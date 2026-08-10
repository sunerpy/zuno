//! Rebuilding `crate::AuthHook` and `crate::ProviderHook` from live handles.
//!
//! Todo 57 already models these resources as trait objects holding callbacks —
//! `AuthTextValidator`, `AuthLoader`, `AuthOAuthAuthorizer`, `ProviderModelLoader`.
//! This module supplies the implementations that call back into the resident
//! JavaScript process. Nothing is copied out of the plugin: every callable field
//! becomes a [`crate::js::JsHandle`] and every invocation is a round trip.
//!
//! # The shape of the descriptors
//!
//! The shim encodes an auth hook as JSON with `{"$fn": id}` wherever the plugin had
//! a function. Measured against the two real plugins:
//!
//! ```text
//! kiro-auth   provider="kiro-auth"  loader=$fn  4 methods, 6 prompt validators
//! google      provider="google"     loader=$fn  2 methods, 0 prompt validators
//! ```
//!
//! `condition` never appears — consistent with the draft's finding that
//! `condition:` occurs zero times in either dist — so the deprecated arm is decoded
//! only because the type has it, not because anything uses it.
//!
//! # Why `validate` is the load-bearing case
//!
//! `AuthTextValidator` is a synchronous `Fn(&str) -> Option<String>`, because that
//! is what `validate?: (value: string) => string | undefined` is
//! (`packages/plugin/src/index.ts` text-prompt arm). It cannot be satisfied by
//! marshalled data: the closure has to run inside the plugin. Rebuilding it as a
//! blocking round trip is the concrete proof that the host is resident, and
//! `js_validate_closure_round_trips_from_rust` asserts a real one.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use oc_auth::Secret;
use oc_error::BoxSource;
use oc_llm::catalog::resolved::{JsonMap, ResolvedModel, ResolvedProvider};
use serde_json::{Value, json};

use crate::js::host::{JsHandle, JsHost};
use crate::{
    AuthApiAuthorizer, AuthApiResult, AuthAutoCallback, AuthCallbackResult, AuthCredentialResolver,
    AuthHook, AuthInputs, AuthLoader, AuthMethod, AuthOAuthAuthorizer, AuthOAuthCallback,
    AuthOAuthResult, AuthPrompt, AuthRule, AuthRuleOperator, AuthSelectOption, AuthSuccess,
    ProviderHook, ProviderHookContext, ProviderModelLoader,
};

/// A descriptor the shim produced that this host cannot use.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeError {
    #[error("plugin `{plugin}` returned an auth hook with no `provider` string")]
    MissingProvider { plugin: String },
    #[error("plugin `{plugin}` returned an auth method with no `type`; expected `oauth` or `api`")]
    MissingMethodType { plugin: String },
    #[error("plugin `{plugin}` returned auth method type `{found}`; expected `oauth` or `api`")]
    UnknownMethodType { plugin: String, found: String },
    #[error("plugin `{plugin}` returned an `oauth` auth method with no `authorize` callback")]
    MissingAuthorize { plugin: String },
    #[error("plugin `{plugin}` returned a provider hook with no `id` string")]
    MissingProviderId { plugin: String },
    #[error(
        "plugin `{plugin}` returned an OAuth result with method `{found}`; \
         expected `auto` or `code`"
    )]
    UnknownOAuthMethod { plugin: String, found: String },
}

/// Rebuild every auth hook the plugin registered.
///
/// # Errors
/// Returns [`BridgeError`] naming the plugin and the malformed field. A descriptor
/// that cannot be understood is reported rather than silently dropped: a missing
/// provider is exactly the "my provider disappeared" symptom this whole todo exists
/// to make explicable.
pub fn auth_hooks(host: &JsHost, descriptors: &[Value]) -> Result<Vec<AuthHook>, BridgeError> {
    descriptors
        .iter()
        .map(|descriptor| auth_hook(host, descriptor))
        .collect()
}

fn auth_hook(host: &JsHost, descriptor: &Value) -> Result<AuthHook, BridgeError> {
    let plugin = host.plugin().to_owned();
    let provider = descriptor
        .get("provider")
        .and_then(Value::as_str)
        .ok_or_else(|| BridgeError::MissingProvider {
            plugin: plugin.clone(),
        })?
        .to_owned();
    let loader = descriptor
        .get("loader")
        .and_then(|value| host.handle(value))
        .map(|handle| -> Arc<dyn AuthLoader> {
            Arc::new(HandleAuthLoader {
                host: host.clone(),
                handle,
            })
        });
    let methods = descriptor
        .get("methods")
        .and_then(Value::as_array)
        .map(|methods| {
            methods
                .iter()
                .map(|method| auth_method(host, method))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(AuthHook {
        provider,
        loader,
        methods,
    })
}

fn auth_method(host: &JsHost, descriptor: &Value) -> Result<AuthMethod, BridgeError> {
    let plugin = host.plugin().to_owned();
    let label = descriptor
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let prompts = descriptor
        .get("prompts")
        .and_then(Value::as_array)
        .map(|prompts| prompts.iter().map(|p| auth_prompt(host, p)).collect())
        .unwrap_or_default();
    let authorize = descriptor.get("authorize").and_then(|v| host.handle(v));
    match descriptor.get("type").and_then(Value::as_str) {
        Some("oauth") => {
            let handle = authorize.ok_or(BridgeError::MissingAuthorize {
                plugin: plugin.clone(),
            })?;
            Ok(AuthMethod::OAuth {
                label,
                prompts,
                authorize: Arc::new(HandleOAuthAuthorizer {
                    host: host.clone(),
                    handle,
                }),
            })
        }
        Some("api") => Ok(AuthMethod::Api {
            label,
            prompts,
            authorize: authorize.map(|handle| -> Arc<dyn AuthApiAuthorizer> {
                Arc::new(HandleApiAuthorizer {
                    host: host.clone(),
                    handle,
                })
            }),
        }),
        Some(found) => Err(BridgeError::UnknownMethodType {
            plugin,
            found: found.to_owned(),
        }),
        None => Err(BridgeError::MissingMethodType { plugin }),
    }
}

fn auth_prompt(host: &JsHost, descriptor: &Value) -> AuthPrompt {
    let key = string_field(descriptor, "key");
    let message = string_field(descriptor, "message");
    let when = descriptor.get("when").and_then(auth_rule);
    // The deprecated imperative arm. Decoded because the type carries it; measured
    // as absent from both real plugins.
    let condition = descriptor.get("condition").and_then(|value| {
        host.handle(value).map(|handle| {
            let host = host.clone();
            Arc::new(move |inputs: &AuthInputs| {
                let payload = json!(
                    inputs
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                        .collect::<serde_json::Map<_, _>>()
                );
                host.call_blocking(&handle, vec![payload])
                    .map(|value| value.as_bool().unwrap_or(true))
                    .unwrap_or(true)
            }) as crate::AuthPromptCondition
        })
    });
    if descriptor.get("type").and_then(Value::as_str) == Some("select") {
        return AuthPrompt::Select {
            key,
            message,
            options: descriptor
                .get("options")
                .and_then(Value::as_array)
                .map(|options| options.iter().map(select_option).collect())
                .unwrap_or_default(),
            condition,
            when,
        };
    }
    AuthPrompt::Text {
        key,
        message,
        placeholder: descriptor
            .get("placeholder")
            .and_then(Value::as_str)
            .map(str::to_owned),
        validate: descriptor.get("validate").and_then(|value| {
            host.handle(value).map(|handle| {
                let host = host.clone();
                Arc::new(move |value: &str| {
                    match host.call_blocking(&handle, vec![Value::String(value.to_owned())]) {
                        // `undefined` arrives as JSON null: the value is valid.
                        Ok(Value::Null) => None,
                        Ok(Value::String(message)) => Some(message),
                        Ok(other) => other.as_str().map(str::to_owned),
                        // A validator that cannot be reached must not silently
                        // accept the input; the message names the failure so the
                        // user does not see an unexplained rejection.
                        Err(error) => Some(error.to_string()),
                    }
                }) as crate::AuthTextValidator
            })
        }),
        condition,
        when,
    }
}

fn select_option(descriptor: &Value) -> AuthSelectOption {
    AuthSelectOption {
        label: string_field(descriptor, "label"),
        value: string_field(descriptor, "value"),
        hint: descriptor
            .get("hint")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn auth_rule(descriptor: &Value) -> Option<AuthRule> {
    let key = descriptor.get("key")?.as_str()?.to_owned();
    let value = descriptor.get("value")?.as_str()?.to_owned();
    let operator = match descriptor.get("operator").and_then(Value::as_str) {
        Some("!=" | "neq" | "notEq") => AuthRuleOperator::NotEq,
        _ => AuthRuleOperator::Eq,
    };
    Some(AuthRule {
        key,
        operator,
        value,
    })
}

/// Rebuild every provider hook the plugin registered.
///
/// # Errors
/// Returns [`BridgeError::MissingProviderId`] when a descriptor has no `id`.
pub fn provider_hooks(
    host: &JsHost,
    descriptors: &[Value],
) -> Result<Vec<ProviderHook>, BridgeError> {
    descriptors
        .iter()
        .map(|descriptor| {
            let id = descriptor
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| BridgeError::MissingProviderId {
                    plugin: host.plugin().to_owned(),
                })?
                .to_owned();
            Ok(ProviderHook {
                id,
                models: descriptor.get("models").and_then(|value| {
                    host.handle(value)
                        .map(|handle| -> Arc<dyn ProviderModelLoader> {
                            Arc::new(HandleModelLoader {
                                host: host.clone(),
                                handle,
                            })
                        })
                }),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Callback implementations
// ---------------------------------------------------------------------------

struct HandleAuthLoader {
    host: JsHost,
    handle: JsHandle,
}

#[async_trait]
impl AuthLoader for HandleAuthLoader {
    async fn load(
        &self,
        auth: &dyn AuthCredentialResolver,
        provider: &ResolvedProvider,
    ) -> Result<JsonMap, BoxSource> {
        // The oracle passes `getAuth` as a *callable* (`auth: () => Promise<Auth>`).
        // Resolving it here and handing over the value is the one place this bridge
        // narrows the contract, and it is safe because both real plugins call
        // `getAuth()` exactly once and immediately: kiro discards the result
        // (`dist/plugin.js:410-411`) and antigravity caches it
        // (`dist/src/plugin.js:1144-1147`).
        let credential = auth.resolve().await?;
        let value = self
            .host
            .call(
                &self.handle,
                vec![
                    credential_value(credential.as_ref()),
                    provider_value(provider),
                ],
            )
            .await?;
        Ok(value
            .as_object()
            .cloned()
            .unwrap_or_else(serde_json::Map::new))
    }
}

struct HandleOAuthAuthorizer {
    host: JsHost,
    handle: JsHandle,
}

#[async_trait]
impl AuthOAuthAuthorizer for HandleOAuthAuthorizer {
    async fn authorize(&self, inputs: Option<&AuthInputs>) -> Result<AuthOAuthResult, BoxSource> {
        let value = self
            .host
            .call_with_terminal(
                &self.handle,
                vec![inputs_value(inputs)],
                "OAuth authorization",
            )
            .await?;
        let plugin = self.host.plugin().to_owned();
        let callback = value
            .get("callback")
            .and_then(|value| self.host.handle(value));
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        let callback = match (method, callback) {
            ("auto", Some(handle)) => AuthOAuthCallback::Auto(Arc::new(HandleAutoCallback {
                host: self.host.clone(),
                handle,
            })),
            ("code", Some(handle)) => AuthOAuthCallback::Code(Arc::new(HandleCodeCallback {
                host: self.host.clone(),
                handle,
            })),
            ("auto" | "code", None) => {
                return Err(Box::new(BridgeError::MissingAuthorize { plugin }));
            }
            (found, _) => {
                return Err(Box::new(BridgeError::UnknownOAuthMethod {
                    plugin,
                    found: found.to_owned(),
                }));
            }
        };
        Ok(AuthOAuthResult {
            url: string_field(&value, "url"),
            instructions: string_field(&value, "instructions"),
            callback,
        })
    }
}

struct HandleApiAuthorizer {
    host: JsHost,
    handle: JsHandle,
}

#[async_trait]
impl AuthApiAuthorizer for HandleApiAuthorizer {
    async fn authorize(&self, inputs: Option<&AuthInputs>) -> Result<AuthApiResult, BoxSource> {
        let value = self
            .host
            .call_with_terminal(
                &self.handle,
                vec![inputs_value(inputs)],
                "authentication prompt",
            )
            .await?;
        if value.get("type").and_then(Value::as_str) != Some("success") {
            return Ok(AuthApiResult::Failed);
        }
        Ok(AuthApiResult::Success {
            key: Secret::new(string_field(&value, "key")),
            provider: value
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_owned),
            metadata: secret_map(value.get("metadata")),
        })
    }
}

struct HandleAutoCallback {
    host: JsHost,
    handle: JsHandle,
}

#[async_trait]
impl AuthAutoCallback for HandleAutoCallback {
    async fn callback(&self) -> Result<AuthCallbackResult, BoxSource> {
        let value = self.host.call(&self.handle, Vec::new()).await?;
        Ok(callback_result(&value))
    }
}

struct HandleCodeCallback {
    host: JsHost,
    handle: JsHandle,
}

#[async_trait]
impl crate::AuthCodeCallback for HandleCodeCallback {
    async fn callback(&self, code: &str) -> Result<AuthCallbackResult, BoxSource> {
        let value = self
            .host
            .call(&self.handle, vec![Value::String(code.to_owned())])
            .await?;
        Ok(callback_result(&value))
    }
}

struct HandleModelLoader {
    host: JsHost,
    handle: JsHandle,
}

#[async_trait]
impl ProviderModelLoader for HandleModelLoader {
    async fn models(
        &self,
        provider: &ResolvedProvider,
        context: ProviderHookContext<'_>,
    ) -> Result<BTreeMap<String, ResolvedModel>, BoxSource> {
        let value = self
            .host
            .call(
                &self.handle,
                vec![
                    provider_value(provider),
                    json!({ "auth": credential_value(context.auth) }),
                ],
            )
            .await?;
        // A plugin's model map is the SDK's `Model` shape, which is a superset of
        // `ResolvedModel`'s serialized form for the fields that matter. Anything
        // that does not deserialize is skipped with its id named, because dropping a
        // whole provider over one malformed model is the worse outcome.
        let mut models = BTreeMap::new();
        if let Some(map) = value.as_object() {
            for (id, model) in map {
                match serde_json::from_value::<ResolvedModel>(model.clone()) {
                    Ok(model) => {
                        models.insert(id.clone(), model);
                    }
                    Err(error) => tracing::debug!(
                        plugin = %self.host.plugin(),
                        model = %id,
                        %error,
                        "skipped a plugin model this host could not decode"
                    ),
                }
            }
        }
        Ok(models)
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

fn callback_result(value: &Value) -> AuthCallbackResult {
    if value.get("type").and_then(Value::as_str) != Some("success") {
        return AuthCallbackResult::Failed;
    }
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(key) = value.get("key").and_then(Value::as_str) {
        return AuthCallbackResult::Success(AuthSuccess::ApiKey {
            provider,
            key: Secret::new(key),
            metadata: secret_map(value.get("metadata")),
        });
    }
    AuthCallbackResult::Success(AuthSuccess::OAuth {
        provider,
        refresh: Secret::new(string_field(value, "refresh")),
        access: Secret::new(string_field(value, "access")),
        expires: value.get("expires").and_then(Value::as_u64).unwrap_or(0),
        account_id: value
            .get("accountId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        enterprise_url: value
            .get("enterpriseUrl")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn secret_map(value: Option<&Value>) -> Option<BTreeMap<String, Secret>> {
    let map = value?.as_object()?;
    Some(
        map.iter()
            .filter_map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), Secret::new(value)))
            })
            .collect(),
    )
}

fn inputs_value(inputs: Option<&AuthInputs>) -> Value {
    inputs.map_or(Value::Null, |inputs| {
        Value::Object(
            inputs
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect(),
        )
    })
}

fn provider_value(provider: &ResolvedProvider) -> Value {
    serde_json::to_value(provider).unwrap_or(Value::Null)
}

/// A credential rendered for the plugin.
///
/// Secrets are exposed here because that is the entire point of an auth loader —
/// the plugin signs requests with them. Nothing on this path is logged; `Secret`'s
/// `Debug` still redacts, and the JSON never reaches a tracing macro.
fn credential_value(credential: Option<&oc_auth::Credential>) -> Value {
    credential.map_or(Value::Null, |credential| {
        serde_json::to_value(credential).unwrap_or(Value::Null)
    })
}

fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
