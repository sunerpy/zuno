//! Live child-session projections and the full-screen view attached to one of them.
//!
//! A delegated turn owns its own engine channel and host. The TUI must not remount the
//! parent merely to inspect that channel: remounting tears the parent host down. This
//! projection is the process-local read model between those independently running hosts
//! and the one terminal surface.

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::Definition;
use crate::views::editor::{EditorSignal, InputEditor, PromptGutter};
use crate::views::message::{ActivityDisplay, Message, StatusView, Transcript, TranscriptView};
use crate::views::session::{PROMPT_MARKER, prompt_frame, prompt_rows};
use crate::views::{ViewContext, fill, padded, pressable_label};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use zuno_engine::r#loop::TurnEvent;
use zuno_types::UsageSnapshot;

const CHILD_PROMPT_PLACEHOLDER: &str = "message this child";

/// Initial durable state published before a child starts producing live events.
#[derive(Debug, Clone)]
pub struct LiveSessionOpen {
    pub session_id: String,
    pub parent_session_id: String,
    pub title: String,
    pub agent: String,
    pub model: String,
    pub effort: Option<zuno_llm::effort::ReasoningEffort>,
    pub messages: Vec<Message>,
    pub usage: Option<UsageSnapshot>,
}

/// One immutable read of a live child session.
#[derive(Debug, Clone)]
pub struct LiveSessionSnapshot {
    pub session_id: String,
    pub parent_session_id: String,
    pub title: String,
    pub agent: String,
    pub model: String,
    pub effort: Option<zuno_llm::effort::ReasoningEffort>,
    pub transcript: Transcript,
    pub generation: u64,
}

#[derive(Debug, Default)]
struct ProjectionState {
    generation: u64,
    order: Vec<String>,
    sessions: BTreeMap<String, LiveSessionSnapshot>,
}

/// Process-local projections for every child host the current TUI composition observes.
#[derive(Debug, Clone, Default)]
pub struct LiveSessions(Arc<Mutex<ProjectionState>>);

impl LiveSessions {
    /// Publish the replayed prefix before the child turn starts.
    pub fn open(&self, opened: LiveSessionOpen) {
        let mut transcript = Transcript::new();
        if let Some(usage) = opened.usage {
            transcript.restore_usage(usage);
        }
        transcript.replay(opened.messages);
        transcript.mark_running();

        let mut state = self.lock();
        if !state.sessions.contains_key(&opened.session_id) {
            state.order.push(opened.session_id.clone());
        }
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.sessions.insert(
            opened.session_id.clone(),
            LiveSessionSnapshot {
                session_id: opened.session_id,
                parent_session_id: opened.parent_session_id,
                title: opened.title,
                agent: opened.agent,
                model: opened.model,
                effort: opened.effort,
                transcript,
                generation,
            },
        );
    }

    /// Fold one live event into exactly the child session that emitted it.
    ///
    /// Returns whether the projection changed visibly or semantically.
    pub fn observe(&self, session_id: &str, event: &TurnEvent) -> bool {
        let mut state = self.lock();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return false;
        };
        let mut changed = session.transcript.observe(event);
        match event {
            TurnEvent::SessionMaterialized { title, .. } if session.title != *title => {
                session.title.clone_from(title);
                changed = true;
            }
            TurnEvent::AgentResolved { agent, .. } if session.agent != *agent => {
                session.agent.clone_from(agent);
                changed = true;
            }
            TurnEvent::ModelResolved {
                provider_id,
                model_id,
                ..
            } => {
                let model = format!("{provider_id}/{model_id}");
                if session.model != model {
                    session.model = model;
                    changed = true;
                }
            }
            _ => {}
        }
        if changed {
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            if let Some(session) = state.sessions.get_mut(session_id) {
                session.generation = generation;
            }
        }
        changed
    }

    /// The newest immutable state for `session_id`.
    #[must_use]
    pub fn snapshot(&self, session_id: &str) -> Option<LiveSessionSnapshot> {
        self.lock().sessions.get(session_id).cloned()
    }

    /// Direct children in first-observed order.
    #[must_use]
    pub fn children(&self, parent_session_id: &str) -> Vec<String> {
        let state = self.lock();
        state
            .order
            .iter()
            .filter(|session_id| {
                state
                    .sessions
                    .get(*session_id)
                    .is_some_and(|session| session.parent_session_id == parent_session_id)
            })
            .cloned()
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, ProjectionState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A main-pane session surface attached to one running or completed child.
pub struct LiveSessionView {
    context: ViewContext,
    sessions: LiveSessions,
    session_id: String,
    parent_session_id: String,
    title: String,
    generation: u64,
    transcript: TranscriptView,
    status: StatusView,
    composer: InputEditor,
}

impl LiveSessionView {
    /// Attach a full-screen view to an observed child.
    #[must_use]
    pub fn attach(
        context: ViewContext,
        sessions: LiveSessions,
        session_id: impl Into<String>,
    ) -> Option<Self> {
        let session_id = session_id.into();
        let snapshot = sessions.snapshot(&session_id)?;
        let mut transcript = TranscriptView::new(context.clone());
        transcript.set_activity_display(ActivityDisplay::Summary);
        *transcript.transcript_mut() = snapshot.transcript.clone();
        let mut status = StatusView::new(context.clone());
        status.describe(&snapshot.agent, &snapshot.model);
        status.set_effort(snapshot.effort);
        if snapshot.transcript.is_running() {
            status.mark_running();
        }
        let composer = InputEditor::new(context.clone()).with_placeholder(CHILD_PROMPT_PLACEHOLDER);
        Some(Self {
            context,
            sessions,
            session_id: snapshot.session_id,
            parent_session_id: snapshot.parent_session_id,
            title: snapshot.title,
            generation: snapshot.generation,
            transcript,
            status,
            composer,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn transcript(&self) -> &Transcript {
        self.transcript.transcript()
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.transcript.transcript().is_running()
    }

    pub fn insert_char(&mut self, character: char) -> EventResult {
        self.composer.insert_char(character);
        EventResult::REDRAW
    }

    pub fn insert_text(&mut self, text: &str) -> EventResult {
        self.composer.insert_text(text);
        EventResult::REDRAW
    }

    pub fn handle_composer_action(&mut self, action: &'static Definition) -> EditorSignal {
        self.composer.handle_action(action)
    }

    pub fn push_user_submission(&mut self, text: impl Into<String>) {
        self.transcript
            .transcript_mut()
            .push(Message::user(text.into()));
        self.transcript.follow();
    }

    pub fn mark_turn_accepted(&mut self) {
        self.transcript.transcript_mut().mark_running();
        self.status.mark_running();
    }

    /// Adopt a newer projection without replacing view-local scroll/disclosure state.
    pub fn sync(&mut self) -> bool {
        let Some(snapshot) = self.sessions.snapshot(&self.session_id) else {
            return false;
        };
        if snapshot.generation == self.generation {
            return false;
        }
        self.generation = snapshot.generation;
        self.parent_session_id = snapshot.parent_session_id;
        self.title = snapshot.title;
        *self.transcript.transcript_mut() = snapshot.transcript.clone();
        self.status = StatusView::new(self.context.clone());
        self.status.describe(&snapshot.agent, &snapshot.model);
        self.status.set_effort(snapshot.effort);
        if snapshot.transcript.is_running() {
            self.status.mark_running();
        }
        true
    }

    /// Transcript actions shared with the root session while this child is attached.
    pub fn handle_action(&mut self, action: &'static Definition) -> EventResult {
        let viewport = self.transcript.viewport_height().max(1);
        let max = self
            .transcript
            .content_height()
            .saturating_sub(self.transcript.viewport_height());
        let offset = self.transcript.offset();
        let moved = |delta: isize| -> usize {
            let target = isize::try_from(offset)
                .unwrap_or(isize::MAX)
                .saturating_add(delta);
            usize::try_from(target.max(0)).unwrap_or(0).min(max)
        };
        let half = isize::try_from(viewport / 2).unwrap_or(1).max(1);
        let page = isize::try_from(viewport).unwrap_or(1);
        match action.name {
            "display_thinking" => self.transcript.toggle_thinking(),
            "tool_details" => self.transcript.toggle_tool_output(),
            "messages_line_up" => self.transcript.set_offset(moved(-1)),
            "messages_line_down" => self.transcript.set_offset(moved(1)),
            "messages_page_up" => self.transcript.set_offset(moved(-page)),
            "messages_page_down" => self.transcript.set_offset(moved(page)),
            "messages_half_page_up" => self.transcript.set_offset(moved(-half)),
            "messages_half_page_down" => self.transcript.set_offset(moved(half)),
            "messages_first" => self.transcript.set_offset(0),
            "messages_last" => {
                self.transcript.set_offset(max);
                self.transcript.follow();
            }
            _ => return EventResult::IGNORED,
        }
        EventResult::REDRAW
    }

    fn header(&self, width: u16) -> Vec<Line<'static>> {
        let state = if self.transcript.transcript().is_running() {
            "running"
        } else {
            "complete"
        };
        vec![
            padded(&format!(" ◇ {}", self.title), width, self.context.title()),
            padded(
                &format!(
                    "   {state} · session {} · parent {}",
                    self.session_id, self.parent_session_id
                ),
                width,
                self.context.muted(),
            ),
        ]
    }

    fn footer(&self, width: u16) -> Line<'static> {
        let submit = if self.is_running() {
            "enter steer"
        } else {
            "enter continue"
        };
        let parent = pressable_label("session_parent", &self.context)
            .map_or_else(|| String::from("parent"), |key| format!("{key} parent"));
        let next = pressable_label("session_child_cycle", &self.context).map_or_else(
            || String::from("next sibling"),
            |key| format!("{key} next sibling"),
        );
        padded(
            &format!(" {submit} · {parent} · {next}"),
            width,
            self.context.muted(),
        )
    }
}

impl Component for LiveSessionView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.sync();
        fill(frame.buffer_mut(), area, self.context.surface());
        let composer_rows = prompt_rows(self.composer.height(), area.height);
        let [header, body, identity, composer, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(u16::from(self.status.has_identity())),
            Constraint::Length(composer_rows),
            Constraint::Length(1),
        ])
        .areas(area);
        Paragraph::new(self.header(header.width))
            .style(self.context.surface())
            .render(header, frame.buffer_mut());
        self.transcript.render(frame, body);
        self.status.render(frame, identity);
        fill(frame.buffer_mut(), composer, self.context.element());
        let (gutter, buffer) = prompt_frame(composer);
        if let Some(gutter) = gutter {
            PromptGutter::new(self.context.clone(), PROMPT_MARKER.to_owned()).render(frame, gutter);
        }
        self.composer.render(frame, buffer);
        Paragraph::new(vec![self.footer(footer.width)])
            .style(self.context.surface())
            .render(footer, frame.buffer_mut());
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        let projection = if self.sync() {
            EventResult::REDRAW
        } else {
            EventResult::IGNORED
        };
        if matches!(event, AppEvent::AnimationFrame) {
            projection.merge(self.transcript.handle_event(event))
        } else {
            projection
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveSessionOpen, LiveSessions};
    use crate::views::message::{Message, Role};
    use zuno_engine::r#loop::TurnEvent;
    use zuno_llm::event::StreamEvent;

    #[test]
    fn child_events_are_visible_before_the_child_completes() {
        let sessions = LiveSessions::default();
        sessions.open(LiveSessionOpen {
            session_id: String::from("ses_child"),
            parent_session_id: String::from("ses_parent"),
            title: String::from("inspect the workspace"),
            agent: String::from("explorer"),
            model: String::from("test/model"),
            effort: None,
            messages: vec![Message {
                role: Role::User,
                id: Some(String::from("msg_child_user")),
                parts: vec![crate::views::message::MessagePart::Text {
                    text: String::from("inspect now"),
                }],
            }],
            usage: None,
        });
        sessions.observe(
            "ses_child",
            &TurnEvent::AssistantMessageCreated {
                step: 1,
                message_id: String::from("msg_child_assistant"),
            },
        );
        sessions.observe(
            "ses_child",
            &TurnEvent::Provider {
                step: 1,
                event: StreamEvent::TextDelta(String::from("still working")),
            },
        );

        let snapshot = sessions
            .snapshot("ses_child")
            .expect("the live child remains projected");
        assert!(snapshot.transcript.is_running());
        assert_eq!(snapshot.parent_session_id, "ses_parent");
        assert!(
            snapshot
                .transcript
                .messages()
                .iter()
                .flat_map(|message| message.parts.iter())
                .filter_map(crate::views::message::MessagePart::text)
                .any(|text| text.contains("still working")),
            "the provider delta was hidden until child completion"
        );
    }

    #[test]
    fn direct_children_keep_a_stable_projection_order() {
        let sessions = LiveSessions::default();
        for (id, title) in [("ses_one", "one"), ("ses_two", "two")] {
            sessions.open(LiveSessionOpen {
                session_id: id.to_owned(),
                parent_session_id: String::from("ses_parent"),
                title: title.to_owned(),
                agent: String::from("explorer"),
                model: String::from("test/model"),
                effort: None,
                messages: Vec::new(),
                usage: None,
            });
        }

        assert_eq!(
            sessions.children("ses_parent"),
            vec![String::from("ses_one"), String::from("ses_two")]
        );
    }
}
