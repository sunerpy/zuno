use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Package, PluginRuntime, STATIC_DIRECTORY, STATIC_MANIFEST};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Whether installation expects a new or existing package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    Add,
    Update,
}

/// Result of one successful local package installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub id: String,
    pub destination: PathBuf,
}

/// Install one validated local package below a Zuno configuration root.
pub fn install_local(
    source: &Path,
    config_root: &Path,
    mode: InstallMode,
) -> Result<InstalledPackage, InstallError> {
    let source_root = source_root(source)?;
    let package = load_source_manifest(&source_root)?;
    validate_runtime_artifacts(&package, &source_root)?;
    let extensions = config_root.join(STATIC_DIRECTORY);
    std::fs::create_dir_all(&extensions).map_err(|source| InstallError::CreateDirectory {
        path: extensions.clone(),
        source,
    })?;
    let destination = extensions.join(&package.id);
    match (mode, destination.exists()) {
        (InstallMode::Add, true) => {
            return Err(InstallError::AlreadyInstalled(package.id));
        }
        (InstallMode::Update, false) => {
            return Err(InstallError::NotInstalled(package.id));
        }
        (InstallMode::Add, false) | (InstallMode::Update, true) => {}
    }

    let staging = unique_sibling(&extensions, &package.id, "installing");
    if let Err(install) = copy_tree(&source_root, &staging) {
        return match std::fs::remove_dir_all(&staging) {
            Ok(()) => Err(install),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(install),
            Err(cleanup) => Err(InstallError::StagingCleanup {
                path: staging,
                install: Box::new(install),
                cleanup,
            }),
        };
    }
    if mode == InstallMode::Add {
        if let Err(source) = std::fs::rename(&staging, &destination) {
            let _ignored = std::fs::remove_dir_all(&staging);
            return Err(InstallError::Rename {
                from: staging,
                to: destination,
                source,
            });
        }
    } else {
        replace_directory(&destination, &staging, &extensions, &package.id)?;
    }
    Ok(InstalledPackage {
        id: package.id,
        destination,
    })
}

/// Remove one installed package from a Zuno configuration root.
pub fn remove_installed(id: &str, config_root: &Path) -> Result<PathBuf, InstallError> {
    Package::validate_id(id).map_err(InstallError::Manifest)?;
    let extensions = config_root.join(STATIC_DIRECTORY);
    let destination = extensions.join(id);
    if !destination.is_dir() {
        return Err(InstallError::NotInstalled(id.to_owned()));
    }
    let removed = unique_sibling(&extensions, id, "removing");
    std::fs::rename(&destination, &removed).map_err(|source| InstallError::Rename {
        from: destination.clone(),
        to: removed.clone(),
        source,
    })?;
    if let Err(source) = std::fs::remove_dir_all(&removed) {
        return Err(InstallError::Remove {
            path: removed,
            source,
        });
    }
    Ok(destination)
}

fn source_root(source: &Path) -> Result<PathBuf, InstallError> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|source_error| InstallError::Read {
            path: source.to_path_buf(),
            source: source_error,
        })?;
    if metadata.file_type().is_symlink() {
        return Err(InstallError::Symlink(source.to_path_buf()));
    }
    if metadata.is_dir() {
        return Ok(source.to_path_buf());
    }
    if metadata.is_file()
        && source
            .file_name()
            .is_some_and(|name| name == STATIC_MANIFEST)
    {
        return source
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| InstallError::InvalidSource(source.to_path_buf()));
    }
    Err(InstallError::InvalidSource(source.to_path_buf()))
}

fn load_source_manifest(root: &Path) -> Result<Package, InstallError> {
    let manifest = root.join(STATIC_MANIFEST);
    let text = std::fs::read_to_string(&manifest).map_err(|source| InstallError::Read {
        path: manifest.clone(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| InstallError::Decode {
        path: manifest,
        source,
    })
}

fn validate_runtime_artifacts(package: &Package, root: &Path) -> Result<(), InstallError> {
    if let Some(PluginRuntime::Wasi { artifact, .. }) = &package.runtime {
        let artifact = root.join(artifact);
        let metadata =
            std::fs::symlink_metadata(&artifact).map_err(|source| InstallError::Read {
                path: artifact.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(InstallError::Symlink(artifact));
        }
        if !metadata.is_file() {
            return Err(InstallError::MissingArtifact(artifact));
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), InstallError> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|source_error| InstallError::Read {
            path: source.to_path_buf(),
            source: source_error,
        })?;
    if metadata.file_type().is_symlink() {
        return Err(InstallError::Symlink(source.to_path_buf()));
    }
    if metadata.is_dir() {
        std::fs::create_dir(destination).map_err(|source_error| InstallError::CreateDirectory {
            path: destination.to_path_buf(),
            source: source_error,
        })?;
        let mut entries = std::fs::read_dir(source)
            .map_err(|source_error| InstallError::ReadDirectory {
                path: source.to_path_buf(),
                source: source_error,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source_error| InstallError::ReadDirectory {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        std::fs::set_permissions(destination, metadata.permissions()).map_err(|source_error| {
            InstallError::Permissions {
                path: destination.to_path_buf(),
                source: source_error,
            }
        })?;
        return Ok(());
    }
    if metadata.is_file() {
        std::fs::copy(source, destination).map_err(|source_error| InstallError::Copy {
            from: source.to_path_buf(),
            to: destination.to_path_buf(),
            source: source_error,
        })?;
        return Ok(());
    }
    Err(InstallError::UnsupportedEntry(source.to_path_buf()))
}

fn replace_directory(
    destination: &Path,
    staging: &Path,
    parent: &Path,
    id: &str,
) -> Result<(), InstallError> {
    let backup = unique_sibling(parent, id, "backup");
    std::fs::rename(destination, &backup).map_err(|source| InstallError::Rename {
        from: destination.to_path_buf(),
        to: backup.clone(),
        source,
    })?;
    if let Err(source) = std::fs::rename(staging, destination) {
        let rollback = std::fs::rename(&backup, destination);
        let _ignored = std::fs::remove_dir_all(staging);
        return match rollback {
            Ok(()) => Err(InstallError::Rename {
                from: staging.to_path_buf(),
                to: destination.to_path_buf(),
                source,
            }),
            Err(rollback) => Err(InstallError::Rollback {
                package: id.to_owned(),
                staging: staging.to_path_buf(),
                backup,
                install: source,
                rollback,
            }),
        };
    }
    std::fs::remove_dir_all(&backup).map_err(|source| InstallError::Remove {
        path: backup,
        source,
    })
}

fn unique_sibling(parent: &Path, id: &str, phase: &str) -> PathBuf {
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{id}.{phase}.{}.{}", std::process::id(), sequence))
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("plugin source {} must be a directory or extension.json", .0.display())]
    InvalidSource(PathBuf),
    #[error("plugin package `{0}` is already installed")]
    AlreadyInstalled(String),
    #[error("plugin package `{0}` is not installed")]
    NotInstalled(String),
    #[error("plugin package requires missing runtime artifact {}", .0.display())]
    MissingArtifact(PathBuf),
    #[error("plugin packages cannot contain symbolic link {}", .0.display())]
    Symlink(PathBuf),
    #[error("plugin packages cannot contain special filesystem entry {}", .0.display())]
    UnsupportedEntry(PathBuf),
    #[error("failed to read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read directory {}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to decode plugin manifest {}", path.display())]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to create directory {}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to copy {} to {}", from.display(), to.display())]
    Copy {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to preserve permissions on {}", path.display())]
    Permissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to rename {} to {}", from.display(), to.display())]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove {}", path.display())]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "plugin staging copy failed ({install}) and cleanup of {} also failed: {cleanup}",
        path.display()
    )]
    StagingCleanup {
        path: PathBuf,
        install: Box<InstallError>,
        cleanup: std::io::Error,
    },
    #[error(
        "plugin `{package}` update and rollback both failed; staging remains at {} and backup at {}",
        staging.display(),
        backup.display()
    )]
    Rollback {
        package: String,
        staging: PathBuf,
        backup: PathBuf,
        #[source]
        install: std::io::Error,
        rollback: std::io::Error,
    },
    #[error(transparent)]
    Manifest(#[from] crate::manifest::ManifestError),
}
