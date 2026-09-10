//! Legacy terminal paste aggregation before keybinding dispatch.
//!
//! Adapted from the state-machine contract in Codex 9ba1d9eb's PasteBurst:
//! explicit bracketed paste is authoritative; rapid legacy text is buffered,
//! and its Enter/Tab events are text, never Submit or Agent-cycle actions.
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const CHAR_INTERVAL: Duration = Duration::from_millis(8);
const ENTER_WINDOW: Duration = Duration::from_millis(120);
const MAX_BUFFER: usize = 8 * 1024 * 1024;

pub(crate) enum NormalizedInput {
    Key(KeyEvent),
    Paste(String),
}

pub(crate) struct PasteBurst {
    pending: Option<KeyEvent>,
    buffer: String,
    last: Option<Instant>,
    active: bool,
    suppress_enter_until: Option<Instant>,
    idle: Duration,
}

impl PasteBurst {
    pub(crate) fn new(windows: bool) -> Self {
        Self {
            pending: None,
            buffer: String::new(),
            last: None,
            active: false,
            suppress_enter_until: None,
            idle: if windows {
                Duration::from_millis(60)
            } else {
                CHAR_INTERVAL
            },
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.pending.is_none() && self.buffer.is_empty() {
            return None;
        }
        self.last.map(|last| {
            last + if self.active {
                self.idle
            } else {
                CHAR_INTERVAL
            }
        })
    }

    pub(crate) fn flush_due(&mut self, now: Instant) -> Vec<NormalizedInput> {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            self.flush()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn flush(&mut self) -> Vec<NormalizedInput> {
        let mut output = Vec::new();
        if self.active {
            if let Some(key) = self.pending.take() {
                self.append(key);
            }
            if !self.buffer.is_empty() {
                output.push(NormalizedInput::Paste(std::mem::take(&mut self.buffer)));
            }
        } else if let Some(key) = self.pending.take() {
            output.push(NormalizedInput::Key(key));
        }
        self.active = false;
        output
    }

    pub(crate) fn reset(&mut self) {
        self.pending = None;
        self.buffer.clear();
        self.last = None;
        self.active = false;
        self.suppress_enter_until = None;
    }

    /// `false` means the caller must forward the original key after `output`.
    pub(crate) fn key(&mut self, key: KeyEvent, now: Instant) -> (Vec<NormalizedInput>, bool) {
        if key.kind == KeyEventKind::Release {
            return (Vec::new(), true);
        }
        let mut output = self.flush_due(now);
        let text_key = matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab)
            && !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            )
            && (matches!(key.code, KeyCode::Char(_)) || key.modifiers.is_empty());
        if !text_key {
            output.extend(self.flush());
            self.reset();
            return (output, false);
        }
        let recent = self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) <= CHAR_INTERVAL);
        let suppress_enter = key.code == KeyCode::Enter
            && self.suppress_enter_until.is_some_and(|until| now <= until);
        if self.active || (recent && self.pending.is_some()) || suppress_enter {
            if let Some(first) = self.pending.take() {
                self.append(first);
            }
            self.append(key);
            self.active = true;
            self.suppress_enter_until = Some(now + ENTER_WINDOW);
        } else if matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii()) {
            // Do not hold the first IME/non-ASCII character.
            output.push(NormalizedInput::Key(key));
            self.suppress_enter_until = Some(now + CHAR_INTERVAL);
        } else {
            self.pending = Some(key);
        }
        self.last = Some(now);
        if self.buffer.len() >= MAX_BUFFER {
            output.extend(self.flush());
        }
        (output, true)
    }

    fn append(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(ch) => self.buffer.push(ch),
            KeyCode::Enter => self.buffer.push('\n'),
            KeyCode::Tab => self.buffer.push('\t'),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_multiline_burst_emits_one_paste_and_never_enter_keys() {
        let mut burst = PasteBurst::new(true);
        let start = Instant::now();
        let input = [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Char('b'),
            KeyCode::Enter,
        ];
        for (index, code) in input.into_iter().enumerate() {
            let (out, held) = burst.key(
                KeyEvent::new(code, KeyModifiers::NONE),
                start + Duration::from_millis(index as u64),
            );
            assert!(held && out.is_empty());
        }
        let output = burst.flush_due(start + Duration::from_millis(65));
        assert!(matches!(&output[..], [NormalizedInput::Paste(text)] if text == "a\n\tb\n"));
        let (output, _) = burst.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            start + Duration::from_millis(300),
        );
        assert!(output.is_empty());
        assert!(
            matches!(&burst.flush_due(start + Duration::from_millis(309))[..],
            [NormalizedInput::Key(key)] if key.code == KeyCode::Enter)
        );
    }

    #[test]
    fn ordinary_typing_and_ime_are_not_lost_or_replayed() {
        let start = Instant::now();
        let mut burst = PasteBurst::new(false);
        burst.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), start);
        assert!(
            matches!(&burst.flush_due(start + Duration::from_millis(9))[..],
            [NormalizedInput::Key(key)] if key.code == KeyCode::Char('a'))
        );
        let (output, held) = burst.key(
            KeyEvent::new(KeyCode::Char('中'), KeyModifiers::NONE),
            start + Duration::from_millis(20),
        );
        assert!(held);
        assert!(
            matches!(&output[..], [NormalizedInput::Key(key)] if key.code == KeyCode::Char('中'))
        );
        assert!(burst.flush_due(start + Duration::from_secs(1)).is_empty());
    }
}
