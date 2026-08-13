use std::sync::{Arc, Mutex};

use oc_db::Pool;
use oc_db::artifact_gc::ArtifactGcPaths;
use oc_db::session::Store;
use oc_error::DbError;
use oc_llm::catalog::models_dev::CatalogDocument;
use oc_paths::{DbLocation, Env, GLOBAL_PROJECT_ID};
use oc_pty::PtyService;

use super::catalog::{LocationBody, LocationEnvelope, OptionalEnvelope, ProjectBody};
use super::error::ApiError;
use crate::EventService;

#[derive(Clone, Debug)]
pub struct ApiState {
    pool: Arc<Pool>,
    pty: PtyService,
    directory: Arc<str>,
    artifact_paths: ArtifactGcPaths,
    events: Option<EventService>,
    /// The environment the catalogue operations resolve config and credentials
    /// from. Injected rather than read from the process on every request so a test
    /// can pin a models.dev document without mutating global state.
    env: Arc<Env>,
    /// The project id `location.project.id` reports.
    project_id: Arc<str>,
    /// The project root `location.project.directory` reports.
    project_directory: Arc<str>,
    /// The parsed models.dev document, loaded at most once per process.
    ///
    /// Re-reading and re-parsing it per request would put a multi-megabyte parse
    /// on the hot path of three read-only endpoints; upstream caches it in the
    /// `ModelsDev` service for the same reason.
    models: Arc<Mutex<Option<Arc<CatalogDocument>>>>,
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
        let project = oc_paths::project::resolve_project(std::path::Path::new(&directory));
        // Upstream reports `/` for the global project rather than the directory it
        // was resolved from (`packages/server/src/location.ts:15-27` reads
        // `location.project`, whose global form has no worktree), so a caller can
        // tell "not in a repository" from "in this repository".
        let project_directory: Arc<str> = if project.id == GLOBAL_PROJECT_ID {
            Arc::from("/")
        } else {
            Arc::from(project.directory.to_string_lossy().into_owned())
        };
        Ok(Self {
            pty: PtyService::new(&directory),
            directory: Arc::from(directory),
            pool: Arc::new(pool),
            artifact_paths,
            events: None,
            env: Arc::new(Env::from_process()),
            project_id: Arc::from(project.id),
            project_directory,
            models: Arc::new(Mutex::new(None)),
        })
    }

    #[must_use]
    pub fn with_events(mut self, events: EventService) -> Self {
        self.events = Some(events);
        self
    }

    /// Pins the environment the catalogue operations resolve from.
    #[must_use]
    pub fn with_env(mut self, env: Env) -> Self {
        self.env = Arc::new(env);
        self
    }

    /// The environment catalogue resolution reads.
    #[must_use]
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The project root `location.project.directory` reports.
    #[must_use]
    pub fn project_directory(&self) -> &str {
        &self.project_directory
    }

    /// Wraps a payload in upstream's `{location, data}` envelope.
    pub(super) fn envelope<T>(&self, data: T) -> LocationEnvelope<T> {
        LocationEnvelope {
            location: self.location(),
            data,
        }
    }

    /// Wraps an optional payload, dropping `data` entirely when absent.
    pub(super) fn optional_envelope<T>(&self, data: Option<T>) -> OptionalEnvelope<T> {
        OptionalEnvelope {
            location: self.location(),
            data,
        }
    }

    fn location(&self) -> LocationBody {
        LocationBody {
            directory: self.directory.to_string(),
            workspace_id: None,
            project: ProjectBody {
                id: self.project_id.to_string(),
                directory: self.project_directory.to_string(),
            },
        }
    }

    /// The models.dev document, loaded from disk once and then cached.
    ///
    /// # Errors
    /// Returns [`ApiError::CatalogUnavailable`] when the document cannot be read.
    /// An unreadable catalogue is **not** flattened into an empty one: answering
    /// `[]` would tell the user they have no models when in fact the cache is
    /// corrupt. A *missing* cache is different and does resolve to an empty
    /// document, which is upstream's behaviour at `models-dev.ts:222`.
    pub(super) fn models_document(&self) -> Result<Arc<CatalogDocument>, ApiError> {
        let mut cached = self
            .models
            .lock()
            .map_err(|_| ApiError::CatalogUnavailable("models cache lock poisoned".to_owned()))?;
        if let Some(document) = cached.as_ref() {
            return Ok(Arc::clone(document));
        }
        let layout = oc_paths::Layout::resolve(&self.env);
        let source = oc_llm::catalog::CatalogSource::resolve(&self.env, &layout);
        let document = source
            .load_from_disk()
            .map_err(|error| ApiError::CatalogUnavailable(error.to_string()))?
            .unwrap_or_default();
        let document = Arc::new(document);
        *cached = Some(Arc::clone(&document));
        Ok(document)
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

    pub(crate) fn pool(&self) -> &Pool {
        &self.pool
    }

    pub(super) fn artifact_paths(&self) -> &ArtifactGcPaths {
        &self.artifact_paths
    }
}
