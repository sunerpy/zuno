//! `skills.urls[]` — the remote skill-index protocol.
//!
//! Port of `packages/opencode/src/skill/discovery.ts` (opencode 1.18.13).
//!
//! # The protocol
//!
//! For a configured `<url>`:
//!
//! 1. `GET <url>/index.json` (`:50-51`; a missing trailing slash is added, so
//!    `https://h/sub` fetches `https://h/sub/index.json`).
//! 2. The document is `{ "skills": [{ "name", "files": [...], "version"? }] }`
//!    (`:13-21`).
//! 3. An entry whose `files` does **not** list `SKILL.md` is warned about and
//!    dropped (`:67-73`). It is never downloaded.
//! 4. Each surviving entry caches to `$XDG_CACHE_HOME/opencode/skills/<name>/`.
//!    Files resolve against `<url>/<name>/` (`:90`).
//! 5. With no `version`, or a cached `.opencode-version` that already matches,
//!    files download in place and an existing file is left alone (`:38`, `:87-92`).
//!    Otherwise the whole skill is staged in `<root>.tmp-<token>`, checked for a
//!    `SKILL.md`, stamped with the version, and swapped in with the previous
//!    directory kept as `<root>.old-<token>` until the rename succeeds
//!    (`:93-125`).
//! 6. A root is returned only if `<root>/SKILL.md` exists (`:126`).
//!
//! # Failure is never fatal
//!
//! Every network step in the oracle is wrapped in `Effect.catch` that logs and
//! substitutes an empty result. One unreachable URL in a config file must not
//! make the agent unusable, so this port keeps that property and adds the two
//! bounds the oracle inherits from its HTTP layer rather than states:
//! [`REMOTE_TIMEOUT`] per request and [`SKILL_CONCURRENCY`] / [`FILE_CONCURRENCY`]
//! in flight. Same shape as `oc_config::instructions`, which proved the pattern
//! against a hanging server.
//!
//! # One deliberate hardening
//!
//! `index.json` is remote input, and the oracle joins `file` onto the cache root
//! with no traversal check — a `"files": ["../../../.bashrc"]` entry would write
//! outside the cache. This port rejects any entry file that escapes its skill
//! root, with a warning. It only rejects what the oracle should not have
//! accepted; a well-formed index is unaffected.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use futures::stream::StreamExt;
use serde::Deserialize;
use url::Url;

use crate::skill::{SkillWarning, SkillWarningKind};

/// How many skills from one index are refreshed at once
/// (`discovery.ts:10`, `skillConcurrency`).
pub const SKILL_CONCURRENCY: usize = 4;

/// How many files of one skill download at once
/// (`discovery.ts:11`, `fileConcurrency`).
pub const FILE_CONCURRENCY: usize = 8;

/// The per-request budget. The oracle inherits this from its HTTP layer; stating
/// it is what makes a hanging index survivable.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);

/// The stamp file that records which `version` a cached skill was built from
/// (`discovery.ts:80`).
pub const VERSION_FILE: &str = ".opencode-version";

/// The filename an index entry must list to be usable.
pub const SKILL_FILENAME: &str = "SKILL.md";

/// One `skills[]` entry of an `index.json`.
#[derive(Debug, Clone, Deserialize)]
struct IndexEntry {
    name: String,
    files: Vec<String>,
    #[serde(default)]
    version: Option<String>,
}

/// The `index.json` document.
#[derive(Debug, Clone, Deserialize)]
struct IndexDocument {
    skills: Vec<IndexEntry>,
}

/// What one `skills.urls[]` entry produced.
#[derive(Debug, Default)]
pub struct Pulled {
    /// Cache directories holding a `SKILL.md`, ready to be scanned.
    pub dirs: Vec<PathBuf>,
    /// Everything that went wrong, none of which stopped the pull.
    pub warnings: Vec<SkillWarning>,
}

/// `Discovery.pull` (`discovery.ts:49-132`) for one configured URL.
///
/// `cache_root` is `$XDG_CACHE_HOME/opencode/skills` — the caller passes it so
/// the whole module stays testable without touching process state.
pub async fn pull(url: &str, cache_root: &Path) -> Pulled {
    let mut pulled = Pulled::default();

    let base = match Url::parse(&with_trailing_slash(url)) {
        Ok(base) => base,
        Err(error) => {
            pulled.push(url, SkillWarningKind::IndexMalformed(error.to_string()));
            return pulled;
        }
    };
    let Ok(index_url) = base.join("index.json") else {
        pulled.push(
            url,
            SkillWarningKind::IndexMalformed("index.json is not resolvable".to_string()),
        );
        return pulled;
    };

    let client = match reqwest::Client::builder().timeout(REMOTE_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            pulled.push(url, SkillWarningKind::IndexUnreachable(error.to_string()));
            return pulled;
        }
    };

    let index = match fetch_index(&client, index_url.as_str()).await {
        Ok(index) => index,
        Err(kind) => {
            pulled.push(index_url.as_str(), kind);
            return pulled;
        }
    };

    let mut usable = Vec::new();
    for entry in index.skills {
        if !entry.files.iter().any(|file| file == SKILL_FILENAME) {
            pulled.push(
                index_url.as_str(),
                SkillWarningKind::EntryMissingSkillMd {
                    skill: entry.name.clone(),
                },
            );
            continue;
        }
        usable.push(entry);
    }

    let results = futures::stream::iter(usable.into_iter().map(|entry| {
        let client = client.clone();
        let base = base.clone();
        let cache_root = cache_root.to_path_buf();
        async move { refresh(&client, &base, &cache_root, entry).await }
    }))
    .buffered(SKILL_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    for outcome in results {
        pulled.warnings.extend(outcome.warnings);
        if let Some(dir) = outcome.dir {
            pulled.dirs.push(dir);
        }
    }
    pulled
}

impl Pulled {
    fn push(&mut self, source: &str, kind: SkillWarningKind) {
        self.warnings.push(SkillWarning::new(source, kind));
    }
}

fn with_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

async fn fetch_index(
    client: &reqwest::Client,
    index_url: &str,
) -> Result<IndexDocument, SkillWarningKind> {
    let request = async {
        let response = client
            .get(index_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(transport_or_timeout)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SkillWarningKind::IndexStatus(status.as_u16()));
        }
        let body = response.text().await.map_err(transport_or_timeout)?;
        serde_json::from_str::<IndexDocument>(&body)
            .map_err(|error| SkillWarningKind::IndexMalformed(error.to_string()))
    };

    match tokio::time::timeout(REMOTE_TIMEOUT, request).await {
        Ok(result) => result,
        Err(_) => Err(SkillWarningKind::IndexTimeout),
    }
}

fn transport_or_timeout(error: reqwest::Error) -> SkillWarningKind {
    if error.is_timeout() {
        return SkillWarningKind::IndexTimeout;
    }
    SkillWarningKind::IndexUnreachable(error.to_string())
}

#[derive(Default)]
struct Refreshed {
    dir: Option<PathBuf>,
    warnings: Vec<SkillWarning>,
}

async fn refresh(
    client: &reqwest::Client,
    base: &Url,
    cache_root: &Path,
    entry: IndexEntry,
) -> Refreshed {
    let mut out = Refreshed::default();
    let root = cache_root.join(&entry.name);

    let files = match safe_files(&entry, &mut out.warnings) {
        Some(files) => files,
        None => return out,
    };

    let current = tokio::fs::read_to_string(root.join(VERSION_FILE))
        .await
        .ok();
    let in_place = match entry.version.as_deref() {
        None => true,
        Some(version) => current.as_deref() == Some(version),
    };

    if in_place {
        out.warnings
            .extend(download_all(client, base, &entry.name, &files, &root).await);
    } else {
        let version = entry.version.as_deref().unwrap_or_default();
        out.warnings
            .extend(stage_and_swap(client, base, &entry.name, &files, &root, version).await);
    }

    if tokio::fs::metadata(root.join(SKILL_FILENAME)).await.is_ok() {
        out.dir = Some(root);
    }
    out
}

/// Drop any entry file that would escape its skill root, and de-duplicate.
fn safe_files(entry: &IndexEntry, warnings: &mut Vec<SkillWarning>) -> Option<Vec<String>> {
    let mut files = BTreeSet::new();
    for file in &entry.files {
        if escapes(file) {
            warnings.push(SkillWarning::new(
                &entry.name,
                SkillWarningKind::UnsafeIndexPath {
                    skill: entry.name.clone(),
                    file: file.clone(),
                },
            ));
            continue;
        }
        files.insert(file.clone());
    }
    if files.iter().all(|file| file != SKILL_FILENAME) {
        // The traversal check removed the one file that made this entry usable.
        return None;
    }
    Some(files.into_iter().collect())
}

fn escapes(file: &str) -> bool {
    let path = Path::new(file);
    if path.is_absolute() {
        return true;
    }
    let mut depth = 0i32;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

async fn download_all(
    client: &reqwest::Client,
    base: &Url,
    name: &str,
    files: &[String],
    dest_root: &Path,
) -> Vec<SkillWarning> {
    let results = futures::stream::iter(files.iter().map(|file| {
        let client = client.clone();
        let base = base.clone();
        let dest = dest_root.join(file);
        let file = file.clone();
        async move {
            let Ok(url) = base.join(&format!("{name}/{file}")) else {
                return Err(SkillWarning::new(
                    name,
                    SkillWarningKind::IndexMalformed(format!("{file} is not resolvable")),
                ));
            };
            download(&client, url.as_str(), &dest).await
        }
    }))
    .buffered(FILE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    results.into_iter().filter_map(Result::err).collect()
}

async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> Result<(), SkillWarning> {
    if tokio::fs::metadata(dest).await.is_ok() {
        return Ok(());
    }
    let fetch = async {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        response
            .bytes()
            .await
            .map_err(|error| error.to_string())
            .map(|body| body.to_vec())
    };

    let body = match tokio::time::timeout(REMOTE_TIMEOUT, fetch).await {
        Ok(Ok(body)) => body,
        Ok(Err(detail)) => {
            return Err(SkillWarning::new(
                url,
                SkillWarningKind::DownloadFailed { detail },
            ));
        }
        Err(_) => {
            return Err(SkillWarning::new(url, SkillWarningKind::IndexTimeout));
        }
    };

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            SkillWarning::new(
                url,
                SkillWarningKind::DownloadFailed {
                    detail: error.to_string(),
                },
            )
        })?;
    }
    tokio::fs::write(dest, body).await.map_err(|error| {
        SkillWarning::new(
            url,
            SkillWarningKind::DownloadFailed {
                detail: error.to_string(),
            },
        )
    })
}

async fn stage_and_swap(
    client: &reqwest::Client,
    base: &Url,
    name: &str,
    files: &[String],
    root: &Path,
    version: &str,
) -> Vec<SkillWarning> {
    let token = uuid::Uuid::new_v4().to_string();
    let staging = sibling(root, &format!(".tmp-{token}"));
    let backup = sibling(root, &format!(".old-{token}"));

    let mut warnings = download_all(client, base, name, files, &staging).await;
    let staged_ok = warnings.is_empty()
        && tokio::fs::metadata(staging.join(SKILL_FILENAME))
            .await
            .is_ok();

    if staged_ok && let Err(error) = swap(&staging, root, &backup, version).await {
        warnings.push(SkillWarning::new(
            name,
            SkillWarningKind::DownloadFailed {
                detail: error.to_string(),
            },
        ));
    }

    // `Effect.ensuring` (`discovery.ts:123`): the staging directory never
    // outlives the attempt, successful or not.
    let _ = tokio::fs::remove_dir_all(&staging).await;
    warnings
}

async fn swap(staging: &Path, root: &Path, backup: &Path, version: &str) -> std::io::Result<()> {
    tokio::fs::write(staging.join(VERSION_FILE), version).await?;
    let cached = tokio::fs::metadata(root).await.is_ok();
    if cached {
        tokio::fs::rename(root, backup).await?;
    }
    match tokio::fs::rename(staging, root).await {
        Ok(()) => {
            if cached {
                let _ = tokio::fs::remove_dir_all(backup).await;
            }
            Ok(())
        }
        Err(error) => {
            if cached {
                let _ = tokio::fs::rename(backup, root).await;
            }
            Err(error)
        }
    }
}

fn sibling(root: &Path, suffix: &str) -> PathBuf {
    let mut name = root.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    root.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_is_added_once() {
        assert_eq!(with_trailing_slash("https://h/sub"), "https://h/sub/");
        assert_eq!(with_trailing_slash("https://h/sub/"), "https://h/sub/");
    }

    #[test]
    fn index_url_is_resolved_under_the_base_path() {
        let base = Url::parse(&with_trailing_slash("https://h/sub")).expect("url");
        assert_eq!(
            base.join("index.json").expect("join").as_str(),
            "https://h/sub/index.json"
        );
        assert_eq!(
            base.join("alpha/SKILL.md").expect("join").as_str(),
            "https://h/sub/alpha/SKILL.md"
        );
    }

    #[test]
    fn traversal_paths_are_rejected() {
        for file in ["../escape.md", "a/../../escape.md", "/etc/passwd"] {
            assert!(escapes(file), "{file} must be rejected");
        }
        for file in ["SKILL.md", "./SKILL.md", "scripts/run.sh", "a/../b.md"] {
            assert!(!escapes(file), "{file} must be accepted");
        }
    }

    #[test]
    fn an_entry_reduced_to_nothing_by_the_traversal_check_is_dropped() {
        let entry = IndexEntry {
            name: "bad".to_string(),
            files: vec!["../SKILL.md".to_string()],
            version: None,
        };
        let mut warnings = Vec::new();
        assert!(safe_files(&entry, &mut warnings).is_none());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn index_document_tolerates_unknown_keys_and_missing_version() {
        let document: IndexDocument = serde_json::from_str(
            r#"{"skills":[{"name":"a","files":["SKILL.md"],"extra":1}],"meta":"x"}"#,
        )
        .expect("index parses");
        assert_eq!(document.skills.len(), 1);
        assert!(document.skills[0].version.is_none());
    }

    #[test]
    fn staging_and_backup_are_siblings_of_the_root() {
        let root = Path::new("/cache/skills/alpha");
        assert_eq!(
            sibling(root, ".tmp-1"),
            Path::new("/cache/skills/alpha.tmp-1")
        );
    }
}
