//! The two memory scopes, their character caps, and where each one lives.
//!
//! # Why two scopes, and why not the reference's two
//!
//! `.omo/refs/hermes-agent/tools/memory_tool.py:5-10` splits its stores along
//! *who the note is about*: `MEMORY.md` for the agent's own observations and
//! `USER.md` for what it has learned about the person. That is the right split for
//! a personal assistant, whose whole context is one user.
//!
//! A coding agent's context is not one user — it is one user across many
//! repositories. Its two real axes are therefore **habits that travel** ("prefers
//! the change explained before it is applied") and **rules that do not** ("this
//! repo's integration gate is `cargo test`, not `cargo build`"). Filing both in
//! one store means every repository pays prompt budget for every other
//! repository's rules; filing habits per-repository means relearning them in each
//! new checkout. This split is a deliberate divergence from the reference and is
//! recorded as one.
//!
//! # Why characters, and never tokens
//!
//! `cli-config.yaml.example:691-693` states the reference's reasoning in three
//! lines: `memory_char_limit: 2200  # ~800 tokens`, at roughly 2.75 chars per
//! token. The conversion is a *comment*, not a computation, and that is the point.
//! A token cap would need a tokenizer, which means a dependency, a model
//! identifier, and a cap that silently moves when either changes — including
//! moving under content already on disk, so a store that fit yesterday overflows
//! today with no write in between. A character cap is decided entirely by the
//! bytes in the file.
//!
//! Counted in `char`s — Unicode scalar values — which is exactly what the
//! reference's `len(str)` counts in Python 3. Bytes were the alternative and are
//! wrong here: the cap exists to bound how much *instruction* rides in the system
//! prompt, and under a byte cap the same instruction written in Chinese would cost
//! three times what it costs in English while occupying a third of the model's
//! attention budget. See [`char_count`].

use oc_paths::PROJECT_CONFIG_DIRECTORY;
use std::path::{Path, PathBuf};

/// Separates entries on disk — `memory_tool.py:67`, byte-for-byte.
///
/// A lone `§` would be wrong: an entry whose prose contains a section sign would
/// split in half on the next read. Delimiting on the full `"\n§\n"` means the
/// character is only structural when it owns its line.
pub const ENTRY_DELIMITER: &str = "\n§\n";

/// Directory under `$CONFIG` holding the global store.
pub const MEMORY_DIRECTORY: &str = "memory";

/// Filename of the global agent-notes store.
pub const GLOBAL_FILE: &str = "MEMORY.md";

/// Filename of the per-repository rules store.
pub const PROJECT_FILE: &str = "RULES.md";

/// Which store a read or write is addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Cross-project agent notes at `$CONFIG/memory/MEMORY.md`, capped at 2200
    /// characters.
    Global,
    /// This repository's rules at `<worktree>/.zuno/RULES.md`, capped at 3000
    /// characters.
    Project,
}

/// Character budgets for both resident-memory scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeLimits {
    global: usize,
    project: usize,
}

impl ScopeLimits {
    /// Construct explicit global and project character budgets.
    #[must_use]
    pub const fn new(global: usize, project: usize) -> Self {
        Self { global, project }
    }

    /// Return the configured budget for one scope.
    #[must_use]
    pub const fn for_scope(self, scope: Scope) -> usize {
        match scope {
            Scope::Global => self.global,
            Scope::Project => self.project,
        }
    }
}

impl Default for ScopeLimits {
    fn default() -> Self {
        Self::new(Scope::Global.cap(), Scope::Project.cap())
    }
}

impl Scope {
    /// Both scopes, for callers that render or audit every store.
    pub const ALL: [Self; 2] = [Self::Global, Self::Project];

    /// The character cap for this scope.
    ///
    /// `Global` is 2200, the reference's `memory_char_limit`
    /// (`cli-config.yaml.example:691`, `memory_tool.py:165`) — carried unchanged
    /// because the quantity it bounds is the same: notes that ride in *every*
    /// session's prompt, including sessions in repositories that have nothing to
    /// do with the note.
    ///
    /// `Project` is 3000 rather than the reference's 1375, because the reference's
    /// smaller store holds facts about a person and this one holds rules about a
    /// codebase. A build command, a test gate, a lint policy and a directory
    /// convention are each a sentence, and they only ever load inside the one
    /// repository that pays for them, so the budget buys more here than a global
    /// note of the same size would.
    #[must_use]
    pub const fn cap(self) -> usize {
        match self {
            Self::Global => 2200,
            Self::Project => 3000,
        }
    }

    /// The header label shown in the rendered block, e.g. `MEMORY (agent notes)`.
    ///
    /// The reference exports the equivalent constants (`memory_tool.py:59-65`) so
    /// its compaction check can spot a block whose scope has since been emptied.
    /// Todo 99 needs the same handle, so the label is public and stable.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Global => "MEMORY (agent notes)",
            Self::Project => "MEMORY (project rules)",
        }
    }

    /// The store's filename.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Global => GLOBAL_FILE,
            Self::Project => PROJECT_FILE,
        }
    }

    /// Where this scope's file lives, given the worktree.
    ///
    /// `$CONFIG` comes from [`oc_paths::config`] rather than from a local reading
    /// of `XDG_CONFIG_HOME`, so this store lands in the same directory the
    /// TypeScript binary's config does and an `OPENCODE_CONFIG_DIR` override is
    /// honoured for free. The reference makes the same point from the other
    /// direction (`memory_tool.py:50-56`): it resolves its directory through a
    /// *function* rather than an import-time constant precisely so a profile
    /// switch is respected. `oc_paths` resolves once into a cached layout and the
    /// environment cannot be mutated in this workspace, so the cache cannot go
    /// stale — but the lookup still belongs there and not here.
    ///
    /// `worktree` is ignored for [`Scope::Global`], which is what makes that store
    /// the same file from every checkout.
    #[must_use]
    pub fn path(self, worktree: &Path) -> PathBuf {
        match self {
            Self::Global => oc_paths::config().join(MEMORY_DIRECTORY).join(GLOBAL_FILE),
            Self::Project => worktree.join(PROJECT_CONFIG_DIRECTORY).join(PROJECT_FILE),
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Global => "global",
            Self::Project => "project",
        })
    }
}

/// The store's size in the unit the cap is expressed in.
///
/// One function so no call site can disagree with another about what "2200 chars"
/// means. See the module docs for why this is `chars().count()` and not `len()`.
#[must_use]
pub fn char_count(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiter_is_the_reference_bytes() {
        assert_eq!(ENTRY_DELIMITER.as_bytes(), "\n\u{a7}\n".as_bytes());
        assert_eq!(ENTRY_DELIMITER.chars().count(), 3);
    }

    #[test]
    fn caps_are_the_plan_values() {
        assert_eq!(Scope::Global.cap(), 2200);
        assert_eq!(Scope::Project.cap(), 3000);
    }

    #[test]
    fn char_count_is_scalar_values_not_bytes() {
        let chinese = "先读代码再改";
        assert_eq!(char_count(chinese), 6);
        assert_eq!(chinese.len(), 18, "the byte length would triple the cost");
    }

    #[test]
    fn project_path_is_worktree_relative() {
        let path = Scope::Project.path(Path::new("/srv/repo"));
        assert_eq!(path, Path::new("/srv/repo/.zuno/RULES.md"));
    }

    #[test]
    fn global_path_ignores_the_worktree_and_sits_under_config() {
        let from_a = Scope::Global.path(Path::new("/srv/a"));
        let from_b = Scope::Global.path(Path::new("/srv/b"));
        assert_eq!(from_a, from_b, "the global store travels between checkouts");
        assert!(from_a.starts_with(oc_paths::config()));
        assert!(from_a.ends_with("memory/MEMORY.md"));
    }

    #[test]
    fn labels_are_distinct_and_carry_the_scope() {
        assert_ne!(Scope::Global.label(), Scope::Project.label());
        assert!(Scope::Global.label().contains("agent notes"));
        assert!(Scope::Project.label().contains("project rules"));
    }
}
