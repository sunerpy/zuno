//! Logical workspace changes. Host paths and archive offsets are private to the
//! environment provider; approval binds the resulting immutable manifest.
use crate::ApplicationError;
use crate::environment::EnvironmentSnapshot;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zuno_types::identity::{EnvironmentId, InvocationId, JobId, OperationId};

pub const MAX_MERGE_FILES: usize = 1024;
pub const MAX_MERGE_MANIFEST_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeContentSide {
    Base,
    Parent,
    Child,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MergeContentRequest {
    pub approval_id: zuno_types::identity::ApprovalId,
    pub side: MergeContentSide,
    pub path: WorkspacePath,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MergeContentContext {
    pub gateway_id: zuno_types::identity::GatewayId,
    pub owner: zuno_types::identity::PrincipalKey,
    pub snapshot: EnvironmentSnapshot,
    pub path: WorkspacePath,
    pub expected: WorkspaceEntry,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergeView {
    pub approval_id: zuno_types::identity::ApprovalId,
    pub operation_id: OperationId,
    pub child_job_id: JobId,
    pub plan: WorkspaceMergePlan,
    pub admitted: bool,
}

/// Internal data-owner lineage proof. It is resolved from durable child Jobs,
/// never accepted as authority from a Worker or a public client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergeSource {
    pub child_job_id: JobId,
    pub child_session_id: zuno_types::identity::SessionId,
    pub child_configuration: crate::runtime::ConfigurationRef,
    pub child_input_version: u64,
    pub gateway_id: zuno_types::identity::GatewayId,
    pub source_environment: crate::environment::EnvironmentSpec,
    pub base: EnvironmentSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergeAdmission {
    pub gateway_id: zuno_types::identity::GatewayId,
    pub lease: crate::runtime::ExecutionLease,
    pub environment: crate::environment::Environment,
    pub source: WorkspaceMergeSource,
    pub operation: WorkspaceMergeOperation,
}
impl WorkspaceMergeAdmission {
    pub fn arguments_digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!(self.operation.plan))
    }
    pub fn resources_digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!([
            self.gateway_id,
            self.environment,
            self.source,
            self.operation.base,
            self.operation.parent,
            self.operation.child
        ]))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergeCompletion {
    pub lease: crate::runtime::ExecutionLease,
    pub operation: WorkspaceMergeOperation,
    pub receipt: WorkspaceMergeReceipt,
}
impl WorkspaceMergeCompletion {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.operation.validate()?;
        if self.receipt.id != self.operation.id
            || self.receipt.environment_id != self.operation.environment_id
            || self.receipt.plan_digest != self.operation.plan.digest()
            || !matches!(
                self.receipt.state,
                WorkspaceMergeState::Committed | WorkspaceMergeState::Cancelled
            )
            || self.receipt.revision
                != match self.receipt.state {
                    WorkspaceMergeState::Committed => self
                        .operation
                        .expected_revision
                        .checked_add(1)
                        .ok_or(ApplicationError::Conflict)?,
                    _ => self.operation.expected_revision,
                }
        {
            return Err(ApplicationError::Invalid(
                "invalid workspace merge completion".to_owned(),
            ));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
pub trait WorkspaceMergeCompletionSink: Send + Sync {
    async fn publish_merge(
        &self,
        completion: &WorkspaceMergeCompletion,
    ) -> Result<(), ApplicationError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergePreviewRequest {
    pub id: OperationId,
    pub invocation_id: InvocationId,
    pub child_job_id: JobId,
    pub environment_id: EnvironmentId,
    pub source_id: EnvironmentId,
    pub base: EnvironmentSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayMergeRequest {
    pub lease: crate::runtime::ExecutionLease,
    pub environment: crate::environment::Environment,
    pub operation: WorkspaceMergeOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergeOperation {
    pub id: OperationId,
    pub invocation_id: InvocationId,
    pub child_job_id: JobId,
    pub environment_id: EnvironmentId,
    pub expected_revision: u64,
    pub base: EnvironmentSnapshot,
    pub parent: EnvironmentSnapshot,
    pub child: EnvironmentSnapshot,
    pub plan: WorkspaceMergePlan,
}
impl WorkspaceMergeOperation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.plan.validate()?;
        if self.expected_revision == 0
            || self.parent.environment_id != self.environment_id
            || self.parent.revision != self.expected_revision
            || self.child.environment_id == self.environment_id
        {
            return Err(ApplicationError::Invalid(
                "invalid workspace merge identity".to_owned(),
            ));
        }
        for snapshot in [&self.base, &self.parent, &self.child] {
            if snapshot.revision == 0
                || snapshot.sha256.len() != 64
                || !snapshot
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || snapshot.bytes > 512 * 1024 * 1024
            {
                return Err(ApplicationError::Invalid(
                    "invalid workspace merge snapshot".to_owned(),
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMergeState {
    Preparing,
    Conflicted,
    Committed,
    Cancelled,
    Uncertain,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergeReceipt {
    pub id: OperationId,
    pub environment_id: EnvironmentId,
    pub state: WorkspaceMergeState,
    pub plan_digest: String,
    pub revision: u64,
}
#[async_trait::async_trait]
pub trait WorkspaceMergeAuthority: Send + Sync {
    /// Implementations bind the immutable plan and source lineage to current
    /// organization approval and the calling Job's live execution lease.
    async fn authorize_merge(
        &self,
        lease: &crate::runtime::ExecutionLease,
        environment: &crate::environment::Environment,
        operation: &WorkspaceMergeOperation,
    ) -> Result<(), ApplicationError>;
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspacePath(String);
impl WorkspacePath {
    pub fn new(path: impl Into<String>) -> Result<Self, ApplicationError> {
        let path = path.into();
        if path == "." {
            return Ok(Self(path));
        }
        if path.is_empty()
            || path.len() > 4096
            || path.contains('\0')
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(ApplicationError::Invalid(
                "invalid logical workspace path".to_owned(),
            ));
        }
        Ok(Self(path))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn root() -> Self {
        Self(".".to_owned())
    }
    pub fn parent(&self) -> Option<Self> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| Self(parent.to_owned()))
    }
}
impl TryFrom<String> for WorkspacePath {
    type Error = ApplicationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<WorkspacePath> for String {
    fn from(value: WorkspacePath) -> Self {
        value.0
    }
}
impl JsonSchema for WorkspacePath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WorkspacePath".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"string","minLength":1,"maxLength":4096,
            "pattern":"^\\.$|^(?!\\.{1,2}(?:/|$))(?![\\s\\S]*/\\.{1,2}(?:/|$))[^/\\u0000]+(?:/[^/\\u0000]+)*$"})
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceEntry {
    Directory {
        mode: u32,
        uid: u32,
        gid: u32,
    },
    File {
        mode: u32,
        uid: u32,
        gid: u32,
        sha256: String,
        bytes: zuno_types::activity::Counter,
    },
    Symlink {
        uid: u32,
        gid: u32,
        target: String,
    },
    Hardlink {
        target: WorkspacePath,
    },
}
impl WorkspaceEntry {
    pub fn content_descriptor(&self) -> Option<(u64, String)> {
        match self {
            Self::File { bytes, sha256, .. } => Some((bytes.0, sha256.clone())),
            Self::Symlink { target, .. } => {
                Some((target.len() as u64, zuno_orchestration::sha256_text(target)))
            }
            _ => None,
        }
    }
    pub fn validate(&self, path: &WorkspacePath) -> Result<(), ApplicationError> {
        let invalid = || ApplicationError::Invalid("invalid workspace entry".to_owned());
        match self {
            Self::Directory { mode, .. } if *mode & !0o1777 != 0 => return Err(invalid()),
            Self::File { mode, .. } if *mode & !0o777 != 0 => return Err(invalid()),
            Self::File { sha256, .. }
                if sha256.len() != 64
                    || !sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
            {
                return Err(invalid());
            }
            Self::Symlink { target, .. } => {
                if target.is_empty()
                    || target.len() > 4096
                    || target.contains('\0')
                    || target.starts_with('/')
                {
                    return Err(invalid());
                }
                let mut depth = path.as_str().split('/').count() - 1;
                for part in target.split('/') {
                    match part {
                        "." | "" => {}
                        ".." if depth > 0 => depth -= 1,
                        ".." => return Err(invalid()),
                        _ => depth += 1,
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeChoice {
    Parent,
    Child,
    Conflict,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceChange {
    pub path: WorkspacePath,
    pub base: Option<WorkspaceEntry>,
    pub parent: Option<WorkspaceEntry>,
    pub child: Option<WorkspaceEntry>,
    pub choice: MergeChoice,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMergePlan {
    pub base_tree: String,
    pub parent_tree: String,
    pub child_tree: String,
    pub changes: Vec<WorkspaceChange>,
}
impl WorkspaceMergePlan {
    pub fn digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!(self))
    }
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let invalid =
            || ApplicationError::Invalid("invalid bounded workspace merge plan".to_owned());
        for digest in [&self.base_tree, &self.parent_tree, &self.child_tree] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(invalid());
            }
        }
        if self.changes.len() > MAX_MERGE_FILES
            || serde_json::to_vec(self)
                .map_err(ApplicationError::storage)?
                .len()
                > MAX_MERGE_MANIFEST_BYTES
        {
            return Err(invalid());
        }
        let mut previous = None;
        for change in &self.changes {
            if previous.is_some_and(|path: &WorkspacePath| path >= &change.path) {
                return Err(invalid());
            }
            previous = Some(&change.path);
            for entry in [&change.base, &change.parent, &change.child]
                .into_iter()
                .flatten()
            {
                entry.validate(&change.path)?;
            }
        }
        Ok(())
    }
}

pub type WorkspaceTree = BTreeMap<WorkspacePath, WorkspaceEntry>;

pub fn validate_tree(tree: &WorkspaceTree) -> Result<(), ApplicationError> {
    for (path, entry) in tree {
        entry.validate(path)?;
        if path == &WorkspacePath::root() && !matches!(entry, WorkspaceEntry::Directory { .. }) {
            return Err(ApplicationError::Invalid(
                "the workspace root must remain a directory".to_owned(),
            ));
        }
        if let WorkspaceEntry::Hardlink { target } = entry
            && !matches!(tree.get(target), Some(WorkspaceEntry::File { .. }))
        {
            return Err(ApplicationError::Invalid(
                "workspace hardlink has no regular-file target".to_owned(),
            ));
        }
        let mut parent = path.parent();
        while let Some(path) = parent {
            if !matches!(tree.get(&path), Some(WorkspaceEntry::Directory { .. })) {
                return Err(ApplicationError::Invalid(
                    "workspace entry has no directory parent".to_owned(),
                ));
            }
            parent = path.parent();
        }
    }
    Ok(())
}

/// Compare complete snapshots, preserving parent-only edits and equal changes.
/// Simultaneous edits to one entry remain explicit conflicts, including binary
/// files and directory/file transitions.
pub fn plan(
    base: &WorkspaceTree,
    parent: &WorkspaceTree,
    child: &WorkspaceTree,
) -> Result<WorkspaceMergePlan, ApplicationError> {
    for tree in [base, parent, child] {
        validate_tree(tree)?;
    }
    let paths = base
        .keys()
        .chain(parent.keys())
        .chain(child.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut changes = Vec::new();
    for path in paths {
        let (b, p, c) = (base.get(&path), parent.get(&path), child.get(&path));
        if c == b || c == p {
            continue;
        }
        changes.push(WorkspaceChange {
            path,
            base: b.cloned(),
            parent: p.cloned(),
            child: c.cloned(),
            choice: if p == b {
                MergeChoice::Child
            } else {
                MergeChoice::Conflict
            },
        });
    }
    let mut candidate = parent.clone();
    for change in &changes {
        if change.choice == MergeChoice::Child {
            match &change.child {
                Some(value) => {
                    candidate.insert(change.path.clone(), value.clone());
                }
                None => {
                    candidate.remove(&change.path);
                }
            }
        }
    }
    // Deleting/replacing a directory cannot discard concurrent parent children.
    for path in candidate.keys() {
        let mut parent = path.parent();
        while let Some(path) = parent {
            if !matches!(candidate.get(&path), Some(WorkspaceEntry::Directory { .. }))
                && let Some(change) = changes.iter_mut().find(|change| change.path == path)
            {
                change.choice = MergeChoice::Conflict;
            }
            parent = path.parent();
        }
        if let Some(WorkspaceEntry::Hardlink { target }) = candidate.get(path)
            && !matches!(candidate.get(target), Some(WorkspaceEntry::File { .. }))
        {
            for change in &mut changes {
                if &change.path == path || &change.path == target {
                    change.choice = MergeChoice::Conflict;
                }
            }
        }
    }
    let result = WorkspaceMergePlan {
        base_tree: zuno_orchestration::sha256_json(&serde_json::json!(base)),
        parent_tree: zuno_orchestration::sha256_json(&serde_json::json!(parent)),
        child_tree: zuno_orchestration::sha256_json(&serde_json::json!(child)),
        changes,
    };
    result.validate()?;
    Ok(result)
}

/// Resolutions are an approval input and must be included in its digest. A
/// previously reviewed parent snapshot can never be silently substituted.
pub fn resolved_tree(
    reviewed: &WorkspaceMergePlan,
    base: &WorkspaceTree,
    parent: &WorkspaceTree,
    child: &WorkspaceTree,
) -> Result<WorkspaceTree, ApplicationError> {
    reviewed.validate()?;
    let canonical = plan(base, parent, child)?;
    if reviewed.base_tree != canonical.base_tree
        || reviewed.parent_tree != canonical.parent_tree
        || reviewed.child_tree != canonical.child_tree
        || reviewed.changes.len() != canonical.changes.len()
        || reviewed
            .changes
            .iter()
            .zip(&canonical.changes)
            .any(|(a, b)| {
                a.path != b.path || a.base != b.base || a.parent != b.parent || a.child != b.child
            })
    {
        return Err(ApplicationError::Conflict);
    }
    let mut result = parent.clone();
    for change in &reviewed.changes {
        if parent.get(&change.path) != change.parent.as_ref() {
            return Err(ApplicationError::Conflict);
        }
        match change.choice {
            MergeChoice::Conflict => return Err(ApplicationError::Conflict),
            MergeChoice::Parent => {}
            MergeChoice::Child => match &change.child {
                Some(value) => {
                    result.insert(change.path.clone(), value.clone());
                }
                None => {
                    result.remove(&change.path);
                }
            },
        }
    }
    validate_tree(&result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(value: &str) -> WorkspaceEntry {
        WorkspaceEntry::File {
            mode: 0o644,
            uid: 0,
            gid: 0,
            sha256: zuno_orchestration::sha256_text(value),
            bytes: zuno_types::activity::Counter(value.len() as u64),
        }
    }
    fn tree(values: &[(&str, &str)]) -> WorkspaceTree {
        values
            .iter()
            .map(|(path, value)| (WorkspacePath::new(*path).unwrap(), file(value)))
            .collect()
    }
    #[test]
    fn disjoint_parent_and_child_changes_preserve_both_and_bind_the_reviewed_snapshot() {
        let base = tree(&[("one", "base"), ("two", "base")]);
        let parent = tree(&[("one", "parent"), ("two", "base")]);
        let child = tree(&[("one", "base"), ("two", "child")]);
        let reviewed = plan(&base, &parent, &child).unwrap();
        assert_eq!(reviewed.changes.len(), 1);
        assert_eq!(
            resolved_tree(&reviewed, &base, &parent, &child).unwrap(),
            tree(&[("one", "parent"), ("two", "child")])
        );
        assert!(
            resolved_tree(
                &reviewed,
                &base,
                &tree(&[("one", "new parent"), ("two", "base")]),
                &child
            )
            .is_err()
        );
        let mut forged = reviewed;
        forged.changes[0].child = Some(file("unreviewed"));
        assert!(resolved_tree(&forged, &base, &parent, &child).is_err());
    }
    #[test]
    fn same_file_changes_and_directory_replacements_need_explicit_consistent_resolution() {
        let base = tree(&[("one", "base")]);
        let parent = tree(&[("one", "parent")]);
        let child = tree(&[("one", "child")]);
        let mut reviewed = plan(&base, &parent, &child).unwrap();
        assert_eq!(reviewed.changes[0].choice, MergeChoice::Conflict);
        assert!(resolved_tree(&reviewed, &base, &parent, &child).is_err());
        let digest = reviewed.digest();
        reviewed.changes[0].choice = MergeChoice::Child;
        assert_ne!(reviewed.digest(), digest);
        assert_eq!(
            resolved_tree(&reviewed, &base, &parent, &child).unwrap(),
            child
        );
        let directory = WorkspacePath::new("dir").unwrap();
        let base: WorkspaceTree = [(
            directory.clone(),
            WorkspaceEntry::Directory {
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        )]
        .into();
        let mut parent = base.clone();
        parent.insert(
            WorkspacePath::new("dir/new").unwrap(),
            file("parent addition"),
        );
        let child = tree(&[("dir", "replacement")]);
        let reviewed = plan(&base, &parent, &child).unwrap();
        assert_eq!(reviewed.changes[0].choice, MergeChoice::Conflict);
        assert!(resolved_tree(&reviewed, &base, &parent, &child).is_err());
    }
    #[test]
    fn link_traversal_and_manifest_order_cannot_change_the_approved_boundary() {
        for path in [
            "/root",
            "../root",
            "a/../root",
            "a//b",
            "a/",
            "",
            "..",
            "a\0b",
        ] {
            assert!(WorkspacePath::new(path).is_err(), "{path:?}");
        }
        let path = WorkspacePath::new("file").unwrap();
        assert!(
            WorkspaceEntry::Symlink {
                uid: 0,
                gid: 0,
                target: "../outside".to_owned()
            }
            .validate(&path)
            .is_err()
        );
        assert!(
            validate_tree(
                &[(
                    path,
                    WorkspaceEntry::Hardlink {
                        target: WorkspacePath::new("missing").unwrap()
                    }
                )]
                .into()
            )
            .is_err()
        );
        let base = WorkspaceTree::new();
        let child = tree(&[("a", "new"), ("b", "new")]);
        let mut reviewed = plan(&base, &base, &child).unwrap();
        reviewed.changes.reverse();
        assert!(reviewed.validate().is_err());
        let mut reviewed = plan(&base, &base, &child).unwrap();
        reviewed.changes.push(reviewed.changes[0].clone());
        assert!(reviewed.validate().is_err());
    }
}
