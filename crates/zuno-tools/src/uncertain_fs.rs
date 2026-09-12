//! Bounded, handle-anchored observations on Unix and Windows.
//!
//! Directory segments and leaves are opened without following links. Reads use
//! retained Dir capabilities, with identities checked from open handles before
//! and after reading. Ambient access only validates the approved workspace root.

use std::fs::File;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsSyncExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zuno_tool::InterruptHandle;

use super::FileInspectionError;

pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_TARGETS: usize = 64;
pub const READ_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceIdentity {
    pub path: PathBuf,
    /// Unix device or Windows volume serial number.
    pub device: Option<u64>,
    /// Unix inode or Windows file index, obtained from an open handle.
    pub inode: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileMetadata {
    pub length: u64,
    pub readonly: bool,
    pub modified_ns: Option<u64>,
    pub created_ns: Option<u64>,
    pub device: u64,
    pub inode: u64,
    pub change_seconds: Option<i64>,
    pub change_nanoseconds: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileObservation {
    pub target: PathBuf,
    pub exists: bool,
    pub sha256: Option<String>,
    pub metadata: Option<FileMetadata>,
    pub observed_at_ms: i64,
}

pub(super) struct ReadControl {
    pub cancelled: Arc<AtomicBool>,
    pub interrupt: Arc<dyn InterruptHandle>,
    pub deadline: Instant,
}

impl ReadControl {
    pub fn check(&self) -> Result<(), FileInspectionError> {
        if self.cancelled.load(Ordering::Acquire) || self.interrupt.is_set() {
            return Err(FileInspectionError::Interrupted);
        }
        if Instant::now() >= self.deadline {
            return Err(FileInspectionError::Timeout);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct Workspace {
    pub identity: WorkspaceIdentity,
    root: Dir,
}

impl Workspace {
    pub fn open(path: &Path) -> Result<Self, FileInspectionError> {
        let canonical = path.canonicalize()?;
        let root = Dir::open_ambient_dir(&canonical, ambient_authority())?;
        let (device, inode) = directory_identity(&root)?;
        let result = Self {
            identity: WorkspaceIdentity {
                path: canonical,
                device: Some(device),
                inode: Some(inode),
            },
            root,
        };
        result.revalidate()?;
        Ok(result)
    }

    pub fn supported(&self) -> bool {
        cfg!(any(unix, windows))
    }

    pub fn revalidate(&self) -> Result<(), FileInspectionError> {
        let current = std::fs::symlink_metadata(&self.identity.path)?;
        if !current.is_dir() || is_link_or_reparse(&current) {
            return Err(changed("workspace is no longer a plain directory"));
        }
        let current = Dir::open_ambient_dir(&self.identity.path, ambient_authority())?;
        let identity = directory_identity(&current)?;
        if (Some(identity.0), Some(identity.1)) != (self.identity.device, self.identity.inode)
            || identity != directory_identity(&self.root)?
        {
            return Err(changed("workspace identity changed"));
        }
        Ok(())
    }

    pub fn relative(&self, target: &Path) -> Result<PathBuf, FileInspectionError> {
        let relative = target
            .strip_prefix(&self.identity.path)
            .map_err(|_| unsupported("target is outside the original workspace"))?;
        let components = relative.components().collect::<Vec<_>>();
        if components.is_empty()
            || components.len() > 128
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(unsupported("target is not a bounded canonical file path"));
        }
        Ok(relative.to_owned())
    }

    fn walk(&self, relative: &Path) -> Result<Dir, FileInspectionError> {
        let mut directory = self.root.try_clone()?;
        for component in relative.components() {
            directory = directory.open_dir_nofollow(component.as_os_str())?;
        }
        Ok(directory)
    }

    fn validate_directory(
        &self,
        directory: &Dir,
        relative: &Path,
    ) -> Result<(), FileInspectionError> {
        self.revalidate()?;
        let current = self.walk(relative)?;
        if directory_identity(&current)? != directory_identity(directory)? {
            return Err(changed("target directory moved during inspection"));
        }
        Ok(())
    }

    pub fn observe(
        &self,
        target: &Path,
        control: &ReadControl,
        remaining: &mut u64,
    ) -> Result<FileObservation, FileInspectionError> {
        control.check()?;
        self.revalidate()?;
        let relative = self.relative(target)?;
        let components = relative.components().collect::<Vec<_>>();
        let mut directory = self.root.try_clone()?;
        let mut parent = PathBuf::new();
        for component in &components[..components.len() - 1] {
            control.check()?;
            match directory.open_dir_nofollow(component.as_os_str()) {
                Ok(child) => {
                    directory = child;
                    parent.push(component.as_os_str());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.validate_directory(&directory, &parent)?;
                    return Ok(missing(target));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.validate_directory(&directory, &parent)?;
        let leaf = components.last().expect("validated path").as_os_str();
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let mut file = match directory.open_with(leaf, &options) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.validate_directory(&directory, &parent)?;
                return Ok(missing(target));
            }
            Err(error) => return Err(error.into()),
        };
        let before = capture_metadata(&file)?;
        if before.length > MAX_FILE_BYTES || before.length > *remaining {
            return Err(FileInspectionError::Bounds(
                "file inspection byte limit exceeded".to_owned(),
            ));
        }
        let mut digest = Sha256::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            control.check()?;
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            length += count as u64;
            if length > MAX_FILE_BYTES || length > *remaining {
                return Err(FileInspectionError::Bounds(
                    "file grew past inspection byte limit".to_owned(),
                ));
            }
            digest.update(&buffer[..count]);
        }
        control.check()?;
        self.validate_directory(&directory, &parent)?;
        let after = capture_metadata(&file)?;
        // Reopen through the same capability to detect replacement of the leaf,
        // not just mutation of the old open file. Links remain refused.
        let current = directory.open_with(leaf, &options)?.into_std();
        if before != after || after != capture_metadata(&current)? || length != after.length {
            return Err(changed("file changed while it was inspected"));
        }
        *remaining -= length;
        Ok(FileObservation {
            target: target.to_owned(),
            exists: true,
            sha256: Some(hex::encode(digest.finalize())),
            metadata: Some(after),
            observed_at_ms: zuno_db::message::now_millis(),
        })
    }
}

fn directory_identity(directory: &Dir) -> Result<(u64, u64), FileInspectionError> {
    identity(&directory.try_clone()?.into_std_file())
}

#[cfg(unix)]
fn identity(file: &File) -> Result<(u64, u64), FileInspectionError> {
    use std::os::unix::fs::MetadataExt as _;
    let value = file.metadata()?;
    Ok((value.dev(), value.ino()))
}

#[cfg(windows)]
fn identity(file: &File) -> Result<(u64, u64), FileInspectionError> {
    let value = winapi_util::file::information(file)?;
    Ok((value.volume_serial_number(), value.file_index()))
}

#[cfg(not(any(unix, windows)))]
fn identity(_: &File) -> Result<(u64, u64), FileInspectionError> {
    Err(unsupported(
        "stable file identity is unavailable on this platform",
    ))
}

fn capture_metadata(file: &File) -> Result<FileMetadata, FileInspectionError> {
    let value = file.metadata()?;
    if !value.is_file() || is_link_or_reparse(&value) {
        return Err(unsupported(
            "inspection targets must be regular files or absent paths",
        ));
    }
    let (device, inode) = identity(file)?;
    let timestamp = |value: std::io::Result<std::time::SystemTime>| {
        value
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
    };
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt as _;
        (Some(value.ctime()), Some(value.ctime_nsec()))
    };
    #[cfg(not(unix))]
    let changed = (None, None);
    Ok(FileMetadata {
        length: value.len(),
        readonly: value.permissions().readonly(),
        modified_ns: timestamp(value.modified()),
        created_ns: timestamp(value.created()),
        device,
        inode,
        change_seconds: changed.0,
        change_nanoseconds: changed.1,
    })
}

fn is_link_or_reparse(value: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        value.file_type().is_symlink() || value.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    value.file_type().is_symlink()
}

fn missing(target: &Path) -> FileObservation {
    FileObservation {
        target: target.to_owned(),
        exists: false,
        sha256: None,
        metadata: None,
        observed_at_ms: zuno_db::message::now_millis(),
    }
}
fn changed(message: &str) -> FileInspectionError {
    FileInspectionError::Conflict(message.to_owned())
}
fn unsupported(message: &str) -> FileInspectionError {
    FileInspectionError::Unsupported {
        part_id: None,
        reason: message.to_owned(),
    }
}
