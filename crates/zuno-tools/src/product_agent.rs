//! Static tools backed by host-installed Codex or Claude Code processes.
//!
//! Each configured instance owns one immutable wire name. The tool only validates
//! arguments, asks the normal subagent permission, and hands a typed request to the
//! composition root. Native authentication, process protocols, durable jobs, and
//! cancellation remain outside this crate.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zuno_error::ToolError;
use zuno_tool::{
    PermissionAsk, ToolConcurrencyPolicy, ToolContext, ToolOutput, ToolReplayPolicy, ToolUiIntent,
    TypedTool,
};

use crate::task::ReportDelivery;

/// Permission key shared with native child-session delegation.
pub const PERMISSION_KEY: &str = "task";

/// Arguments common to every product-agent instance.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProductAgentParams {
    /// The task for the external product to perform.
    pub prompt: String,
    /// A short label shown in clients and diagnostics.
    #[serde(default)]
    pub description: Option<String>,
    /// Run asynchronously and return a durable job id immediately.
    #[serde(default)]
    pub background: Option<bool>,
    /// How a background result reaches the parent session.
    #[serde(default, rename = "reportDelivery")]
    pub report_delivery: Option<ReportDelivery>,
}

/// One admitted invocation as received by the session layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductAgentRequest {
    /// The parent Zuno session.
    pub parent_session_id: String,
    /// Configured instance name.
    pub instance: String,
    /// Stable product kind (`codex` or `claude-code`).
    pub product: String,
    /// Static tool name used for this invocation.
    pub tool: String,
    /// Exact task text.
    pub prompt: String,
    /// Optional short label.
    pub description: Option<String>,
    /// Whether to return before the native product settles.
    pub background: bool,
    /// Durable report behavior for a background invocation.
    pub report_delivery: ReportDelivery,
}

/// Result returned by the session layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductAgentTurn {
    /// One-shot native invocation id, distinct from a durable job id.
    pub run_id: String,
    /// Durable job handle for background execution.
    pub job_id: Option<String>,
    /// Final answer or a running notice.
    pub output: String,
}

/// Product execution effects supplied by the CLI composition root.
#[async_trait]
pub trait ProductAgentHost: Send + Sync + 'static {
    /// Run or enqueue one configured product invocation.
    async fn dispatch(
        &self,
        request: ProductAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentTurn, String>;
}

/// One statically named product-agent tool.
pub struct ProductAgentTool {
    id: String,
    instance: String,
    product: String,
    description: String,
    host: Arc<dyn ProductAgentHost>,
}

impl ProductAgentTool {
    /// Bind one configured instance to its static wire name.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        instance: impl Into<String>,
        product: impl Into<String>,
        host: Arc<dyn ProductAgentHost>,
    ) -> Self {
        let id = id.into();
        let instance = instance.into();
        let product = product.into();
        let description = format!(
            "Delegate one bounded task to the configured `{instance}` {product} product agent. \
             Foreground waits for its final answer. `background: true` returns a durable job id; \
             `reportDelivery` defaults to `nextStep`. Native credentials, model selection, \
             working directory, and proxy environment remain owned by {product}."
        );
        Self {
            id,
            instance,
            product,
            description,
            host,
        }
    }
}

#[async_trait]
impl TypedTool for ProductAgentTool {
    type Params = ProductAgentParams;

    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::IsolatedBackground
    }

    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Subagent
    }

    async fn run(
        &self,
        params: ProductAgentParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let background = params.background.unwrap_or(false);
        if !background && params.report_delivery.is_some() {
            return Err(invalid(
                &self.id,
                "`reportDelivery` requires `background: true`; remove it or run in background",
            ));
        }
        if params.prompt.trim().is_empty() {
            return Err(invalid(&self.id, "`prompt` must not be empty"));
        }
        let report_delivery = params.report_delivery.unwrap_or_default();
        let mut metadata = Map::new();
        metadata.insert("product".to_owned(), Value::String(self.product.clone()));
        metadata.insert("instance".to_owned(), Value::String(self.instance.clone()));
        if let Some(description) = &params.description {
            metadata.insert("description".to_owned(), Value::String(description.clone()));
        }
        ctx.ask(
            &self.id,
            PermissionAsk {
                permission: PERMISSION_KEY.to_owned(),
                patterns: vec![format!("product:{}", self.instance)],
                metadata,
                always: vec![format!("product:{}", self.instance)],
                ..PermissionAsk::default()
            },
        )
        .await?;

        let cancellation = CancellationToken::new();
        let request = ProductAgentRequest {
            parent_session_id: ctx.session_id,
            instance: self.instance.clone(),
            product: self.product.clone(),
            tool: self.id.clone(),
            prompt: params.prompt.clone(),
            description: params.description.clone(),
            background,
            report_delivery,
        };
        let dispatch = self.host.dispatch(request, cancellation.clone());
        tokio::pin!(dispatch);
        let turn = if background {
            dispatch.await
        } else {
            tokio::select! {
                result = &mut dispatch => result,
                () = ctx.interrupt.notified() => {
                    cancellation.cancel();
                    dispatch.await
                }
            }
        }
        .map_err(|message| ToolError::Failed {
            tool: self.id.clone(),
            source: Box::new(std::io::Error::other(message)),
        })?;
        if background && turn.job_id.is_none() {
            return Err(ToolError::Failed {
                tool: self.id.clone(),
                source: Box::new(std::io::Error::other(
                    "a background product-agent dispatch did not return a job id",
                )),
            });
        }
        Ok(render(&self.product, &self.instance, &params, &turn))
    }
}

fn render(
    product: &str,
    instance: &str,
    params: &ProductAgentParams,
    turn: &ProductAgentTurn,
) -> ToolOutput {
    let state = if turn.job_id.is_some() {
        "running"
    } else {
        "completed"
    };
    let delivery = match params.report_delivery.unwrap_or_default() {
        ReportDelivery::NextStep => "nextStep",
        ReportDelivery::Quiet => "quiet",
    };
    let open = match turn.job_id.as_deref() {
        Some(job) => format!(
            "<product-agent product=\"{product}\" instance=\"{instance}\" run=\"{}\" \
             job=\"{job}\" state=\"{state}\" reportDelivery=\"{delivery}\">",
            turn.run_id
        ),
        None => format!(
            "<product-agent product=\"{product}\" instance=\"{instance}\" run=\"{}\" \
             state=\"{state}\">",
            turn.run_id
        ),
    };
    let mut lines = vec![open];
    if let Some(description) = &params.description {
        lines.push(format!("<summary>{description}</summary>"));
    }
    lines.push("<product_agent_result>".to_owned());
    lines.push(turn.output.clone());
    lines.push("</product_agent_result>".to_owned());
    lines.push("</product-agent>".to_owned());
    let metadata = json!({
        "product": product,
        "instance": instance,
        "runID": turn.run_id,
        "jobID": turn.job_id,
        "state": state,
        "reportDelivery": delivery,
        "description": params.description,
        "result": turn.output,
    });
    ToolOutput::text(
        params
            .description
            .clone()
            .unwrap_or_else(|| format!("{product} product agent")),
        lines.join("\n"),
    )
    .with_metadata("subagent", metadata)
}

fn invalid(tool: &str, message: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_owned(),
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

/// Stable product identifier used in jobs and client projections.
#[must_use]
pub const fn product_id(
    kind: zuno_config::schema::product_agent::ProductAgentKind,
) -> &'static str {
    match kind {
        zuno_config::schema::product_agent::ProductAgentKind::Codex => "codex",
        zuno_config::schema::product_agent::ProductAgentKind::ClaudeCode => "claude-code",
    }
}
