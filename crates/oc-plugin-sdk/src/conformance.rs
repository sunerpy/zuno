use std::collections::HashSet;

use serde_json::Value;

use crate::{HandlerError, HookCall, Plugin, ToolCall, ToolContext};

/// One callback probe with an exact expected mutation.
#[derive(Debug, Clone, PartialEq)]
pub struct HookCase {
    pub hook: String,
    pub input: Value,
    pub output: Value,
    pub expected_output: Value,
}

impl HookCase {
    /// Build one exact callback probe.
    #[must_use]
    pub fn new(
        hook: impl Into<String>,
        input: Value,
        output: Value,
        expected_output: Value,
    ) -> Self {
        Self {
            hook: hook.into(),
            input,
            output,
            expected_output,
        }
    }
}

/// One tool probe with an exact expected successful result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCase {
    pub tool: String,
    pub arguments: Value,
    pub expected_output: crate::ToolOutput,
}

impl ToolCase {
    /// Build one exact tool probe.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        arguments: Value,
        expected_output: crate::ToolOutput,
    ) -> Self {
        Self {
            tool: tool.into(),
            arguments,
            expected_output,
        }
    }
}

/// Reusable executable contract a plugin can run against its own definition.
#[derive(Debug, Clone, Default)]
pub struct ConformanceSuite {
    hooks: Vec<HookCase>,
    tools: Vec<ToolCase>,
}

impl ConformanceSuite {
    /// Start an empty suite; every declared callback and tool must receive a case.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hooks: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Add one callback mutation case.
    #[must_use]
    pub fn hook(mut self, case: HookCase) -> Self {
        self.hooks.push(case);
        self
    }

    /// Add one tool execution case.
    #[must_use]
    pub fn tool(mut self, case: ToolCase) -> Self {
        self.tools.push(case);
        self
    }

    /// Round-trip the manifest and execute every declared callback and tool.
    ///
    /// # Errors
    /// Returns [`ConformanceError`] for incomplete coverage, callback failure, or
    /// output that differs from the case's exact expected value.
    pub async fn run(&self, plugin: &Plugin) -> Result<ConformanceReport, ConformanceError> {
        plugin.validate()?;
        let manifest = plugin.manifest();
        let encoded = serde_json::to_value(&manifest)?;
        let decoded: crate::PluginManifest = serde_json::from_value(encoded)?;
        if decoded != manifest {
            return Err(ConformanceError::ManifestRoundTrip);
        }

        let declared_hooks = manifest
            .hooks
            .iter()
            .filter(|hook| hook.as_str() != "tool")
            .collect::<HashSet<_>>();
        let covered_hooks = self.hooks.iter().map(|case| &case.hook).collect();
        if declared_hooks != covered_hooks {
            return Err(ConformanceError::HookCoverage);
        }
        let declared_tools = manifest
            .tools
            .iter()
            .map(|tool| &tool.id)
            .collect::<HashSet<_>>();
        let covered_tools = self.tools.iter().map(|case| &case.tool).collect();
        if declared_tools != covered_tools {
            return Err(ConformanceError::ToolCoverage);
        }

        for case in &self.hooks {
            let call = HookCall {
                hook: case.hook.clone(),
                input: case.input.clone(),
                output: case.output.clone(),
            };
            let call: HookCall = serde_json::from_value(serde_json::to_value(call)?)?;
            let result = plugin.call_hook(call).await?;
            if result.output != case.expected_output {
                return Err(ConformanceError::HookMismatch {
                    hook: case.hook.clone(),
                    expected: case.expected_output.clone(),
                    actual: result.output,
                });
            }
        }

        for case in &self.tools {
            let call = ToolCall {
                tool: case.tool.clone(),
                arguments: case.arguments.clone(),
                context: ToolContext {
                    session_id: "conformance".to_owned(),
                    message_id: "message".to_owned(),
                    call_id: "call".to_owned(),
                    agent: "conformance".to_owned(),
                    depth: 0,
                },
            };
            let call: ToolCall = serde_json::from_value(serde_json::to_value(call)?)?;
            let output = plugin.call_tool(call).await?;
            if output != case.expected_output {
                return Err(ConformanceError::ToolMismatch {
                    tool: case.tool.clone(),
                    expected: case.expected_output.clone(),
                    actual: output,
                });
            }
        }

        Ok(ConformanceReport {
            hooks_checked: self.hooks.len(),
            tools_checked: self.tools.len(),
        })
    }
}

/// Counts reported by a successful self-conformance run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConformanceReport {
    pub hooks_checked: usize,
    pub tools_checked: usize,
}

/// A plugin's declared protocol surface failed its own executable cases.
#[derive(Debug, thiserror::Error)]
pub enum ConformanceError {
    #[error("plugin definition is invalid")]
    Build(#[from] crate::BuildError),
    #[error("conformance value could not cross the JSON boundary")]
    Json(#[from] serde_json::Error),
    #[error("manifest changed during a JSON round trip")]
    ManifestRoundTrip,
    #[error("conformance cases must cover every declared callback exactly once")]
    HookCoverage,
    #[error("conformance cases must cover every declared tool exactly once")]
    ToolCoverage,
    #[error("hook `{hook}` returned the wrong mutation")]
    HookMismatch {
        hook: String,
        expected: Value,
        actual: Value,
    },
    #[error("tool `{tool}` returned the wrong output")]
    ToolMismatch {
        tool: String,
        expected: crate::ToolOutput,
        actual: crate::ToolOutput,
    },
    #[error("plugin callback failed during conformance")]
    Handler(#[from] HandlerError),
}
