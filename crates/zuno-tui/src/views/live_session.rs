//! Live child-session projections and the full-screen view attached to one of them.
//!
//! A delegated turn owns its own engine channel and host. The TUI must not remount the
//! parent merely to inspect that channel: remounting tears the parent host down. This
//! projection is the read model between durable child history, independently running hosts,
//! and the one terminal surface.

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::Definition;
use crate::views::editor::{EditorSignal, InputEditor, PromptGutter};
use crate::views::message::{ActivityDisplay, Message, StatusView, Transcript, TranscriptView};
use crate::views::scroll::Scroller;
use crate::views::session::{PROMPT_MARKER, compact_live_tokens, prompt_frame, prompt_rows};
use crate::views::{ViewContext, fill, padded, pressable_label};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
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

/// Durable and live projections for every child the current TUI composition can inspect.
#[derive(Debug, Clone, Default)]
pub struct LiveSessions(Arc<Mutex<ProjectionState>>);

impl LiveSessions {
    /// Publish the replayed prefix before the child turn starts.
    pub fn open(&self, opened: LiveSessionOpen) {
        self.publish(opened, true);
    }

    /// Restore one durable child whose turn is not currently process-owned.
    pub fn restore(&self, opened: LiveSessionOpen) {
        self.publish(opened, false);
    }

    fn publish(&self, opened: LiveSessionOpen, running: bool) {
        let mut transcript = Transcript::new();
        if let Some(usage) = opened.usage {
            transcript.restore_usage(usage);
        }
        transcript.replay(opened.messages);
        if running {
            transcript.mark_running();
        }

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
            TurnEvent::SessionMaterialized { title, .. }
            | TurnEvent::SessionTitleUpdated { title }
                if session.title != *title =>
            {
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
    scroller: Scroller,
    status: StatusView,
    composer: InputEditor,
    attachments: crate::views::attachment::AttachmentDraft,
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
        let scroller = Scroller::new(&context.config);
        let composer = InputEditor::new(context.clone()).with_placeholder(CHILD_PROMPT_PLACEHOLDER);
        Some(Self {
            context,
            sessions,
            session_id: snapshot.session_id,
            parent_session_id: snapshot.parent_session_id,
            title: snapshot.title,
            generation: snapshot.generation,
            transcript,
            scroller,
            status,
            composer,
            attachments: crate::views::attachment::AttachmentDraft::default(),
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

    #[must_use]
    pub fn composer_is_empty(&self) -> bool {
        self.composer.is_empty()
    }

    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.transcript.offset()
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.transcript.set_offset(offset);
        self.sync_scroller();
    }

    pub fn insert_char(&mut self, character: char) -> EventResult {
        self.composer.insert_char(character);
        EventResult::REDRAW
    }

    pub fn insert_text(&mut self, text: &str) -> EventResult {
        self.composer.insert_text(text);
        EventResult::REDRAW
    }

    pub fn attach_pasted_image(&mut self, text: &str) -> Result<bool, String> {
        let Some(placeholder) = self.attachments.attach_pasted_path(text)? else {
            return Ok(false);
        };
        self.composer.insert_text(&placeholder);
        Ok(true)
    }

    pub fn attach_clipboard_image(&mut self, media_type: &str, data: &str) -> Result<(), String> {
        let placeholder = self.attachments.attach_clipboard_image(media_type, data)?;
        self.composer.insert_text(&placeholder);
        Ok(())
    }

    pub(crate) fn take_attached_prompt(
        &mut self,
        text: &str,
    ) -> Option<crate::views::attachment::AttachedPrompt> {
        self.attachments.take_prompt(text)
    }

    pub fn handle_composer_action(&mut self, action: &'static Definition) -> EditorSignal {
        let attached_submission = matches!(
            action.name,
            "input_submit" | "input_force_submit" | "prompt_submit"
        ) && self
            .attachments
            .has_attached_prompt(&self.composer.submission_text());
        if attached_submission {
            self.composer.handle_action_without_history(action)
        } else {
            self.composer.handle_action(action)
        }
    }

    /// Route a pointer gesture into the attached child's own composer.
    pub fn handle_mouse(&mut self, mouse: &MouseEvent) -> EditorSignal {
        let signal = self.composer.handle_mouse(mouse);
        if signal == EditorSignal::Changed
            && matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
            && let Some(text) = self.composer.selection()
        {
            return EditorSignal::Copy(text);
        }
        signal
    }

    /// Scroll this child's transcript while preserving its independent viewport.
    pub fn scroll_wheel(&mut self, notches: f64, now_ms: u64) -> EventResult {
        self.sync_scroller();
        if self.scroller.wheel(notches, now_ms) == 0 {
            return EventResult::IGNORED;
        }
        self.transcript.set_offset(self.scroller.offset());
        EventResult::REDRAW
    }

    fn sync_scroller(&mut self) {
        self.scroller.total = self.transcript.content_height();
        self.scroller.viewport = self.transcript.viewport_height();
        self.scroller.sync_offset(self.transcript.offset());
    }

    pub fn push_user_submission(&mut self, text: impl Into<String>) {
        self.transcript
            .transcript_mut()
            .push(Message::user(text.into()));
        self.transcript.follow();
    }

    pub(crate) fn push_user_submission_with_attachments(
        &mut self,
        text: impl Into<String>,
        attachments: &[crate::views::attachment::AttachmentLabel],
    ) {
        let mut message = Message::user(text.into());
        for attachment in attachments {
            message.attach(&attachment.filename, Some(attachment.mime.clone()));
        }
        self.transcript.transcript_mut().push(message);
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

    fn footer(&self, width: u16) -> Vec<Line<'static>> {
        let submit = if self.is_running() {
            "enter steer"
        } else {
            "enter continue"
        };
        let parent = pressable_label("session_parent", &self.context)
            .map_or_else(|| String::from("parent"), |key| format!("{key} parent"));
        let previous = pressable_label("session_child_previous_direct", &self.context)
            .unwrap_or_else(|| String::from("left"));
        let next = pressable_label("session_child_next_direct", &self.context)
            .unwrap_or_else(|| String::from("right"));
        let siblings = self.sessions.children(&self.parent_session_id);
        let position = siblings
            .iter()
            .position(|session| session == &self.session_id)
            .map_or(1, |index| index + 1);
        let total = siblings.len().max(1);
        let mut facts = Vec::new();
        if let Some(context) = self.transcript.transcript().context_window() {
            facts.push(format!(
                "ctx {}{}/{} ({:.0}%)",
                if context.estimated { "≈" } else { "" },
                compact_live_tokens(context.prompt_tokens),
                compact_live_tokens(context.limit),
                context.percent()
            ));
        }
        facts.push(format!("child {position}/{total}"));
        let state = facts.join(" · ");
        let navigation = format!("{previous}/{next} siblings · {parent} · {submit}");
        let columns = usize::from(width);
        let mut left = vec![Span::styled(" ".to_owned(), self.context.surface())];
        left.extend(self.status.compact_spans());
        let align = |mut left: Vec<Span<'static>>, right: String| {
            let left_width = crate::views::markdown::row_width(&left);
            let right_width = crate::views::display_width(&right);
            if left_width + right_width < columns {
                left.push(Span::styled(
                    " ".repeat(columns - left_width - right_width),
                    self.context.surface(),
                ));
                left.push(Span::styled(right, self.context.muted()));
                return Line::from(left);
            }
            if right_width <= columns {
                return Line::from(vec![
                    Span::styled(" ".repeat(columns - right_width), self.context.surface()),
                    Span::styled(right, self.context.muted()),
                ]);
            }
            Line::from(crate::views::markdown::truncate_row(left, columns))
        };
        vec![
            align(left, state),
            align(
                vec![Span::styled(" ".to_owned(), self.context.surface())],
                navigation,
            ),
        ]
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
            Constraint::Length(2),
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
        Paragraph::new(self.footer(footer.width))
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
    fn restored_child_history_is_attachable_without_claiming_a_live_turn() {
        let sessions = LiveSessions::default();
        sessions.restore(LiveSessionOpen {
            session_id: String::from("ses_restored_child"),
            parent_session_id: String::from("ses_parent"),
            title: String::from("completed child"),
            agent: String::from("explorer"),
            model: String::from("test/model"),
            effort: None,
            messages: vec![
                Message::user("inspect the repository"),
                Message {
                    role: Role::Assistant,
                    id: None,
                    parts: vec![crate::views::message::MessagePart::Text {
                        text: String::from("inspection complete"),
                    }],
                },
            ],
            usage: None,
        });

        let snapshot = sessions
            .snapshot("ses_restored_child")
            .expect("restored child is indexed");
        assert!(
            !snapshot.transcript.is_running(),
            "durable history was restored as an active child turn"
        );
        assert_eq!(
            sessions.children("ses_parent"),
            [String::from("ses_restored_child")]
        );
    }

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
