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
//! use oc_tool::{ToolContext, ToolOutput, TypedTool, erase};
//! use oc_error::ToolError;
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
use oc_error::ToolError;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

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

    /// The parameter schema **before** central augmentation.
    ///
    /// Named for what it is. Sending this to a provider would skip the cross-cutting
    /// properties; [`Tool::definition`] is the method that produces a schema fit to
    /// send.
    fn raw_parameters_schema(&self) -> Value;

    /// Runs the tool against raw arguments.
    ///
    /// The arguments still carry the injected properties; [`erase`]'s adapter strips
    /// them before a typed params struct sees them.
    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;

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
        }
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

    #[tokio::test]
    async fn execute_decodes_into_the_params_struct() {
        let output = erase(Echo)
            .execute(json!({ "text": "ab", "times": 3 }), context())
            .await
            .expect("valid arguments");

        assert_eq!(output.output, "ababab");
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
