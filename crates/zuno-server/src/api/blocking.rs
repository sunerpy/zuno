//! The blocking-pool budget every off-reactor API handler runs inside.
//!
//! # Why the bound lives in one place
//!
//! `zuno serve` polls this router on a single-threaded runtime
//! (`Builder::new_current_thread`), so a synchronous handler — a whole-file read, a
//! retention scan, a skill-tree walk, an image decode — has to leave the reactor or it
//! freezes every SSE stream and every live turn until the disk or the decoder answers.
//!
//! Moving that work to `tokio::task::spawn_blocking` removes the serialization the
//! reactor was providing. Without a bound, concurrency is limited only by the blocking
//! pool's default 512 threads, which is also where [`crate::EventService`]'s durable
//! writes, the permission and question settles, and the goal resumes the agent loop
//! depends on run. Bounding one group of handlers while a sibling group stays unbounded
//! only moves the starvation one endpoint over, so every group is named here and
//! charged to a budget.
//!
//! # The permit belongs to the work, not to the caller
//!
//! A `spawn_blocking` closure whose `JoinHandle` is dropped still runs to completion.
//! A permit held in the handler's future is therefore handed back the moment a client
//! hangs up, while the read it queued keeps its whole-file buffer resident: the bound
//! would limit *connected callers* rather than work in flight, which on an
//! unauthenticated-by-default server is no bound at all. The permit is moved **into**
//! the closure, so it is released when the work ends.
//!
//! # What the budgets are not
//!
//! They are cost bounds, not throughput tuning knobs, and no request field can raise
//! one. A caller that cannot have a permit waits for one; work that has not started
//! holds no buffer, and a caller that disconnects while waiting never starts the work
//! at all. The released 0.6.6 build ran the filesystem and maintenance handlers inline
//! on the reactor — an effective process-wide concurrency of one — so these budgets are
//! looser than the shipped behaviour they replace and cannot refuse a request the
//! previous release served.

use std::sync::{Arc, LazyLock};

use tokio::sync::Semaphore;

use super::error::ApiError;

/// How many `/api/fs` operations run at once across the whole process.
///
/// `fs/read` buffers a whole file up to `READ_MAX_BYTES` (32 MiB), so this is the
/// factor between the per-request ceiling and the process-wide resident ceiling:
/// four keeps it at 128 MiB instead of the blocking pool's 16 GiB.
const FILESYSTEM_SLOTS: usize = 4;

/// How many session prune previews or mutations run at once.
///
/// `GET /api/session/prune?olderThan=0&allProjects=true` is a full retention scan over
/// every project, and a mutation additionally unlinks artifacts and writes rows. Two
/// keeps a flood of cheap GETs from occupying the blocking pool the durable event
/// commits and permission settles share.
const MAINTENANCE_SLOTS: usize = 2;

/// How many catalogue discovery walks run at once.
///
/// `GET /api/skill` walks the skill tree and may fetch remote skill indexes, so its
/// duration is bounded by a network peer rather than by local disk.
const CATALOG_SLOTS: usize = 8;

/// How many prompt attachment admissions decode at once.
///
/// `POST /api/session/{sessionID}/prompt` decodes every inline `prompt.files[]` image
/// before the prompt is persisted. The attachment crate bounds one decode's live
/// working set at 512,000,000 bytes (`MAX_DECODE_WORKING_BYTES`), and the worst legal
/// default admission -- an 8-bit gray 30,117,000 x 1 PNG of 146,036 source bytes --
/// measures about 522 MB peak RSS and 3.4 s on Linux x86_64. Two keeps the process-wide
/// resident ceiling near 1 GiB. The released build ran this loop inline on the
/// single-threaded serve reactor, an effective concurrency of one, so two is looser than
/// the shipped behaviour and cannot refuse a prompt the previous release admitted.
const ADMISSION_SLOTS: usize = 2;

static FILESYSTEM: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(FILESYSTEM_SLOTS)));

static MAINTENANCE: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAINTENANCE_SLOTS)));

static CATALOG: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(CATALOG_SLOTS)));

static ADMISSION: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(ADMISSION_SLOTS)));

/// Which budget one piece of off-reactor work is charged to.
///
/// A budget is per group of endpoints rather than per process, because the groups have
/// different costs and different callers: a shared counter would let a frequent cheap
/// walk spend the allowance that exists for the expensive scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Budget {
    /// `GET /api/fs/read`, `GET /api/fs/list`, `GET /api/fs/find`.
    Filesystem,
    /// `GET /api/session/prune` and `POST /api/session/prune`.
    Maintenance,
    /// `GET /api/skill` discovery.
    Catalog,
    /// `POST /api/session/{sessionID}/prompt` attachment admission: image header
    /// parse, decode gate, decode, fit, re-encode and object write.
    Admission,
}

impl Budget {
    fn slots(self) -> &'static Arc<Semaphore> {
        match self {
            Self::Filesystem => &FILESYSTEM,
            Self::Maintenance => &MAINTENANCE,
            Self::Catalog => &CATALOG,
            Self::Admission => &ADMISSION,
        }
    }

    /// What the caller is told when the work could not run at all.
    ///
    /// A closed semaphore would mean the process is shutting down, and a panicking
    /// worker leaves no result; there is no path that closes these, and failing the
    /// request is the fail-closed answer if one is ever added.
    fn unavailable(self) -> ApiError {
        match self {
            Self::Filesystem => ApiError::FilesystemUnavailable,
            Self::Maintenance => {
                ApiError::MutationFailed("session maintenance did not finish".to_owned())
            }
            Self::Catalog => {
                ApiError::CatalogUnavailable("catalogue discovery did not finish".to_owned())
            }
            Self::Admission => {
                ApiError::MutationFailed("prompt attachment admission did not finish".to_owned())
            }
        }
    }

    /// How many permits this budget has in total.
    #[cfg(test)]
    pub(super) const fn size(self) -> usize {
        match self {
            Self::Filesystem => FILESYSTEM_SLOTS,
            Self::Maintenance => MAINTENANCE_SLOTS,
            Self::Catalog => CATALOG_SLOTS,
            Self::Admission => ADMISSION_SLOTS,
        }
    }

    /// How many permits are free right now.
    #[cfg(test)]
    pub(super) fn available(self) -> usize {
        self.slots().available_permits()
    }

    /// Takes every permit, so a test can assert that a handler waits for one.
    #[cfg(test)]
    pub(super) async fn hold_all(self) -> tokio::sync::OwnedSemaphorePermit {
        let all = u32::try_from(self.size()).expect("a budget fits a permit count");
        Arc::clone(self.slots())
            .acquire_many_owned(all)
            .await
            .expect("the budget is open")
    }
}

/// Runs one synchronous operation off the reactor, inside `budget`.
///
/// # Errors
/// Returns the budget's own unavailable failure when the work cannot be run or does not
/// finish; the work's own `ApiError` otherwise.
pub(super) async fn run<T, F>(budget: Budget, work: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    let permit = Arc::clone(budget.slots())
        .acquire_owned()
        .await
        .map_err(|_closed| budget.unavailable())?;
    tokio::task::spawn_blocking(move || {
        let outcome = work();
        // Released here rather than in the caller's future: this closure runs to
        // completion even after the client disconnects, so a permit that went back on
        // disconnect would bound connected callers instead of resident work.
        drop(permit);
        outcome
    })
    .await
    .map_err(|_panicked| budget.unavailable())?
}
