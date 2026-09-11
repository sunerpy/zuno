//! Memory policy is independent of tool approval and of persistence.

use crate::service::MemoryServiceError;
use zuno_types::MemoryScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAccess {
    Read,
    Propose,
    Apply,
    Reject,
    Edit,
    Undo,
    Import,
    Maintain,
    Forget,
}

/// A host-configured policy instance, bound to its authenticated actor and
/// resource namespace. Serialized model/tool parameters never supply this value.
///
/// This preflight does not replace transactional backend checks: session
/// generation policy, source validity and learning leases must still be checked
/// in the committing transaction. Enterprise backends must additionally check
/// their current organization authorization there.
pub trait MemoryAuthority: Send + Sync {
    fn authorize(&self, scope: MemoryScope, access: MemoryAccess)
    -> Result<(), MemoryServiceError>;
}

/// Existing trusted single-user behavior. This is not an enterprise identity or
/// approval provider. Local path ownership, candidate review, session policy and
/// learning-lease checks remain enforced by their owning layers.
#[derive(Debug, Default)]
pub struct LocalMemoryAuthority;

impl MemoryAuthority for LocalMemoryAuthority {
    fn authorize(
        &self,
        _scope: MemoryScope,
        _access: MemoryAccess,
    ) -> Result<(), MemoryServiceError> {
        Ok(())
    }
}
