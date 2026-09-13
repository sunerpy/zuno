use super::*;
use crate::workspace_merge::{SnapshotTree, WorkspaceContent};
use std::io::{Read, Seek, SeekFrom};
use zuno_application::{
    workspace_files::*,
    workspace_merge::{WorkspaceEntry, WorkspacePath},
};
use zuno_types::activity::Counter;

fn file_bytes(
    tree: &SnapshotTree,
    path: &WorkspacePath,
    entry: &WorkspaceEntry,
    offset: u64,
    maximum: usize,
) -> Result<Vec<u8>, ApplicationError> {
    let WorkspaceContent::File {
        mut file,
        offset: start,
        bytes,
        ..
    } = tree.content(path, entry)?
    else {
        return Err(ApplicationError::Invalid(
            "entry has no regular file body".to_owned(),
        ));
    };
    if offset > bytes {
        return Err(ApplicationError::Invalid(
            "file offset exceeds its size".to_owned(),
        ));
    }
    file.seek(SeekFrom::Start(start + offset))
        .map_err(crate::storage)?;
    let mut output = Vec::new();
    file.take((bytes - offset).min(maximum as u64))
        .read_to_end(&mut output)
        .map_err(crate::storage)?;
    Ok(output)
}

fn contained(path: &WorkspacePath, root: &WorkspacePath) -> bool {
    root.as_str() == "."
        || path == root
        || path
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|tail| tail.starts_with('/'))
}

fn query(
    tree: &SnapshotTree,
    request: &WorkspaceFileQuery,
) -> Result<WorkspaceFileResult, ApplicationError> {
    request.validate()?;
    match request {
        WorkspaceFileQuery::Read {
            path,
            offset,
            maximum_bytes,
        } => {
            let entry = tree
                .entries()
                .get(path)
                .ok_or(ApplicationError::NotFound)?
                .clone();
            let item = WorkspaceFileItem {
                path: path.clone(),
                entry: entry.clone(),
            };
            if !matches!(
                entry,
                WorkspaceEntry::File { .. } | WorkspaceEntry::Hardlink { .. }
            ) {
                return Ok(WorkspaceFileResult::Read {
                    item,
                    text: None,
                    offset: *offset,
                    next_offset: *offset,
                    truncated: false,
                });
            }
            let size = match tree.content(path, &entry)? {
                WorkspaceContent::File { bytes, .. } => bytes,
                _ => return Err(ApplicationError::Conflict),
            };
            let bytes = file_bytes(tree, path, &entry, offset.0, *maximum_bytes as usize)?;
            let (text, consumed) = match std::str::from_utf8(&bytes) {
                Ok(text) if !text.contains('\0') => (Some(text.to_owned()), bytes.len()),
                Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => (
                    Some(
                        std::str::from_utf8(&bytes[..error.valid_up_to()])
                            .map_err(crate::storage)?
                            .to_owned(),
                    ),
                    error.valid_up_to(),
                ),
                _ => (None, bytes.len()),
            };
            let next = offset.0 + consumed as u64;
            Ok(WorkspaceFileResult::Read {
                item,
                text,
                offset: *offset,
                next_offset: Counter(next),
                truncated: next < size,
            })
        }
        WorkspaceFileQuery::List { path, after, limit } => {
            if !matches!(
                tree.entries().get(path),
                Some(WorkspaceEntry::Directory { .. })
            ) {
                return Err(ApplicationError::NotFound);
            }
            let candidates = tree.entries().iter().filter(|(candidate, _)| {
                *candidate != path
                    && candidate.parent().unwrap_or_else(WorkspacePath::root) == *path
                    && after.as_ref().is_none_or(|after| *candidate > after)
            });
            let mut entries = Vec::new();
            let mut encoded = 0usize;
            let mut more = false;
            for (path, entry) in candidates {
                let item = WorkspaceFileItem {
                    path: path.clone(),
                    entry: entry.clone(),
                };
                let bytes = serde_json::to_vec(&item).map_err(crate::storage)?.len();
                if entries.len() >= *limit as usize || encoded + bytes > 65536 {
                    more = true;
                    break;
                }
                encoded += bytes;
                entries.push(item);
            }
            let after = if more {
                entries.last().map(|entry| entry.path.clone())
            } else {
                None
            };
            Ok(WorkspaceFileResult::List { entries, after })
        }
        WorkspaceFileQuery::Search { path, text, limit } => {
            if !tree.entries().contains_key(path) {
                return Err(ApplicationError::NotFound);
            }
            let mut matches = Vec::new();
            let mut scanned = 0usize;
            let mut rendered = 0usize;
            let mut skipped = 0u64;
            let mut truncated = false;
            for (candidate, entry) in tree.entries() {
                if !contained(candidate, path)
                    || !matches!(
                        entry,
                        WorkspaceEntry::File { .. } | WorkspaceEntry::Hardlink { .. }
                    )
                {
                    continue;
                }
                if scanned >= 8 * 1024 * 1024 {
                    truncated = true;
                    break;
                }
                let bytes = file_bytes(tree, candidate, entry, 0, 262145)?;
                scanned += bytes.len();
                if bytes.len() > 262144 {
                    skipped += 1;
                    continue;
                }
                let Ok(content) = std::str::from_utf8(&bytes) else {
                    skipped += 1;
                    continue;
                };
                if content.contains('\0') {
                    skipped += 1;
                    continue;
                }
                for (index, line) in content.lines().enumerate() {
                    let Some(found) = line.find(text) else {
                        continue;
                    };
                    if matches.len() >= *limit as usize || rendered >= 65536 {
                        truncated = true;
                        break;
                    }
                    let available = (65536 - rendered).min(2048);
                    if available < text.len() {
                        truncated = true;
                        break;
                    }
                    let mut start = found.saturating_sub((available - text.len()).min(512));
                    while !line.is_char_boundary(start) {
                        start += 1;
                    }
                    let mut end = line.len().min(start + available);
                    while !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    rendered += end - start;
                    matches.push(WorkspaceMatch {
                        path: candidate.clone(),
                        line: Counter(index as u64 + 1),
                        text: line[start..end].to_owned(),
                        truncated: start > 0 || end < line.len(),
                    });
                }
                if truncated {
                    break;
                }
            }
            Ok(WorkspaceFileResult::Search {
                matches,
                truncated,
                skipped_files: Counter(skipped),
            })
        }
    }
}

#[async_trait]
impl WorkspaceFileReader for DockerGateway {
    async fn query_files(
        &self,
        lease: &ExecutionLease,
        operation: &WorkspaceFileOperation,
        authority: &dyn WorkspaceFileAuthority,
    ) -> Result<WorkspaceFileReceipt, ApplicationError> {
        operation.validate()?;
        let permit = self
            .file_reads
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        let mut environment = self.get(&lease.owner, &operation.environment_id).await?;
        if environment.spec.session_id != lease.session_id {
            return Err(ApplicationError::Forbidden);
        }
        let id = EnvironmentSnapshotId::new(format!(
            "files_{}",
            zuno_orchestration::sha256_json(&json!([
                lease.owner,
                operation.environment_id,
                operation.expected_revision
            ]))
        ))
        .map_err(crate::storage)?;
        let existing = match self.ledger.snapshot(&lease.owner, &id) {
            Ok(snapshot) => {
                if snapshot.environment_id != operation.environment_id
                    || snapshot.revision != operation.expected_revision
                {
                    return Err(ApplicationError::Conflict);
                }
                environment.revision = snapshot.revision;
                Some(snapshot)
            }
            Err(ApplicationError::NotFound) => None,
            Err(error) => return Err(error),
        };
        authority
            .authorize_files(lease, &environment, operation)
            .await?;
        let snapshot = match existing {
            Some(snapshot) => snapshot,
            None => {
                self.snapshot_named(
                    &lease.owner,
                    &operation.environment_id,
                    operation.expected_revision,
                    &id,
                )
                .await?
            }
        };
        let path = self.snapshot_path(&lease.owner, &snapshot.id);
        let expected = snapshot.clone();
        let request = operation.query.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let tree = SnapshotTree::read(&path, &expected.sha256, expected.bytes)?;
            query(&tree, &request)
        })
        .await
        .map_err(crate::storage)??;
        // A long archive scan cannot extend revoked authority or a lost lease.
        authority
            .authorize_files(lease, &environment, operation)
            .await?;
        Ok(WorkspaceFileReceipt {
            operation_id: operation.id.clone(),
            snapshot,
            result,
        })
    }
}
