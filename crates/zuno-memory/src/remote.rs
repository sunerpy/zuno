//! Data-only Memory application contract. Authentication and workspace binding
//! belong to the host; neither a command nor a model can select its owner.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zuno_db::memory_evidence::MemoryEvidenceReference;
use zuno_types::identity::{InputId, OperationId, RequestId, SessionId};
use zuno_types::{MemoryAction, MemoryCandidateProjection, MemoryScope};

use crate::{MemoryServiceError, MemorySnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRequest {
    pub request_id: RequestId,
    pub command: MemoryCommand,
}

/// Source and authority are deliberately absent from a user/model change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryChange {
    pub scope: MemoryScope,
    pub action: MemoryAction,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub reason: String,
    pub expected_revision: Option<i64>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MemoryCommand {
    Read,
    ReadEntries {
        query: MemoryQuery,
    },
    Candidates,
    Candidate {
        candidate_id: String,
    },
    Propose {
        change: MemoryChange,
    },
    Apply {
        candidate_id: String,
        expected_state: String,
    },
    Reject {
        candidate_id: String,
        expected_state: String,
    },
    Undo {
        candidate_id: String,
        expected_state: String,
    },
    Edit {
        candidate_id: String,
        expected_state: String,
        content: Option<String>,
        old_text: Option<String>,
        reason: String,
    },
    Policy {
        session_id: Option<SessionId>,
    },
    SetPolicy {
        session_id: Option<SessionId>,
        expected_revision: u64,
        use_memories: bool,
        generate_private: bool,
    },
    RecordEvidence {
        origin: MemoryEvidenceOrigin,
        excerpt: String,
    },
    Forget {
        evidence_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MemoryEvidenceOrigin {
    UserInput {
        session_id: SessionId,
        input_id: InputId,
    },
    Operation {
        operation_id: OperationId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryPolicy {
    /// Zero means the enterprise default, before a user's first explicit choice.
    pub revision: u64,
    pub use_memories: bool,
    pub generate_private: bool,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            revision: 0,
            use_memories: true,
            generate_private: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MemoryReply {
    Snapshot {
        documents: Vec<MemorySnapshot>,
    },
    Entries {
        scopes: Vec<MemoryReadScope>,
    },
    Candidates {
        candidates: Vec<MemoryCandidateView>,
    },
    Candidate {
        candidate: MemoryCandidateProjection,
        state_digest: String,
    },
    Policy {
        policy: MemoryPolicy,
    },
    Evidence {
        reference: MemoryEvidenceReference,
    },
    Forgotten {
        evidence_ids: Vec<String>,
        retracted: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryCandidateView {
    pub candidate: MemoryCandidateProjection,
    pub state_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryQuery {
    pub target: Option<MemoryScope>,
    pub query: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryReadScope {
    pub scope: MemoryScope,
    pub revision: i64,
    pub entries: Vec<String>,
    pub withheld_entries: usize,
    pub truncated: bool,
}

/// The same bounded read semantics for local tools, remote tools and Web.
pub fn read_scopes(
    views: Vec<zuno_db::resident_memory::ResidentMemoryView>,
    query: MemoryQuery,
) -> Result<Vec<MemoryReadScope>, MemoryServiceError> {
    if query
        .query
        .as_ref()
        .is_some_and(|query| query.len() > 2048 || query.chars().count() > 512)
        || query.limit.is_some_and(|limit| !(1..=128).contains(&limit))
    {
        return Err(MemoryServiceError::Invalid(
            "Memory query or limit exceeds its bound".to_owned(),
        ));
    }
    let text = query
        .query
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let limit = query.limit.unwrap_or(32) as usize;
    let mut remaining_bytes = 32_768;
    let mut scopes = Vec::new();
    for view in views {
        if query
            .target
            .is_some_and(|target| target != view.document.scope)
        {
            continue;
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in view.entries {
            if !text.is_empty() && !entry.to_lowercase().contains(&text) {
                continue;
            }
            if entries.len() >= limit || entry.len() > remaining_bytes {
                truncated = true;
                continue;
            }
            remaining_bytes -= entry.len();
            entries.push(entry);
        }
        scopes.push(MemoryReadScope {
            scope: view.document.scope,
            revision: view.document.revision,
            entries,
            withheld_entries: view.suppressed.len(),
            truncated,
        });
    }
    Ok(scopes)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryFailure {
    Denied,
    Unavailable,
    Conflict,
    InvalidData,
    Invalid { message: String },
}

impl From<MemoryServiceError> for MemoryFailure {
    fn from(error: MemoryServiceError) -> Self {
        match error {
            MemoryServiceError::Denied => Self::Denied,
            MemoryServiceError::InvalidData => Self::InvalidData,
            MemoryServiceError::Unavailable => Self::Unavailable,
            MemoryServiceError::Conflict
            | MemoryServiceError::Database(zuno_error::DbError::Conflict { .. }) => Self::Conflict,
            MemoryServiceError::Invalid(message) => Self::Invalid { message },
            MemoryServiceError::Resident(error) if error.is_proposal_correctable() => {
                Self::Invalid {
                    message: error.to_string(),
                }
            }
            _ => Self::Unavailable,
        }
    }
}
impl From<MemoryFailure> for MemoryServiceError {
    fn from(error: MemoryFailure) -> Self {
        match error {
            MemoryFailure::Denied => Self::Denied,
            MemoryFailure::InvalidData => Self::InvalidData,
            MemoryFailure::Unavailable => Self::Unavailable,
            MemoryFailure::Conflict => Self::Conflict,
            MemoryFailure::Invalid { message } => Self::Invalid(message),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryResponse {
    pub result: Result<MemoryReply, MemoryFailure>,
}

/// Bound to one authenticated actor and workspace by the host. A remote Worker
/// calls this async port; it never receives a synchronous database provider.
#[async_trait]
pub trait MemoryDataService: Send + Sync {
    async fn request(&self, request: MemoryRequest) -> Result<MemoryReply, MemoryServiceError>;
}
