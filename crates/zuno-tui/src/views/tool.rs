//! Per-tool call rendering: the argument summary on a call row, the icon that labels
//! it, and how much of its output the transcript is willing to show.
//!
//! # Why this is its own module
//!
//! [`crate::views::message`] renders a *transcript* — roles, rules, wrapping, scroll
//! arithmetic. What a `grep` call should say about itself is a different question with
//! a different answer per tool, and there are twenty-one of them. Keeping the two apart
//! is what lets the per-tool table be read as a table.
//!
//! Directly under `src/views/`, not in a `views/tool/` subdirectory:
//! [`crate::views::views_tests`]'s two discipline scans read `src/views/*.rs`
//! **non-recursively**, so a file one directory down would silently escape both the
//! colour scan and the keybind scan. A module that is exempt from the disciplines
//! because of where it sits is worse than no module.
//!
//! # Colours
//!
//! Every style here is read from the owning [`ViewContext`]'s resolved palette — see
//! [`status_style`], which is the only function in this file that paints. The rest
//! produce plain strings and let the transcript style them.
//!
//! # The argument summary is the point of P2-4
//!
//! Before this module a completed call rendered as `✓ → Read diff.rs` — the tool's own
//! `title`, which names the *kind* of work and not the work. Which file it read was
//! nowhere on screen, and while the call was still running not even the kind was: the
//! row said `Writing command...`. A transcript of six tool calls was six rows that
//! could not be told apart. The arguments are what distinguish them, so the arguments
//! are what the row carries.

use crate::views::{ViewContext, display_width, message::ToolDisplay, truncate};
use ratatui::style::Style;
use serde_json::Value;

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;

/// Which end of a summary is dropped when it does not fit.
///
/// A summary is one row and the row is frequently narrower than the argument, so
/// *which* half survives is a real decision rather than a formatting detail. Both
/// answers are wrong for the other tool: cutting the tail off
/// `crates/zuno-tui/src/views/diff.rs` leaves `crates/zuno-tui/src/vi`, which names no
/// file, while cutting the head off `cargo test --workspace --offline` leaves
/// `…--offline`, which names no command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elide {
    /// Keep the tail — for a path, whose basename identifies it and whose prefix every
    /// sibling shares.
    Head,
    /// Keep the head — for a command, a query or a pattern, which are read left to right.
    Tail,
}

/// What a tool call says about itself on one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The argument that identifies the call. Never dropped whole; only cut.
    pub text: String,
    /// Qualifying detail, dropped entirely before [`Self::text`] is cut.
    ///
    /// The two are separate fields because they must degrade in a fixed order, and one
    /// string cannot express that. Rendered as one string, a narrow row cut
    /// `crates/zuno-tui/src/views/diff.rs [offset=1,limit=162]` from the left and produced
    /// `…iff.rs [offset=1,limit=162]` — the optional read window survived intact while the
    /// *filename*, the only part that says which call this is, was the thing sacrificed for
    /// it. Observed at 40 columns in the rendered frame; nothing asserted about it was
    /// false, which is why it took looking.
    pub detail: Option<String>,
    /// Which end of [`Self::text`] survives when it is still too long.
    pub elide: Elide,
}

impl Summary {
    fn head(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            detail: None,
            elide: Elide::Head,
        }
    }

    fn tail(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            detail: None,
            elide: Elide::Tail,
        }
    }

    /// Attach qualifying detail that yields before the identifying text is cut.
    fn detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail.filter(|text| !text.is_empty());
        self
    }

    /// The summary fitted to `room` columns: detail first if it fits, else the text alone,
    /// cut at the end [`Self::elide`] names.
    ///
    /// Measured in terminal columns throughout, like every other width in this crate: a CJK
    /// path counted in characters comes back "short enough" and then overruns the frame by
    /// one column per glyph.
    #[must_use]
    pub fn fit(&self, room: usize) -> String {
        if let Some(detail) = &self.detail {
            let whole = format!("{} {detail}", self.text);
            if display_width(&whole) <= room {
                return whole;
            }
        }
        if display_width(&self.text) <= room {
            return self.text.clone();
        }
        match self.elide {
            Elide::Head => crate::views::ambient::elide_left(&self.text, room),
            Elide::Tail => {
                // The mark is charged a column, the same way `elide_left` charges it, so the
                // two cuts occupy the same width and a column of tool rows stays aligned
                // whichever end each one lost.
                if room <= 1 {
                    return truncate(&self.text, room);
                }
                format!(
                    "{}{}",
                    truncate(&self.text, room - 1),
                    crate::views::message::ELIDED
                )
            }
        }
    }
}

/// Every tool this module has a hand-written summary rule for.
///
/// **Not a hand-kept list of what exists.** It is a hand-kept list of what has been
/// *given a rule*, and `tool_summaries_cover_every_tool_the_registry_can_expose` fails
/// the build when the registry grows a tool this table has not been taught. The
/// authoritative names are read out of `zuno-tools`' own sources by that test rather
/// than copied here, because a copied list is a claim only until somebody edits one of
/// the two — which is how `editor_open` in this project ended up unreachable.
///
/// Reading the registry's *source* rather than depending on the crate is deliberate:
/// `zuno-tui` does not link the tool stack, and pulling twenty transitive crates into
/// the render layer to learn twenty-one strings is a worse trade than a scan. It is the
/// same technique [`crate::views::views_tests`] already uses for the colour and keybind
/// disciplines.
pub const SUMMARISED: [&str; 26] = [
    // The 18 `BuiltinSlot` positions, in `BUILTIN_ORDER`.
    "invalid",
    "question",
    "bash",
    "bg",
    "read",
    "glob",
    "grep",
    "edit",
    "write",
    "task",
    "job",
    "webfetch",
    "web_search",
    "skill",
    "apply_patch",
    "execute",
    "lsp",
    "plan_exit",
    // Built-ins registered outside the slot table: memory, goal, plan, and todo state.
    "memory_propose",
    "goal_get",
    "goal_propose",
    "goal_update",
    "plan_get",
    "plan_update",
    "todo_get",
    "todo_update",
];

/// What `name` should say about itself, given the arguments the model wrote.
///
/// `arguments` is the **raw** streamed JSON, which is why the parse failure is a normal
/// outcome rather than an error: while a call is still `Pending` the model has sent a
/// prefix of an object, and a prefix of JSON does not parse. Returning `None` then is
/// correct — the row falls back to the placeholder — and it is why nothing here reports
/// a parse error to the user. A malformed argument is the model's problem and the tool
/// will reject it; a transcript that shouted about it would be shouting about a row
/// that is about to be replaced.
///
/// `None` is also the answer for a tool with nothing worth quoting (`plan_exit`,
/// `goal_get`) and for a tool this table does not know (an MCP or plugin tool, whose
/// argument shapes are not knowable here). All three cases render the same way, which
/// is the honest rendering: the row states the tool and claims nothing about its input.
#[must_use]
pub fn summary(name: &str, arguments: &str) -> Option<Summary> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    let text = |key: &str| field(&value, key);
    match name {
        // `$ cmd`, the oracle's own shell form. The prompt character is carried in the
        // summary rather than in the icon because `bash`'s icon is already `$`: printing
        // it twice would read as a nested shell.
        "bash" => text("command").map(|command| {
            let mut out = command.replace('\n', " ");
            if value.get("background").and_then(Value::as_bool) == Some(true) {
                // Stated because it changes what the *result* row will mean: a background
                // command's output arrives later and its exit code is not this row's.
                out.push_str(" &");
            }
            Summary::tail(out)
        }),
        // The read window is appended only when the model asked for one. A permanent
        // `[offset=0,limit=∞]` on every read would be three tools' worth of noise on the
        // one tool that is called most.
        "read" => text("filePath").map(|path| {
            let offset = value.get("offset").and_then(Value::as_u64);
            let limit = value.get("limit").and_then(Value::as_u64);
            let window = match (offset, limit) {
                (None, None) => None,
                (Some(offset), None) => Some(format!("[offset={offset}]")),
                (None, Some(limit)) => Some(format!("[limit={limit}]")),
                (Some(offset), Some(limit)) => Some(format!("[offset={offset},limit={limit}]")),
            };
            // The window is *detail*, so a narrow row drops it whole rather than spending
            // the path's columns on it. `Head` on what remains, because a path is
            // identified by its basename.
            Summary::head(path).detail(window)
        }),
        "write" | "edit" => text("filePath").map(Summary::head),
        // `apply_patch` carries no path field at all — the paths are inside the patch
        // envelope, one per file. Reading the first one is what makes this row say the
        // same kind of thing `edit` says, instead of `apply_patch` twice.
        "apply_patch" => text("patchText").map(|patch| {
            let files = patch_paths(&patch);
            match files.len() {
                0 => Summary::tail(String::from("patch")),
                1 => Summary::head(files[0].clone()),
                // The count, not the list: five paths do not fit and the first one plus a
                // count says which change this is while staying inside one row.
                more => Summary::head(format!("{} +{} more", files[0], more - 1)),
            }
        }),
        // Quoted, because a glob is punctuation all the way through and an unquoted
        // `**/*.rs` beside a path reads as one token.
        "glob" | "grep" => text("pattern").map(|pattern| {
            let mut out = format!("\"{pattern}\"");
            if let Some(path) = text("path") {
                out.push_str(&format!(" in {path}"));
            }
            if let Some(include) = text("include") {
                out.push_str(&format!(" ({include})"));
            }
            // `Tail`: the pattern is the discriminating half and it is on the left. The
            // `in <path>` suffix is context a reader can lose.
            Summary::tail(out)
        }),
        // A URL's tail is its path, which is what distinguishes two fetches of one host.
        "webfetch" => text("url").map(Summary::head),
        "web_search" => {
            let queries = value
                .get("queries")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            (!queries.is_empty()).then(|| Summary::tail(queries))
        }
        // `<subagent_type>: <description>`, so a transcript of six delegations says which
        // six. `description` alone would omit the agent; `prompt` is a paragraph.
        "task" => {
            let what = text("description").or_else(|| text("prompt"))?;
            Some(Summary::tail(match text("subagent_type") {
                Some(agent) => format!("{agent}: {what}"),
                None => what,
            }))
        }
        "job" => text("jobID").map(Summary::tail),
        "bg" => {
            let action = text("action")?;
            Some(Summary::tail(match text("taskID") {
                Some(id) => format!("{action} {id}"),
                None => action,
            }))
        }
        "plan_update" => text("title").map(Summary::tail),
        "plan_get" | "todo_get" => None,
        // The count plus the first changed item. A bare count says nothing about the work,
        // and the whole change set is not a summary — it is the thing being summarised.
        "todo_update" => {
            let changes = value.get("changes").and_then(Value::as_array)?;
            let first = changes
                .first()
                .and_then(|item| field(item, "subject").or_else(|| field(item, "id")))
                .unwrap_or_default();
            Some(Summary::tail(format!(
                "{} changes · {first}",
                changes.len()
            )))
        }
        // The first question, and how many follow. A user is about to be asked these, so
        // the text matters more here than for any other tool.
        "question" => {
            let questions = value.get("questions").and_then(Value::as_array)?;
            let first = questions.first().and_then(|item| field(item, "question"))?;
            Some(Summary::tail(match questions.len() {
                0 | 1 => first,
                more => format!("{first} (+{} more)", more - 1),
            }))
        }
        // No local implementation, so no authoritative schema. `name` then `skill` covers
        // both spellings a skill tool has used, and an unknown shape falls through to the
        // placeholder rather than guessing.
        "skill" => text("name").or_else(|| text("skill")).map(Summary::tail),
        // Same: schema-less here. The operation is what a diagnostics row is about.
        "lsp" => text("action")
            .or_else(|| text("operation"))
            .map(Summary::tail),
        // The batch's shape: how many calls, and which tool leads. Naming all of them
        // would be the batch itself rather than a summary of it.
        "execute" => {
            let calls = value.get("tool_calls").and_then(Value::as_array)?;
            let first = calls
                .first()
                .and_then(|call| field(call, "tool"))
                .unwrap_or_default();
            Some(Summary::tail(format!("{} calls · {first}", calls.len())))
        }
        // The name the model *tried* to call. This is the whole content of an `invalid`
        // call: the error text below it explains why, and the row above it has to say what.
        "invalid" => text("tool").map(Summary::tail),
        // `<action> <target>: <entry>`, e.g. `add project: run cargo fmt`. The action and
        // target identify the mutation while the entry distinguishes concurrent proposals.
        "memory_propose" => {
            let target = text("target").unwrap_or_else(|| String::from("memory"));
            let action = text("action").or_else(|| {
                value
                    .get("operations")
                    .and_then(Value::as_array)
                    .and_then(|operations| operations.first())
                    .and_then(|operation| field(operation, "action"))
            });
            let operation = match action {
                Some(action) => format!("{action} {target}"),
                None => target,
            };
            let entry = text("content").or_else(|| text("old_text"));
            Some(Summary::tail(match entry {
                Some(entry) => format!("{operation}: {entry}"),
                None => operation,
            }))
        }
        "goal_propose" => text("objective").map(Summary::tail),
        // The blocking condition is the informative half when there is one — `blocked`
        // without a reason is a state a reader cannot act on.
        "goal_update" => text("status").map(|status| {
            Summary::tail(match text("blocking_condition") {
                Some(reason) => format!("{status}: {reason}"),
                None => status,
            })
        }),
        // `plan_exit` and `goal_get` take no arguments, so there is nothing to summarise
        // and nothing is invented. Anything else is a plugin or MCP tool whose argument
        // shape this table cannot know.
        _ => None,
    }
}

/// One string field of `value`, empty treated as absent.
///
/// Empty is absent rather than adopted for the same reason [`crate::views::message`]'s
/// status strip drops an empty branch: a blank where an argument should be is
/// indistinguishable from a field that failed to arrive, and it still costs the
/// separator's columns.
fn field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The paths an `apply_patch` envelope names, in order.
///
/// Scanned line-wise for the `*** <verb> File: <path>` header the envelope format uses.
/// Line-wise and not a parse, for the same reason `§7.4` locates hunks by scanning for
/// `@@`: the transcript needs the paths, not a validated patch, and the tool itself is
/// what rejects a malformed one.
fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| line.trim().strip_prefix("*** "))
        .filter_map(|rest| rest.split_once("File: "))
        .map(|(_verb, path)| path.trim().to_owned())
        .filter(|path| !path.is_empty())
        .collect()
}

/// How much of a tool's output the transcript will lay out.
///
/// Two limits rather than one because they fail differently. A row cap keeps the frame's
/// arithmetic bounded; a character cap keeps *one* pathological row — a minified bundle
/// on a single line — from costing the wrap more work than the whole rest of the
/// transcript. Either alone leaves the other case open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudget {
    /// Rows of output laid out.
    pub rows: usize,
    /// Characters of output the wrap is allowed to consume.
    pub chars: usize,
}

/// Rows a `read` result shows when expanded, per §7.5.
///
/// Higher than every other tool because a `read` result *is* the file: the user asked
/// for its contents, so the output is the answer rather than evidence about the answer.
pub const READ_EXPANDED_ROWS: usize = 80;

/// Characters a `read` result shows when expanded, per §7.5.
pub const READ_EXPANDED_CHARS: usize = 6_000;

/// Rows every other tool shows when expanded, per §7.5.
pub const EXPANDED_ROWS: usize = 60;

/// Characters every other tool shows when expanded, per §7.5.
pub const EXPANDED_CHARS: usize = 4_000;

/// Characters shown while collapsed.
///
/// Generous next to [`crate::views::message::TOOL_OUTPUT_PREVIEW_ROWS`], because in the
/// collapsed state the row cap is the binding one and a character cap that bit first
/// would cut a preview row mid-word for no reason a reader could see.
pub const COLLAPSED_CHARS: usize = 2_000;

/// What `name`'s output is allowed to occupy in `display`.
///
/// The per-tool fork is `read` against everything else, which is the fork §7.5 draws. It
/// is deliberately **not** the same shape as `zuno-tool`'s own caps (2,000 lines /
/// 51,200 bytes) and must not be confused with them: those decide whether a result is
/// returned to the *model* at all, and on breach that layer refuses and writes the full
/// text to a file rather than truncating it. These decide how many rows a *frame* spends,
/// and on breach the transcript says so and keeps the text. One is a contract with the
/// provider, the other is a viewport.
#[must_use]
pub fn output_budget(name: &str, display: ToolDisplay) -> OutputBudget {
    match display {
        ToolDisplay::Collapsed => OutputBudget {
            rows: crate::views::message::TOOL_OUTPUT_PREVIEW_ROWS,
            chars: COLLAPSED_CHARS,
        },
        ToolDisplay::Expanded if name == "read" => OutputBudget {
            rows: READ_EXPANDED_ROWS,
            chars: READ_EXPANDED_CHARS,
        },
        ToolDisplay::Expanded => OutputBudget {
            rows: EXPANDED_ROWS,
            chars: EXPANDED_CHARS,
        },
    }
}

/// The style a tool row is painted in, by how far the call has got.
///
/// Here rather than in the transcript because it is the one part of a tool row that is a
/// *colour* decision, and keeping it beside the icon table means the two cannot drift
/// into disagreeing about which states are terminal.
#[must_use]
pub fn status_style(
    status: crate::views::message::ToolStatus,
    intent: zuno_tool::ToolUiIntent,
    context: &ViewContext,
) -> Style {
    use crate::views::message::ToolStatus;
    match status {
        ToolStatus::Error => context.error(),
        ToolStatus::Blocked => context.warning(),
        ToolStatus::Completed if intent == zuno_tool::ToolUiIntent::Subagent => {
            context.delegation()
        }
        ToolStatus::Completed | ToolStatus::Pending => context.tool(),
        ToolStatus::Running => context.running(),
    }
}
