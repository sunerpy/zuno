//! Private Worker progress writes, validated independently of durable history.

use serde::{Deserialize, Serialize};
use zuno_types::activity::LiveItem;

use crate::ApplicationError;

pub const MAX_LIVE_ITEMS: usize = 64;
pub const MAX_LIVE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveUpdate {
    pub generation: String,
    pub sequence: u64,
    pub message_id: Option<String>,
    pub items: Vec<LiveItem>,
}

impl LiveUpdate {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.generation.is_empty()
            || self.generation.len() > 128
            || !self
                .generation
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || self.sequence == 0
            || self.items.len() > MAX_LIVE_ITEMS
            || serde_json::to_vec(self)
                .map_err(ApplicationError::storage)?
                .len()
                > MAX_LIVE_BYTES
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded live update".to_owned(),
            ));
        }
        if self
            .message_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 512 || id.chars().any(char::is_control))
            || (!self.items.is_empty() && self.message_id.is_none())
        {
            return Err(ApplicationError::Invalid(
                "live content requires its originating message".to_owned(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for item in &self.items {
            let id = match item {
                LiveItem::Text {
                    id,
                    parent_id,
                    text,
                    ..
                }
                | LiveItem::Thinking {
                    id,
                    parent_id,
                    text,
                    ..
                } => {
                    if text.len() > 64 * 1024
                        || parent_id
                            .as_ref()
                            .is_some_and(|id| id.len() > 512 || id.chars().any(char::is_control))
                    {
                        return Err(ApplicationError::Invalid("invalid live text".to_owned()));
                    }
                    if parent_id.as_deref()
                        != self
                            .message_id
                            .as_deref()
                            .map(crate::activity::message_id)
                            .as_deref()
                    {
                        return Err(ApplicationError::Invalid(
                            "live item belongs to another message".to_owned(),
                        ));
                    }
                    id.as_str()
                }
                LiveItem::Invocation { id, label } => {
                    if label.len() > 256 || label.chars().any(char::is_control) {
                        return Err(ApplicationError::Invalid(
                            "invalid live invocation label".to_owned(),
                        ));
                    }
                    id.as_str()
                }
            };
            if id.is_empty()
                || id.len() > 512
                || id.chars().any(char::is_control)
                || !ids.insert((matches!(item, LiveItem::Invocation { .. }), id))
            {
                return Err(ApplicationError::Invalid(
                    "invalid or repeated live item identity".to_owned(),
                ));
            }
        }
        Ok(())
    }
}
