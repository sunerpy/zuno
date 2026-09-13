//! Data-only Skill effects. A host resolves the target and holds its write
//! coordination; storage records before/after snapshots before any actual write.
use crate::{Result, digest_text};
use serde::{Deserialize, Serialize};
use zuno_error::LearningError;
use zuno_types::SkillCandidateStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillFileSnapshot {
    pub exists: bool,
    pub content: String,
}
impl SkillFileSnapshot {
    pub fn validate(&self) -> Result<()> {
        if !self.exists && !self.content.is_empty() {
            return Err(LearningError::InvalidRequest {
                field: "skill.snapshot".to_owned(),
                detail: "an absent Skill snapshot cannot contain bytes".to_owned(),
            }
            .into());
        }
        Ok(())
    }
    pub fn digest(&self) -> String {
        digest_text(&self.content)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEffectKind {
    Apply,
    Undo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedSkillEffect {
    pub candidate_id: String,
    pub operation_id: String,
    pub kind: SkillEffectKind,
    pub before: SkillFileSnapshot,
    pub after: SkillFileSnapshot,
}
impl PreparedSkillEffect {
    pub fn validate(&self) -> Result<()> {
        self.before.validate()?;
        self.after.validate()?;
        if self.candidate_id.trim().is_empty()
            || self.operation_id.trim().is_empty()
            || self.candidate_id.len() > 256
            || self.operation_id.len() > 256
        {
            return Err(LearningError::InvalidRequest {
                field: "skill.effect".to_owned(),
                detail: "invalid stable Skill effect identity".to_owned(),
            }
            .into());
        }
        Ok(())
    }
    pub fn expected_state(&self) -> SkillCandidateStatus {
        match self.kind {
            SkillEffectKind::Apply => SkillCandidateStatus::Applying,
            SkillEffectKind::Undo => SkillCandidateStatus::Undoing,
        }
    }
    /// Classifies observed target bytes without issuing another write.
    pub fn observed_state(&self, current: &SkillFileSnapshot) -> Result<SkillCandidateStatus> {
        self.validate()?;
        current.validate()?;
        Ok(if current == &self.after {
            match self.kind {
                SkillEffectKind::Apply => SkillCandidateStatus::Applied,
                SkillEffectKind::Undo => SkillCandidateStatus::Undone,
            }
        } else if current == &self.before {
            SkillCandidateStatus::Failed
        } else {
            SkillCandidateStatus::Uncertain
        })
    }
}
