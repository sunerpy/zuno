use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn read(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    space: &MemorySpaceId,
) -> Result<Vec<SharedEvidenceBinding>, ApplicationError> {
    let rows=query("SELECT content,content_digest,grants FROM zuno_enterprise_preview.shared_memory_support WHERE tenant_id=$1 AND space_id=$2 ORDER BY content")
        .bind(principal.tenant_id().as_str()).bind(space.as_str()).fetch_all(&mut **tx).await.map_err(database_error)?;
    let mut values = Vec::new();
    for row in rows {
        let content: String = row.try_get("content").map_err(database_error)?;
        let grants: Vec<RequestId> =
            serde_json::from_value(row.try_get("grants").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if zuno_orchestration::sha256_text(&content)
            != row
                .try_get::<String, _>("content_digest")
                .map_err(database_error)?
            || grants.is_empty()
            || grants.len() > 16
        {
            return Err(ApplicationError::Conflict);
        }
        values.push(SharedEvidenceBinding { content, grants });
    }
    values.sort_by(|left, right| left.content.cmp(&right.content));
    Ok(values)
}
pub(super) async fn suppressed(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    space: &MemorySpaceId,
    entries: &[String],
) -> Result<Vec<String>, ApplicationError> {
    let mut result = Vec::new();
    let mut checked = BTreeMap::new();
    for binding in read(tx, principal, space).await? {
        if !entries.contains(&binding.content) {
            return Err(ApplicationError::Conflict);
        }
        let mut current = false;
        for id in &binding.grants {
            let value = if let Some(current) = checked.get(id) {
                *current
            } else {
                let current = evidence::grant(tx, principal, space, id).await?.current;
                checked.insert(id.clone(), current);
                current
            };
            current |= value;
        }
        if !current {
            result.push(binding.content);
        }
    }
    Ok(result)
}
pub(super) async fn prepare(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    space: &MemorySpaceId,
    current: &[String],
    after: &[String],
    request: &ProposeSharedMemory,
) -> Result<Option<SharedEvidenceTransition>, ApplicationError> {
    let before = read(tx, principal, space).await?;
    let mut next = before
        .iter()
        .filter(|b| after.contains(&b.content))
        .map(|b| (b.content.clone(), b.grants.clone()))
        .collect::<BTreeMap<_, _>>();
    // An explicit edit with no evidence is a manual reviewable assertion. It
    // does not accidentally inherit withdrawn support from identical old text.
    for edit in &request.edits {
        if let SharedMemoryEdit::Add { content } | SharedMemoryEdit::Replace { content, .. } = edit
        {
            next.remove(content.trim());
        }
    }
    let mut bound = BTreeSet::new();
    for binding in &request.evidence {
        if binding.content.trim() != binding.content
            || !after.contains(&binding.content)
            || !bound.insert(&binding.content)
        {
            return Err(invalid());
        }
        let mut unique = BTreeSet::new();
        for id in &binding.grants {
            if !unique.insert(id) {
                return Err(invalid());
            }
            if !evidence::grant(tx, principal, space, id).await?.current {
                return Err(ApplicationError::Conflict);
            }
        }
        next.insert(binding.content.clone(), binding.grants.clone());
    }
    if before.iter().any(|b| !current.contains(&b.content)) {
        return Err(ApplicationError::Conflict);
    }
    let after = next
        .into_iter()
        .map(|(content, grants)| SharedEvidenceBinding { content, grants })
        .collect::<Vec<_>>();
    if before.is_empty() && after.is_empty() {
        Ok(None)
    } else {
        Ok(Some(SharedEvidenceTransition { before, after }))
    }
}
pub(super) async fn settle(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    space: &MemorySpaceId,
    value: &SharedMemoryChange,
    undo: bool,
) -> Result<(), ApplicationError> {
    let empty = SharedEvidenceTransition {
        before: Vec::new(),
        after: Vec::new(),
    };
    let transition = value.evidence.as_ref().unwrap_or(&empty);
    let (expected, next) = if undo {
        (&transition.after, &transition.before)
    } else {
        (&transition.before, &transition.after)
    };
    if read(tx, principal, space).await? != *expected {
        return Err(ApplicationError::Conflict);
    }
    if !undo {
        for binding in next {
            if expected.contains(binding) {
                continue;
            }
            for id in &binding.grants {
                if !evidence::grant(tx, principal, space, id).await?.current {
                    return Err(ApplicationError::Conflict);
                }
            }
        }
    }
    query("DELETE FROM zuno_enterprise_preview.shared_memory_support WHERE tenant_id=$1 AND space_id=$2")
        .bind(principal.tenant_id().as_str()).bind(space.as_str()).execute(&mut **tx).await.map_err(database_error)?;
    for binding in next {
        query("INSERT INTO zuno_enterprise_preview.shared_memory_support(tenant_id,space_id,content_digest,content,grants) VALUES($1,$2,$3,$4,$5)")
            .bind(principal.tenant_id().as_str()).bind(space.as_str()).bind(zuno_orchestration::sha256_text(&binding.content))
            .bind(&binding.content).bind(json!(binding.grants)).execute(&mut **tx).await.map_err(database_error)?;
    }
    Ok(())
}
