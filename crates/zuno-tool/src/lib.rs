//! The tool trait, argument schemas, and the result shape every tool returns.
//!
//! # One artifact, not two
//!
//! A tool declares a params struct. The wire schema is *derived* from that struct by
//! [`schemars`], and the same struct is what [`serde`] deserializes the model's
//! arguments into. There is no second declaration to fall out of step with the
//! first, which is the single defect this crate exists to prevent: the reference
//! implementation in `.omo/refs/claw-code/rust/crates/tools/src/lib.rs` hand-writes
//! 250 `json!` schemas and pairs them with independently declared serde structs
//! hundreds of lines away, so a renamed field advertises a parameter the
//! deserializer will never read and nothing fails until a model tries to use it.
//!
//! Implement [`TypedTool`] and the schema is not yours to write. [`erase`] turns it
//! into the object-safe [`Tool`] the registry stores.
//!
//! ```
//! use async_trait::async_trait;
//! use zuno_tool::{ToolContext, ToolOutput, TypedTool, erase};
//! use zuno_error::ToolError;
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize, JsonSchema)]
//! struct GreetParams {
//!     /// Who to greet.
//!     name: String,
//! }
//!
//! struct Greet;
//!
//! #[async_trait]
//! impl TypedTool for Greet {
//!     type Params = GreetParams;
//!
//!     fn id(&self) -> &str { "greet" }
//!     fn description(&self) -> &str { "Greet someone." }
//!
//!     async fn run(&self, params: GreetParams, _ctx: ToolContext)
//!         -> Result<ToolOutput, ToolError>
//!     {
//!         Ok(ToolOutput::text("greet", format!("hello {}", params.name)))
//!     }
//! }
//!
//! let definition = erase(Greet).definition();
//! assert_eq!(definition.parameters["properties"]["name"]["type"], "string");
//! // Injected centrally, by nothing the tool wrote:
//! assert_eq!(definition.parameters["properties"]["intent"]["type"], "string");
//! ```
//!
//! # The layers
//!
//! - [`schema`] derives parameter schemas and augments every object schema with the
//!   cross-cutting properties, once, at definition time.
//! - [`guard`] reads those properties back off raw arguments, through the same
//!   constants that wrote them.
//! - [`context`] carries the permission ask, the cancellation signal, the call
//!   coordinates, and derives child contexts for composed calls.
//! - [`output`] is the result shape plus size **detection**.
//! - [`store`] persists full output that detection found oversized.
//!
//! # What this crate does not decide
//!
//! Detection and storage are here. The user-visible consequence of oversized output
//! — refuse and require the `accept_large_output` opt-in, rather than return a
//! truncated prefix — is owned solely by the output-policy layer (todo 72). Nothing
//! in this crate truncates, and nothing here asserts what a caller receives when
//! output is over a limit.

pub mod context;
pub mod guard;
pub mod output;
pub mod schema;
pub mod store;

pub use crate::context::{
    AllowAll, DenyAll, InterruptHandle, NeverInterrupted, PermissionAsk, PermissionAsker,
    ToolContext,
};
pub use crate::output::{
    Attachment, LimitExceeded, OutputLimits, SizeMeasurement, SizeVerdict, ToolOutput,
};
pub use crate::schema::{ACCEPT_LARGE_OUTPUT_KEY, INTENT_KEY};
pub use crate::store::{StoredOutput, ToolOutputStore};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use zuno_error::ToolError;

/// A tool as a provider sees it.
///
/// Produced only by [`Tool::definition`], so the augmentation cannot be skipped by
/// assembling one of these from parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// The name the model calls.
    pub id: String,
    /// The description the model reads.
    pub description: String,
    /// The augmented JSON Schema for the arguments.
    pub parameters: Value,
    /// Stable client presentation intent, independent from the wire name.
    pub ui_intent: ToolUiIntent,
}

/// A tool's durable client presentation category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolUiIntent {
    /// Ordinary tool rendering.
    #[default]
    Generic,
    /// Native or product subagent execution.
    Subagent,
}

/// Whether an identical tool call may be issued again after a transient failure.
///
/// `Never` is the default because a lost response does not prove a mutation failed:
/// the external side effect may already exist. `Safe` is reserved for idempotent,
/// read-only operations whose implementation owns that guarantee.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ToolReplayPolicy {
    /// Verify authoritative external state before issuing another call.
    #[default]
    Never,
    /// The identical call is read-only or otherwise idempotent and may be retried.
    Safe,
}

/// Whether separate model-issued calls may overlap in one assistant step.
///
/// This policy is independent from [`ToolReplayPolicy`]. A read-only tool may be
/// safe to retry but still require exclusive access to a process or protocol
/// stream, while an isolated child execution may overlap yet remain unsafe to
/// replay after an uncertain outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ToolConcurrencyPolicy {
    /// Run after every earlier call settles and before any later call starts.
    #[default]
    Exclusive,
    /// Calls are read-only or otherwise explicitly safe to execute concurrently.
    ParallelSafe,
    /// The tool owns an isolated child execution or durable background job.
    IsolatedBackground,
}

/// An executable tool, erased to a trait object for the registry.
///
/// Object-safe: `#[async_trait]` boxes the returned future so `Arc<dyn Tool>` works.
/// Most tools should implement [`TypedTool`] instead and reach this through
/// [`erase`]; implement `Tool` directly only when the parameters are not describable
/// by a Rust type, which in practice means an MCP proxy relaying a remote server's
/// schema.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The name the model calls, and the key the registry stores.
    fn id(&self) -> &str;

    /// The description the model reads.
    fn description(&self) -> &str;

    /// Whether an identical call may be retried after a typed transient failure.
    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    /// Whether separate model-issued calls may overlap in one assistant step.
    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::Exclusive
    }

    /// Stable client presentation intent.
    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Generic
    }

    /// The parameter schema **before** central augmentation.
    ///
    /// Named for what it is. Sending this to a provider would skip the cross-cutting
    /// properties; [`Tool::definition`] is the method that produces a schema fit to
    /// send.
    fn raw_parameters_schema(&self) -> Value;

    /// Runs the tool against arguments carrying only the injected properties this
    /// implementation claimed through [`Tool::consumed_injected_keys`].
    ///
    /// Reached through [`Tool::invoke`], never called directly by the dispatch or
    /// registry boundary.
    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;

    /// The injected properties this implementation reads off its raw arguments.
    ///
    /// Empty by default, and that default is the whole safety property. An
    /// implementation that forwards its arguments to a callee with its own declared
    /// schema — an MCP proxy, a plugin host, a config-directory tool — must not pass
    /// on a property that callee never declared, or a strict server rejects the
    /// entire call. Claiming a key is therefore a deliberate act; forgetting to claim
    /// one cannot leak it.
    fn consumed_injected_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Runs the tool. **The single point where injected properties are removed, and
    /// the single point where a failed call is recorded.**
    ///
    /// Every caller outside this crate goes through here rather than
    /// [`Tool::execute`], so the removal covers derived and proxied tools alike, in
    /// the same spirit as [`Tool::definition`] covering both for injection. Runs
    /// after the permission gate, which reads the intent off the un-stripped
    /// arguments. Not intended to be overridden.
    ///
    /// # Why the failure record is emitted here
    ///
    /// The two production callers — the engine's dispatch boundary and the registry
    /// that composed sub-calls run through — both reach every tool through this one
    /// method, so a record placed here covers builtins, MCP proxies, plugin tools and
    /// batch sub-calls together. Placing it at either caller would have covered one of
    /// those sets and left the other silent, which is how a session full of failing
    /// MCP calls produced a log containing no MCP entry at all.
    async fn invoke(&self, mut args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        guard::strip_injected_except(&mut args, self.consumed_injected_keys());
        let session_id = ctx.session_id.clone();
        let call_id = ctx.call_id.clone();
        let result = self.execute(args, ctx).await;
        if let Err(error) = &result {
            report_tool_failure(self.id(), &session_id, &call_id, error);
        }
        result
    }

    /// The provider-facing definition. **The single augmentation point.**
    ///
    /// Every tool passes through here, including MCP proxies whose schema this binary
    /// never authored, which is why the cross-cutting properties are injected here
    /// rather than by each tool. Not intended to be overridden.
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            id: self.id().to_owned(),
            description: self.description().to_owned(),
            parameters: schema::augment(self.raw_parameters_schema()),
            ui_intent: self.ui_intent(),
        }
    }
}

/// Record one failed tool call: which tool, which call, and every cause.
///
/// # Why only failures, and why nothing on success
///
/// A per-call record on a *successful* tool would be the defect this project already
/// lived through in another place. Duplicate-skill precedence was reported with one
/// `WARN` per duplicate per launch, which put 189 lines demanding attention into a
/// 202-line log and buried the twelve warnings that were real faults. Volume at a
/// level reserved for action is how a true signal becomes unreadable, so the
/// successful path stays silent and only an outcome a user may need to act on is
/// recorded.
///
/// # Why a denial is quieter than the rest
///
/// [`ToolError::Denied`] is the permission layer doing exactly what it was
/// configured to do, at the user's own instruction, and the user was already told at
/// the moment it happened. Recording it at `WARN` would emit a line every time
/// someone declines a prompt — a normal condition at a level that demands attention,
/// which is the same mistake in a new place. It stays at `DEBUG`, so
/// `--log-level debug` still recovers it. Every other variant is either a fault or a
/// call the model could not complete, and those surface by default.
///
/// # What is deliberately not recorded
///
/// The arguments are never logged. They carry file contents, prompts, patch bodies,
/// URLs and anything else the model chose to pass, so a record of them is a
/// credential-disclosure risk that outweighs its diagnostic value; the tool name and
/// the cause chain are what a reader needs to act. The session and call ids are
/// recorded because they are generated identifiers with no user content, and the call
/// id is the key that locates the failing row without reading the database by hand.
///
/// The cause chain itself is reproduced verbatim, because it *is* the diagnosis.
/// [`zuno_error::source::describe`] documents that it cannot know which bytes are
/// sensitive; a caller that both walks a chain and knows a secret is the one
/// responsible for keeping it out, and no secret is known at this layer.
fn report_tool_failure(tool: &str, session_id: &str, call_id: &str, error: &ToolError) {
    let reason = zuno_error::source::describe(error);
    if matches!(error, ToolError::Denied { .. }) {
        tracing::debug!(
            "tool.name" = tool,
            "session.id" = session_id,
            "tool.call_id" = call_id,
            "tool call denied: {reason}"
        );
    } else {
        tracing::warn!(
            "tool.name" = tool,
            "session.id" = session_id,
            "tool.call_id" = call_id,
            "tool call failed: {reason}"
        );
    }
}

/// A tool whose arguments are a Rust type.
///
/// The schema is derived from [`TypedTool::Params`], so there is nothing to
/// hand-write and nothing to keep in sync. Renaming a field changes both the wire
/// schema and the deserializer in the same edit, and every use of the old name stops
/// compiling.
#[async_trait]
pub trait TypedTool: Send + Sync + 'static {
    /// The arguments, deserialized and schema-derived from one declaration.
    type Params: JsonSchema + DeserializeOwned + Send;

    /// The name the model calls.
    fn id(&self) -> &str;

    /// The description the model reads.
    fn description(&self) -> &str;

    /// Whether an identical call may be retried after a typed transient failure.
    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    /// Whether separate model-issued calls may overlap in one assistant step.
    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::Exclusive
    }

    /// Stable client presentation intent.
    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Generic
    }

    /// Runs the tool against decoded arguments.
    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
}

/// Adapts a [`TypedTool`] to the object-safe [`Tool`].
///
/// A named wrapper rather than a blanket `impl<T: TypedTool> Tool for T`, which would
/// collide with any direct `impl Tool` — the compiler cannot prove an MCP proxy is not
/// also a `TypedTool` — and would therefore close the door this crate has to leave
/// open for proxied tools.
#[derive(Debug, Clone, Copy)]
pub struct Typed<T>(pub T);

#[async_trait]
impl<T: TypedTool> Tool for Typed<T> {
    fn id(&self) -> &str {
        self.0.id()
    }

    fn description(&self) -> &str {
        self.0.description()
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        self.0.replay_policy()
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        self.0.concurrency_policy()
    }

    fn ui_intent(&self) -> ToolUiIntent {
        self.0.ui_intent()
    }

    fn raw_parameters_schema(&self) -> Value {
        schema::derive_params_schema::<T::Params>()
    }

    async fn execute(&self, mut args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        // The oracle's span, attribute for attribute (`tool/tool.ts:105-110`), so a
        // trace from either binary reads the same.
        let span = tracing::info_span!(
            "Tool.execute",
            "tool.name" = self.0.id(),
            "session.id" = %ctx.session_id,
            "message.id" = %ctx.message_id,
            "tool.call_id" = %ctx.call_id,
        );
        let _entered = span.enter();

        // Removed before decoding so a params struct never has to declare fields it
        // did not ask for, and `deny_unknown_fields` stays usable. Read them off the
        // raw arguments with `guard` if you need them.
        guard::strip_cross_cutting(&mut args);

        let params =
            serde_json::from_value::<T::Params>(args).map_err(|error| ToolError::InvalidArgs {
                tool: self.0.id().to_owned(),
                source: Box::new(error),
            })?;

        self.0.run(params, ctx).await
    }
}

/// Erases a [`TypedTool`] into the registry's element type.
#[must_use]
pub fn erase<T: TypedTool>(tool: T) -> Arc<dyn Tool> {
    Arc::new(Typed(tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct EchoParams {
        /// The text to echo.
        text: String,
        /// How many times.
        #[serde(default)]
        times: Option<u32>,
    }

    struct Echo;

    #[async_trait]
    impl TypedTool for Echo {
        type Params = EchoParams;

        fn id(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echo text."
        }

        fn replay_policy(&self) -> ToolReplayPolicy {
            ToolReplayPolicy::Safe
        }

        async fn run(
            &self,
            params: EchoParams,
            _ctx: ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(
                "echo",
                params.text.repeat(params.times.unwrap_or(1) as usize),
            ))
        }
    }

    /// An MCP proxy: no Rust type describes its parameters, so it implements `Tool`
    /// directly with a schema that arrived over the wire.
    struct Proxied {
        remote_schema: Value,
    }

    #[async_trait]
    impl Tool for Proxied {
        fn id(&self) -> &str {
            "codegraph_search"
        }

        fn description(&self) -> &str {
            "Search an indexed codebase."
        }

        fn raw_parameters_schema(&self) -> Value {
            self.remote_schema.clone()
        }

        async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("proxied", args.to_string()))
        }
    }

    fn context() -> ToolContext {
        ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    #[test]
    fn a_typed_tool_gets_its_schema_without_writing_any_json() {
        let definition = erase(Echo).definition();

        assert_eq!(definition.id, "echo");
        assert_eq!(definition.parameters["type"], "object");
        assert_eq!(
            definition.parameters["properties"]["text"]["type"],
            "string"
        );
        assert_eq!(
            definition.parameters["properties"]["text"]["description"],
            "The text to echo."
        );
        assert_eq!(
            definition.parameters["required"],
            json!(["text", INTENT_KEY])
        );
    }

    #[test]
    fn a_proxied_schema_is_augmented_by_the_same_single_point() {
        let proxy = Proxied {
            remote_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": { "query": { "type": "string" } },
            }),
        };

        let definition = proxy.definition();

        assert_eq!(
            definition.parameters["properties"][INTENT_KEY]["type"],
            "string"
        );
        assert_eq!(
            definition.parameters["properties"][ACCEPT_LARGE_OUTPUT_KEY]["type"],
            "boolean"
        );
        assert_eq!(
            definition.parameters["required"],
            json!(["query", INTENT_KEY])
        );
    }

    #[test]
    fn the_raw_schema_is_named_for_being_un_augmented() {
        let raw = Typed(Echo).raw_parameters_schema();

        assert!(
            raw["properties"].get(INTENT_KEY).is_none(),
            "augmentation happens at definition time, not derivation time"
        );
    }

    #[test]
    fn replay_is_forbidden_by_default_and_typed_tools_delegate_explicit_opt_in() {
        let proxy = Proxied {
            remote_schema: json!({ "type": "object", "properties": {} }),
        };

        assert_eq!(proxy.replay_policy(), ToolReplayPolicy::Never);
        assert_eq!(erase(Echo).replay_policy(), ToolReplayPolicy::Safe);
    }

    #[tokio::test]
    async fn execute_decodes_into_the_params_struct() {
        let output = erase(Echo)
            .execute(json!({ "text": "ab", "times": 3 }), context())
            .await
            .expect("valid arguments");

        assert_eq!(output.output, "ababab");
    }

    #[tokio::test]
    async fn a_proxied_tool_receives_no_injected_property_it_did_not_claim() {
        // The defect this closes: `Proxied` forwards its arguments verbatim to a
        // remote server, and claims nothing, so `invoke` must hand it neither key.
        let proxy = Proxied {
            remote_schema: json!({ "type": "object", "properties": {} }),
        };

        let output = proxy
            .invoke(
                json!({ INTENT_KEY: "why", ACCEPT_LARGE_OUTPUT_KEY: true, "query": "x" }),
                context(),
            )
            .await
            .expect("the proxy runs");

        assert_eq!(output.output, json!({ "query": "x" }).to_string());
        assert!(
            proxy.consumed_injected_keys().is_empty(),
            "claiming nothing is the default that makes forwarding safe"
        );
    }

    #[tokio::test]
    async fn a_claimed_key_survives_invocation_while_the_rest_are_removed() {
        struct Claiming;

        #[async_trait]
        impl Tool for Claiming {
            fn id(&self) -> &str {
                "claiming"
            }

            fn description(&self) -> &str {
                "Read the size opt-in off its own raw arguments."
            }

            fn raw_parameters_schema(&self) -> Value {
                json!({ "type": "object", "properties": {} })
            }

            fn consumed_injected_keys(&self) -> &'static [&'static str] {
                &[ACCEPT_LARGE_OUTPUT_KEY]
            }

            async fn execute(
                &self,
                args: Value,
                _ctx: ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text("claiming", args.to_string()))
            }
        }

        let output = Claiming
            .invoke(
                json!({ INTENT_KEY: "why", ACCEPT_LARGE_OUTPUT_KEY: true }),
                context(),
            )
            .await
            .expect("the tool runs");

        assert_eq!(
            output.output,
            json!({ ACCEPT_LARGE_OUTPUT_KEY: true }).to_string()
        );
    }

    #[tokio::test]
    async fn injected_properties_never_reach_a_strict_params_struct() {
        // `EchoParams` uses `deny_unknown_fields`; without stripping, this call would
        // fail on a property the tool never declared and the schema injected for it.
        let output = erase(Echo)
            .execute(
                json!({ "text": "x", INTENT_KEY: "why", ACCEPT_LARGE_OUTPUT_KEY: true }),
                context(),
            )
            .await
            .expect("the cross-cutting keys must be stripped before decoding");

        assert_eq!(output.output, "x");
    }

    #[tokio::test]
    async fn bad_arguments_are_model_correctable_and_name_the_tool() {
        let error = erase(Echo)
            .execute(json!({ "times": 3 }), context())
            .await
            .expect_err("text is required");

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert_eq!(error.tool(), "echo");
        assert!(error.is_model_correctable());
    }

    #[test]
    fn tools_are_storable_as_trait_objects() {
        let registry: Vec<Arc<dyn Tool>> = vec![
            erase(Echo),
            Arc::new(Proxied {
                remote_schema: json!({ "type": "object" }),
            }),
        ];

        let ids: Vec<&str> = registry.iter().map(|tool| tool.id()).collect();
        assert_eq!(ids, vec!["echo", "codegraph_search"]);
    }
}
