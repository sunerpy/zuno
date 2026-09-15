use crate::MAX_INLINE_SCRIPT_BYTES;
use crate::ValidatedWorkflow;
use crate::WorkflowCompileRequest;
use crate::WorkflowError;
use crate::validate_identity;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

mod local;
mod script;

use local::diagnostic;
use local::diagnostic_sort_key;
pub use local::load_local_workflow_registry;

const DEFAULT_MAX_ROOTS: usize = 128;
const DEFAULT_MAX_FILES: usize = 1_024;
const DEFAULT_MAX_DEPTH: usize = 16;
const DEFAULT_MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;

/// Ownership layer for a user-supplied workflow.
///
/// Zuno deliberately has no application/builtin workflow layer. Project workflows can
/// specialize a user's defaults, and user workflows can override plugin examples.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowSourceScope {
    Plugin,
    User,
    Project,
}

impl WorkflowSourceScope {
    pub(crate) const fn precedence(self) -> u8 {
        match self {
            Self::Plugin => 10,
            Self::User => 20,
            Self::Project => 30,
        }
    }

    pub(crate) const fn scheme(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Stable owner identity attached to every discovered workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowSource {
    pub scope: WorkflowSourceScope,
    pub id: String,
}

impl WorkflowSource {
    pub fn new(scope: WorkflowSourceScope, id: impl Into<String>) -> Result<Self, WorkflowError> {
        let id = id.into();
        validate_identity("workflow source owner", &id)?;
        Ok(Self { scope, id })
    }
}

/// One local file or directory whose contents are owned by [`WorkflowSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWorkflowRoot {
    pub source: WorkflowSource,
    pub path: PathBuf,
    /// Missing required roots are diagnostics. Optional conventional directories are silent.
    pub required: bool,
}

impl LocalWorkflowRoot {
    pub fn required(source: WorkflowSource, path: impl Into<PathBuf>) -> Self {
        Self {
            source,
            path: path.into(),
            required: true,
        }
    }

    pub fn optional(source: WorkflowSource, path: impl Into<PathBuf>) -> Self {
        Self {
            source,
            path: path.into(),
            required: false,
        }
    }
}

/// Bounded local discovery controls. A caller should run discovery on a blocking task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalWorkflowLoadLimits {
    pub max_roots: usize,
    pub max_files: usize,
    pub max_depth: usize,
    pub max_document_bytes: u64,
    pub max_script_bytes: u64,
}

impl Default for LocalWorkflowLoadLimits {
    fn default() -> Self {
        Self {
            max_roots: DEFAULT_MAX_ROOTS,
            max_files: DEFAULT_MAX_FILES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_document_bytes: DEFAULT_MAX_DOCUMENT_BYTES,
            max_script_bytes: MAX_INLINE_SCRIPT_BYTES as u64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowDiagnosticLevel {
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowDiagnosticCode {
    RootLimit,
    MissingRoot,
    Inspect,
    SymlinkRejected,
    DepthLimit,
    FileLimit,
    DocumentTooLarge,
    Read,
    Parse,
    DocumentOutsideRoot,
    ScriptMissing,
    ScriptTooLarge,
    ScriptOutsideRoot,
    DuplicateName,
}

/// A source-local failure. Invalid documents do not hide valid workflows from other owners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDiagnostic {
    pub level: WorkflowDiagnosticLevel,
    pub code: WorkflowDiagnosticCode,
    pub source: WorkflowSource,
    pub path: String,
    pub message: String,
}

/// Exact executable material for an active or shadowed workflow.
#[derive(Debug, Clone)]
pub struct RegisteredWorkflow {
    source: WorkflowSource,
    document_path: PathBuf,
    workflow: Arc<ValidatedWorkflow>,
    resolved_script: Option<Arc<str>>,
    executable_digest: String,
}

impl RegisteredWorkflow {
    pub fn source(&self) -> &WorkflowSource {
        &self.source
    }

    pub fn document_path(&self) -> &Path {
        &self.document_path
    }

    pub fn workflow(&self) -> &Arc<ValidatedWorkflow> {
        &self.workflow
    }

    /// Digest of the workflow document plus an external script, when present.
    pub fn executable_digest(&self) -> &str {
        &self.executable_digest
    }

    pub fn compile_request(&self) -> WorkflowCompileRequest {
        WorkflowCompileRequest {
            workflow: Arc::clone(&self.workflow),
            resolved_script: self.resolved_script.clone(),
        }
    }
}

/// Immutable, transactionally replaceable workflow catalog.
#[derive(Debug, Clone, Default)]
pub struct WorkflowRegistry {
    active: BTreeMap<String, Arc<RegisteredWorkflow>>,
    shadowed: Vec<Arc<RegisteredWorkflow>>,
}

impl WorkflowRegistry {
    pub fn get(&self, name: &str) -> Option<&Arc<RegisteredWorkflow>> {
        self.active.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<RegisteredWorkflow>)> {
        self.active
            .iter()
            .map(|(name, workflow)| (name.as_str(), workflow))
    }

    pub fn shadowed(&self) -> &[Arc<RegisteredWorkflow>] {
        &self.shadowed
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowRegistryLoadOutcome {
    pub registry: WorkflowRegistry,
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

fn resolve_candidates(
    candidates: Vec<Arc<RegisteredWorkflow>>,
    mut diagnostics: Vec<WorkflowDiagnostic>,
) -> WorkflowRegistryLoadOutcome {
    let mut by_name = BTreeMap::<String, Vec<Arc<RegisteredWorkflow>>>::new();
    for candidate in candidates {
        by_name
            .entry(candidate.workflow.identity().name.clone())
            .or_default()
            .push(candidate);
    }

    let mut registry = WorkflowRegistry::default();
    for (name, mut named) in by_name {
        named.sort_by(|left, right| {
            right
                .source
                .scope
                .precedence()
                .cmp(&left.source.scope.precedence())
                .then_with(|| {
                    normalized_path(&left.document_path).cmp(&normalized_path(&right.document_path))
                })
        });
        let winning_precedence = named[0].source.scope.precedence();
        let mut winning_count = 0usize;
        let mut start = 0usize;
        while start < named.len() {
            let precedence = named[start].source.scope.precedence();
            let mut end = start + 1;
            while end < named.len() && named[end].source.scope.precedence() == precedence {
                end += 1;
            }
            if precedence == winning_precedence {
                winning_count = end - start;
            }
            if end - start > 1 {
                let conflicts = named[start..end]
                    .iter()
                    .map(|candidate| candidate.workflow.identity().source.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let first = &named[start];
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Error,
                    WorkflowDiagnosticCode::DuplicateName,
                    &first.source,
                    &first.document_path,
                    format!(
                        "workflow `{name}` has multiple definitions at the same precedence: {conflicts}"
                    ),
                ));
            }
            start = end;
        }
        if winning_count > 1 {
            registry.shadowed.extend(named);
            continue;
        }
        let winner = named.remove(0);
        registry.active.insert(name, winner);
        registry.shadowed.extend(named);
    }

    diagnostics.sort_by(|left, right| diagnostic_sort_key(left).cmp(&diagnostic_sort_key(right)));
    WorkflowRegistryLoadOutcome {
        registry,
        diagnostics,
    }
}

pub(crate) fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
