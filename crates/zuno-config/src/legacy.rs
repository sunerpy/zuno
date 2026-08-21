//! Rejection of deprecated configuration inputs.
//!
//! The TypeScript oracle *normalizes* every form named here: it rewrites `mode`
//! into `agent`, folds an agent's `tools` into `permission`, falls back from
//! `steps` to `maxSteps`, reads a global TOML `config` file and overwrites it with
//! `config.json`, and quietly ignores `layout`. This port deliberately does not
//! carry any of that forward — legacy compatibility is out of scope — so this
//! module exists to make the omission *loud*.
//!
//! # Why rejection rather than omission
//!
//! Silently dropping a deprecated block is the worst available behaviour. A
//! `mode.build` block that is parsed and discarded presents as a working config
//! that changes nothing: no error, no effect, no way for the author to tell the
//! difference between "applied" and "ignored". Every form below therefore becomes
//! a [`ConfigError`] naming both what was found and what to write instead, so the
//! error alone is a complete repair instruction.
//!
//! # What this module does not do
//!
//! It never writes. The oracle's TOML migration
//! (`packages/opencode/src/config/config.ts:262-275`) rewrites `config.json` and
//! `unlink`s the old file; this pass only reports. Detection and reporting, never
//! repair.
//!
//! # The twelve forms
//!
//! | Form | Oracle |
//! |---|---|
//! | `mode.<name>` | `packages/core/src/v1/config/config.ts:95`, normalized at `packages/opencode/src/config/config.ts:536-543` |
//! | a `{mode,modes}/` agent directory | `packages/opencode/src/config/agent.ts:32-58` |
//! | agent `tools` | `packages/core/src/v1/config/agent.ts:68-77` |
//! | agent `maxSteps` | `packages/core/src/v1/config/agent.ts:79` |
//! | agent `variant` with no `model` | `packages/opencode/src/session/prompt.ts:648-654` gates the variant on the agent's own model, so this combination is inert upstream too — Zuno rejects rather than ignoring it |
//! | `layout` | `packages/core/src/v1/config/config.ts:127` |
//! | `autoshare` | `packages/core/src/v1/config/config.ts:61-63` |
//! | `CONTEXT.md` | `packages/opencode/src/session/instruction.ts:68` |
//! | a global TOML `config` file | `packages/opencode/src/config/config.ts:262-275` |
//! | `reference` | `packages/core/src/v1/config/config.ts:48-50` |
//! | auth-prompt `condition` | `packages/plugin/src/index.ts:102-103,115-116` |
//! | a legacy-named config file | Zuno-only: the pre-rename `opencode.json(c)` / global `config.json` |
//!
//! # Overlap with the schema
//!
//! [`crate::schema`] already refuses `mode`, `layout`, and `autoshare` — they are
//! not fields of [`crate::Config`], and `deny_unknown_fields` turns each into an
//! `unrecognized key` issue. That message is correct but not actionable: it does
//! not say what to write instead. [`check_config`] must therefore run **before**
//! the strict parse, so the actionable message is the one the author sees. The
//! schema's job is to refuse to absorb these keys; this module's job is to explain
//! them.

#[cfg(test)]
mod tests;

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use zuno_error::{ConfigError, ConfigIssue};

/// The name of the deprecated TOML config file in the global config directory.
///
/// `packages/opencode/src/config/config.ts:262` joins `Global.Path.config` with
/// the literal `"config"` — no extension, which is how it was told apart from the
/// `config.json` the oracle rewrote it into.
///
/// Zuno accepts JSONC and strict JSON only, and has no TOML parser at all: the
/// deeply nested `provider.*.models.*.variants` schema, and the JSON-shaped
/// keybind, MCP, and agent definitions, are what the config vocabulary is built
/// around, and [`crate::discovery::strip_jsonc`] already covers comments, trailing
/// commas, and a BOM while preserving byte offsets so error positions stay
/// accurate. This constant therefore exists to *reject*, and the replacement it
/// names is a JSON file.
pub const LEGACY_TOML_CONFIG_FILE: &str = "config";

/// The deprecated instruction filename (`session/instruction.ts:68`, where the
/// oracle's own comment reads `// deprecated`).
pub const LEGACY_INSTRUCTION_FILE: &str = "CONTEXT.md";

/// The directory names the oracle's `loadMode` globs
/// (`packages/opencode/src/config/agent.ts:37`).
pub const LEGACY_AGENT_DIRECTORIES: &[&str] = &["mode", "modes"];

/// One deprecated input form.
///
/// A closed set, deliberately not `#[non_exhaustive]`: a caller that wants to
/// react per form should be forced to revisit its `match` when a twelfth form
/// appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeprecatedForm {
    /// A top-level `mode` block, or one named entry inside it.
    ModeBlock,
    /// An agent definition under `mode/` or `modes/`.
    ModeDirectory,
    /// An agent's `tools` map.
    AgentTools,
    /// An agent's `maxSteps`.
    AgentMaxSteps,
    /// An agent's `variant` with no `model` to apply it to.
    AgentVariantWithoutModel,
    /// The top-level `layout` key.
    Layout,
    /// The top-level `autoshare` key.
    Autoshare,
    /// A discovered `CONTEXT.md`.
    ContextFile,
    /// A global TOML `config` file.
    TomlConfig,
    /// The singular `reference` spelling of `references`.
    Reference,
    /// An auth-prompt `condition` predicate.
    AuthPromptCondition,
    /// A configuration file under one of the pre-rename filenames.
    ConfigFileName,
}

impl DeprecatedForm {
    /// The noun the message uses for this kind of input, so a reader knows whether
    /// to look in a JSON document or on disk.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::ModeBlock
            | Self::AgentTools
            | Self::AgentMaxSteps
            | Self::AgentVariantWithoutModel
            | Self::Layout
            | Self::Autoshare
            | Self::Reference
            | Self::AuthPromptCondition => "key",
            Self::ModeDirectory => "agent definition",
            Self::ContextFile => "instruction file",
            Self::TomlConfig => "TOML config file",
            Self::ConfigFileName => "config filename",
        }
    }
}

/// One deprecated input, located precisely enough to fix without guessing.
///
/// [`path`](Self::path) is the exact file — not the config layer's root, not the
/// directory that was scanned — and [`pointer`](Self::pointer) is the structured
/// JSON path within it (`["agent", "build", "maxSteps"]`), empty for forms that
/// are files rather than keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deprecation {
    form: DeprecatedForm,
    path: PathBuf,
    pointer: Vec<String>,
    found: String,
    replacement: String,
}

impl Deprecation {
    /// A deprecated key at a JSON pointer.
    fn keyed(path: &Path, pointer: Vec<String>, form: DeprecatedForm, replacement: String) -> Self {
        Self {
            form,
            path: path.to_path_buf(),
            found: pointer.join("."),
            pointer,
            replacement,
        }
    }

    /// A deprecated file or directory entry, named by `found` rather than by a
    /// JSON pointer.
    fn filed(
        path: &Path,
        form: DeprecatedForm,
        found: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            form,
            path: path.to_path_buf(),
            pointer: Vec::new(),
            found: found.into(),
            replacement: replacement.into(),
        }
    }

    /// Which form this is.
    #[must_use]
    pub const fn form(&self) -> DeprecatedForm {
        self.form
    }

    /// The exact file the deprecated input lives in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The structured JSON path of the offending key, empty for file-level forms.
    #[must_use]
    pub fn pointer(&self) -> &[String] {
        &self.pointer
    }

    /// What was found, rendered: a dotted JSON pointer, or a filename.
    #[must_use]
    pub fn found(&self) -> &str {
        &self.found
    }

    /// What to write instead.
    #[must_use]
    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    /// The full repair instruction: kind, exact location, and replacement.
    ///
    /// Self-contained on purpose. It is carried as a [`ConfigIssue`] detail inside
    /// a [`ConfigError::Invalid`] whose `path` is the *scanned root*, which for a
    /// directory scan is not the offending file — so the issue has to name its own
    /// path or the report loses it.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "deprecated {} `{}` at {}; {}",
            self.form.kind(),
            self.found,
            self.path.display(),
            self.replacement,
        )
    }

    /// This deprecation as a validation issue.
    #[must_use]
    pub fn issue(&self) -> ConfigIssue {
        ConfigIssue::new(self.pointer.clone(), self.message())
    }

    /// This deprecation on its own as an error.
    #[must_use]
    pub fn into_error(self) -> ConfigError {
        let issue = self.issue();
        ConfigError::Invalid {
            path: self.path,
            issues: vec![issue],
        }
    }
}

/// Turn a scan's findings into an error, or `Ok(())` when there were none.
///
/// `root` is what was scanned — the config file, or the directory. Every issue
/// additionally names its own file, so a finding under a scanned directory is
/// still traceable to the exact `.md` that carries it.
pub fn reject(root: &Path, found: Vec<Deprecation>) -> Result<(), ConfigError> {
    if found.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Invalid {
        path: root.to_path_buf(),
        issues: found.iter().map(Deprecation::issue).collect(),
    })
}

/// Every deprecated form in one config document.
///
/// Covers seven of the twelve: `mode.<name>`, `layout`, `autoshare`, `reference`,
/// and agent-level `tools`, `maxSteps`, and `variant` without `model`.
///
/// Agent-level forms are looked for under **both** `agent` and `mode`, because the
/// oracle spreads each `mode` entry into `agent` verbatim
/// (`config/config.ts:536-543`) — so a `mode.build.maxSteps` is two deprecated
/// inputs, and reporting only the outer one would send the author round the loop
/// twice.
#[must_use]
pub fn inspect_config(path: &Path, value: &Value) -> Vec<Deprecation> {
    let mut found = Vec::new();
    let Some(root) = value.as_object() else {
        return found;
    };

    if let Some(mode) = root.get("mode") {
        let named = mode.as_object().filter(|entries| !entries.is_empty());
        match named {
            Some(entries) => {
                for name in entries.keys() {
                    found.push(Deprecation::keyed(
                        path,
                        vec!["mode".to_owned(), name.clone()],
                        DeprecatedForm::ModeBlock,
                        format!("use `agent.{name}` with `mode: \"primary\"`"),
                    ));
                }
            }
            None => found.push(Deprecation::keyed(
                path,
                vec!["mode".to_owned()],
                DeprecatedForm::ModeBlock,
                "use `agent`, giving each entry `mode: \"primary\"`".to_owned(),
            )),
        }
    }

    if root.contains_key("layout") {
        found.push(Deprecation::keyed(
            path,
            vec!["layout".to_owned()],
            DeprecatedForm::Layout,
            "removed — delete it; the layout is always stretched".to_owned(),
        ));
    }

    if root.contains_key("autoshare") {
        found.push(Deprecation::keyed(
            path,
            vec!["autoshare".to_owned()],
            DeprecatedForm::Autoshare,
            "use `share` — `autoshare: true` is `share: \"auto\"`".to_owned(),
        ));
    }

    if root.contains_key("reference") {
        found.push(Deprecation::keyed(
            path,
            vec!["reference".to_owned()],
            DeprecatedForm::Reference,
            "use `references`".to_owned(),
        ));
    }

    for map_key in ["agent", "mode"] {
        if let Some(agents) = root.get(map_key).and_then(Value::as_object) {
            for (name, definition) in agents {
                let Some(definition) = definition.as_object() else {
                    continue;
                };
                found.extend(inspect_agent(
                    path,
                    &[map_key.to_owned(), name.clone()],
                    definition,
                ));
            }
        }
    }

    found
}

/// Deprecated keys in one agent definition — a Markdown agent's frontmatter, or
/// one entry of the `agent` map.
///
/// The pointer is relative to the definition, so a frontmatter finding reports
/// `maxSteps` while a map finding reports `agent.build.maxSteps`.
#[must_use]
pub fn inspect_agent_frontmatter(path: &Path, value: &Value) -> Vec<Deprecation> {
    let Some(definition) = value.as_object() else {
        return Vec::new();
    };
    inspect_agent(path, &[], definition)
}

fn inspect_agent(
    path: &Path,
    prefix: &[String],
    definition: &Map<String, Value>,
) -> Vec<Deprecation> {
    let mut found = Vec::new();
    let forms = [
        (
            "tools",
            DeprecatedForm::AgentTools,
            "use `permission` — `write`, `edit`, and `patch` all collapse to `permission.edit`",
        ),
        ("maxSteps", DeprecatedForm::AgentMaxSteps, "use `steps`"),
    ];
    for (key, form, replacement) in forms {
        if definition.contains_key(key) {
            let mut pointer = prefix.to_vec();
            pointer.push(key.to_owned());
            found.push(Deprecation::keyed(
                path,
                pointer,
                form,
                replacement.to_owned(),
            ));
        }
    }
    if definition.contains_key("variant") && !definition.get("model").is_some_and(Value::is_string)
    {
        let mut pointer = prefix.to_vec();
        pointer.push("variant".to_owned());
        found.push(Deprecation::keyed(
            path,
            pointer,
            DeprecatedForm::AgentVariantWithoutModel,
            "add `model`, or delete `variant` — a variant names a level the agent's own \
             model declares, so without `model` it can never be applied"
                .to_owned(),
        ));
    }
    found
}

/// The deprecated auth-prompt field name (`packages/plugin/src/index.ts:102`,
/// `:116`, `:132`, `:146`, each annotated `@deprecated Use `when` instead`).
pub const LEGACY_AUTH_PROMPT_KEY: &str = "condition";

/// Whether one auth-prompt descriptor carries the deprecated `condition` field.
///
/// **This is the primary detector for the `condition` form, and it deliberately
/// does not take a [`Value`].** Upstream's `condition` is
/// `(inputs: Record<string, string>) => boolean` — a JavaScript closure
/// (`packages/plugin/src/index.ts:103`), replaced by the static `Rule` object
/// `{ key, op: "eq" | "neq", value }` at `:104`. A closure has no JSON encoding,
/// so a prompt that uses `condition` can never appear in a config file and
/// scanning config text for it would prove nothing. What a plugin bridge *can*
/// always produce is the descriptor's field names, which is what this takes.
///
/// The call site is not in this crate. `condition` is read only while an auth
/// method's prompts are being presented — `packages/opencode/src/cli/cmd/providers.ts:68-77`
/// evaluates `prompt.when` and then `prompt.condition` during `auth login` — so
/// the plugin wave owns the wiring and this is the predicate it calls.
#[must_use]
pub fn auth_prompt_uses_condition<I, S>(keys: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    keys.into_iter()
        .any(|key| key.as_ref() == LEGACY_AUTH_PROMPT_KEY)
}

/// The deprecation for one auth prompt, given the fields it declares.
///
/// `source` is whatever identifies the plugin — its module path, or its package
/// name — and `pointer` locates the prompt within the auth hook, so the report
/// says *which* prompt of *which* method rather than just "a plugin".
#[must_use]
pub fn auth_prompt_deprecation<I, S>(
    source: &Path,
    pointer: Vec<String>,
    keys: I,
) -> Option<Deprecation>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if !auth_prompt_uses_condition(keys) {
        return None;
    }
    let mut pointer = pointer;
    pointer.push(LEGACY_AUTH_PROMPT_KEY.to_owned());
    Some(Deprecation::keyed(
        source,
        pointer,
        DeprecatedForm::AuthPromptCondition,
        "use `when` — a `{ key, op, value }` rule, not a predicate".to_owned(),
    ))
}

/// Auth-prompt `condition` fields in an auth definition that *has* a JSON form.
///
/// A convenience over [`auth_prompt_deprecation`] for a bridge that reflects an
/// `AuthHook` into JSON with its function fields reduced to markers. It finds any
/// object inside a `prompts` array that carries a `condition` key. When the bridge
/// drops function-valued fields instead — which is what a plain
/// `structuredClone`-style reflection does — this returns nothing and
/// [`auth_prompt_uses_condition`] is the only detector that works, so prefer that
/// one at the call site.
#[must_use]
pub fn inspect_auth(path: &Path, value: &Value) -> Vec<Deprecation> {
    let mut found = Vec::new();
    walk_prompts(path, value, &mut Vec::new(), &mut found);
    found
}

fn walk_prompts(
    path: &Path,
    value: &Value,
    pointer: &mut Vec<String>,
    found: &mut Vec<Deprecation>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                pointer.push(key.clone());
                if key == "prompts"
                    && let Some(prompts) = child.as_array()
                {
                    for (index, prompt) in prompts.iter().enumerate() {
                        let carries = prompt
                            .as_object()
                            .is_some_and(|prompt| prompt.contains_key("condition"));
                        if carries {
                            let mut condition = pointer.clone();
                            condition.push(index.to_string());
                            condition.push("condition".to_owned());
                            found.push(Deprecation::keyed(
                                path,
                                condition,
                                DeprecatedForm::AuthPromptCondition,
                                "use `when`".to_owned(),
                            ));
                        }
                    }
                }
                walk_prompts(path, child, pointer, found);
                pointer.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                pointer.push(index.to_string());
                walk_prompts(path, child, pointer, found);
                pointer.pop();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Deprecated inputs that live on disk beside a config file, in any config
/// directory — project or global.
///
/// Two forms: a `{mode,modes}/` agent directory, and a `CONTEXT.md`.
///
/// A `mode/` directory is reported only when it holds at least one `*.md`
/// directly, which is exactly what the oracle's `{mode,modes}/*.md` glob
/// (`config/agent.ts:37`) would have loaded. An empty leftover directory changes
/// no behaviour in the oracle, so rejecting it would be a false positive — and
/// this pass blocks a config load, so a false positive is expensive.
///
/// Entries are sorted by filename: `read_dir` order is not defined, and an error
/// report that reorders between runs is not diffable.
#[must_use]
pub fn inspect_directory(dir: &Path) -> Vec<Deprecation> {
    let mut found = Vec::new();

    for legacy in LEGACY_AGENT_DIRECTORIES {
        let candidate = dir.join(legacy);
        if !candidate.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&candidate) else {
            continue;
        };
        let mut definitions: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        definitions.sort();
        for definition in definitions {
            let name = definition
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            found.push(Deprecation::filed(
                &definition,
                DeprecatedForm::ModeDirectory,
                format!("{legacy}/{name}"),
                format!("move it to `agent/{name}`"),
            ));
        }
    }

    found.extend(inspect_instruction_file(&dir.join(LEGACY_INSTRUCTION_FILE)));

    found
}

/// Whether a filename is the deprecated instruction filename.
///
/// `packages/opencode/src/session/instruction.ts:64-68` lists `AGENTS.md`,
/// `CLAUDE.md`, and then `CONTEXT.md` with the oracle's own `// deprecated`
/// comment. The cascade stops at the first filename class that matches anywhere
/// up the tree (`:124-132`), so `CONTEXT.md` is only ever reached when neither
/// modern name exists — which is exactly the case where its author believes it is
/// being read.
#[must_use]
pub fn is_legacy_instruction_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == LEGACY_INSTRUCTION_FILE)
}

/// The deprecation for one candidate instruction file, or `None`.
///
/// **This is the seam the instruction-file loader calls.** `CONTEXT.md` is a
/// filesystem fact, not a config key: it is found by walking up from the working
/// directory to the worktree root, not by reading any JSON. Nothing in this crate
/// performs that walk, so this takes a path the walk has already produced and
/// rejects it. Detection therefore lives *at* the cascade rather than duplicating
/// the cascade here — a second traversal would be a second chance to disagree
/// with the first about which file wins.
///
/// Returns `None` for a name that is not `CONTEXT.md`, and for a `CONTEXT.md`
/// that does not exist, so a loader can hand it every candidate it considers.
#[must_use]
pub fn inspect_instruction_file(path: &Path) -> Option<Deprecation> {
    if !is_legacy_instruction_file(path) || !path.is_file() {
        return None;
    }
    Some(Deprecation::filed(
        path,
        DeprecatedForm::ContextFile,
        LEGACY_INSTRUCTION_FILE,
        "rename it to `AGENTS.md`",
    ))
}

/// Deprecated inputs in the global config directory: everything
/// [`inspect_directory`] finds, plus the TOML `config` file.
///
/// The TOML file is global-only — `config/config.ts:262` looks for it under
/// `Global.Path.config` and nowhere else.
#[must_use]
pub fn inspect_global_directory(dir: &Path) -> Vec<Deprecation> {
    let mut found = Vec::new();
    let toml = dir.join(LEGACY_TOML_CONFIG_FILE);
    if toml.is_file() {
        found.push(Deprecation::filed(
            &toml,
            DeprecatedForm::TomlConfig,
            LEGACY_TOML_CONFIG_FILE,
            format!(
                "migrate it to `{}` — there is no TOML config path",
                canonical_config_name("config.json")
            ),
        ));
    }
    found.extend(inspect_directory(dir));
    found
}

/// Which config directory a legacy-named file turned up in.
///
/// It decides what the repair instruction can honestly offer. In one of Zuno's
/// own directories the file was unambiguously written *for Zuno*, so renaming it
/// is the whole fix. On the walk up from the working directory it is a bare file
/// at somebody's repository root, where it may belong to opencode rather than to
/// Zuno — so that message additionally names the switch that stops Zuno reading
/// project config at all, which is the correct fix for that case and would be
/// misleading advice at the global root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigFileScope {
    /// The global root, `.zuno/`, `ZUNO_CONFIG_DIR`, or the managed directory.
    Owned,
    /// A bare file on the walk up from the working directory to the worktree root.
    ProjectAncestor,
}

impl ConfigFileScope {
    fn escape_hatch(self) -> &'static str {
        match self {
            Self::Owned => "",
            Self::ProjectAncestor => {
                ", or set `ZUNO_DISABLE_PROJECT_CONFIG=1` if the file belongs to another product"
            }
        }
    }
}

/// The canonical filename that replaces a legacy one, keeping its extension.
///
/// `opencode.jsonc` becomes `zuno.jsonc`; every other legacy spelling — including
/// the global-only `config.json` — becomes `zuno.json`. Migration advice has to
/// preserve the extension, because a JSONC document renamed to `.json` stops
/// parsing the moment it contains the comment it was given the extension for.
#[must_use]
pub fn canonical_config_name(legacy: &str) -> String {
    let stem = zuno_paths::CONFIG_FILE_STEM;
    if legacy.ends_with(".jsonc") {
        format!("{stem}.jsonc")
    } else {
        format!("{stem}.json")
    }
}

/// The deprecation for one candidate config file, or `None` when it is absent.
///
/// **This is the seam config discovery calls, and it must run before the file is
/// read.** A legacy-named file that is merely skipped presents as a config that
/// changes nothing — the exact silent failure this module exists to prevent, and
/// the one that would be introduced by deleting a filename from the probe list.
#[must_use]
pub fn inspect_config_filename(path: &Path, scope: ConfigFileScope) -> Option<Deprecation> {
    if !path.is_file() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    let canonical = canonical_config_name(&name);
    Some(Deprecation::filed(
        path,
        DeprecatedForm::ConfigFileName,
        name,
        format!("rename it to `{canonical}`{}", scope.escape_hatch()),
    ))
}

/// Legacy-named config files in one of Zuno's own config directories.
#[must_use]
pub fn inspect_config_directory(dir: &Path) -> Vec<Deprecation> {
    zuno_paths::LEGACY_CONFIG_NAMES
        .iter()
        .filter_map(|name| inspect_config_filename(&dir.join(name), ConfigFileScope::Owned))
        .collect()
}

/// Legacy-named config files in the global config root, which also accepted
/// `config.json` and therefore has one more name to report than any other layer.
#[must_use]
pub fn inspect_global_config_directory(dir: &Path) -> Vec<Deprecation> {
    zuno_paths::LEGACY_GLOBAL_CONFIG_NAMES
        .iter()
        .filter_map(|name| inspect_config_filename(&dir.join(name), ConfigFileScope::Owned))
        .collect()
}

/// Reject every deprecated form in one config document.
///
/// Run this **before** [`crate::Config::from_json_value`]: the schema refuses
/// `mode`, `layout`, and `autoshare` as unrecognized keys, and whichever check
/// runs first decides whether the author is told what to write instead.
pub fn check_config(path: &Path, value: &Value) -> Result<(), ConfigError> {
    reject(path, inspect_config(path, value))
}

/// Reject every deprecated key in one agent definition's frontmatter.
pub fn check_agent_frontmatter(path: &Path, value: &Value) -> Result<(), ConfigError> {
    reject(path, inspect_agent_frontmatter(path, value))
}

/// Reject every deprecated auth-prompt `condition`.
pub fn check_auth(path: &Path, value: &Value) -> Result<(), ConfigError> {
    reject(path, inspect_auth(path, value))
}

/// Reject the deprecated on-disk forms in a config directory.
pub fn check_directory(dir: &Path) -> Result<(), ConfigError> {
    reject(dir, inspect_directory(dir))
}

/// Reject the deprecated on-disk forms in the global config directory.
pub fn check_global_directory(dir: &Path) -> Result<(), ConfigError> {
    reject(dir, inspect_global_directory(dir))
}
