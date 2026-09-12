use crate::storage;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path};
use zuno_application::ApplicationError;

pub(crate) fn verify(
    path: &Path,
    expected_sha: &str,
    expected_bytes: u64,
) -> Result<(), ApplicationError> {
    let mut file = std::fs::File::open(path).map_err(storage)?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(storage)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(ApplicationError::Conflict)?;
        if bytes > expected_bytes {
            return Err(ApplicationError::Conflict);
        }
        digest.update(&buffer[..count]);
    }
    if bytes != expected_bytes || hex::encode(digest.finalize()) != expected_sha {
        return Err(ApplicationError::Conflict);
    }
    let file = std::fs::File::open(path).map_err(storage)?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries().map_err(storage)? {
        let entry = entry.map_err(storage)?;
        let path = entry.path().map_err(storage)?;
        let components = relative(&path)?;
        if components.first().map(String::as_str) != Some("workspace") {
            return Err(ApplicationError::Conflict);
        }
        let kind = entry.header().entry_type();
        if entry.header().mode().map_err(storage)? & 0o6000 != 0 {
            return Err(ApplicationError::Forbidden);
        }
        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(storage)?
                .ok_or(ApplicationError::Conflict)?;
            if kind.is_hard_link() {
                let target = relative(&target)?;
                if target.first().map(String::as_str) != Some("workspace") {
                    return Err(ApplicationError::Forbidden);
                }
            } else {
                let mut depth = components.len().saturating_sub(1);
                for component in target.components() {
                    match component {
                        Component::Normal(_) => depth += 1,
                        Component::CurDir => {}
                        Component::ParentDir if depth > 1 => depth -= 1,
                        _ => return Err(ApplicationError::Forbidden),
                    }
                }
            }
        } else if !kind.is_file() && !kind.is_dir() {
            return Err(ApplicationError::Forbidden);
        }
    }
    Ok(())
}

fn relative(path: &Path) -> Result<Vec<String>, ApplicationError> {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(|component| match component {
            Component::Normal(name) => name
                .to_str()
                .map(str::to_owned)
                .ok_or(ApplicationError::Conflict),
            _ => Err(ApplicationError::Forbidden),
        })
        .collect()
}

/// Docker's extractor deliberately skips metadata for a "." directory entry.
/// The private restore helper applies only these bounded numeric root facts
/// after the archive contents have landed in an unpublished volume.
pub(crate) fn root_metadata(source: &Path) -> Result<Option<(u32, u32, u32)>, ApplicationError> {
    let mut archive = tar::Archive::new(std::fs::File::open(source).map_err(storage)?);
    let mut metadata = None;
    for entry in archive.entries().map_err(storage)? {
        let entry = entry.map_err(storage)?;
        if relative(&entry.path().map_err(storage)?)? == ["workspace"] {
            if !entry.header().entry_type().is_dir() || metadata.is_some() {
                return Err(ApplicationError::Conflict);
            }
            let mode = entry.header().mode().map_err(storage)?;
            if mode & !0o1777 != 0 {
                return Err(ApplicationError::Forbidden);
            }
            metadata = Some((
                mode,
                u32::try_from(entry.header().uid().map_err(storage)?).map_err(storage)?,
                u32::try_from(entry.header().gid().map_err(storage)?).map_err(storage)?,
            ));
        }
    }
    Ok(metadata)
}

/// Docker's archive API checks writability at the destination mount. Strip the
/// verified top-level workspace directory while streaming, without extracting
/// any member into the gateway host filesystem.
pub(crate) fn for_restore(source: &Path, destination: &Path) -> Result<u64, ApplicationError> {
    let input = std::fs::File::open(source).map_err(storage)?;
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(storage)?;
    let mut reader = tar::Archive::new(input);
    let mut writer = tar::Builder::new(output);
    for entry in reader.entries().map_err(storage)? {
        let mut entry = entry.map_err(storage)?;
        let path = entry.path().map_err(storage)?.into_owned();
        let relative = path
            .strip_prefix("workspace")
            .map_err(|_| ApplicationError::Forbidden)?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let mut header = entry.header().clone();
        header.set_path(relative).map_err(storage)?;
        if header.entry_type().is_hard_link() {
            let target = entry
                .link_name()
                .map_err(storage)?
                .ok_or(ApplicationError::Conflict)?
                .into_owned();
            header
                .set_link_name(
                    target
                        .strip_prefix("workspace")
                        .map_err(|_| ApplicationError::Forbidden)?,
                )
                .map_err(storage)?;
        }
        header.set_cksum();
        writer.append(&header, &mut entry).map_err(storage)?;
    }
    writer.finish().map_err(storage)?;
    let output = writer.into_inner().map_err(storage)?;
    output.sync_all().map_err(storage)?;
    Ok(output.metadata().map_err(storage)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(path: &Path, link: Option<&str>) -> (String, u64) {
        let file = std::fs::File::create(path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        if let Some(target) = link {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name(target).unwrap();
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(4);
        }
        header.set_path("workspace/file").unwrap();
        header.set_cksum();
        builder
            .append(
                &header,
                if link.is_some() {
                    &b""[..]
                } else {
                    &b"data"[..]
                },
            )
            .unwrap();
        builder.finish().unwrap();
        drop(builder);
        let bytes = std::fs::read(path).unwrap();
        (hex::encode(Sha256::digest(&bytes)), bytes.len() as u64)
    }

    #[test]
    fn snapshot_identity_and_workspace_link_boundary_are_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tar");
        let (sha, size) = archive(&path, None);
        verify(&path, &sha, size).unwrap();
        assert!(verify(&path, &"0".repeat(64), size).is_err());
        assert!(verify(&path, &sha, size - 1).is_err());
        let restored = dir.path().join("restore.tar");
        for_restore(&path, &restored).unwrap();
        let mut reader = tar::Archive::new(std::fs::File::open(restored).unwrap());
        assert_eq!(
            reader
                .entries()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .unwrap()
                .as_ref(),
            Path::new("file")
        );
        for target in ["/etc/passwd", "../outside", "../../outside"] {
            let path = dir.path().join("bad.tar");
            let (sha, size) = archive(&path, Some(target));
            assert!(verify(&path, &sha, size).is_err(), "{target}");
        }
    }
}
