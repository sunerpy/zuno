use std::sync::Arc;

use oc_db::Pool;
use oc_db::artifact_gc::ArtifactGcPaths;
use oc_db::session::Store;
use oc_error::DbError;
use oc_paths::{DbLocation, GLOBAL_PROJECT_ID};
use oc_pty::PtyService;

use crate::EventService;

#[derive(Clone, Debug)]
pub struct ApiState {
    pool: Arc<Pool>,
    pty: PtyService,
    directory: Arc<str>,
    artifact_paths: ArtifactGcPaths,
    events: Option<EventService>,
}

impl ApiState {
    /// Creates an isolated API state and installs the current database schema.
    ///
    /// # Errors
    /// Returns the classified database failure when opening or seeding fails.
    pub fn memory(directory: impl Into<String>) -> Result<Self, DbError> {
        let directory = directory.into();
        let artifact_root = std::env::temp_dir().join(format!(
            "opencode-rust-api-{}",
            uuid::Uuid::new_v4().simple()
        ));
        Self::initialize(
            Pool::open(&DbLocation::Memory)?,
            directory,
            ArtifactGcPaths::from_data_root(&artifact_root),
        )
    }

    /// Opens the process database and installs the current API services.
    ///
    /// # Errors
    /// Returns the classified database failure when opening or migrating fails.
    pub fn open_default(directory: impl Into<String>) -> Result<Self, DbError> {
        Self::initialize(
            Pool::open_default()?,
            directory.into(),
            ArtifactGcPaths::in_layout(oc_paths::global()),
        )
    }

    /// Creates API state from caller-owned database and artifact storage.
    ///
    /// # Errors
    /// Returns the classified database failure when migration or project seeding fails.
    pub fn from_pool(
        pool: Pool,
        directory: impl Into<String>,
        artifact_paths: ArtifactGcPaths,
    ) -> Result<Self, DbError> {
        Self::initialize(pool, directory.into(), artifact_paths)
    }

    fn initialize(
        pool: Pool,
        directory: String,
        artifact_paths: ArtifactGcPaths,
    ) -> Result<Self, DbError> {
        {
            let mut connection = pool.get()?;
            oc_db::migration::apply(&mut connection)?;
            connection
                .execute(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (id) DO NOTHING",
                    [GLOBAL_PROJECT_ID, directory.as_str(), "0", "0", "[]"],
                )
                .map_err(oc_db::map_error)?;
        }
        Ok(Self {
            pty: PtyService::new(&directory),
            directory: Arc::from(directory),
            pool: Arc::new(pool),
            artifact_paths,
            events: None,
        })
    }

    #[must_use]
    pub fn with_events(mut self, events: EventService) -> Self {
        self.events = Some(events);
        self
    }

    #[must_use]
    pub fn sessions(&self) -> Store<'_> {
        Store::new(&self.pool)
    }

    #[must_use]
    pub fn pty(&self) -> &PtyService {
        &self.pty
    }

    #[must_use]
    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub(super) fn events(&self) -> Option<&EventService> {
        self.events.as_ref()
    }

    pub(super) fn pool(&self) -> &Pool {
        &self.pool
    }

    pub(super) fn artifact_paths(&self) -> &ArtifactGcPaths {
        &self.artifact_paths
    }
}
