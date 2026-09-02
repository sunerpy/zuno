//! Session-scoped, atomically published Skill catalog generations.
//!
//! Every consumer reads the same [`SkillCatalogSnapshot`]. Filesystem events are
//! coalesced by `zuno-watch`; an overflow or relevant path change triggers a complete
//! rescan, then one atomic generation publication. A malformed `SKILL.md` preserves
//! the previous valid source entry while exposing the new warning.

use sha2::{Digest as _, Sha256};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{Mutex as AsyncMutex, watch};
use zuno_watch::flags::ZUNO_EXPERIMENTAL_FILEWATCHER;
use zuno_watch::{EventStream, WatchEvent, WatchOptions, Watcher};

use super::{Skill, SkillExposure, SkillOptions, SkillWarning, SkillWarningKind, Skills};

/// One immutable catalog generation shared by prompt, tools, slash commands, and ACP.
#[derive(Debug, Clone)]
pub struct SkillCatalogSnapshot {
    generation: u64,
    digest: String,
    skills: Arc<Skills>,
    warnings: Arc<[SkillWarning]>,
}

impl SkillCatalogSnapshot {
    /// Monotonic session-local generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// SHA-256 over ordered metadata and warnings.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The exact source set for this generation.
    #[must_use]
    pub fn skills(&self) -> &Arc<Skills> {
        &self.skills
    }

    /// Discovery, parsing, remote, and watcher warnings for this generation.
    #[must_use]
    pub fn warnings(&self) -> &[SkillWarning] {
        &self.warnings
    }
}

type Visibility = dyn Fn(&Skill) -> bool + Send + Sync;

/// Live catalog owner for one resolved session.
pub struct SkillCatalogService {
    options: SkillOptions,
    overlays: Arc<[Skill]>,
    visibility: Arc<Visibility>,
    current: RwLock<Arc<SkillCatalogSnapshot>>,
    sender: watch::Sender<Arc<SkillCatalogSnapshot>>,
    refresh: AsyncMutex<()>,
    watcher_warnings: Mutex<Vec<SkillWarning>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    watchers: Mutex<Vec<Arc<Mutex<Watcher>>>>,
}

impl std::fmt::Debug for SkillCatalogService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.snapshot();
        formatter
            .debug_struct("SkillCatalogService")
            .field("generation", &snapshot.generation)
            .field("digest", &snapshot.digest)
            .field("skills", &snapshot.skills.all().len())
            .finish_non_exhaustive()
    }
}

impl SkillCatalogService {
    /// Load, filter, publish, and begin watching every effective local root.
    pub async fn start(
        options: SkillOptions,
        overlays: impl IntoIterator<Item = Skill>,
        visibility: Arc<Visibility>,
    ) -> Arc<Self> {
        let overlays = overlays.into_iter().collect::<Vec<_>>();
        let initial = scan(&options, &overlays, &visibility, None).await;
        Self::start_with_initial(options, overlays, visibility, initial)
    }

    /// Begin watching from an initial generation already loaded by the composition root.
    #[must_use]
    pub fn start_with_initial(
        options: SkillOptions,
        overlays: impl IntoIterator<Item = Skill>,
        visibility: Arc<Visibility>,
        initial: Skills,
    ) -> Arc<Self> {
        let overlays = overlays.into_iter().collect::<Vec<_>>();
        let snapshot = Arc::new(snapshot(1, initial, &[]));
        let (sender, _) = watch::channel(Arc::clone(&snapshot));
        let service = Arc::new(Self {
            options,
            overlays: overlays.into(),
            visibility,
            current: RwLock::new(snapshot),
            sender,
            refresh: AsyncMutex::new(()),
            watcher_warnings: Mutex::new(Vec::new()),
            tasks: Mutex::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
        });
        service.install_watchers();
        service
    }

    /// A non-watching catalog for tests and embedded composition.
    #[must_use]
    pub fn fixed(skills: Arc<Skills>) -> Arc<Self> {
        let options = SkillOptions::new(
            ".",
            Option::<&str>::None,
            &zuno_paths::Env::empty(),
            Vec::new(),
            Vec::new(),
        );
        let snapshot = Arc::new(snapshot(1, (*skills).clone(), &[]));
        let (sender, _) = watch::channel(Arc::clone(&snapshot));
        Arc::new(Self {
            options,
            overlays: Arc::from([]),
            visibility: Arc::new(|_| true),
            current: RwLock::new(snapshot),
            sender,
            refresh: AsyncMutex::new(()),
            watcher_warnings: Mutex::new(Vec::new()),
            tasks: Mutex::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
        })
    }

    /// Current immutable generation.
    #[must_use]
    pub fn snapshot(&self) -> Arc<SkillCatalogSnapshot> {
        Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Subscribe to future atomic generations.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<SkillCatalogSnapshot>> {
        self.sender.subscribe()
    }

    /// Force one complete rescan. Concurrent triggers collapse behind one lock.
    pub async fn refresh(&self) -> Arc<SkillCatalogSnapshot> {
        let _guard = self.refresh.lock().await;
        let previous = self.snapshot();
        let skills = scan(
            &self.options,
            &self.overlays,
            &self.visibility,
            Some(previous.skills()),
        )
        .await;
        let watcher_warnings = self
            .watcher_warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let next = Arc::new(snapshot(
            previous.generation.saturating_add(1),
            skills,
            &watcher_warnings,
        ));
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::clone(&next);
        self.sender.send_replace(Arc::clone(&next));
        next
    }

    fn install_watchers(self: &Arc<Self>) {
        for root in self.options.watch_roots() {
            let watch_env = self
                .options
                .env()
                .clone()
                .with(ZUNO_EXPERIMENTAL_FILEWATCHER, "true");
            match Watcher::start(
                WatchOptions::new(&root)
                    .env(watch_env)
                    .watch_missing_ancestors(),
            ) {
                Ok((watcher, stream)) => {
                    let watcher = Arc::new(Mutex::new(watcher));
                    self.watchers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(Arc::clone(&watcher));
                    let weak = Arc::downgrade(self);
                    let source = root.clone();
                    let task = tokio::spawn(async move {
                        consume(stream, weak, watcher, source).await;
                    });
                    self.tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(task);
                }
                Err(error) => {
                    self.set_watcher_error(&root, Some(error.to_string()));
                }
            }
        }
    }

    fn set_watcher_error(&self, root: &std::path::Path, detail: Option<String>) {
        let source = root.to_string_lossy();
        let mut warnings = self
            .watcher_warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        warnings.retain(|warning| {
            warning.source() != source
                || !matches!(warning.kind(), SkillWarningKind::WatchFailed(_))
        });
        if let Some(detail) = detail {
            warnings.push(SkillWarning::new(
                source,
                SkillWarningKind::WatchFailed(detail),
            ));
        }
    }

    /// Stop background consumers before dropping their native watcher handles.
    pub fn shutdown(&self) {
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        self.watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl Drop for SkillCatalogService {
    fn drop(&mut self) {
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        self.watchers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

async fn consume(
    mut stream: EventStream,
    service: std::sync::Weak<SkillCatalogService>,
    watcher: Arc<Mutex<Watcher>>,
    source: std::path::PathBuf,
) {
    while let Some(first) = stream.recv().await {
        let mut relevant = event_is_relevant(&first);
        while let Some(event) = stream.try_recv() {
            relevant |= event_is_relevant(&event);
        }
        let Some(service) = service.upgrade() else {
            break;
        };
        match watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reconcile()
        {
            Ok(changed) => {
                relevant |= changed;
                if changed {
                    service.set_watcher_error(&source, None);
                }
            }
            Err(error) => {
                service.set_watcher_error(&source, Some(error.to_string()));
                relevant = true;
            }
        }
        if !relevant {
            continue;
        }
        service.refresh().await;
    }
}

fn event_is_relevant(event: &WatchEvent) -> bool {
    match event {
        WatchEvent::Overflow { .. } => true,
        WatchEvent::Changed(event) => {
            let path = &event.path;
            path.file_name().is_some_and(|name| name == "SKILL.md")
                || path.components().any(|component| {
                    matches!(
                        component.as_os_str().to_str(),
                        Some("skill" | "skills" | ".agents" | ".zuno")
                    )
                })
        }
    }
}

async fn scan(
    options: &SkillOptions,
    overlays: &[Skill],
    visibility: &Arc<Visibility>,
    previous: Option<&Arc<Skills>>,
) -> Skills {
    let mut skills = super::load(options)
        .await
        .with_overlay(overlays.iter().cloned());
    if let Some(previous) = previous {
        let stale_sources = skills
            .warnings()
            .iter()
            .filter(|warning| preserves_previous_source(warning.kind()))
            .filter_map(|warning| previous_source_for_warning(previous, warning))
            .collect::<Vec<_>>();
        skills = skills.with_overlay(
            stale_sources
                .iter()
                .filter_map(|source| previous.by_source(source).cloned()),
        );
    }
    skills.retaining(|skill| visibility(skill))
}

fn preserves_previous_source(kind: &SkillWarningKind) -> bool {
    matches!(
        kind,
        SkillWarningKind::Unreadable(_)
            | SkillWarningKind::Frontmatter(_)
            | SkillWarningKind::MissingName
            | SkillWarningKind::InvalidDescription
            | SkillWarningKind::MetadataUnreadable(_)
            | SkillWarningKind::MetadataMalformed(_)
    )
}

fn previous_source_for_warning(previous: &Skills, warning: &SkillWarning) -> Option<String> {
    if let Some(skill) = previous.by_source(warning.source()) {
        return Some(skill.location.clone());
    }
    previous
        .all()
        .iter()
        .find(|skill| {
            skill
                .metadata_sources
                .iter()
                .any(|source| source == warning.source())
        })
        .map(|skill| skill.location.clone())
}

fn snapshot(
    generation: u64,
    skills: Skills,
    watcher_warnings: &[SkillWarning],
) -> SkillCatalogSnapshot {
    let mut warnings = skills.warnings().to_vec();
    warnings.extend_from_slice(watcher_warnings);
    let digest = digest(&skills, &warnings);
    SkillCatalogSnapshot {
        generation,
        digest,
        skills: Arc::new(skills),
        warnings: warnings.into(),
    }
}

fn digest(skills: &Skills, warnings: &[SkillWarning]) -> String {
    let mut digest = Sha256::new();
    for skill in skills.all() {
        digest.update(skill.name.as_bytes());
        digest.update([0]);
        digest.update(skill.location.as_bytes());
        digest.update([0]);
        digest.update(skill.description.as_deref().unwrap_or_default().as_bytes());
        digest.update([0]);
        digest.update(skill.display_name.as_deref().unwrap_or_default().as_bytes());
        digest.update([0]);
        digest.update(
            skill
                .short_description
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        digest.update([match skill.exposure {
            SkillExposure::Index => 0,
            SkillExposure::Search => 1,
            SkillExposure::Explicit => 2,
        }]);
        for source in &skill.metadata_sources {
            digest.update(source.as_bytes());
            digest.update([0]);
        }
        if let Some(source_digest) = skill.source_digest() {
            digest.update(source_digest);
        }
        digest.update([0xff]);
    }
    for warning in warnings {
        digest.update(warning.source().as_bytes());
        digest.update([0]);
        digest.update(warning.to_string().as_bytes());
        digest.update([0xff]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    fn write_skill(root: &Path, directory: &str, name: &str, description: &str) {
        let skill = root.join(".agents/skills").join(directory);
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .expect("skill file");
    }

    fn isolated_env(root: &Path) -> zuno_paths::Env {
        zuno_paths::Env::empty()
            .with("HOME", root.join("home").to_string_lossy())
            .with("XDG_CONFIG_HOME", root.join("config").to_string_lossy())
            .with("XDG_CACHE_HOME", root.join("cache").to_string_lossy())
            .with("XDG_DATA_HOME", root.join("data").to_string_lossy())
            .with("XDG_STATE_HOME", root.join("state").to_string_lossy())
    }

    #[test]
    fn watcher_overflow_always_requires_a_complete_rescan() {
        assert!(event_is_relevant(&WatchEvent::Overflow { dropped: 1 }));
    }

    #[tokio::test]
    async fn refresh_publishes_add_rename_delete_and_preserves_a_broken_source() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "sheet", "spreadsheet", "Edit sheets");
        let env = isolated_env(root.path());
        let service = SkillCatalogService::start(
            SkillOptions::new(root.path(), Some(root.path()), &env, Vec::new(), Vec::new()),
            Vec::new(),
            Arc::new(|_| true),
        )
        .await;
        let initial = service.snapshot();
        let source = initial
            .skills()
            .get("spreadsheet")
            .unwrap()
            .location
            .clone();

        std::fs::write(&source, "---\nname:\n---\nbroken").expect("break skill");
        let broken = service.refresh().await;
        assert!(broken.skills().by_source(&source).is_some());
        assert!(!broken.warnings().is_empty());

        std::fs::write(
            &source,
            "---\nname: workbook\ndescription: Edit workbooks\n---\nbody",
        )
        .expect("rename skill");
        let renamed = service.refresh().await;
        assert!(renamed.skills().get("spreadsheet").is_none());
        assert!(renamed.skills().get("workbook").is_some());

        std::fs::remove_file(&source).expect("delete skill");
        let deleted = service.refresh().await;
        assert!(deleted.skills().get("workbook").is_none());
        assert!(deleted.generation() > initial.generation());
        assert_ne!(deleted.digest(), initial.digest());
        service.shutdown();
    }

    #[tokio::test]
    async fn body_only_edits_change_the_snapshot_digest() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "sheet", "spreadsheet", "Edit sheets");
        let env = isolated_env(root.path());
        let service = SkillCatalogService::start(
            SkillOptions::new(root.path(), Some(root.path()), &env, Vec::new(), Vec::new()),
            Vec::new(),
            Arc::new(|_| true),
        )
        .await;
        let initial = service.snapshot();
        let source = initial
            .skills()
            .get("spreadsheet")
            .expect("spreadsheet")
            .location
            .clone();

        std::fs::write(
            source,
            "---\nname: spreadsheet\ndescription: Edit sheets\n---\nchanged body",
        )
        .expect("edit body");
        let edited = service.refresh().await;

        assert!(edited.generation() > initial.generation());
        assert_ne!(edited.digest(), initial.digest());
        service.shutdown();
    }

    #[tokio::test]
    async fn sidecar_edits_change_digest_and_malformed_metadata_keeps_the_last_valid_projection() {
        let root = tempfile::tempdir().expect("root");
        write_skill(root.path(), "sheet", "spreadsheet", "Edit sheets");
        let skill_root = root.path().join(".agents/skills/sheet");
        let metadata_root = skill_root.join("agents");
        let metadata = metadata_root.join("zuno.yaml");
        std::fs::create_dir_all(&metadata_root).expect("metadata directory");
        std::fs::write(
            &metadata,
            "interface:\n  display_name: Sheet One\n  short_description: First summary\npolicy:\n  exposure: search\n",
        )
        .expect("initial metadata");

        let env = isolated_env(root.path());
        let service = SkillCatalogService::start(
            SkillOptions::new(root.path(), Some(root.path()), &env, Vec::new(), Vec::new()),
            Vec::new(),
            Arc::new(|_| true),
        )
        .await;
        let initial = service.snapshot();
        let initial_skill = initial.skills().get("spreadsheet").expect("initial skill");
        assert_eq!(initial_skill.catalog_display_name(), "Sheet One");
        assert_eq!(initial_skill.exposure, SkillExposure::Search);

        std::fs::write(
            &metadata,
            "interface:\n  display_name: Sheet Two\n  short_description: Second summary\npolicy:\n  exposure: explicit\n",
        )
        .expect("updated metadata");
        let updated = service.refresh().await;
        let updated_skill = updated.skills().get("spreadsheet").expect("updated skill");
        assert_eq!(updated_skill.catalog_display_name(), "Sheet Two");
        assert_eq!(updated_skill.exposure, SkillExposure::Explicit);
        assert_ne!(updated.digest(), initial.digest());

        std::fs::write(&metadata, "policy:\n  exposure: [\n").expect("malformed metadata");
        let malformed = service.refresh().await;
        let projected = malformed
            .skills()
            .get("spreadsheet")
            .expect("last valid skill projection");
        assert_eq!(projected.catalog_display_name(), "Sheet Two");
        assert_eq!(projected.exposure, SkillExposure::Explicit);
        let canonical_metadata =
            std::fs::canonicalize(&metadata).expect("canonical metadata source");
        assert!(malformed.warnings().iter().any(|warning| {
            matches!(warning.kind(), SkillWarningKind::MetadataMalformed(_))
                && std::fs::canonicalize(warning.source()).ok().as_deref()
                    == Some(canonical_metadata.as_path())
        }));
        service.shutdown();
    }

    #[tokio::test]
    async fn watcher_detects_a_skill_installed_after_session_start() {
        let root = tempfile::tempdir().expect("root");
        let env = isolated_env(root.path());
        let service = SkillCatalogService::start(
            SkillOptions::new(root.path(), Some(root.path()), &env, Vec::new(), Vec::new()),
            Vec::new(),
            Arc::new(|_| true),
        )
        .await;
        let mut updates = service.subscribe();
        write_skill(root.path(), "sheet", "spreadsheet", "Edit sheets");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                updates.changed().await.expect("catalog sender");
                if updates.borrow().skills().get("spreadsheet").is_some() {
                    break;
                }
            }
        })
        .await
        .expect("watcher refresh");
        service.shutdown();
    }

    #[tokio::test]
    async fn watcher_follows_a_missing_canonical_skill_root_from_its_existing_ancestor() {
        let root = tempfile::tempdir().expect("root");
        let project = root.path().join("project");
        let home = root.path().join("home");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&home).expect("home");
        let env = isolated_env(root.path());
        let requested = root.path().join("config/zuno/skill");
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical root");
        let service = SkillCatalogService::start(
            SkillOptions::new(&project, Some(&project), &env, Vec::new(), Vec::new()),
            Vec::new(),
            Arc::new(|_| true),
        )
        .await;

        let scopes = service
            .watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|watcher| {
                let watcher = watcher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    watcher.requested_root().to_path_buf(),
                    watcher.active_root().map(Path::to_path_buf),
                    watcher.watches_recursively(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            scopes.iter().any(|(logical, active, recursive)| {
                logical == &requested
                    && active.as_deref() == Some(canonical_root.as_path())
                    && !recursive
            }),
            "the missing canonical root must be followed from its nearest existing ancestor: {scopes:?}"
        );

        let mut updates = service.subscribe();
        let skill = requested.join("sheet");
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: global-sheet\ndescription: Edit sheets\n---\nbody",
        )
        .expect("skill file");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                updates.changed().await.expect("catalog sender");
                if updates.borrow().skills().get("global-sheet").is_some() {
                    break;
                }
            }
        })
        .await
        .expect("adaptive watcher refresh");
        service.shutdown();
    }
}
