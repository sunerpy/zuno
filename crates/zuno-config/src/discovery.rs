//! Configuration discovery and precedence.
//!
//! This is a direct port of the local layers in
//! `packages/opencode/src/config/config.ts:246-260,398-475,516-584`, with the
//! cloud-only well-known and hosted-organization layers deliberately absent.
//! Sources are merged from lowest to highest precedence. Objects merge deeply,
//! arrays and scalars are replaced, except that `instructions` keeps the target
//! entries first, appends source entries, and removes later duplicates.

use crate::Config;
use crate::schema::JsonMap;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use zuno_error::ConfigError;
use zuno_paths::{CONFIG_FILE_STEM, Env, Layout};

const ZUNO_CONFIG: &str = "ZUNO_CONFIG";
const ZUNO_CONFIG_CONTENT: &str = "ZUNO_CONFIG_CONTENT";
const ZUNO_PERMISSION: &str = "ZUNO_PERMISSION";
const ZUNO_DISABLE_AUTOCOMPACT: &str = "ZUNO_DISABLE_AUTOCOMPACT";
const ZUNO_DISABLE_PRUNE: &str = "ZUNO_DISABLE_PRUNE";
const ZUNO_TEST_MANAGED_CONFIG_DIR: &str = "ZUNO_TEST_MANAGED_CONFIG_DIR";
const MERGED_CONFIG_SOURCE: &str = "<merged config>";

/// A decoded macOS managed-preferences document and the plist it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPreferences {
    source: PathBuf,
    text: String,
}

impl ManagedPreferences {
    /// Construct a managed-preferences layer.
    #[must_use]
    pub fn new(source: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            text: text.into(),
        }
    }

    /// The virtual `mobileconfig:` source name.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// The JSON produced from the managed plist.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone)]
enum ManagedPreferencesMode {
    Native,
    Injected(ManagedPreferences),
}

/// Explicit inputs to config discovery.
///
/// Production callers normally use [`DiscoveryOptions::from_process`]. Tests use
/// [`DiscoveryOptions::new`] so the same immutable environment can also be handed
/// to the TypeScript oracle without mutating this process's environment.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    env: Env,
    layout: Layout,
    managed_config_dir: PathBuf,
    managed_preferences: ManagedPreferencesMode,
    default_username: String,
}

impl DiscoveryOptions {
    /// Resolve all path inputs from an explicit environment snapshot.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        worktree: Option<impl Into<PathBuf>>,
        env: Env,
    ) -> Self {
        let layout = Layout::resolve(&env);
        let managed_config_dir = env
            .truthy_value(ZUNO_TEST_MANAGED_CONFIG_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(|| system_managed_config_dir(&env));
        Self {
            directory: directory.into(),
            worktree: worktree.map(Into::into),
            env,
            layout,
            managed_config_dir,
            managed_preferences: ManagedPreferencesMode::Native,
            default_username: process_username(),
        }
    }

    /// Snapshot the process environment for a production discovery run.
    #[must_use]
    pub fn from_process(
        directory: impl Into<PathBuf>,
        worktree: Option<impl Into<PathBuf>>,
    ) -> Self {
        Self::new(directory, worktree, Env::from_process())
    }

    /// Inject the macOS-only final layer on any host.
    ///
    /// This is the test seam that proves managed preferences override every other
    /// layer without requiring a macOS CI runner or changing the real preference
    /// domain.
    #[must_use]
    pub fn with_managed_preferences(mut self, preferences: ManagedPreferences) -> Self {
        self.managed_preferences = ManagedPreferencesMode::Injected(preferences);
        self
    }

    /// Override the fallback username used when config does not set one.
    #[must_use]
    pub fn with_default_username(mut self, username: impl Into<String>) -> Self {
        self.default_username = username.into();
        self
    }

    /// The directory from which ancestor discovery starts.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The inclusive boundary for project discovery.
    #[must_use]
    pub fn worktree(&self) -> Option<&Path> {
        self.worktree.as_deref()
    }
}

/// Discover config from the current process environment.
///
/// # Errors
///
/// Returns [`ConfigError`] for the first existing layer that cannot be read,
/// parsed, or validated.
pub fn discover(directory: &Path, worktree: Option<&Path>) -> Result<Config, ConfigError> {
    discover_with(&DiscoveryOptions::from_process(directory, worktree))
}

/// Discover and merge every local config layer in oracle precedence order.
///
/// # Errors
///
/// Returns [`ConfigError`] for the first existing layer that cannot be read,
/// parsed, or validated.
pub fn discover_with(options: &DiscoveryOptions) -> Result<Config, ConfigError> {
    ensure_default_global_config(options)?;
    let mut result = RawJson::empty_object();

    // Each file is one layer, so list-like values retain their per-layer merge
    // semantics between JSON and JSONC too.
    for path in Layout::file_in_directory(options.layout.config(), CONFIG_FILE_STEM) {
        merge_file(&mut result, &path)?;
    }

    // config.ts:401-404.
    if let Some(path) = options.env.truthy_value(ZUNO_CONFIG) {
        merge_file(&mut result, &resolve_from(&options.directory, path))?;
    }

    // config.ts:406-410 and ConfigPaths.files: ancestors first, nearest last.
    if !options.layout.project_config_disabled() {
        for path in Layout::config_files(CONFIG_FILE_STEM, &options.directory, options.worktree()) {
            merge_file(&mut result, &path)?;
        }
    }

    result.insert_if_absent("agent", RawJson::empty_object());

    for directory in
        read_config_directories(&options.layout, &options.directory, options.worktree())
    {
        for path in Layout::file_in_directory(&directory, CONFIG_FILE_STEM) {
            merge_file(&mut result, &path)?;
        }
    }
    result.insert_if_absent("command", RawJson::empty_object());

    // config.ts:468-475. This virtual layer does not receive schema injection.
    if let Some(text) = options.env.truthy_value(ZUNO_CONFIG_CONTENT) {
        let layer = parse_layer(Path::new(ZUNO_CONFIG_CONTENT), text)?;
        merge_config(&mut result, layer);
    }

    // config.ts:516-522.
    if options.managed_config_dir.exists() {
        for path in Layout::file_in_directory(&options.managed_config_dir, CONFIG_FILE_STEM) {
            merge_file(&mut result, &path)?;
        }
    }

    // config.ts:524-534. This assignment is deliberately after every other
    // config source, making it testably highest precedence even on Linux.
    if let Some(preferences) = managed_preferences(options) {
        let layer = parse_layer(preferences.source(), preferences.text())?;
        merge_config(&mut result, layer);
    }

    // config.ts:545-551: invalid JSON is ignored, valid JSON deep-merges last.
    if let Some(permission) = options.env.truthy_value(ZUNO_PERMISSION)
        && let Ok(layer) = serde_json::from_str::<RawJson>(permission)
    {
        let target = result.entry_or_insert("permission", RawJson::empty_object());
        merge_deep(target, layer);
    }

    apply_tools_permissions(&mut result);
    apply_username(&mut result, &options.default_username);
    apply_compaction_flags(&mut result, &options.env);
    config_from_raw(Path::new(MERGED_CONFIG_SOURCE), &result)
}

/// Merge already-validated config layers with the same semantics discovery uses.
///
/// This is intentionally public so property tests and downstream config sources
/// share the same merge rather than reproducing it.
///
/// # Errors
///
/// Returns [`ConfigError`] if a programmatically constructed config contains a
/// value that cannot be represented as JSON.
pub fn merge_layers<I>(layers: I) -> Result<Config, ConfigError>
where
    I: IntoIterator<Item = Config>,
{
    let path = Path::new(MERGED_CONFIG_SOURCE);
    let mut result = RawJson::empty_object();
    for layer in layers {
        merge_config(&mut result, raw_from_config(path, &layer)?);
    }
    config_from_raw(path, &result)
}

/// Convert a `serde_json` line/column into its zero-based byte offset in `text`.
///
/// JSONC stripping is byte-stable, so an error from the stripped document points
/// at the same byte in the user's original file.
#[must_use]
pub fn json_error_byte_offset(text: &str, error: &serde_json::Error) -> usize {
    let line = error.line().max(1);
    let column = error.column().max(1);
    let line_start = text
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    line_start
        .saturating_add(column.saturating_sub(1))
        .min(text.len())
}

/// The subset of [`Layout::config_directories`] that discovery reads config files
/// from, in precedence order.
///
/// `config_directories` also yields the global root and `$HOME/.zuno`, which the
/// file walk deliberately skips. This is public because `zuno debug` replays
/// discovery to attribute plugin origins: two copies of this predicate is how the
/// diagnostic comes to report plugins from a file the runtime does not load.
#[must_use]
pub fn read_config_directories(
    layout: &Layout,
    directory: &Path,
    worktree: Option<&Path>,
) -> Vec<PathBuf> {
    layout
        .config_directories(directory, worktree)
        .into_iter()
        .filter(|candidate| {
            let is_override = layout
                .config_dir_override()
                .filter(|value| !value.is_empty())
                .is_some_and(|value| candidate == Path::new(value));
            candidate
                .to_string_lossy()
                .ends_with(zuno_paths::PROJECT_CONFIG_DIRECTORY)
                || is_override
        })
        .collect()
}

fn ensure_default_global_config(options: &DiscoveryOptions) -> Result<(), ConfigError> {
    if [
        ZUNO_CONFIG,
        zuno_paths::env::ZUNO_CONFIG_DIR,
        ZUNO_CONFIG_CONTENT,
    ]
    .iter()
    .any(|key| options.env.truthy_value(key).is_some())
    {
        return Ok(());
    }

    let candidates = Layout::file_in_directory(options.layout.config(), CONFIG_FILE_STEM);
    if candidates.iter().any(|path| path.exists()) {
        return Ok(());
    }

    let path = &candidates[0];
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, "{}\n");
    Ok(())
}

fn merge_file(result: &mut RawJson, path: &Path) -> Result<(), ConfigError> {
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let layer = parse_layer(path, &text)?;
    merge_config(result, layer);
    Ok(())
}

fn parse_layer(path: &Path, text: &str) -> Result<RawJson, ConfigError> {
    // Todo 9's variable substitution seam is immediately before this byte-stable
    // JSONC pass. Discovery itself deliberately does not interpret {env:...} or
    // {file:...} tokens.
    let strict = strip_jsonc(text);
    let config = Config::from_json_str(path, &strict)?;
    raw_from_config(path, &config)
}

fn raw_from_config(path: &Path, config: &Config) -> Result<RawJson, ConfigError> {
    let text = serde_json::to_string(config).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn config_from_raw(path: &Path, raw: &RawJson) -> Result<Config, ConfigError> {
    let text = serde_json::to_string(raw).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let mut config = Config::from_json_str(path, &text)?;
    restore_merged_agent_options(&mut config, raw, path)?;
    Ok(config)
}

/// Agent deserialization sweeps unknown keys into `options`. Layers have already
/// undergone that normalization before merging, so a second sweep must not let an
/// older unknown key overwrite a newer explicit `options` value.
fn restore_merged_agent_options(
    config: &mut Config,
    raw: &RawJson,
    path: &Path,
) -> Result<(), ConfigError> {
    let Some(agents) = config.agent.take() else {
        return Ok(());
    };
    let raw_agents = raw.get("agent");
    let mut restored = crate::schema::ordered::OrderedMap::new();
    for (name, mut agent) in agents {
        if let Some(options) = raw_agents
            .and_then(|value| value.get(name.as_str()))
            .and_then(|value| value.get("options"))
        {
            let text = serde_json::to_string(options).map_err(|source| ConfigError::Json {
                path: path.to_path_buf(),
                source,
            })?;
            agent.options = Some(serde_json::from_str::<JsonMap>(&text).map_err(|source| {
                ConfigError::Json {
                    path: path.to_path_buf(),
                    source,
                }
            })?);
        }
        restored.insert(name, agent);
    }
    config.agent = Some(restored);
    Ok(())
}

fn merge_config(target: &mut RawJson, source: RawJson) {
    let instructions = match (target.get("instructions"), source.get("instructions")) {
        (Some(RawJson::Array(target)), Some(RawJson::Array(source))) => {
            let mut seen = HashSet::new();
            let mut merged = Vec::with_capacity(target.len() + source.len());
            for item in target.iter().chain(source) {
                if let RawJson::String(instruction) = item
                    && seen.insert(instruction.clone())
                {
                    merged.push(RawJson::String(instruction.clone()));
                }
            }
            Some(RawJson::Array(merged))
        }
        _ => None,
    };
    merge_deep(target, source);
    if let Some(instructions) = instructions {
        target.insert("instructions", instructions);
    }
}

fn merge_deep(target: &mut RawJson, source: RawJson) {
    match source {
        RawJson::Object(source_entries) => match target {
            RawJson::Object(target_entries) => {
                for (key, source_value) in source_entries {
                    match target_entries
                        .iter_mut()
                        .find(|(candidate, _)| *candidate == key)
                    {
                        Some((_, target_value)) => merge_deep(target_value, source_value),
                        None => target_entries.push((key, source_value)),
                    }
                }
            }
            target => *target = RawJson::Object(source_entries),
        },
        source => *target = source,
    }
}

fn apply_tools_permissions(result: &mut RawJson) {
    let Some(RawJson::Object(tools)) = result.get("tools").cloned() else {
        return;
    };
    let mut defaults = RawJson::empty_object();
    for (tool, enabled) in tools {
        let RawJson::Bool(enabled) = enabled else {
            continue;
        };
        let key = if matches!(tool.as_str(), "write" | "edit" | "patch") {
            "edit"
        } else {
            tool.as_str()
        };
        defaults.insert(
            key,
            RawJson::String(if enabled { "allow" } else { "deny" }.to_owned()),
        );
    }
    if let Some(permission) = result.get("permission").cloned() {
        merge_deep(&mut defaults, permission);
    }
    result.insert("permission", defaults);
}

fn apply_username(result: &mut RawJson, fallback: &str) {
    let has_username =
        matches!(result.get("username"), Some(RawJson::String(value)) if !value.is_empty());
    if !has_username {
        result.insert("username", RawJson::String(fallback.to_owned()));
    }
}

fn apply_compaction_flags(result: &mut RawJson, env: &Env) {
    if !env.flag(ZUNO_DISABLE_AUTOCOMPACT) && !env.flag(ZUNO_DISABLE_PRUNE) {
        return;
    }
    let compaction = result.entry_or_insert("compaction", RawJson::empty_object());
    if env.flag(ZUNO_DISABLE_AUTOCOMPACT) {
        compaction.insert("auto", RawJson::Bool(false));
    }
    if env.flag(ZUNO_DISABLE_PRUNE) {
        compaction.insert("prune", RawJson::Bool(false));
    }
}

fn resolve_from(directory: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        directory.join(path)
    }
}

fn system_managed_config_dir(env: &Env) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = env;
        PathBuf::from("/Library/Application Support/zuno")
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(env.truthy_value("ProgramData").unwrap_or("C:\\ProgramData")).join("zuno")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = env;
        PathBuf::from("/etc/zuno")
    }
}

fn managed_preferences(options: &DiscoveryOptions) -> Option<ManagedPreferences> {
    match &options.managed_preferences {
        ManagedPreferencesMode::Injected(preferences) => Some(preferences.clone()),
        ManagedPreferencesMode::Native => {
            read_native_managed_preferences(&options.default_username)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn read_native_managed_preferences(_username: &str) -> Option<ManagedPreferences> {
    None
}

#[cfg(target_os = "macos")]
fn read_native_managed_preferences(username: &str) -> Option<ManagedPreferences> {
    use std::process::Command;

    const DOMAIN: &str = "ai.zuno.managed";
    const METADATA: &[&str] = &[
        "PayloadDisplayName",
        "PayloadIdentifier",
        "PayloadType",
        "PayloadUUID",
        "PayloadVersion",
        "_manualProfile",
    ];
    let paths = [
        PathBuf::from("/Library/Managed Preferences")
            .join(username)
            .join(format!("{DOMAIN}.plist")),
        PathBuf::from("/Library/Managed Preferences").join(format!("{DOMAIN}.plist")),
    ];
    for path in paths {
        if !path.exists() {
            continue;
        }
        let output = Command::new("plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&path)
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let mut raw = serde_json::from_slice::<RawJson>(&output.stdout).ok()?;
        if let RawJson::Object(entries) = &mut raw {
            entries.retain(|(key, _)| !METADATA.contains(&key.as_str()));
        }
        let text = serde_json::to_string(&raw).ok()?;
        return Some(ManagedPreferences::new(
            format!("mobileconfig:{}", path.display()),
            text,
        ));
    }
    None
}

fn process_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "user".to_owned())
}

/// Strip JSONC comments and trailing commas without changing byte offsets.
pub fn strip_jsonc(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        String,
        LineComment,
        BlockComment,
    }

    let mut bytes = text.as_bytes().to_vec();
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes[..3].fill(b' ');
    }
    let mut state = State::Normal;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        match state {
            State::Normal => match bytes[index] {
                b'"' => state = State::String,
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    state = State::LineComment;
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    state = State::BlockComment;
                    index += 1;
                }
                _ => {}
            },
            State::String => {
                if escaped {
                    escaped = false;
                } else if bytes[index] == b'\\' {
                    escaped = true;
                } else if bytes[index] == b'"' {
                    state = State::Normal;
                }
            }
            State::LineComment => {
                if bytes[index] == b'\n' || bytes[index] == b'\r' {
                    state = State::Normal;
                } else {
                    bytes[index] = b' ';
                }
            }
            State::BlockComment => {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    state = State::Normal;
                    index += 1;
                } else if bytes[index] != b'\n' && bytes[index] != b'\r' {
                    bytes[index] = b' ';
                }
            }
        }
        index += 1;
    }

    state = State::Normal;
    escaped = false;
    index = 0;
    while index < bytes.len() {
        match state {
            State::Normal => {
                if bytes[index] == b'"' {
                    state = State::String;
                } else if bytes[index] == b',' {
                    let previous = bytes[..index]
                        .iter()
                        .rposition(|byte| !byte.is_ascii_whitespace())
                        .map(|position| bytes[position]);
                    let mut next = index + 1;
                    while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                        next += 1;
                    }
                    if matches!(bytes.get(next), Some(b'}' | b']'))
                        && !matches!(previous, Some(b':' | b',' | b'[' | b'{'))
                    {
                        bytes[index] = b' ';
                    }
                }
            }
            State::String => {
                if escaped {
                    escaped = false;
                } else if bytes[index] == b'\\' {
                    escaped = true;
                } else if bytes[index] == b'"' {
                    state = State::Normal;
                }
            }
            State::LineComment | State::BlockComment => unreachable!("comments were stripped"),
        }
        index += 1;
    }
    String::from_utf8(bytes).expect("JSONC stripping only replaces ASCII bytes")
}

#[derive(Debug, Clone, PartialEq)]
enum RawJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl RawJson {
    fn empty_object() -> Self {
        Self::Object(Vec::new())
    }

    fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn insert_if_absent(&mut self, key: impl Into<String>, value: Self) {
        let key = key.into();
        if self.get(&key).is_none() {
            self.insert(key, value);
        }
    }

    fn insert(&mut self, key: impl Into<String>, value: Self) {
        let key = key.into();
        match self {
            Self::Object(entries) => {
                if let Some((_, slot)) = entries.iter_mut().find(|(candidate, _)| *candidate == key)
                {
                    *slot = value;
                } else {
                    entries.push((key, value));
                }
            }
            this => *this = Self::Object(vec![(key, value)]),
        }
    }

    fn entry_or_insert(&mut self, key: impl Into<String>, value: Self) -> &mut Self {
        let key = key.into();
        if !matches!(self, Self::Object(_)) {
            *self = Self::empty_object();
        }
        let Self::Object(entries) = self else {
            unreachable!("replaced with object")
        };
        if let Some(index) = entries.iter().position(|(candidate, _)| *candidate == key) {
            return &mut entries[index].1;
        }
        entries.push((key, value));
        &mut entries.last_mut().expect("just inserted").1
    }
}

impl Serialize for RawJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawJsonVisitor;

        impl<'de> Visitor<'de> for RawJsonVisitor {
            type Value = RawJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(RawJson::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(RawJson::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(RawJson::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(RawJson::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(RawJson::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(RawJson::String(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(RawJson::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(RawJson::Null)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(RawJson::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries: Vec<(String, RawJson)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, RawJson>()? {
                    if let Some((_, slot)) =
                        entries.iter_mut().find(|(candidate, _)| *candidate == key)
                    {
                        *slot = value;
                    } else {
                        entries.push((key, value));
                    }
                }
                Ok(RawJson::Object(entries))
            }
        }

        deserializer.deserialize_any(RawJsonVisitor)
    }
}
