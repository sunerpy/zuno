//! Native current-session history recovery and durable Agent working notes.
//!
//! The model tools are optional consumers. SQLite providers and profile
//! composition remain typed services, so a host can maintain durable Goal/Plan
//! state independently of whether either tool is visible.

mod component;
mod error;
pub mod history;
pub mod notes;
mod sqlite;
mod token;

pub use component::{ContinuityService, ContinuitySettings, profile_overlay};
pub use error::ContinuityError;
pub use history::{HISTORY_TOOL_ID, HistoryParams, HistoryProvider, HistoryTool};
pub use notes::{NOTES_TOOL_ID, NoteScope, NotesParams, NotesProvider, NotesTool};
pub use sqlite::SqliteContinuityProvider;
pub use zuno_db::continuity::{MAX_NOTE_DOCUMENT_BYTES, MAX_NOTE_DOCUMENTS, MAX_NOTE_SCOPE_BYTES};
