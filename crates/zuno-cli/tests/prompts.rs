//! The prompt gate: every static tool description the model reads is a standalone
//! file, and the bytes those files contribute to a provider request are pinned.
//!
//! # What problem this target solves
//!
//! A tool description is prompt text. Rewording one changes when a model reaches
//! for the tool, so it is behaviour, not documentation. Historically most of these
//! descriptions were Rust string literals — `concat!` chains and `"\` continuation
//! strings spread across fourteen modules — which meant three bad properties:
//!
//! 1. Changing a prompt meant editing a `.rs` file, so prompt review and code
//!    review were the same review.
//! 2. `concat!` hides where the newlines are. A literal ending mid-sentence and
//!    continuing on the next source line produces *no* newline, while the source
//!    looks like a list of lines. Transcribing such a literal into a file by hand
//!    silently inserts newlines the model never saw.
//! 3. Nothing pinned the assembled result, so (2) could not be caught.
//!
//! The externalised form fixes (1) and (2), and this target fixes (3).
//!
//! # Why the golden is not a vacuous self-comparison
//!
//! [`the_committed_golden_is_the_wire_text_of_every_static_description`] compares
//! a golden file against text rendered from the **consts**, which are
//! `include_str!` of the description files. So the two sides come from different
//! artifacts: change any description file and the golden goes stale.
//!
//! [`every_description_is_backed_by_the_file_the_table_names`] closes the other
//! direction. It re-reads each file **from disk at runtime** and compares it to
//! the compiled-in const. That is not a tautology: `include_str!` resolves at
//! compile time relative to the source file, this test resolves at run time
//! relative to the workspace root, so a const repointed at some other file, or a
//! file that is present but empty, fails here.
//!
//! # Regeneration
//!
//! `ZUNO_PROMPT_REGENERATE=1 cargo test -p zuno-cli --test prompts` rewrites the
//! golden from the consts and then re-asserts, mirroring `ZUNO_DOCS_REGENERATE`
//! in `tests/docs.rs`. Regenerating is the intended response to an *intended*
//! prompt edit; it is never the response to a surprise.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The committed golden of every static description's exact bytes.
const GOLDEN: &str = "crates/zuno-cli/tests/prompts/tool-descriptions.txt";

/// One externalised description: what the model calls the tool, where the text
/// lives, and the text the binary compiled in.
struct Description {
    /// The id the model sees on the wire. Not always the module name — the module
    /// is `shell`, and the wire id is also `shell`.
    wire_id: &'static str,
    /// The description file, relative to the workspace root.
    file: &'static str,
    /// The compiled-in text, i.e. `include_str!` of `file`.
    text: &'static str,
}

/// Every static (`A`-category) tool description, ordered by wire id.
///
/// Ordering is by wire id rather than by crate so the golden reads as the model
/// sees the toolset, and so adding a tool lands next to its alphabetical peers
/// instead of at the end.
///
/// Deliberately absent: `websearch`. Its text interpolates the current year
/// (`zuno_tools::websearch::DESCRIPTION_TEMPLATE`, substituted at
/// `websearch/mod.rs`), so it is a template plus a runtime value, not a static
/// file. Externalising the template alone would move the prose out of the code
/// while leaving the substitution in it, which is the outcome this target exists
/// to avoid claiming.
fn descriptions() -> Vec<Description> {
    vec![
        Description {
            wire_id: "apply_patch",
            file: "crates/zuno-tools/src/description/apply-patch.txt",
            text: zuno_tools::apply_patch::DESCRIPTION,
        },
        Description {
            wire_id: "shell",
            file: "crates/zuno-tools/src/description/shell.txt",
            text: zuno_tools::shell::DESCRIPTION,
        },
        Description {
            wire_id: "batch",
            file: "crates/zuno-tools/src/description/batch.txt",
            text: zuno_tools::batch::DESCRIPTION,
        },
        Description {
            wire_id: "goal_propose",
            file: "crates/zuno-goal/src/description/create-goal.txt",
            text: zuno_goal::tools::CREATE_DESCRIPTION,
        },
        Description {
            wire_id: "edit",
            file: "crates/zuno-tools/src/description/edit.txt",
            text: zuno_tools::edit::DESCRIPTION,
        },
        Description {
            wire_id: "goal_get",
            file: "crates/zuno-goal/src/description/get-goal.txt",
            text: zuno_goal::tools::GET_DESCRIPTION,
        },
        Description {
            wire_id: "glob",
            file: "crates/zuno-tools/src/description/glob.txt",
            text: zuno_tools::glob::DESCRIPTION,
        },
        Description {
            wire_id: "grep",
            file: "crates/zuno-tools/src/description/grep.txt",
            text: zuno_tools::grep::DESCRIPTION,
        },
        Description {
            wire_id: "invalid",
            file: "crates/zuno-tools/src/description/invalid.txt",
            text: zuno_tools::invalid::DESCRIPTION,
        },
        Description {
            wire_id: "job",
            file: "crates/zuno-tools/src/description/job.txt",
            text: zuno_tools::job::DESCRIPTION,
        },
        Description {
            wire_id: "memory_propose",
            file: "crates/zuno-tools/src/description/memory-propose.txt",
            text: zuno_tools::memory::DESCRIPTION,
        },
        Description {
            wire_id: "plan_exit",
            file: "crates/zuno-tools/src/description/plan-exit.txt",
            text: zuno_tools::plan_exit::DESCRIPTION,
        },
        Description {
            wire_id: "question",
            file: "crates/zuno-tools/src/description/question.txt",
            text: zuno_tools::question::DESCRIPTION,
        },
        Description {
            wire_id: "read",
            file: "crates/zuno-tools/src/description/read.txt",
            text: zuno_tools::read::DESCRIPTION,
        },
        Description {
            wire_id: "session_search",
            file: "crates/zuno-tools/src/description/session-search.txt",
            text: zuno_tools::session_search::DESCRIPTION,
        },
        Description {
            wire_id: "skill",
            file: "crates/zuno-tools/src/description/skill.txt",
            text: zuno_tools::skill::DESCRIPTION,
        },
        Description {
            wire_id: "task",
            file: "crates/zuno-tools/src/description/task.txt",
            text: zuno_tools::task::DESCRIPTION,
        },
        Description {
            wire_id: "plan_get",
            file: "crates/zuno-tools/src/description/plan-get.txt",
            text: zuno_tools::PLAN_GET_DESCRIPTION,
        },
        Description {
            wire_id: "plan_update",
            file: "crates/zuno-tools/src/description/plan-update.txt",
            text: zuno_tools::PLAN_UPDATE_DESCRIPTION,
        },
        Description {
            wire_id: "todo_get",
            file: "crates/zuno-tools/src/description/todo-get.txt",
            text: zuno_tools::TODO_GET_DESCRIPTION,
        },
        Description {
            wire_id: "todo_update",
            file: "crates/zuno-tools/src/description/todo-update.txt",
            text: zuno_tools::TODO_UPDATE_DESCRIPTION,
        },
        Description {
            wire_id: "goal_update",
            file: "crates/zuno-goal/src/description/update-goal.txt",
            text: zuno_goal::tools::UPDATE_DESCRIPTION,
        },
        Description {
            wire_id: "webfetch",
            file: "crates/zuno-tools/src/description/webfetch.txt",
            text: zuno_tools::webfetch::DESCRIPTION,
        },
        Description {
            wire_id: "write",
            file: "crates/zuno-tools/src/description/write.txt",
            text: zuno_tools::write::DESCRIPTION,
        },
    ]
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crates/zuno-cli has a workspace root two levels up")
        .to_path_buf()
}

fn regenerating() -> bool {
    matches!(
        std::env::var("ZUNO_PROMPT_REGENERATE").as_deref(),
        Ok("1" | "true")
    )
}

/// Marks the start of a description body. Never appears inside one.
const OPEN: &str = "<<<";
/// Marks the end of a description body. Never appears inside one as a whole line,
/// which is what makes [`parse_golden`] unambiguous.
const CLOSE: &str = ">>>";

/// Header token for a description whose last byte is a newline.
const ENDS_NEWLINE: &str = "trailing-newline: yes";
/// Header token for a description whose last byte is not a newline.
const ENDS_BARE: &str = "trailing-newline: no";

/// Renders the golden.
///
/// Two header fields exist because both are prompt behaviour that a diff would
/// otherwise hide:
///
/// * the byte count, so a same-length substitution still shows as a changed
///   header line rather than only as a changed body;
/// * whether the text ends in a newline, because it *varies between tools* — the
///   descriptions that were always files end with one and the ones lifted from
///   `concat!` literals do not, and the provider receives that difference. Left
///   implicit, it is the single easiest byte to gain or lose while editing.
///
/// The body is always written newline-terminated so [`CLOSE`] is a whole line; the
/// terminating newline is the renderer's, and the header says whether the text also
/// had one.
fn render_golden(entries: &[Description]) -> String {
    let mut out = String::from(
        "# Every static tool description, byte for byte, as the provider receives it.\n\
         # Generated by `ZUNO_PROMPT_REGENERATE=1 cargo test -p zuno-cli --test prompts`.\n\
         # Never hand-edit: edit the named file instead, then regenerate.\n",
    );
    for entry in entries {
        let bytes = entry.text.len();
        let ending = if entry.text.ends_with('\n') {
            ENDS_NEWLINE
        } else {
            ENDS_BARE
        };
        writeln!(
            out,
            "\n## {} | {} | {bytes} bytes | {ending}",
            entry.wire_id, entry.file
        )
        .expect("writing to a String cannot fail");
        out.push_str(OPEN);
        out.push('\n');
        out.push_str(entry.text);
        if !entry.text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(CLOSE);
        out.push('\n');
    }
    out
}

/// The golden equals what the compiled binary will put on the wire.
///
/// Failure means a description file changed. If that was intended, regenerate and
/// read the diff; if it was not, the file has drifted and the prompt the model
/// reads is no longer the reviewed one.
#[test]
fn the_committed_golden_is_the_wire_text_of_every_static_description() {
    let entries = descriptions();
    let expected = render_golden(&entries);
    let path = workspace_root().join(GOLDEN);

    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    if actual == expected {
        return;
    }

    if regenerating() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
        }
        std::fs::write(&path, &expected)
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        eprintln!("regenerated {GOLDEN}");
        return;
    }

    let mut drift = String::new();
    let committed = parse_golden(&actual);
    for entry in &entries {
        let was = committed.iter().find(|(id, _)| id == entry.wire_id);
        match was {
            Some((_, text)) if text == entry.text => {}
            Some((_, text)) => {
                let _ = writeln!(
                    drift,
                    "  {} changed: {} bytes -> {} bytes ({})",
                    entry.wire_id,
                    text.len(),
                    entry.text.len(),
                    entry.file
                );
            }
            None => {
                let _ = writeln!(drift, "  {} is new, from {}", entry.wire_id, entry.file);
            }
        }
    }
    for (id, _) in &committed {
        if !entries.iter().any(|entry| entry.wire_id == id) {
            let _ = writeln!(drift, "  {id} was removed");
        }
    }
    if drift.is_empty() {
        drift.push_str("  (header or ordering changed)\n");
    }

    panic!(
        "{} is stale — the text the model reads is not the text that was reviewed.\n\
         What moved:\n{drift}\
         If the change was intended, take the generated version with\n\
         \n    ZUNO_PROMPT_REGENERATE=1 cargo test -p zuno-cli --test prompts\n\
         \nand review the resulting diff. If it was not intended, restore the named file.",
        path.display()
    );
}

/// Recovers `(wire_id, text)` pairs from a rendered golden.
///
/// Only used to describe a failure, but it must be *exact*: a diagnostic that
/// misreports a description's length sends the reader after the wrong tool. The
/// parser is fence-driven rather than header-driven because a body may legitimately
/// contain a line starting with `## ` while no body contains a
/// whole line equal to [`CLOSE`]. Header lines are therefore only recognised
/// *outside* a fence.
fn parse_golden(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(header) = line.strip_prefix("## ") else {
            continue;
        };
        let mut fields = header.split(" | ");
        let Some(id) = fields.next() else {
            continue;
        };
        let bare = header.contains(ENDS_BARE);
        if lines.next() != Some(OPEN) {
            continue;
        }
        let mut body = String::new();
        for line in lines.by_ref() {
            if line == CLOSE {
                break;
            }
            body.push_str(line);
            body.push('\n');
        }
        if bare {
            body.pop();
        }
        out.push((id.to_owned(), body));
    }
    out
}

/// Every description is the file the table names, and no file is empty.
///
/// The empty case is called out separately because it is the one failure that is
/// otherwise silent: `include_str!` of an empty file compiles, and a tool then
/// reaches the provider with `description: ""`. A model cannot ask why a tool has
/// no description, so nothing downstream would report it.
#[test]
fn every_description_is_backed_by_the_file_the_table_names() {
    let root = workspace_root();
    for entry in descriptions() {
        let path = root.join(entry.file);

        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "tool `{}` claims its description lives at {} and that file cannot be read: \
                 {error}\nA description is prompt text; it must be a file that exists.",
                entry.wire_id,
                path.display()
            )
        });

        assert!(
            !on_disk.trim().is_empty(),
            "the description file for tool `{}` is empty: {}\nAn empty file compiles, so the \
             tool would reach the provider with no description at all and nothing else would \
             report it. Restore the text.",
            entry.wire_id,
            path.display()
        );

        assert_eq!(
            on_disk,
            entry.text,
            "tool `{}` does not carry the bytes of the file it names.\nfile:     {} ({} bytes)\n\
             compiled: {} bytes\nThe const must be `include_str!` of exactly this file — a \
             different path, or a literal left inline, makes the file a decoration.",
            entry.wire_id,
            path.display(),
            on_disk.len(),
            entry.text.len(),
        );
    }
}

/// Every file under a description directory is claimed by the table.
///
/// Without this, deleting a tool leaves its prompt file behind and nothing says
/// so, and a file added in the expectation that something reads it stays unread.
#[test]
fn no_description_file_is_unclaimed() {
    let root = workspace_root();
    let entries = descriptions();
    for directory in [
        "crates/zuno-tools/src/description",
        "crates/zuno-goal/src/description",
    ] {
        let path = root.join(directory);
        let read = std::fs::read_dir(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for item in read {
            let item = item.unwrap_or_else(|error| panic!("walk {}: {error}", path.display()));
            let name = item.file_name();
            let name = name.to_string_lossy();
            let relative = format!("{directory}/{name}");
            assert!(
                entries.iter().any(|entry| entry.file == relative),
                "{relative} is not claimed by any tool in this target's table, so no tool \
                 reads it and nothing pins its bytes. Either wire it to a tool or delete it."
            );
        }
    }
}
