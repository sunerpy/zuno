use std::sync::Arc;

use oc_db::Pool;
use oc_db::session::Store;
use oc_error::DbError;
use oc_paths::{DbLocation, GLOBAL_PROJECT_ID};
use oc_pty::PtyService;

#[derive(Clone, Debug)]
pub struct ApiState {
    pool: Arc<Pool>,
    pty: PtyService,
    directory: Arc<str>,
}

impl ApiState {
    /// Creates an isolated API state and installs the current database schema.
    ///
    /// # Errors
    /// Returns the classified database failure when opening or seeding fails.
    pub fn memory(directory: impl Into<String>) -> Result<Self, DbError> {
        Self::initialize(Pool::open(&DbLocation::Memory)?, directory.into())
    }

    /// Opens the process database and installs the current API services.
    ///
    /// # Errors
    /// Returns the classified database failure when opening or migrating fails.
    pub fn open_default(directory: impl Into<String>) -> Result<Self, DbError> {
        Self::initialize(Pool::open_default()?, directory.into())
    }

    fn initialize(pool: Pool, directory: String) -> Result<Self, DbError> {
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
        })
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
}
