//! Composer ownership until durable admission acknowledges a submission.
//!
//! A channel send is not admission. Failed/stale steering restores the original
//! text, paste payloads and images, without overwriting a newer draft or retrying
//! into a different turn.

use std::collections::BTreeMap;

use crate::views::attachment::AttachmentDraft;
use crate::views::editor::{HistoryDraft, InputEditor};
use crate::views::session::PromptTarget;

#[derive(Clone)]
pub(crate) struct SavedDraft {
    pub target: PromptTarget,
    editor: HistoryDraft,
    attachments: AttachmentDraft,
}

impl SavedDraft {
    pub(crate) fn capture(
        target: PromptTarget,
        editor: &InputEditor,
        attachments: &AttachmentDraft,
    ) -> Self {
        Self {
            target,
            editor: editor.save_draft(),
            attachments: attachments.clone(),
        }
    }

    pub(crate) fn restore(
        self,
        editor: &mut InputEditor,
        attachments: &mut AttachmentDraft,
    ) -> Option<Self> {
        if !editor.is_empty() {
            return Some(self);
        }
        editor.restore_draft(self.editor);
        *attachments = self.attachments;
        None
    }
}

#[derive(Default)]
pub(crate) struct DraftRecovery {
    pub prepared: Option<SavedDraft>,
    pending: BTreeMap<String, SavedDraft>,
    pub rejected: Vec<SavedDraft>,
}

impl DraftRecovery {
    pub(crate) fn sent(&mut self, request_id: &str) {
        if let Some(draft) = self.prepared.take() {
            self.pending.insert(request_id.to_owned(), draft);
        }
    }

    pub(crate) fn rejected_before_send(&mut self) {
        self.rejected.extend(self.prepared.take());
    }

    pub(crate) fn acknowledge(
        &mut self,
        request_id: &str,
        error: Option<String>,
    ) -> Option<String> {
        let draft = self.pending.remove(request_id)?;
        if error.is_some() {
            self.rejected.push(draft);
        }
        error
    }
}
