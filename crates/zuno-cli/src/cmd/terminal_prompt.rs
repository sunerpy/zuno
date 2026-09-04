//! Small terminal prompts for bounded CLI workflows.
//!
//! These prompts deliberately live outside the resident TUI. Provider login is a
//! short-lived command, but it still needs the same discoverability contract:
//! visible choices, keyboard navigation, filtering, cancellation, and no escape
//! sequences when standard input or standard error is redirected.

use std::io::{IsTerminal as _, Write as _};

use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};

const MAX_VISIBLE_ROWS: usize = 8;

/// One selectable terminal row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    value: String,
    label: String,
    hint: String,
}

impl Choice {
    #[must_use]
    pub(crate) fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            hint: String::new(),
        }
    }

    #[must_use]
    pub(crate) fn hinted(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }
}

/// Whether a cursor-driven prompt can safely own this process's terminal.
#[must_use]
pub(crate) fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask one yes/no question on standard error and read the answer from standard input.
///
/// Fails closed: without an interactive terminal there is nobody to answer, so the
/// caller gets an error rather than a default. Only `y`/`yes` (any case) is a yes.
pub(crate) fn confirm(message: &str) -> Result<bool, String> {
    confirm_with(message, is_interactive(), || {
        eprint!("{message} [y/N] ");
        std::io::stderr()
            .flush()
            .map_err(|error| error.to_string())?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        Ok(answer)
    })
}

fn confirm_with(
    message: &str,
    interactive: bool,
    read_answer: impl FnOnce() -> Result<String, String>,
) -> Result<bool, String> {
    if !interactive {
        return Err(format!("{message} requires an interactive terminal"));
    }
    let answer = read_answer()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Ask a Yes/No question as a two-row picker and return `true` only for an explicit Yes.
///
/// Built on [`select`], so it inherits its terminal contract: it fails closed with
/// `"{message} requires an interactive terminal"` when standard input or standard
/// error is redirected, and it never reads a default from a pipe. "No" is the first
/// row, so pressing Enter without moving the cursor declines; Esc and Ctrl-C decline
/// too. A caller that must proceed without a terminal has to say so through its own
/// explicit flag rather than through this prompt.
pub(crate) fn confirm_choice(message: &str) -> Result<bool, String> {
    const YES: &str = "yes";
    const NO: &str = "no";
    let choices = vec![
        Choice::new(NO, "No").hinted("leave everything untouched"),
        Choice::new(YES, "Yes").hinted("run it"),
    ];
    Ok(select(message, choices)?.as_deref() == Some(YES))
}

/// Select one value with arrows, paging, and type-to-filter.
pub(crate) fn select(message: &str, choices: Vec<Choice>) -> Result<Option<String>, String> {
    if choices.is_empty() {
        return Err(format!("{message} has no choices"));
    }
    if !is_interactive() {
        return Err(format!("{message} requires an interactive terminal"));
    }

    let mut terminal = TerminalSession::enter()?;
    let mut state = SelectionState::new(choices);
    let mut rendered_lines = 0_u16;
    loop {
        rendered_lines = terminal.render(message, &state, rendered_lines)?;
        match crossterm::event::read().map_err(|error| error.to_string())? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c'))
                    || matches!(key.code, KeyCode::Esc)
                {
                    terminal.clear(rendered_lines)?;
                    return Ok(None);
                }
                match key.code {
                    KeyCode::Enter => {
                        if let Some(choice) = state.selected().cloned() {
                            terminal.clear(rendered_lines)?;
                            drop(terminal);
                            eprintln!("{message}: {}", choice.label);
                            return Ok(Some(choice.value));
                        }
                    }
                    KeyCode::Up => state.move_cursor(-1),
                    KeyCode::Down => state.move_cursor(1),
                    KeyCode::Home => state.move_home(),
                    KeyCode::End => state.move_end(),
                    KeyCode::PageUp => state.move_cursor(-(MAX_VISIBLE_ROWS as isize)),
                    KeyCode::PageDown => state.move_cursor(MAX_VISIBLE_ROWS as isize),
                    KeyCode::Backspace => state.backspace(),
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.clear_filter();
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL
                                | KeyModifiers::ALT
                                | KeyModifiers::SUPER
                                | KeyModifiers::HYPER
                                | KeyModifiers::META,
                        ) =>
                    {
                        state.push(character);
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => state.extend(&text),
            Event::Resize(_, _) => {}
            _ => continue,
        }
    }
}

struct TerminalSession;

impl TerminalSession {
    fn enter() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        if let Err(error) = execute!(std::io::stderr(), Hide) {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error.to_string());
        }
        Ok(Self)
    }

    fn render(
        &mut self,
        message: &str,
        state: &SelectionState,
        previous_lines: u16,
    ) -> Result<u16, String> {
        self.clear(previous_lines)?;
        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
        let rows = usize::from(height.saturating_sub(4))
            .clamp(1, MAX_VISIBLE_ROWS)
            .min(state.visible.len().max(1));
        let mut lines = Vec::with_capacity(rows + 3);
        lines.push(format!("? {message}"));
        lines.push(if state.filter.is_empty() {
            "  Search: type to filter".to_owned()
        } else {
            format!("  Search: {}", state.filter)
        });

        if state.visible.is_empty() {
            lines.push("  No matches".to_owned());
        } else {
            for (position, index) in state.window(rows) {
                let choice = &state.choices[*index];
                let marker = if position == state.cursor { ">" } else { " " };
                let hint = if choice.hint.is_empty() {
                    String::new()
                } else {
                    format!("  {}", choice.hint)
                };
                lines.push(format!("{marker} {}{hint}", choice.label));
            }
        }
        lines.push("  ↑↓ move  type search  enter select  esc cancel".to_owned());

        let mut stderr = std::io::stderr();
        for line in &lines {
            let line = truncate(line, usize::from(width.saturating_sub(1)));
            queue!(stderr, MoveToColumn(0), Clear(ClearType::CurrentLine))
                .map_err(|error| error.to_string())?;
            write!(stderr, "{line}\r\n").map_err(|error| error.to_string())?;
        }
        stderr.flush().map_err(|error| error.to_string())?;
        u16::try_from(lines.len()).map_err(|_| "terminal prompt is too tall".to_owned())
    }

    fn clear(&mut self, lines: u16) -> Result<(), String> {
        if lines == 0 {
            return Ok(());
        }
        let mut stderr = std::io::stderr();
        queue!(
            stderr,
            MoveUp(lines),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )
        .map_err(|error| error.to_string())?;
        stderr.flush().map_err(|error| error.to_string())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(std::io::stderr(), Show);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[derive(Debug)]
struct SelectionState {
    choices: Vec<Choice>,
    visible: Vec<usize>,
    filter: String,
    cursor: usize,
}

impl SelectionState {
    fn new(choices: Vec<Choice>) -> Self {
        let visible = (0..choices.len()).collect();
        Self {
            choices,
            visible,
            filter: String::new(),
            cursor: 0,
        }
    }

    fn selected(&self) -> Option<&Choice> {
        self.visible
            .get(self.cursor)
            .and_then(|index| self.choices.get(*index))
    }

    fn window(&self, rows: usize) -> impl Iterator<Item = (usize, &usize)> {
        let start = self.cursor.saturating_sub(rows.saturating_sub(1));
        self.visible.iter().enumerate().skip(start).take(rows)
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.visible.is_empty() {
            self.cursor = 0;
            return;
        }
        self.cursor =
            ((self.cursor as isize + delta).rem_euclid(self.visible.len() as isize)) as usize;
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.visible.len().saturating_sub(1);
    }

    fn push(&mut self, character: char) {
        self.filter.push(character);
        self.rebuild();
    }

    fn extend(&mut self, text: &str) {
        self.filter.push_str(text);
        self.rebuild();
    }

    fn backspace(&mut self) {
        self.filter.pop();
        self.rebuild();
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut ranked = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                choice_score(choice, &self.filter).map(|score| (score, index))
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(score, index)| (std::cmp::Reverse(*score), *index));
        self.visible = ranked.into_iter().map(|(_, index)| index).collect();
        self.cursor = 0;
    }
}

fn choice_score(choice: &Choice, filter: &str) -> Option<u32> {
    if filter.is_empty() {
        return Some(0);
    }
    [
        fuzzy_score(&choice.label, filter),
        fuzzy_score(&choice.value, filter),
        fuzzy_score(&choice.hint, filter),
    ]
    .into_iter()
    .flatten()
    .max()
}

fn fuzzy_score(candidate: &str, filter: &str) -> Option<u32> {
    let candidate = candidate.to_lowercase();
    let filter = filter.to_lowercase();
    if filter.is_empty() {
        return Some(0);
    }
    if let Some(position) = candidate.find(&filter) {
        return Some(
            10_000_u32
                .saturating_add(
                    u32::try_from(filter.chars().count())
                        .unwrap_or(u32::MAX)
                        .saturating_mul(100),
                )
                .saturating_sub(u32::try_from(position).unwrap_or(u32::MAX)),
        );
    }

    let mut characters = candidate.chars().enumerate();
    let mut first = None;
    let mut last = 0_usize;
    for wanted in filter.chars() {
        let (position, _) = characters.find(|(_, character)| *character == wanted)?;
        first.get_or_insert(position);
        last = position;
    }
    let span = last.saturating_sub(first.unwrap_or(last));
    Some(
        1_000_u32
            .saturating_add(
                u32::try_from(filter.chars().count())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(10),
            )
            .saturating_sub(u32::try_from(span).unwrap_or(u32::MAX)),
    )
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut truncated = text.chars().take(width - 1).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<Choice> {
        vec![
            Choice::new("openai", "OpenAI").hinted("ChatGPT Plus/Pro or API key"),
            Choice::new("myopenai", "My OpenAI").hinted("configured"),
            Choice::new("anthropic", "Anthropic"),
        ]
    }

    #[test]
    fn filtering_matches_labels_ids_and_hints() {
        let mut state = SelectionState::new(choices());
        state.extend("myopen");
        assert_eq!(
            state.selected().map(|choice| choice.value.as_str()),
            Some("myopenai")
        );

        state.clear_filter();
        state.extend("chatgpt");
        assert_eq!(
            state.selected().map(|choice| choice.value.as_str()),
            Some("openai")
        );
    }

    #[test]
    fn cursor_wraps_and_filtering_resets_it() {
        let mut state = SelectionState::new(choices());
        state.move_cursor(-1);
        assert_eq!(
            state.selected().map(|choice| choice.value.as_str()),
            Some("anthropic")
        );
        state.push('o');
        assert_eq!(state.cursor, 0);
        assert!(!state.visible.is_empty());
    }

    #[test]
    fn confirm_fails_closed_without_a_terminal_and_accepts_only_yes() {
        let error = confirm_with("Continue?", false, || {
            panic!("a non-interactive confirm must not read standard input")
        })
        .expect_err("no terminal, no answer");
        assert_eq!(error, "Continue? requires an interactive terminal");

        for (answer, expected) in [
            ("y\n", true),
            ("Yes\n", true),
            ("  YES  \n", true),
            ("n\n", false),
            ("\n", false),
            ("maybe\n", false),
        ] {
            let accepted = confirm_with("Continue?", true, || Ok(answer.to_owned()))
                .expect("an interactive answer is read");
            assert_eq!(accepted, expected, "answer {answer:?}");
        }

        let error = confirm_with("Continue?", true, || Err("closed".to_owned()))
            .expect_err("a failed read is an error, not a no");
        assert_eq!(error, "closed");
    }

    #[test]
    fn narrow_lines_are_truncated_without_splitting_characters() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("你好世界", 3), "你好…");
        assert_eq!(truncate("abc", 8), "abc");
    }
}
