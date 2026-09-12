use crate::storage;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
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
    bounded_headers(path, expected_bytes)?;
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

/// Bound extension-header allocation before the tar decoder processes PAX or
/// GNU long-name payloads. File data is skipped by offset, never buffered.
fn bounded_headers(path: &Path, bytes: u64) -> Result<(), ApplicationError> {
    let mut file = std::fs::File::open(path).map_err(storage)?;
    let mut position = 0u64;
    let mut count = 0usize;
    while position < bytes {
        if bytes - position < 512 {
            return Err(ApplicationError::Conflict);
        }
        file.seek(SeekFrom::Start(position)).map_err(storage)?;
        let mut block = [0u8; 512];
        file.read_exact(&mut block).map_err(storage)?;
        if block.iter().all(|byte| *byte == 0) {
            return Ok(());
        }
        count += 1;
        if count > 100_000 {
            return Err(ApplicationError::Invalid(
                "workspace archive has too many headers".to_owned(),
            ));
        }
        let header = tar::Header::from_byte_slice(&block);
        let kind = header.entry_type();
        let size = header.size().map_err(storage)?;
        if kind.is_gnu_sparse() {
            return Err(ApplicationError::Forbidden);
        }
        if (kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions())
            && size > 65536
        {
            return Err(ApplicationError::Invalid(
                "workspace archive metadata exceeds its bound".to_owned(),
            ));
        }
        let padded = size
            .checked_add(511)
            .map(|value| (value / 512) * 512)
            .ok_or(ApplicationError::Conflict)?;
        position = position
            .checked_add(512)
            .and_then(|position| position.checked_add(padded))
            .filter(|position| *position <= bytes)
            .ok_or(ApplicationError::Conflict)?;
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
    #[test]
    fn untrusted_extension_headers_are_bounded_before_tar_allocation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oversized.tar");
        let mut header = tar::Header::new_gnu();
        header.set_path("pax").unwrap();
        header.set_entry_type(tar::EntryType::XHeader);
        header.set_size(65537);
        header.set_mode(0o644);
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.resize(512 + 66048, 0);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            verify(
                &path,
                &hex::encode(Sha256::digest(&bytes)),
                bytes.len() as u64
            )
            .is_err()
        );
    }
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
