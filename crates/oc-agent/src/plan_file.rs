//! Where a session's plan document lives, and how it is written.
//!
//! Oracle: `packages/opencode/src/session/session.ts:331-335`.
//!
//! ```ts
//! export function plan(input: { slug: string; time: { created: number } }, instance: InstanceContext) {
//!   const base = instance.project.vcs
//!     ? path.join(instance.worktree, ".zuno", "plans")
//!     : path.join(Global.Path.data, "plans")
//!   return path.join(base, [input.time.created, input.slug].join("-") + ".md")
//! }
//! ```
//!
//! Two facts are worth stating plainly, because both are easy to get subtly wrong:
//! the name is `<created>-<slug>.md` with `created` a **millisecond epoch integer**
//! and `slug` the session's own adjective-noun slug (`core/src/util/slug.ts:68-73`);
//! and the choice of directory is made by **`project.vcs`**, not by whether a
//! worktree happens to be known. A project that is not a repository writes to the
//! global data directory.
//!
//! # Why the fallback exists
//!
//! `.zuno/plans/` is the location a human will actually find, and in a
//! repository it is a path they can gitignore or commit as they choose. Outside a
//! repository there is no such place: writing `.zuno/` into whatever directory
//! the user happened to start in litters unrelated trees with files nothing will
//! ever clean up, and there is no `.gitignore` to keep them out of anyone's commit.
//! So the global data directory takes over — the plan is still durable, just not
//! sitting in a directory that does not belong to the project.
//!
//! `oc-goal`'s `.zuno/goal/<sessionID>.md` (todo 69) makes the same two-way
//! choice for the same reason, and says so in its own module docs. That is not a
//! coincidence: it copied this convention.
//!
//! # Why the write is atomic, and why it does not `fsync`
//!
//! A temporary file in the destination's own directory, then a rename. Same
//! filesystem, so the rename is atomic, so a reader — the model on the next turn,
//! or a human with the file open — always sees one complete document rather than a
//! half-written one. `oc-memory` (todo 98) settled on this and `oc-goal` (todo 69)
//! repeated it; both helpers are private to their crates, so this is the same
//! technique rather than the same function, and the temp-name details that were
//! learned the hard way are carried over rather than rediscovered:
//!
//! - `with_file_name`, **not** `with_extension`. `with_extension` would replace the
//!   `.md` — and worse, a slug containing a dot would make the temp name collide
//!   with a *different* plan's document.
//! - nanos in the temp name, so two concurrent writes cannot land on one temp file
//!   and interleave.
//! - the rename's error arm removes the temp file, so a failure leaves no litter
//!   beside a document a human is about to open.
//!
//! No `sync_all`. A plan is a document a user is iterating on with the editing
//! tools, not a ledger: a rename that reaches the directory entry is what the next
//! reader needs, and paying an fsync per keystroke-sized edit buys nothing the
//! rename does not already give.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The project-local directory the oracle keeps its own project state in.
pub const PROJECT_DIRECTORY: &str = ".zuno";

/// The subdirectory holding plan documents, in both locations
/// (`session/session.ts:332-334`).
pub const PLANS_DIRECTORY: &str = "plans";

/// Which of the two locations a plan goes to.
///
/// Named after the condition the oracle branches on rather than after the
/// directories, because `project.vcs` is the fact — a caller that has a worktree
/// but no repository must still choose [`PlanLocation::Global`], and a parameter
/// called something like `worktree: Option<&Path>` invites exactly that mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLocation<'a> {
    /// `project.vcs` is set: `<worktree>/.zuno/plans`.
    Worktree(&'a Path),
    /// `project.vcs` is unset: `<data>/plans`.
    Global,
}

impl PlanLocation<'_> {
    /// The oracle's `base` (`session/session.ts:332-334`).
    #[must_use]
    pub fn directory(&self) -> PathBuf {
        match self {
            Self::Worktree(worktree) => worktree.join(PROJECT_DIRECTORY).join(PLANS_DIRECTORY),
            Self::Global => oc_paths::data().join(PLANS_DIRECTORY),
        }
    }
}

/// The identity a plan's filename is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanKey<'a> {
    /// `time.created`, a millisecond epoch integer.
    pub created: i64,
    /// The session's slug, e.g. `swift-otter`.
    pub slug: &'a str,
}

impl PlanKey<'_> {
    /// `<created>-<slug>.md` (`session/session.ts:335`).
    ///
    /// `None` for a slug that is not a single ordinary path component. The slug
    /// reaches this crate from a session record and becomes part of a path here, so
    /// `../../etc/x` has to be refused rather than joined.
    ///
    /// The check is on the **slug**, not on the filename built from it: appending
    /// `.md` turns `..` into the perfectly legal `...md`, so validating the derived
    /// name accepts exactly the input that most needs refusing. `oc-goal`'s
    /// `document_path` shipped that bug in its first draft.
    #[must_use]
    pub fn file_name(&self) -> Option<String> {
        let slug = self.slug;
        if slug.trim() != slug {
            return None;
        }
        let mut components = Path::new(slug).components();
        match components.next() {
            Some(Component::Normal(first)) if first == std::ffi::OsStr::new(slug) => {}
            _ => return None,
        }
        if components.next().is_some() {
            return None;
        }
        Some(format!("{created}-{slug}.md", created = self.created))
    }
}

/// The absolute path of a session's plan document.
///
/// `None` when [`PlanKey::file_name`] refuses the slug.
#[must_use]
pub fn plan_path(location: PlanLocation<'_>, key: PlanKey<'_>) -> Option<PathBuf> {
    Some(location.directory().join(key.file_name()?))
}

/// Write `body` to the session's plan document, creating the directory.
///
/// Returns the path written. The write is atomic, for the reasons in the module
/// docs.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when the slug is refused; otherwise whatever the
/// filesystem reported while creating the directory, writing the temporary file, or
/// renaming it into place.
pub fn write_plan(location: PlanLocation<'_>, key: PlanKey<'_>, body: &str) -> io::Result<PathBuf> {
    let path = plan_path(location, key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{:?} is not a usable plan slug", key.slug),
        )
    })?;
    write_atomic(&path, body)?;
    Ok(path)
}

/// Read a session's plan document, if it has one.
///
/// `Ok(None)` for a plan that does not exist yet, because "no plan" is the ordinary
/// state of a new session rather than an error — `reminders.ts:55,73` branches on
/// exactly this and tells the model to create the plan instead of failing.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when the slug is refused; otherwise whatever the
/// filesystem reported while reading.
pub fn read_plan(location: PlanLocation<'_>, key: PlanKey<'_>) -> io::Result<Option<String>> {
    let path = plan_path(location, key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{:?} is not a usable plan slug", key.slug),
        )
    })?;
    match std::fs::read_to_string(&path) {
        Ok(body) => Ok(Some(body)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_atomic(path: &Path, body: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().map_or_else(
        || "plan.md".to_owned(),
        |name| name.to_string_lossy().into(),
    );
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let temporary = path.with_file_name(format!("{name}.tmp.{nanos}"));

    std::fs::write(&temporary, body)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        drop(std::fs::remove_file(&temporary));
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
