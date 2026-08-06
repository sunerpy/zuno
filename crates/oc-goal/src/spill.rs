//! The 4,000-character objective cap, and what happens above it.
//!
//! # Two mechanisms in codex, one here
//!
//! codex splits this in half. `codex-rs/protocol/src/protocol.rs:4076-4088`
//! defines `MAX_THREAD_GOAL_OBJECTIVE_CHARS = 4_000` and *rejects* anything
//! longer; the TUI, one layer up, spills first
//! (`codex-rs/tui/src/goal_files.rs:121-136`) so the store never sees an
//! oversized value. Splitting it that way means every future caller has to
//! remember to spill.
//!
//! Here both live below the store's API. [`store_objective`] is the only way an
//! objective reaches the column, so the column's contract — never longer than
//! [`MAX_OBJECTIVE_CHARS`] — holds for callers that have never heard of a spill
//! file.
//!
//! # Why characters and not bytes
//!
//! `chars().count()`, matching `protocol.rs:4082`. The cap exists to bound what
//! is pasted into a model's context, and that is counted in characters; a byte
//! cap would silently cut a CJK objective to a third of the length of an ASCII
//! one.
//!
//! # Why the pointer is a sentence and not a path
//!
//! The stored value is read back into the model's context verbatim. A bare path
//! there is ambiguous — it looks like part of the objective. An instruction is
//! not: the model reads the file and continues. Same shape as codex's
//! `GOAL_FILE_PREFIX` / `GOAL_FILE_SUFFIX` (`goal_files.rs:18-19`), in this
//! project's own words.

use crate::error::GoalError;
use std::path::{Path, PathBuf};

/// The longest objective the `goal.objective` column will hold.
///
/// Ports `MAX_THREAD_GOAL_OBJECTIVE_CHARS` (`protocol.rs:4076`).
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;

/// What the pointer sentence starts with.
pub const OBJECTIVE_POINTER_PREFIX: &str = "Read the full goal objective at ";

/// What the pointer sentence ends with.
pub const OBJECTIVE_POINTER_SUFFIX: &str = " before continuing.";

/// The file a spilled objective is written to.
pub const OBJECTIVE_FILE_NAME: &str = "goal-objective.md";

/// Store `objective`, spilling to a file under `spill_dir` when it is too long.
///
/// Returns the value to put in the column: `objective` unchanged when it fits,
/// or the pointer sentence when it did not.
///
/// The pointer is built *before* the file is written, so a directory too deep
/// for the pointer to fit fails without leaving an orphan file behind — the same
/// ordering as `goal_files.rs:125-135`.
///
/// # Errors
///
/// [`GoalError::EmptyObjective`] for an empty or whitespace-only objective,
/// [`GoalError::PointerTooLong`] when the pointer would exceed the cap, and
/// [`GoalError::Spill`] when the directory or file cannot be written.
pub fn store_objective(spill_dir: &Path, objective: &str) -> Result<String, GoalError> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Err(GoalError::EmptyObjective);
    }
    if objective.chars().count() <= MAX_OBJECTIVE_CHARS {
        return Ok(objective.to_owned());
    }
    let path = spill_path(spill_dir);
    let pointer = objective_pointer(&path)?;
    let parent = path.parent().unwrap_or(spill_dir);
    std::fs::create_dir_all(parent).map_err(|source| GoalError::Spill {
        path: path.clone(),
        source,
    })?;
    std::fs::write(&path, objective).map_err(|source| GoalError::Spill {
        path: path.clone(),
        source,
    })?;
    Ok(pointer)
}

/// The sentence that stands in for a spilled objective.
///
/// # Errors
///
/// [`GoalError::PointerTooLong`] when the sentence exceeds
/// [`MAX_OBJECTIVE_CHARS`], which can only happen for an absurdly long path.
pub fn objective_pointer(path: &Path) -> Result<String, GoalError> {
    let pointer = format!(
        "{OBJECTIVE_POINTER_PREFIX}{}{OBJECTIVE_POINTER_SUFFIX}",
        path.display()
    );
    let actual = pointer.chars().count();
    if actual > MAX_OBJECTIVE_CHARS {
        return Err(GoalError::PointerTooLong {
            path: path.to_path_buf(),
            actual,
            max: MAX_OBJECTIVE_CHARS,
        });
    }
    Ok(pointer)
}

/// The spill file a stored objective points at, if it is a pointer at all.
///
/// # Why this validates instead of just parsing
///
/// Whoever calls this is about to read the file it names, and the objective it
/// parses may have been written by the model. So the path has to be one this
/// crate could actually have produced: inside `spill_dir`, named
/// [`OBJECTIVE_FILE_NAME`], in a directory whose name is a v4 UUID. codex
/// applies the same three checks at `goal_files.rs:164-171`. Without them the
/// pointer is an arbitrary-file-read primitive handed to the model.
#[must_use]
pub fn objective_pointer_path(spill_dir: &Path, objective: &str) -> Option<PathBuf> {
    let path = objective
        .strip_prefix(OBJECTIVE_POINTER_PREFIX)?
        .strip_suffix(OBJECTIVE_POINTER_SUFFIX)?;
    let path = PathBuf::from(path);
    if path.file_name()? != OBJECTIVE_FILE_NAME {
        return None;
    }
    let directory = path.parent()?;
    if directory.parent()? != spill_dir {
        return None;
    }
    let attachment = directory.file_name()?.to_str()?;
    uuid::Uuid::parse_str(attachment).ok()?;
    Some(path)
}

fn spill_path(spill_dir: &Path) -> PathBuf {
    spill_dir
        .join(uuid::Uuid::new_v4().to_string())
        .join(OBJECTIVE_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spill_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create spill directory")
    }

    #[test]
    fn an_objective_at_the_cap_is_stored_verbatim_and_writes_no_file() {
        let dir = spill_dir();
        let objective = "x".repeat(MAX_OBJECTIVE_CHARS);
        let stored = store_objective(dir.path(), &objective).expect("store objective");
        assert_eq!(stored, objective);
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("read spill directory")
                .count(),
            0
        );
    }

    #[test]
    fn one_character_over_the_cap_spills_and_the_pointer_round_trips_to_the_file() {
        let dir = spill_dir();
        let objective = "y".repeat(MAX_OBJECTIVE_CHARS + 1);
        let stored = store_objective(dir.path(), &objective).expect("store objective");
        assert_ne!(stored, objective);
        let path = objective_pointer_path(dir.path(), &stored)
            .expect("the stored pointer must resolve back to its file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read spill"),
            objective
        );
        assert_eq!(stored, objective_pointer(&path).expect("rebuild pointer"));
    }

    #[test]
    fn the_cap_counts_characters_so_a_multibyte_objective_is_not_cut_short() {
        let dir = spill_dir();
        let objective = "目".repeat(MAX_OBJECTIVE_CHARS);
        assert!(
            objective.len() > MAX_OBJECTIVE_CHARS,
            "must exceed in bytes"
        );
        let stored = store_objective(dir.path(), &objective).expect("store objective");
        assert_eq!(
            stored, objective,
            "a byte cap would have spilled this; a character cap must not"
        );
    }

    #[test]
    fn an_empty_or_whitespace_only_objective_is_refused() {
        let dir = spill_dir();
        for objective in ["", "   ", "\n\t "] {
            assert!(
                matches!(
                    store_objective(dir.path(), objective),
                    Err(GoalError::EmptyObjective)
                ),
                "{objective:?} must not be storable"
            );
        }
    }

    #[test]
    fn an_objective_that_is_not_a_pointer_resolves_to_no_path() {
        let dir = spill_dir();
        assert_eq!(objective_pointer_path(dir.path(), "ship the release"), None);
    }

    #[test]
    fn a_pointer_the_model_forged_is_refused_on_all_three_checks() {
        let dir = spill_dir();
        let uuid = uuid::Uuid::new_v4().to_string();
        let forged = [
            dir.path().join(&uuid).join("id_rsa"),
            dir.path().join("not-a-uuid").join(OBJECTIVE_FILE_NAME),
            PathBuf::from("/etc").join(&uuid).join(OBJECTIVE_FILE_NAME),
            dir.path().join(OBJECTIVE_FILE_NAME),
        ];
        for path in forged {
            let pointer = objective_pointer(&path).expect("build pointer");
            assert_eq!(
                objective_pointer_path(dir.path(), &pointer),
                None,
                "{} must not be reachable through a pointer",
                path.display()
            );
        }
    }

    #[test]
    fn a_spill_directory_too_deep_for_the_pointer_fails_without_writing_anything() {
        let dir = spill_dir();
        let deep = dir.path().join("d".repeat(MAX_OBJECTIVE_CHARS));
        let error = store_objective(&deep, &"z".repeat(MAX_OBJECTIVE_CHARS + 1))
            .expect_err("an unusable pointer must fail rather than truncate");
        assert!(
            matches!(&error, GoalError::PointerTooLong { max, .. } if *max == MAX_OBJECTIVE_CHARS),
            "{error:?}"
        );
        assert!(!deep.exists(), "no directory should have been created");
    }
}
