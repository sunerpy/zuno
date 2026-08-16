//! Where a session's plan document lives.
//!
//! Oracle: `packages/opencode/src/session/session.ts:331-335`.
//!
//! ```ts
//! export function plan(input: { slug: string; time: { created: number } }, instance: InstanceContext) {
//!   const base = instance.project.vcs
//!     ? path.join(instance.worktree, ".opencode", "plans")
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
//! `zuno-goal`'s `.zuno/goal/<sessionID>.md` (todo 69) makes the same two-way
//! choice for the same reason, and says so in its own module docs. That is not a
//! coincidence: it copied this convention.
//!
//! # This module names the document; it does not read or write it
//!
//! Deliberately, and the oracle draws the same line: `Session.plan` is a *path*
//! function, and the only code that touches the file is
//! `session/reminders.ts:54-88`, which checks existence, ensures the directory, and
//! then tells the model where to write — *"You should create your plan at ${plan}
//! using the write tool"*. The file is produced by the ordinary `write` and `edit`
//! tools, under the same permission and formatting rules as any other file the
//! model touches, which is what [`crate`]'s own module docs state.
//!
//! So a `write_plan`/`read_plan` pair lived here until 2026-08-16 with no caller in
//! any production path — the reader also carried a legacy-path diagnostic that
//! nothing could ever reach, which is worse than no diagnostic because it reads as
//! protection. Both are gone. What a caller needs is [`plan_path`]: the seam that
//! wants it is `zuno_tools::plan_exit::PlanExitHost::plan_path`, whose own docs
//! describe exactly this computation (`tool/plan.ts:29`).

use std::path::{Component, Path, PathBuf};

pub use zuno_paths::PROJECT_DIRECTORY;

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
            Self::Global => zuno_paths::data().join(PLANS_DIRECTORY),
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
    /// name accepts exactly the input that most needs refusing. `zuno-goal`'s
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

#[cfg(test)]
mod tests;
