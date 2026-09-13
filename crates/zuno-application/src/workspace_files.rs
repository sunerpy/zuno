//! Typed read-only workspace operations. Paths are logical archive entries;
//! caller data cannot select a host directory or an execution command.
use crate::{
    ApplicationError,
    environment::{Environment, EnvironmentSnapshot},
    runtime::ExecutionLease,
    workspace_merge::{WorkspaceEntry, WorkspacePath},
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::{
    activity::Counter,
    identity::{EnvironmentId, InvocationId, OperationId},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WorkspaceFileQuery {
    Read {
        path: WorkspacePath,
        offset: Counter,
        maximum_bytes: u32,
    },
    List {
        path: WorkspacePath,
        after: Option<WorkspacePath>,
        limit: u32,
    },
    Search {
        path: WorkspacePath,
        text: String,
        limit: u32,
    },
}
impl WorkspaceFileQuery {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let valid = match self {
            Self::Read { maximum_bytes, .. } => (1..=65536).contains(maximum_bytes),
            Self::List { limit, .. } => (1..=200).contains(limit),
            Self::Search { text, limit, .. } => {
                !text.is_empty()
                    && text.len() <= 1024
                    && !text.contains('\0')
                    && !text.contains('\n')
                    && (1..=100).contains(limit)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(ApplicationError::Invalid(
                "invalid bounded workspace query".to_owned(),
            ))
        }
    }
    pub fn effect(&self) -> zuno_permission::enterprise::EffectKind {
        use zuno_permission::enterprise::EffectKind;
        match self {
            Self::Read { .. } => EffectKind::FileRead,
            Self::List { .. } => EffectKind::FileList,
            Self::Search { .. } => EffectKind::FileSearch,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceFileOperation {
    pub id: OperationId,
    pub invocation_id: InvocationId,
    pub environment_id: EnvironmentId,
    pub expected_revision: u64,
    pub query: WorkspaceFileQuery,
}
impl WorkspaceFileOperation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.expected_revision == 0 {
            return Err(ApplicationError::Invalid(
                "invalid workspace revision".to_owned(),
            ));
        }
        self.query.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayFileRequest {
    pub lease: ExecutionLease,
    pub environment: Environment,
    pub operation: WorkspaceFileOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceFileItem {
    pub path: WorkspacePath,
    pub entry: WorkspaceEntry,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMatch {
    pub path: WorkspacePath,
    pub line: Counter,
    pub text: String,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WorkspaceFileResult {
    Read {
        item: WorkspaceFileItem,
        /// None denotes binary data or link metadata; these bytes are not
        /// mislabeled as UTF-8 text and links are never followed.
        text: Option<String>,
        offset: Counter,
        next_offset: Counter,
        truncated: bool,
    },
    List {
        entries: Vec<WorkspaceFileItem>,
        after: Option<WorkspacePath>,
    },
    Search {
        matches: Vec<WorkspaceMatch>,
        truncated: bool,
        skipped_files: Counter,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceFileReceipt {
    pub operation_id: OperationId,
    pub snapshot: EnvironmentSnapshot,
    pub result: WorkspaceFileResult,
}
impl WorkspaceFileReceipt {
    pub fn validate_for(&self, operation: &WorkspaceFileOperation) -> Result<(), ApplicationError> {
        let contained = |path: &WorkspacePath, root: &WorkspacePath| {
            root.as_str() == "."
                || path == root
                || path
                    .as_str()
                    .strip_prefix(root.as_str())
                    .is_some_and(|tail| tail.starts_with('/'))
        };
        let valid = self.operation_id == operation.id
            && self.snapshot.environment_id == operation.environment_id
            && self.snapshot.revision == operation.expected_revision
            && self.snapshot.sha256.len() == 64
            && self
                .snapshot
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && self.snapshot.bytes <= 512 * 1024 * 1024;
        if !valid {
            return Err(ApplicationError::Conflict);
        }
        match (&operation.query, &self.result) {
            (
                WorkspaceFileQuery::Read {
                    path,
                    offset,
                    maximum_bytes,
                },
                WorkspaceFileResult::Read {
                    item,
                    text,
                    offset: actual,
                    next_offset,
                    ..
                },
            ) => {
                item.path == *path
                    && actual == offset
                    && next_offset.0 >= offset.0
                    && next_offset.0 - offset.0 <= u64::from(*maximum_bytes)
                    && text
                        .as_ref()
                        .is_none_or(|text| text.len() as u64 == next_offset.0 - offset.0)
                    && item.entry.validate(&item.path).is_ok()
            }
            (
                WorkspaceFileQuery::List {
                    path,
                    after: before,
                    limit,
                },
                WorkspaceFileResult::List { entries, after },
            ) => {
                entries.len() <= *limit as usize
                    && entries.iter().all(|entry| {
                        entry.path != *path
                            && entry.path.parent().unwrap_or_else(WorkspacePath::root) == *path
                            && before.as_ref().is_none_or(|before| entry.path > *before)
                            && entry.entry.validate(&entry.path).is_ok()
                    })
                    && entries.windows(2).all(|pair| pair[0].path < pair[1].path)
                    && after.as_ref().is_none_or(|after| {
                        entries.last().is_some_and(|entry| entry.path == *after)
                    })
            }
            (
                WorkspaceFileQuery::Search { path, text, limit },
                WorkspaceFileResult::Search { matches, .. },
            ) => {
                matches.len() <= *limit as usize
                    && matches.iter().all(|entry| {
                        contained(&entry.path, path)
                            && entry.line.0 > 0
                            && entry.text.contains(text)
                            && entry.text.len() <= 2048
                    })
                    && matches.iter().map(|entry| entry.text.len()).sum::<usize>() <= 65536
            }
            _ => false,
        }
        .then_some(())
        .ok_or(ApplicationError::Conflict)
    }
}

#[async_trait]
pub trait WorkspaceFileAuthority: Send + Sync {
    async fn authorize_files(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &WorkspaceFileOperation,
    ) -> Result<(), ApplicationError>;
}
#[async_trait]
pub trait WorkspaceFileReader: Send + Sync {
    async fn query_files(
        &self,
        lease: &ExecutionLease,
        operation: &WorkspaceFileOperation,
        authority: &dyn WorkspaceFileAuthority,
    ) -> Result<WorkspaceFileReceipt, ApplicationError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_queries_reject_unbounded_work_and_host_paths() {
        for path in ["/etc/passwd", "../secret", "src/../../secret", "src//file"] {
            assert!(
                serde_json::from_value::<WorkspaceFileQuery>(serde_json::json!({
                    "kind":"read","path":path,"offset":"0","maximumBytes":4096
                }))
                .is_err()
            );
        }
        assert!(
            WorkspaceFileQuery::Search {
                path: WorkspacePath::root(),
                text: "\n".to_owned(),
                limit: 1,
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkspaceFileQuery::Read {
                path: WorkspacePath::root(),
                offset: Counter(0),
                maximum_bytes: 65537,
            }
            .validate()
            .is_err()
        );
    }
}
