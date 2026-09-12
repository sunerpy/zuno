//! Snapshot comparison and archive construction without host extraction.
//! The gateway owns the files; only logical entry metadata reaches clients.
use crate::storage;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use zuno_application::{
    ApplicationError,
    workspace_merge::{WorkspaceEntry, WorkspaceMergePlan, WorkspacePath, WorkspaceTree},
};
use zuno_types::activity::Counter;

const MAX_TREE_ENTRIES: usize = 50_000;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

struct Blob {
    offset: u64,
    size: u64,
}
pub struct SnapshotTree {
    source: PathBuf,
    archive_sha256: String,
    archive_bytes: u64,
    entries: WorkspaceTree,
    blobs: BTreeMap<WorkspacePath, Blob>,
}
pub enum WorkspaceContent {
    File {
        file: std::fs::File,
        offset: u64,
        bytes: u64,
        sha256: String,
    },
    Text {
        bytes: Vec<u8>,
        sha256: String,
    },
}
impl SnapshotTree {
    pub fn content(
        &self,
        path: &WorkspacePath,
        expected: &WorkspaceEntry,
    ) -> Result<WorkspaceContent, ApplicationError> {
        if self.entries.get(path) != Some(expected) {
            return Err(ApplicationError::Conflict);
        }
        let target = match expected {
            WorkspaceEntry::Hardlink { target } => target,
            WorkspaceEntry::Symlink { target, .. } => {
                return Ok(WorkspaceContent::Text {
                    bytes: target.as_bytes().to_vec(),
                    sha256: zuno_orchestration::sha256_text(target),
                });
            }
            WorkspaceEntry::File { .. } => path,
            _ => return Err(invalid("directory metadata has no file body")),
        };
        let Some(WorkspaceEntry::File { sha256, bytes, .. }) = self.entries.get(target) else {
            return Err(ApplicationError::Conflict);
        };
        let blob = self.blobs.get(target).ok_or(ApplicationError::Conflict)?;
        if blob.size != bytes.0 {
            return Err(ApplicationError::Conflict);
        }
        Ok(WorkspaceContent::File {
            file: std::fs::File::open(&self.source).map_err(storage)?,
            offset: blob.offset,
            bytes: blob.size,
            sha256: sha256.clone(),
        })
    }
    pub fn read(source: &Path, sha256: &str, bytes: u64) -> Result<Self, ApplicationError> {
        if bytes > MAX_ARCHIVE_BYTES {
            return Err(invalid("workspace snapshot exceeds the merge bound"));
        }
        crate::archive::verify(source, sha256, bytes)?;
        let mut archive = tar::Archive::new(std::fs::File::open(source).map_err(storage)?);
        let mut entries = WorkspaceTree::new();
        let mut blobs = BTreeMap::new();
        let mut total = 0u64;
        let mut root_seen = false;
        for item in archive.entries().map_err(storage)? {
            let mut item = item.map_err(storage)?;
            let raw = item.path().map_err(storage)?.into_owned();
            let path = member_path(&raw)?;
            let kind = item.header().entry_type();
            if path.is_none() {
                if !kind.is_dir() || root_seen {
                    return Err(invalid("invalid or duplicate workspace root"));
                }
                root_seen = true;
            }
            let path = path.unwrap_or_else(WorkspacePath::root);
            if entries.len() >= MAX_TREE_ENTRIES {
                return Err(invalid("workspace contains too many merge entries"));
            }
            let uid = u32::try_from(item.header().uid().map_err(storage)?).map_err(storage)?;
            let gid = u32::try_from(item.header().gid().map_err(storage)?).map_err(storage)?;
            let mode = item.header().mode().map_err(storage)?;
            let entry = if kind.is_dir() {
                if item.size() != 0 {
                    return Err(invalid("directory contains archive data"));
                }
                WorkspaceEntry::Directory { mode, uid, gid }
            } else if kind.is_symlink() {
                if item.size() != 0 {
                    return Err(invalid("symlink contains archive data"));
                }
                let target = item
                    .link_name()
                    .map_err(storage)?
                    .ok_or_else(|| invalid("symlink target is missing"))?;
                WorkspaceEntry::Symlink {
                    uid,
                    gid,
                    target: target
                        .to_str()
                        .ok_or_else(|| invalid("symlink is not UTF-8"))?
                        .to_owned(),
                }
            } else if kind.is_hard_link() {
                if item.size() != 0 {
                    return Err(invalid("hardlink contains archive data"));
                }
                let target = item
                    .link_name()
                    .map_err(storage)?
                    .ok_or_else(|| invalid("hardlink target is missing"))?;
                WorkspaceEntry::Hardlink {
                    target: member_path(&target)?
                        .ok_or_else(|| invalid("hardlink cannot name the workspace root"))?,
                }
            } else if kind.is_file() {
                let size = item.size();
                total = total.checked_add(size).ok_or(ApplicationError::Conflict)?;
                if total > MAX_ARCHIVE_BYTES {
                    return Err(invalid("workspace member bytes exceed the merge bound"));
                }
                let offset = item.raw_file_position();
                let mut hasher = Sha256::new();
                let mut buffer = [0u8; 65536];
                let mut read = 0u64;
                loop {
                    let count = item.read(&mut buffer).map_err(storage)?;
                    if count == 0 {
                        break;
                    }
                    read = read
                        .checked_add(count as u64)
                        .ok_or(ApplicationError::Conflict)?;
                    if read > size {
                        return Err(invalid("workspace member exceeds its declared size"));
                    }
                    hasher.update(&buffer[..count]);
                }
                if read != size {
                    return Err(invalid("workspace member is truncated"));
                }
                blobs.insert(path.clone(), Blob { offset, size });
                WorkspaceEntry::File {
                    mode,
                    uid,
                    gid,
                    sha256: hex::encode(hasher.finalize()),
                    bytes: Counter(size),
                }
            } else {
                return Err(invalid("unsupported workspace archive member"));
            };
            entry.validate(&path)?;
            if entries.insert(path, entry).is_some() {
                return Err(invalid("duplicate workspace archive member"));
            }
        }
        entries
            .entry(WorkspacePath::root())
            .or_insert(WorkspaceEntry::Directory {
                mode: 0o755,
                uid: 0,
                gid: 0,
            });
        // Docker normally includes directories. Omitted parents can be normalized
        // without accepting a file/symlink as a traversal prefix.
        let parents = entries
            .keys()
            .filter_map(WorkspacePath::parent)
            .collect::<Vec<_>>();
        for parent in parents {
            let mut next = Some(parent);
            while let Some(path) = next {
                if entries.len() >= MAX_TREE_ENTRIES && !entries.contains_key(&path) {
                    return Err(invalid("workspace contains too many merge entries"));
                }
                entries
                    .entry(path.clone())
                    .or_insert(WorkspaceEntry::Directory {
                        mode: 0o755,
                        uid: 0,
                        gid: 0,
                    });
                next = path.parent();
            }
        }
        let aliases = entries
            .iter()
            .filter_map(|(path, entry)| match entry {
                WorkspaceEntry::Hardlink { target } => Some((path.clone(), target.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (path, mut target) in aliases {
            let mut seen = std::collections::BTreeSet::new();
            seen.insert(path.clone());
            loop {
                if !seen.insert(target.clone()) {
                    return Err(invalid("cyclic workspace hardlink"));
                }
                match entries.get(&target) {
                    Some(WorkspaceEntry::Hardlink { target: next }) => target = next.clone(),
                    Some(WorkspaceEntry::File { .. }) => break,
                    _ => return Err(invalid("workspace hardlink has no regular-file source")),
                }
            }
            entries.insert(path, WorkspaceEntry::Hardlink { target });
        }
        zuno_application::workspace_merge::validate_tree(&entries)?;
        Ok(Self {
            source: source.to_owned(),
            archive_sha256: sha256.to_owned(),
            archive_bytes: bytes,
            entries,
            blobs,
        })
    }
    pub fn entries(&self) -> &WorkspaceTree {
        &self.entries
    }
}
fn invalid(message: &str) -> ApplicationError {
    ApplicationError::Invalid(message.to_owned())
}

fn member_path(path: &Path) -> Result<Option<WorkspacePath>, ApplicationError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("workspace paths must be UTF-8"))?;
    let text = text
        .strip_prefix("./")
        .unwrap_or(text)
        .trim_end_matches('/');
    if text == "workspace" {
        return Ok(None);
    }
    let path = text
        .strip_prefix("workspace/")
        .ok_or_else(|| invalid("archive member escapes the workspace"))?;
    Ok(Some(WorkspacePath::new(path)?))
}

/// Revalidates all immutable archives and all manifest bindings before reading
/// blobs. The output is a new archive; no existing workspace is mutated here.
pub fn write_merged_archive(
    reviewed: &WorkspaceMergePlan,
    base: &SnapshotTree,
    parent: &SnapshotTree,
    child: &SnapshotTree,
    destination: &Path,
) -> Result<(String, u64), ApplicationError> {
    let resolved = zuno_application::workspace_merge::resolved_tree(
        reviewed,
        &base.entries,
        &parent.entries,
        &child.entries,
    )?;
    for snapshot in [base, parent, child] {
        crate::archive::verify(
            &snapshot.source,
            &snapshot.archive_sha256,
            snapshot.archive_bytes,
        )?;
    }
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(storage)?;
    let mut output = tar::Builder::new(file);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_path("workspace").map_err(storage)?;
    let Some(WorkspaceEntry::Directory { mode, uid, gid }) = resolved.get(&WorkspacePath::root())
    else {
        return Err(invalid("merged snapshot has no root directory"));
    };
    header.set_mode(*mode);
    header.set_uid((*uid).into());
    header.set_gid((*gid).into());
    header.set_size(0);
    header.set_mtime(0);
    header.set_cksum();
    output.append(&header, std::io::empty()).map_err(storage)?;
    // Hardlink targets must be emitted before aliases, regardless of path order.
    for links in [false, true] {
        for (path, entry) in &resolved {
            if path == &WorkspacePath::root() {
                continue;
            }
            if matches!(entry, WorkspaceEntry::Hardlink { .. }) != links {
                continue;
            }
            let mut header = tar::Header::new_gnu();
            header.set_mtime(0);
            header.set_size(0);
            match entry {
                WorkspaceEntry::Directory { mode, uid, gid } => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(*mode);
                    header.set_uid((*uid).into());
                    header.set_gid((*gid).into());
                    output
                        .append_data(
                            &mut header,
                            format!("workspace/{}", path.as_str()),
                            std::io::empty(),
                        )
                        .map_err(storage)?;
                }
                WorkspaceEntry::File {
                    mode,
                    uid,
                    gid,
                    sha256,
                    bytes,
                } => {
                    let source = [parent, child]
                        .into_iter()
                        .find(|snapshot| snapshot.entries.get(path) == Some(entry))
                        .ok_or_else(|| invalid("merged member has no immutable blob source"))?;
                    let blob = source.blobs.get(path).ok_or(ApplicationError::Conflict)?;
                    if blob.size != bytes.0 {
                        return Err(ApplicationError::Conflict);
                    }
                    let mut file = std::fs::File::open(&source.source).map_err(storage)?;
                    file.seek(SeekFrom::Start(blob.offset)).map_err(storage)?;
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(*mode);
                    header.set_uid((*uid).into());
                    header.set_gid((*gid).into());
                    header.set_size(bytes.0);
                    let mut data = HashingRead {
                        inner: file.take(bytes.0),
                        hash: Sha256::new(),
                        read: 0,
                    };
                    output
                        .append_data(
                            &mut header,
                            format!("workspace/{}", path.as_str()),
                            &mut data,
                        )
                        .map_err(storage)?;
                    if data.read != bytes.0 || hex::encode(data.hash.finalize()) != *sha256 {
                        return Err(ApplicationError::Conflict);
                    }
                }
                WorkspaceEntry::Symlink { uid, gid, target } => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_uid((*uid).into());
                    header.set_gid((*gid).into());
                    output
                        .append_link(&mut header, format!("workspace/{}", path.as_str()), target)
                        .map_err(storage)?;
                }
                WorkspaceEntry::Hardlink { target } => {
                    let Some(WorkspaceEntry::File { mode, uid, gid, .. }) = resolved.get(target)
                    else {
                        return Err(invalid("hardlink target disappeared from the merged tree"));
                    };
                    header.set_entry_type(tar::EntryType::Link);
                    header.set_mode(*mode);
                    header.set_uid((*uid).into());
                    header.set_gid((*gid).into());
                    output
                        .append_link(
                            &mut header,
                            format!("workspace/{}", path.as_str()),
                            format!("workspace/{}", target.as_str()),
                        )
                        .map_err(storage)?;
                }
            }
        }
    }
    output.finish().map_err(storage)?;
    let file = output.into_inner().map_err(storage)?;
    file.sync_all().map_err(storage)?;
    let size = file.metadata().map_err(storage)?.len();
    if size > MAX_ARCHIVE_BYTES {
        return Err(invalid("merged archive exceeds its bound"));
    }
    let mut file = std::fs::File::open(destination).map_err(storage)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(storage)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok((hex::encode(hash.finalize()), size))
}
struct HashingRead<R> {
    inner: R,
    hash: Sha256,
    read: u64,
}
impl<R: Read> Read for HashingRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hash.update(&buffer[..count]);
        self.read += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    enum Member<'a> {
        File(&'a str, &'a [u8]),
        Link(&'a str, &'a str, bool),
    }
    fn archive(path: &Path, members: &[Member<'_>]) -> SnapshotTree {
        let mut archive = tar::Builder::new(std::fs::File::create(path).unwrap());
        let mut root = tar::Header::new_gnu();
        root.set_uid(21);
        root.set_gid(34);
        root.set_mode(0o750);
        root.set_size(0);
        root.set_entry_type(tar::EntryType::Directory);
        archive
            .append_data(&mut root, "workspace", std::io::empty())
            .unwrap();
        for member in members {
            let mut header = tar::Header::new_gnu();
            header.set_uid(21);
            header.set_gid(34);
            header.set_mtime(123);
            header.set_mode(0o640);
            match member {
                Member::File(path, data) => {
                    header.set_size(data.len() as u64);
                    header.set_entry_type(tar::EntryType::Regular);
                    archive
                        .append_data(&mut header, format!("workspace/{path}"), *data)
                        .unwrap();
                }
                Member::Link(path, target, hard) => {
                    header.set_size(0);
                    header.set_entry_type(if *hard {
                        tar::EntryType::Link
                    } else {
                        tar::EntryType::Symlink
                    });
                    archive
                        .append_link(
                            &mut header,
                            format!("workspace/{path}"),
                            if *hard {
                                format!("workspace/{target}")
                            } else {
                                target.to_string()
                            },
                        )
                        .unwrap();
                }
            }
        }
        archive.finish().unwrap();
        drop(archive);
        let bytes = std::fs::read(path).unwrap();
        SnapshotTree::read(
            path,
            &hex::encode(Sha256::digest(&bytes)),
            bytes.len() as u64,
        )
        .unwrap()
    }
    #[test]
    fn merged_archives_preserve_binary_data_links_and_metadata_without_host_extraction() {
        let root = tempfile::tempdir().unwrap();
        let base = archive(
            &root.path().join("base.tar"),
            &[Member::File("one", b"base"), Member::File("two", b"base")],
        );
        let parent = archive(
            &root.path().join("parent.tar"),
            &[Member::File("one", b"parent"), Member::File("two", b"base")],
        );
        let child = archive(
            &root.path().join("child.tar"),
            &[
                Member::File("one", b"base"),
                Member::File("two", b"\0\xffbinary"),
                Member::Link("z-alias", "two", true),
                Member::Link("link", "two", false),
                Member::File("nested/new\nfile", b"new"),
            ],
        );
        let reviewed = zuno_application::workspace_merge::plan(
            base.entries(),
            parent.entries(),
            child.entries(),
        )
        .unwrap();
        let output = root.path().join("result.tar");
        let (sha, bytes) =
            write_merged_archive(&reviewed, &base, &parent, &child, &output).unwrap();
        let merged = SnapshotTree::read(&output, &sha, bytes).unwrap();
        assert_eq!(
            merged.entries(),
            &zuno_application::workspace_merge::resolved_tree(
                &reviewed,
                base.entries(),
                parent.entries(),
                child.entries()
            )
            .unwrap()
        );
        assert!(!root.path().join("workspace").exists());
        assert_eq!(
            merged.entries().get(&WorkspacePath::root()),
            Some(&WorkspaceEntry::Directory {
                uid: 21,
                gid: 34,
                mode: 0o750
            })
        );
        assert!(matches!(
            merged.entries().get(&WorkspacePath::new("two").unwrap()),
            Some(WorkspaceEntry::File {
                uid: 21,
                gid: 34,
                mode: 0o640,
                ..
            })
        ));
    }
    #[test]
    fn duplicate_members_and_archive_tampering_fail_before_a_workspace_is_modified() {
        let root = tempfile::tempdir().unwrap();
        let base = archive(
            &root.path().join("base.tar"),
            &[Member::File("one", b"base")],
        );
        let parent = archive(
            &root.path().join("parent.tar"),
            &[Member::File("one", b"base")],
        );
        let child = archive(
            &root.path().join("child.tar"),
            &[Member::File("one", b"child")],
        );
        let reviewed = zuno_application::workspace_merge::plan(
            base.entries(),
            parent.entries(),
            child.entries(),
        )
        .unwrap();
        std::fs::write(&child.source, b"tampered").unwrap();
        let output = root.path().join("result.tar");
        assert!(write_merged_archive(&reviewed, &base, &parent, &child, &output).is_err());
        assert!(!output.exists());
        let path = root.path().join("duplicates.tar");
        let mut archive = tar::Builder::new(std::fs::File::create(&path).unwrap());
        for _ in 0..2 {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o644);
            archive
                .append_data(&mut header, "workspace/same", std::io::empty())
                .unwrap();
        }
        archive.finish().unwrap();
        drop(archive);
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            SnapshotTree::read(
                &path,
                &hex::encode(Sha256::digest(&bytes)),
                bytes.len() as u64
            )
            .is_err()
        );
    }
}
