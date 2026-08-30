use std::path::{Path, PathBuf};

use crate::Package;

/// Directory below every Zuno configuration root that contains static packages.
pub const STATIC_DIRECTORY: &str = "extensions";
/// Manifest filename inside one static package directory.
pub const STATIC_MANIFEST: &str = "extension.json";

/// A validated static package and the manifest that supplied it.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticPackage {
    package: Package,
    manifest: PathBuf,
}

impl StaticPackage {
    /// Attach filesystem provenance and require `<directory>/<id>/extension.json`.
    pub fn new(package: Package, manifest: impl Into<PathBuf>) -> Result<Self, StaticLoadError> {
        let manifest = manifest.into();
        let directory_id = manifest
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if directory_id != package.id {
            return Err(StaticLoadError::DirectoryId {
                path: manifest,
                directory: directory_id,
                package: package.id,
            });
        }
        Ok(Self { package, manifest })
    }

    #[must_use]
    pub fn package(&self) -> &Package {
        &self.package
    }

    #[must_use]
    pub fn manifest(&self) -> &Path {
        &self.manifest
    }
}

/// Discover static extension packages from all active configuration roots.
///
/// Every package is active at composition time. A malformed package is fatal so
/// startup never advertises a partial extension set.
pub fn discover_static(
    directory: &Path,
    worktree: Option<&Path>,
    env: &zuno_paths::Env,
) -> Result<Vec<StaticPackage>, StaticLoadError> {
    let layout = zuno_paths::Layout::resolve(env);
    let mut packages = Vec::new();
    for config_root in layout.config_directories(directory, worktree) {
        let root = config_root.join(STATIC_DIRECTORY);
        if !root.is_dir() {
            continue;
        }
        let mut package_dirs = std::fs::read_dir(&root)
            .map_err(|source| StaticLoadError::ReadDirectory {
                path: root.clone(),
                source,
            })?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|source| StaticLoadError::ReadDirectory {
                        path: root.clone(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        package_dirs.sort();
        for package_dir in package_dirs {
            if !package_dir.is_dir() {
                continue;
            }
            let manifest = package_dir.join(STATIC_MANIFEST);
            if !manifest.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest).map_err(|source| {
                StaticLoadError::ReadManifest {
                    path: manifest.clone(),
                    source,
                }
            })?;
            let package = serde_json::from_str::<Package>(&text).map_err(|source| {
                StaticLoadError::Decode {
                    path: manifest.clone(),
                    source,
                }
            })?;
            packages.push(StaticPackage::new(package, manifest)?);
        }
    }
    Ok(packages)
}

#[derive(Debug, thiserror::Error)]
pub enum StaticLoadError {
    #[error("failed to read extension directory {}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read extension manifest {}", path.display())]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to decode extension manifest {}", path.display())]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "static extension directory `{directory}` does not match package id `{package}` in {}",
        path.display()
    )]
    DirectoryId {
        path: PathBuf,
        directory: String,
        package: String,
    },
}
