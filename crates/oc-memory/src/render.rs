//! Live usage accounting and the system-prompt block a store renders into.

use crate::scope::{ENTRY_DELIMITER, Scope, char_count};

/// The rule drawn above and below the header — `memory_tool.py:746`, 46 `═`.
const RULE_WIDTH: usize = 46;

/// How full a store is, in the unit its cap is expressed in.
///
/// Returned by every successful write so the caller can report progress without a
/// second read, and rendered into the block header so the model sees its own
/// remaining budget in the prompt rather than having to ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Which store this describes.
    pub scope: Scope,
    /// Characters currently held, delimiters included — the same number the cap is
    /// compared against, so a caller can never be shown a figure that disagrees
    /// with the one that admitted or refused the write.
    pub current: usize,
    /// The scope's cap, from [`Scope::cap`].
    pub limit: usize,
    /// How many entries are held.
    pub entries: usize,
}

impl Usage {
    /// Percentage of the cap in use, clamped to 100.
    ///
    /// Integer arithmetic where the reference computes a float and truncates
    /// (`memory_tool.py:710`, `:739`). Same result for every value a store can
    /// hold, and exact rather than exact-looking.
    #[must_use]
    pub const fn percent(self) -> usize {
        if self.limit == 0 {
            return 0;
        }
        let raw = self.current * 100 / self.limit;
        if raw > 100 { 100 } else { raw }
    }

    /// Characters still available before the cap refuses a write.
    #[must_use]
    pub const fn remaining(self) -> usize {
        self.limit.saturating_sub(self.current)
    }
}

impl std::fmt::Display for Usage {
    /// `63% — 1,390/2,200 chars`, the reference's `usage` string
    /// (`memory_tool.py:713`) including its em dash and thousands separators.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}% — {}/{} chars",
            self.percent(),
            group_thousands(self.current),
            group_thousands(self.limit),
        )
    }
}

/// `1390` → `1,390`.
///
/// Hand-rolled because the reference's `{:,}` format spec has no Rust equivalent
/// and a formatting crate would be a dependency for one comma.
fn group_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let first_group = digits.len() % 3;
    for (index, ch) in digits.char_indices() {
        if index > 0 && index % 3 == first_group {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The current size of a store holding `entries`.
#[must_use]
pub fn usage_of(scope: Scope, entries: &[String]) -> Usage {
    usage_of_with_limit(scope, entries, scope.cap())
}

/// The current size of a store under an explicit character budget.
#[must_use]
pub fn usage_of_with_limit(scope: Scope, entries: &[String], limit: usize) -> Usage {
    Usage {
        scope,
        current: char_count(&serialize(entries)),
        limit,
        entries: entries.len(),
    }
}

/// Join entries into the exact text that goes on disk.
///
/// The one place the delimiter is applied. The cap is measured against this
/// string, not against the sum of the entries, because the delimiters are real
/// characters in the prompt.
#[must_use]
pub fn serialize(entries: &[String]) -> String {
    entries.join(ENTRY_DELIMITER)
}

/// Split stored text back into trimmed, non-empty entries.
///
/// Blank entries are dropped rather than preserved: a trailing newline or a
/// double delimiter would otherwise become an entry that renders as nothing but
/// still spends three characters of budget.
#[must_use]
pub fn parse(raw: &str) -> Vec<String> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    raw.split(ENTRY_DELIMITER)
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Render the block that gets injected into the system prompt.
///
/// Shape from `memory_tool.py:731-747`: a 46-character rule, the scope label with
/// live usage in brackets, the same rule, then the entries joined by the
/// delimiter.
///
/// **Empty in, empty out** — the reference returns `""` for a store with no
/// entries (`:733-734`) and that is not a cosmetic choice. A header claiming
/// `0% — 0/2,200 chars` would occupy prompt space to announce that it has nothing
/// to say, and would leave a block for todo 99's consistency check to find after
/// the last entry is removed.
///
/// The usage shown is the usage *at render time*. Todo 99 freezes the rendered
/// string for the life of a session, so a mid-session write updates the file and
/// the next session's header, never this one.
#[must_use]
pub fn render_block(scope: Scope, entries: &[String]) -> String {
    render_block_with_limit(scope, entries, scope.cap())
}

/// Render a block whose header reports an explicit character budget.
#[must_use]
pub fn render_block_with_limit(scope: Scope, entries: &[String], limit: usize) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let rule = "═".repeat(RULE_WIDTH);
    let body = serialize(entries);
    let usage = Usage {
        scope,
        current: char_count(&body),
        limit,
        entries: entries.len(),
    };
    format!("{rule}\n{} [{usage}]\n{rule}\n{body}", scope.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separator_matches_the_reference_format() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(63), "63");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_390), "1,390");
        assert_eq!(group_thousands(2_200), "2,200");
        assert_eq!(group_thousands(10_000), "10,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn usage_renders_the_header_string_from_the_plan() {
        let usage = Usage {
            scope: Scope::Global,
            current: 1_390,
            limit: 2_200,
            entries: 7,
        };
        assert_eq!(usage.to_string(), "63% — 1,390/2,200 chars");
        assert_eq!(
            format!("{} [{usage}]", Scope::Global.label()),
            "MEMORY (agent notes) [63% — 1,390/2,200 chars]"
        );
    }

    #[test]
    fn percent_is_clamped_and_never_divides_by_zero() {
        let over = Usage {
            scope: Scope::Global,
            current: 5_000,
            limit: 2_200,
            entries: 1,
        };
        assert_eq!(over.percent(), 100);
        assert_eq!(over.remaining(), 0);

        let degenerate = Usage {
            scope: Scope::Global,
            current: 10,
            limit: 0,
            entries: 1,
        };
        assert_eq!(degenerate.percent(), 0);
    }

    #[test]
    fn serialize_and_parse_round_trip() {
        let entries = vec![
            "first note".to_string(),
            "second note\nwith a second line".to_string(),
            "third § note with a section sign inline".to_string(),
        ];
        assert_eq!(parse(&serialize(&entries)), entries);
    }

    #[test]
    fn parse_drops_blank_entries_and_trims() {
        let raw = format!("  a  {ENTRY_DELIMITER}{ENTRY_DELIMITER}  b  \n");
        assert_eq!(parse(&raw), vec!["a".to_string(), "b".to_string()]);
        assert!(parse("   \n  ").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn usage_counts_the_delimiters() {
        let entries = vec!["ab".to_string(), "cd".to_string()];
        let usage = usage_of(Scope::Global, &entries);
        assert_eq!(usage.current, 7, "2 + 3 delimiter chars + 2");
        assert_eq!(usage.entries, 2);
    }

    #[test]
    fn an_empty_store_renders_nothing() {
        assert_eq!(render_block(Scope::Global, &[]), "");
        assert_eq!(render_block(Scope::Project, &[]), "");
    }

    #[test]
    fn block_has_the_reference_shape() {
        let entries = vec!["run cargo test, not cargo build".to_string()];
        let block = render_block(Scope::Project, &entries);
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0].chars().count(), RULE_WIDTH);
        assert_eq!(lines[0], lines[2]);
        assert!(lines[0].chars().all(|c| c == '═'));
        assert_eq!(
            lines[1], "MEMORY (project rules) [1% — 31/3,000 chars]",
            "the header carries live usage, not a static label"
        );
        assert_eq!(lines[3], "run cargo test, not cargo build");
    }
}
