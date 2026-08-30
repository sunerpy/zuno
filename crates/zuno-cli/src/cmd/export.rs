//! Portable Zuno user-environment export and import.
//!
//! A bundle contains only logical roots and forward-slash relative paths. It
//! never records the source machine's absolute paths, so the same archive can
//! be restored into the layout resolved on Linux, macOS, or Windows. Session
//! databases, logs, caches, and other runtime state are deliberately outside
//! the format. Credentials are included only after an explicit CLI opt-in.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::StartupEnvironment;
use crate::command::{ExportArgs, ImportArgs};

const BUNDLE_FORMAT: &str = "zuno-portable-bundle";
const BUNDLE_SCHEMA_VERSION: u32 = 1;
const MANIFEST_PATH: &str = "bundle.json";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRY_COUNT: usize = 50_000;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum BundleRoot {
    Config,
    HomeZuno,
    ProviderCredentials,
    McpCredentials,
}

impl BundleRoot {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::HomeZuno => "home-zuno",
            Self::ProviderCredentials => "provider-credentials",
            Self::McpCredentials => "mcp-credentials",
        }
    }

    const fn is_directory(self) -> bool {
        matches!(self, Self::Config | Self::HomeZuno)
    }

    fn target(self, layout: &zuno_paths::Layout) -> PathBuf {
        match self {
            Self::Config => layout.config().to_path_buf(),
            Self::HomeZuno => layout.home().join(".zuno"),
            Self::ProviderCredentials => layout.auth_file(),
            Self::McpCredentials => layout.mcp_auth_file(),
        }
    }

    const fn credential_path(self) -> Option<&'static str> {
        match self {
            Self::ProviderCredentials => Some(zuno_paths::files::AUTH_FILE),
            Self::McpCredentials => Some(zuno_paths::files::MCP_AUTH_FILE),
            Self::Config | Self::HomeZuno => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleEntry {
    root: BundleRoot,
    path: String,
    sha256: String,
    size: u64,
    unix_mode: u32,
}

impl BundleEntry {
    fn archive_path(&self) -> String {
        format!("payload/{}/{}", self.root.as_str(), self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    format: String,
    schema_version: u32,
    product: String,
    zuno_version: String,
    created_at: String,
    source_platform: String,
    includes_credentials: bool,
    roots: Vec<BundleRoot>,
    excluded: Vec<String>,
    entries: Vec<BundleEntry>,
}

#[derive(Debug)]
struct SourceEntry {
    metadata: BundleEntry,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct DecodedEntry {
    metadata: BundleEntry,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct DecodedBundle {
    manifest: BundleManifest,
    entries: Vec<DecodedEntry>,
}

#[derive(Debug)]
struct InstallOperation {
    root: BundleRoot,
    target: PathBuf,
    staging: PathBuf,
}

#[derive(Debug)]
struct CommittedOperation {
    root: BundleRoot,
    target: PathBuf,
    backup: Option<PathBuf>,
}

/// Export all Zuno-owned global configuration and optional credentials.
pub(super) fn export(args: &ExportArgs, environment: &StartupEnvironment) -> Result<(), String> {
    let layout = zuno_paths::Layout::resolve(environment.resolved());
    let current = std::env::current_dir().map_err(|error| error.to_string())?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| current.join(default_bundle_name()));
    let output = lexical_absolute(&output, &current);

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create bundle directory {}: {error}",
                parent.display()
            )
        })?;
    }
    for root in [layout.config(), &layout.home().join(".zuno")] {
        let root = lexical_absolute(root, &current);
        if output.starts_with(&root) {
            return Err(format!(
                "bundle output {} cannot be inside exported root {}",
                output.display(),
                root.display()
            ));
        }
    }
    if output.exists() && !args.force {
        return Err(format!(
            "bundle already exists: {}; pass --force to replace it",
            output.display()
        ));
    }

    let mut entries = Vec::new();
    collect_directory(BundleRoot::Config, layout.config(), &mut entries)?;
    collect_directory(
        BundleRoot::HomeZuno,
        &layout.home().join(".zuno"),
        &mut entries,
    )?;
    if args.include_credentials {
        collect_file(
            BundleRoot::ProviderCredentials,
            &layout.auth_file(),
            zuno_paths::files::AUTH_FILE,
            &mut entries,
        )?;
        collect_file(
            BundleRoot::McpCredentials,
            &layout.mcp_auth_file(),
            zuno_paths::files::MCP_AUTH_FILE,
            &mut entries,
        )?;
    }
    entries.sort_by(|left, right| {
        (left.metadata.root, left.metadata.path.as_str())
            .cmp(&(right.metadata.root, right.metadata.path.as_str()))
    });
    validate_source_entries(&entries)?;

    let roots = entries
        .iter()
        .map(|entry| entry.metadata.root)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let includes_credentials = roots.iter().any(|root| root.credential_path().is_some());
    let manifest = BundleManifest {
        format: BUNDLE_FORMAT.to_owned(),
        schema_version: BUNDLE_SCHEMA_VERSION,
        product: "zuno".to_owned(),
        zuno_version: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|error| format!("failed to format bundle timestamp: {error}"))?,
        source_platform: std::env::consts::OS.to_owned(),
        includes_credentials,
        roots,
        excluded: vec![
            "session databases and transcripts".to_owned(),
            "logs, caches, snapshots, tool output, and temporary files".to_owned(),
            "external shared Skill roots outside Zuno configuration".to_owned(),
        ],
        entries: entries.iter().map(|entry| entry.metadata.clone()).collect(),
    };

    write_bundle(&output, args.force, &manifest, &entries)?;
    println!("Exported Zuno bundle: {}", output.display());
    if includes_credentials {
        eprintln!(
            "warning: bundle contains unencrypted credentials; protect it during transfer and delete it when no longer needed"
        );
    }
    Ok(())
}

/// Validate and install one portable Zuno bundle into this machine's layout.
pub(super) fn import(args: &ImportArgs, environment: &StartupEnvironment) -> Result<(), String> {
    let file_text = args.file.to_string_lossy();
    if file_text.starts_with("http://") || file_text.starts_with("https://") {
        return Err("portable imports require a local `.zuno-bundle` file".to_owned());
    }
    let decoded = read_bundle(&args.file)?;
    let layout = zuno_paths::Layout::resolve(environment.resolved());
    preflight_targets(&decoded, &layout, args.replace)?;

    let total_bytes = decoded
        .manifest
        .entries
        .iter()
        .map(|entry| entry.size)
        .sum::<u64>();
    if args.dry_run {
        println!(
            "Validated Zuno bundle: {} file(s), {} byte(s), {} target root(s)",
            decoded.entries.len(),
            total_bytes,
            decoded.manifest.roots.len()
        );
        return Ok(());
    }

    let operations = stage_bundle(&decoded, &layout)?;
    commit_operations(operations)?;
    println!(
        "Imported Zuno bundle: {} file(s), {} byte(s)",
        decoded.entries.len(),
        total_bytes
    );
    if decoded.manifest.includes_credentials {
        eprintln!(
            "warning: imported credential stores from an unencrypted bundle; delete the transferred bundle when no longer needed"
        );
    }
    Ok(())
}

fn default_bundle_name() -> String {
    let timestamp = OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_else(|_| "unknown-time".to_owned());
    format!("zuno-export-{timestamp}.zuno-bundle")
}

fn collect_directory(
    root_kind: BundleRoot,
    root: &Path,
    output: &mut Vec<SourceEntry>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let root_metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("failed to inspect {}: {error}", root.display()))?;
    if root_metadata.file_type().is_symlink() {
        return Err(format!(
            "portable bundle roots cannot be symbolic links: {}",
            root.display()
        ));
    }
    if !root_metadata.is_dir() {
        return Err(format!(
            "portable bundle root is not a directory: {}",
            root.display()
        ));
    }
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("failed to resolve bundle root {}: {error}", root.display()))?;

    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !excluded_directory(entry.file_name()));
    for entry in walker {
        let entry = entry.map_err(|error| format!("failed to walk {}: {error}", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() && !entry.file_type().is_symlink() {
            return Err(format!(
                "portable bundles cannot contain special filesystem entries: {}",
                entry.path().display()
            ));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| format!("failed to relativize {}: {error}", entry.path().display()))?;
        if excluded_relative_path(relative) {
            continue;
        }
        let portable = portable_path(relative)?;
        let source = if entry.file_type().is_symlink() {
            let target = std::fs::canonicalize(entry.path()).map_err(|error| {
                format!(
                    "failed to resolve symbolic link {}: {error}",
                    entry.path().display()
                )
            })?;
            if !target.starts_with(&canonical_root) {
                return Err(format!(
                    "symbolic link target is outside exported root: {} -> {}",
                    entry.path().display(),
                    target.display()
                ));
            }
            let target_relative = target.strip_prefix(&canonical_root).map_err(|error| {
                format!(
                    "failed to relativize symbolic link target {}: {error}",
                    target.display()
                )
            })?;
            if excluded_relative_path(target_relative) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&target).map_err(|error| {
                format!(
                    "failed to inspect symbolic link target {}: {error}",
                    target.display()
                )
            })?;
            if !metadata.is_file() {
                return Err(format!(
                    "symbolic link must resolve to a regular file inside the exported root: {} -> {}",
                    entry.path().display(),
                    target.display()
                ));
            }
            target
        } else {
            entry.path().to_path_buf()
        };
        collect_regular_file(root_kind, &source, portable, output)?;
    }
    Ok(())
}

fn collect_file(
    root_kind: BundleRoot,
    source: &Path,
    portable: &str,
    output: &mut Vec<SourceEntry>,
) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("failed to inspect {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "credential store must be a regular file: {}",
            source.display()
        ));
    }
    collect_regular_file(root_kind, source, portable.to_owned(), output)
}

fn collect_regular_file(
    root: BundleRoot,
    source: &Path,
    portable: String,
    output: &mut Vec<SourceEntry>,
) -> Result<(), String> {
    validate_portable_path(&portable)?;
    let metadata = std::fs::metadata(source)
        .map_err(|error| format!("failed to inspect {}: {error}", source.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is too large for a portable bundle ({} bytes; maximum {})",
            source.display(),
            metadata.len(),
            MAX_FILE_BYTES
        ));
    }
    let bytes = std::fs::read(source)
        .map_err(|error| format!("failed to read {}: {error}", source.display()))?;
    let size = u64::try_from(bytes.len()).map_err(|error| error.to_string())?;
    output.push(SourceEntry {
        metadata: BundleEntry {
            root,
            path: portable,
            sha256: sha256_hex(&bytes),
            size,
            unix_mode: file_mode(&metadata),
        },
        bytes,
    });
    Ok(())
}

fn validate_source_entries(entries: &[SourceEntry]) -> Result<(), String> {
    if entries.len() > MAX_ENTRY_COUNT {
        return Err(format!(
            "portable bundle has too many files ({}; maximum {MAX_ENTRY_COUNT})",
            entries.len()
        ));
    }
    let mut total = 0_u64;
    let mut exact = BTreeSet::new();
    let mut portable = BTreeMap::new();
    for entry in entries {
        total = total
            .checked_add(entry.metadata.size)
            .ok_or_else(|| "portable bundle size overflow".to_owned())?;
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "portable bundle exceeds the {} byte total limit",
                MAX_TOTAL_BYTES
            ));
        }
        let key = (entry.metadata.root, entry.metadata.path.clone());
        if !exact.insert(key.clone()) {
            return Err(format!(
                "portable bundle contains duplicate path {}/{}",
                entry.metadata.root.as_str(),
                entry.metadata.path
            ));
        }
        let folded = (entry.metadata.root, entry.metadata.path.to_lowercase());
        if let Some(existing) = portable.insert(folded, entry.metadata.path.clone())
            && existing != entry.metadata.path
        {
            return Err(format!(
                "paths `{existing}` and `{}` collide on case-insensitive filesystems",
                entry.metadata.path
            ));
        }
    }
    Ok(())
}

fn write_bundle(
    output: &Path,
    force: bool,
    manifest: &BundleManifest,
    entries: &[SourceEntry],
) -> Result<(), String> {
    let parent = output
        .parent()
        .ok_or_else(|| format!("bundle path has no parent: {}", output.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "failed to create temporary bundle in {}: {error}",
            parent.display()
        )
    })?;
    {
        let mut archive = ZipWriter::new(temporary.as_file_mut());
        let manifest_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);
        archive
            .start_file(MANIFEST_PATH, manifest_options)
            .map_err(|error| format!("failed to start bundle manifest: {error}"))?;
        let manifest_bytes = serde_json::to_vec_pretty(manifest)
            .map_err(|error| format!("failed to encode bundle manifest: {error}"))?;
        archive
            .write_all(&manifest_bytes)
            .map_err(|error| format!("failed to write bundle manifest: {error}"))?;

        for entry in entries {
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .unix_permissions(entry.metadata.unix_mode);
            archive
                .start_file(entry.metadata.archive_path(), options)
                .map_err(|error| {
                    format!(
                        "failed to start bundle entry {}/{}: {error}",
                        entry.metadata.root.as_str(),
                        entry.metadata.path
                    )
                })?;
            archive.write_all(&entry.bytes).map_err(|error| {
                format!(
                    "failed to write bundle entry {}/{}: {error}",
                    entry.metadata.root.as_str(),
                    entry.metadata.path
                )
            })?;
        }
        archive
            .finish()
            .map_err(|error| format!("failed to finish bundle: {error}"))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("failed to sync temporary bundle: {error}"))?;
    let persisted = if force {
        temporary.persist(output)
    } else {
        temporary.persist_noclobber(output)
    };
    persisted.map_err(|error| {
        format!(
            "failed to install bundle {}: {}",
            output.display(),
            error.error
        )
    })?;
    Ok(())
}

fn read_bundle(path: &Path) -> Result<DecodedBundle, String> {
    let file = File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("Bundle not found: {}", path.display()),
        std::io::ErrorKind::PermissionDenied => {
            format!(
                "Failed to read bundle {}: permission denied",
                path.display()
            )
        }
        _ => format!("Failed to read bundle {}: {error}", path.display()),
    })?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| format!("Invalid Zuno bundle {}: {error}", path.display()))?;
    if archive.len() > MAX_ENTRY_COUNT + 1 {
        return Err(format!(
            "bundle contains too many archive entries ({}; maximum {})",
            archive.len(),
            MAX_ENTRY_COUNT + 1
        ));
    }

    let mut archive_names = BTreeSet::new();
    let mut archive_total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("failed to inspect bundle entry {index}: {error}"))?;
        let name = entry.name().to_owned();
        if !archive_names.insert(name.clone()) {
            return Err(format!("bundle contains duplicate archive entry `{name}`"));
        }
        if entry.is_dir() {
            return Err(format!(
                "bundle contains unsupported directory entry `{name}`"
            ));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(format!("bundle contains symbolic link entry `{name}`"));
        }
        if entry.size() > MAX_FILE_BYTES && name != MANIFEST_PATH {
            return Err(format!(
                "bundle entry `{name}` exceeds the {MAX_FILE_BYTES} byte limit"
            ));
        }
        archive_total = archive_total
            .checked_add(entry.size())
            .ok_or_else(|| "bundle size overflow".to_owned())?;
        if archive_total > MAX_TOTAL_BYTES + MAX_MANIFEST_BYTES {
            return Err(format!(
                "bundle exceeds the {} byte uncompressed limit",
                MAX_TOTAL_BYTES + MAX_MANIFEST_BYTES
            ));
        }
    }

    let manifest = {
        let mut entry = archive
            .by_name(MANIFEST_PATH)
            .map_err(|_| format!("bundle is missing `{MANIFEST_PATH}`"))?;
        if entry.size() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "bundle manifest exceeds the {MAX_MANIFEST_BYTES} byte limit"
            ));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("failed to read bundle manifest: {error}"))?;
        serde_json::from_slice::<BundleManifest>(&bytes)
            .map_err(|error| format!("invalid bundle manifest: {error}"))?
    };
    validate_manifest(&manifest, &archive_names)?;

    let mut decoded = Vec::with_capacity(manifest.entries.len());
    for metadata in &manifest.entries {
        let archive_path = metadata.archive_path();
        let mut entry = archive
            .by_name(&archive_path)
            .map_err(|_| format!("bundle is missing `{archive_path}`"))?;
        if entry.size() != metadata.size {
            return Err(format!(
                "bundle entry `{archive_path}` size mismatch: manifest {}, archive {}",
                metadata.size,
                entry.size()
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.size as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("failed to read bundle entry `{archive_path}`: {error}"))?;
        if sha256_hex(&bytes) != metadata.sha256 {
            return Err(format!(
                "bundle entry `{archive_path}` failed SHA-256 validation"
            ));
        }
        decoded.push(DecodedEntry {
            metadata: metadata.clone(),
            bytes,
        });
    }
    Ok(DecodedBundle {
        manifest,
        entries: decoded,
    })
}

fn validate_manifest(
    manifest: &BundleManifest,
    archive_names: &BTreeSet<String>,
) -> Result<(), String> {
    if manifest.format != BUNDLE_FORMAT {
        return Err(format!(
            "unsupported bundle format `{}`; expected `{BUNDLE_FORMAT}`",
            manifest.format
        ));
    }
    if manifest.schema_version != BUNDLE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported bundle schema version {}; expected {BUNDLE_SCHEMA_VERSION}",
            manifest.schema_version
        ));
    }
    if manifest.product != "zuno" {
        return Err(format!(
            "bundle product is `{}`, not `zuno`",
            manifest.product
        ));
    }
    OffsetDateTime::parse(&manifest.created_at, &Rfc3339)
        .map_err(|error| format!("invalid bundle creation timestamp: {error}"))?;
    if manifest.entries.len() > MAX_ENTRY_COUNT {
        return Err(format!(
            "bundle manifest contains too many files ({}; maximum {MAX_ENTRY_COUNT})",
            manifest.entries.len()
        ));
    }

    let root_set = manifest.roots.iter().copied().collect::<BTreeSet<_>>();
    if root_set.len() != manifest.roots.len() {
        return Err("bundle manifest contains duplicate roots".to_owned());
    }
    let credential_roots = root_set.iter().any(|root| root.credential_path().is_some());
    if manifest.includes_credentials != credential_roots {
        return Err("bundle credential marker does not match its roots".to_owned());
    }

    let mut expected_names = BTreeSet::from([MANIFEST_PATH.to_owned()]);
    let mut exact = BTreeSet::new();
    let mut portable = BTreeMap::new();
    let mut total = 0_u64;
    for entry in &manifest.entries {
        if !root_set.contains(&entry.root) {
            return Err(format!(
                "bundle entry {}/{} references an undeclared root",
                entry.root.as_str(),
                entry.path
            ));
        }
        validate_portable_path(&entry.path)?;
        if let Some(expected) = entry.root.credential_path()
            && entry.path != expected
        {
            return Err(format!(
                "credential root `{}` must contain exactly `{expected}`",
                entry.root.as_str()
            ));
        }
        if entry.size > MAX_FILE_BYTES {
            return Err(format!(
                "bundle entry {}/{} exceeds the {MAX_FILE_BYTES} byte limit",
                entry.root.as_str(),
                entry.path
            ));
        }
        if entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(format!(
                "bundle entry {}/{} has an invalid SHA-256 digest",
                entry.root.as_str(),
                entry.path
            ));
        }
        let key = (entry.root, entry.path.clone());
        if !exact.insert(key) {
            return Err(format!(
                "bundle manifest contains duplicate path {}/{}",
                entry.root.as_str(),
                entry.path
            ));
        }
        let folded = (entry.root, entry.path.to_lowercase());
        if let Some(existing) = portable.insert(folded, entry.path.clone())
            && existing != entry.path
        {
            return Err(format!(
                "bundle paths `{existing}` and `{}` collide on case-insensitive filesystems",
                entry.path
            ));
        }
        total = total
            .checked_add(entry.size)
            .ok_or_else(|| "bundle size overflow".to_owned())?;
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "bundle manifest exceeds the {MAX_TOTAL_BYTES} byte total limit"
            ));
        }
        expected_names.insert(entry.archive_path());
    }

    if &expected_names != archive_names {
        let missing = expected_names
            .difference(archive_names)
            .cloned()
            .collect::<Vec<_>>();
        let unexpected = archive_names
            .difference(&expected_names)
            .cloned()
            .collect::<Vec<_>>();
        return Err(format!(
            "bundle archive entries do not match the manifest; missing={missing:?}, unexpected={unexpected:?}"
        ));
    }
    Ok(())
}

fn preflight_targets(
    bundle: &DecodedBundle,
    layout: &zuno_paths::Layout,
    replace: bool,
) -> Result<(), String> {
    for root in &bundle.manifest.roots {
        if !bundle
            .entries
            .iter()
            .any(|entry| entry.metadata.root == *root)
        {
            continue;
        }
        let target = root.target(layout);
        if let Ok(metadata) = std::fs::symlink_metadata(&target) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "import target cannot be a symbolic link: {}",
                    target.display()
                ));
            }
            let conflicts = if metadata.is_dir() {
                std::fs::read_dir(&target)
                    .map_err(|error| format!("failed to inspect {}: {error}", target.display()))?
                    .next()
                    .transpose()
                    .map_err(|error| format!("failed to inspect {}: {error}", target.display()))?
                    .is_some()
            } else {
                true
            };
            if conflicts && !replace {
                return Err(format!(
                    "import target {} is not empty; pass --replace to transactionally replace bundle-owned targets",
                    target.display()
                ));
            }
        }
    }
    Ok(())
}

fn stage_bundle(
    bundle: &DecodedBundle,
    layout: &zuno_paths::Layout,
) -> Result<Vec<InstallOperation>, String> {
    let mut operations: Vec<InstallOperation> = Vec::new();
    for root in &bundle.manifest.roots {
        let entries = bundle
            .entries
            .iter()
            .filter(|entry| entry.metadata.root == *root)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            continue;
        }
        let target = root.target(layout);
        let parent = target
            .parent()
            .ok_or_else(|| format!("import target has no parent: {}", target.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        let staging = unique_sibling(&target, "staging");
        let result = if root.is_directory() {
            std::fs::create_dir(&staging)
                .map_err(|error| format!("failed to create {}: {error}", staging.display()))?;
            for entry in entries {
                let relative = path_from_portable(&entry.metadata.path);
                let destination = staging.join(relative);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("failed to create {}: {error}", parent.display())
                    })?;
                }
                write_staged_file(&destination, entry)
                    .map_err(|error| format!("{error} (root {})", root.as_str()))?;
            }
            Ok(())
        } else if entries.len() != 1 {
            Err(format!(
                "credential root `{}` contains {} entries; expected one",
                root.as_str(),
                entries.len()
            ))
        } else {
            write_staged_file(&staging, entries[0])
        };
        if let Err(error) = result {
            let _ignored = remove_path(&staging);
            for operation in &operations {
                let _ignored = remove_path(&operation.staging);
            }
            return Err(error);
        }
        operations.push(InstallOperation {
            root: *root,
            target,
            staging,
        });
    }
    Ok(operations)
}

fn write_staged_file(destination: &Path, entry: &DecodedEntry) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| format!("failed to create {}: {error}", destination.display()))?;
    file.write_all(&entry.bytes)
        .map_err(|error| format!("failed to write {}: {error}", destination.display()))?;
    file.sync_all()
        .map_err(|error| format!("failed to sync {}: {error}", destination.display()))?;
    set_file_mode(destination, entry.metadata.unix_mode)?;
    Ok(())
}

fn commit_operations(operations: Vec<InstallOperation>) -> Result<(), String> {
    let mut committed = Vec::new();
    for (index, operation) in operations.iter().enumerate() {
        let backup = if operation.target.exists() {
            let backup = unique_sibling(&operation.target, "backup");
            std::fs::rename(&operation.target, &backup).map_err(|error| {
                cleanup_staging(&operations[index..]);
                rollback_committed(&committed);
                format!(
                    "failed to move existing import target {} to {}: {error}",
                    operation.target.display(),
                    backup.display()
                )
            })?;
            Some(backup)
        } else {
            None
        };

        if let Err(error) = std::fs::rename(&operation.staging, &operation.target) {
            let restore = backup
                .as_ref()
                .map_or(Ok(()), |backup| std::fs::rename(backup, &operation.target));
            cleanup_staging(&operations[index..]);
            rollback_committed(&committed);
            return match restore {
                Ok(()) => Err(format!(
                    "failed to install {} at {}: {error}",
                    operation.root.as_str(),
                    operation.target.display()
                )),
                Err(rollback) => Err(format!(
                    "failed to install {} at {}: {error}; rollback also failed: {rollback}",
                    operation.root.as_str(),
                    operation.target.display()
                )),
            };
        }
        committed.push(CommittedOperation {
            root: operation.root,
            target: operation.target.clone(),
            backup,
        });
    }

    for operation in committed {
        if let Some(backup) = operation.backup
            && let Err(error) = remove_path(&backup)
        {
            eprintln!(
                "warning: imported {} but could not remove backup {}: {error}",
                operation.root.as_str(),
                backup.display()
            );
        }
    }
    Ok(())
}

fn rollback_committed(committed: &[CommittedOperation]) {
    for operation in committed.iter().rev() {
        let _ignored = remove_path(&operation.target);
        if let Some(backup) = &operation.backup
            && let Err(error) = std::fs::rename(backup, &operation.target)
        {
            eprintln!(
                "warning: failed to restore import backup {} to {}: {error}",
                backup.display(),
                operation.target.display()
            );
        }
    }
}

fn cleanup_staging(operations: &[InstallOperation]) {
    for operation in operations {
        let _ignored = remove_path(&operation.staging);
    }
}

fn unique_sibling(target: &Path, phase: &str) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zuno");
    parent.join(format!(
        ".{name}.zuno-import-{phase}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

fn remove_path(path: &Path) -> Result<(), std::io::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn portable_path(path: &Path) -> Result<String, String> {
    let mut segments = Vec::new();
    for component in path.components() {
        let Component::Normal(segment) = component else {
            return Err(format!(
                "path cannot be represented in a portable bundle: {}",
                path.display()
            ));
        };
        let segment = segment.to_str().ok_or_else(|| {
            format!(
                "path is not UTF-8 and cannot be exported portably: {}",
                path.display()
            )
        })?;
        segments.push(segment);
    }
    let portable = segments.join("/");
    validate_portable_path(&portable)?;
    Ok(portable)
}

fn path_from_portable(path: &str) -> PathBuf {
    path.split('/').collect()
}

fn validate_portable_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.contains('\0')
    {
        return Err(format!("unsafe portable bundle path `{path}`"));
    }
    for segment in path.split('/') {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || segment.ends_with(' ')
            || segment.ends_with('.')
            || windows_reserved_name(segment)
        {
            return Err(format!("unsafe portable bundle path `{path}`"));
        }
    }
    Ok(())
}

fn windows_reserved_name(segment: &str) -> bool {
    let base = segment
        .split_once('.')
        .map_or(segment, |(base, _extension)| base)
        .to_ascii_uppercase();
    matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        .is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

fn excluded_directory(name: &std::ffi::OsStr) -> bool {
    matches!(name.to_str(), Some(".git" | ".omo" | "__pycache__"))
}

fn excluded_file(relative: &Path) -> bool {
    let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == zuno_paths::files::AUTH_FILE
        || name == zuno_paths::files::MCP_AUTH_FILE
        || name == "logs.sqlite"
        || name == "prompt-history.jsonl"
        || name.ends_with(".db")
        || name.ends_with(".db-wal")
        || name.ends_with(".db-shm")
}

fn excluded_relative_path(relative: &Path) -> bool {
    excluded_file(relative)
        || relative.components().any(
            |component| matches!(component, Component::Normal(name) if excluded_directory(name)),
        )
}

fn lexical_absolute(path: &Path, current: &Path) -> PathBuf {
    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in source.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _popped = normalized.pop();
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    normalized
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))
        .map_err(|error| format!("failed to set permissions on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
        .permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| format!("failed to set permissions on {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_paths_reject_traversal_windows_aliases_and_case_hazards() {
        for path in [
            "",
            "/absolute",
            "../escape",
            "a/../../escape",
            "C:/drive",
            r"dir\\file",
            "dir/trailing.",
            "dir/trailing ",
            "CON",
            "nested/com1.txt",
            "nested/Lpt9",
        ] {
            assert!(
                validate_portable_path(path).is_err(),
                "{path:?} must not be portable"
            );
        }
        for path in [
            "AGENTS.md",
            "skill/example/SKILL.md",
            "extensions/example/extension.json",
            "profiles/kiro/zuno.json",
        ] {
            assert!(
                validate_portable_path(path).is_ok(),
                "{path:?} should be portable"
            );
        }
    }

    #[test]
    fn case_insensitive_collisions_are_rejected_before_export() {
        let entries = ["Skill/A.md", "skill/a.md"]
            .into_iter()
            .map(|path| SourceEntry {
                metadata: BundleEntry {
                    root: BundleRoot::Config,
                    path: path.to_owned(),
                    sha256: sha256_hex(b"x"),
                    size: 1,
                    unix_mode: 0o644,
                },
                bytes: vec![b'x'],
            })
            .collect::<Vec<_>>();
        let error = validate_source_entries(&entries).expect_err("collision");
        assert!(error.contains("case-insensitive"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn internal_file_symlinks_are_materialized_but_external_targets_are_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join("skill/example")).expect("skill directory");
        std::fs::write(root.path().join("shared.md"), "portable body").expect("shared file");
        symlink(
            "../../shared.md",
            root.path().join("skill/example/reference.md"),
        )
        .expect("internal symlink");

        let mut entries = Vec::new();
        collect_directory(BundleRoot::Config, root.path(), &mut entries)
            .expect("internal file symlink should be portable");
        let materialized = entries
            .iter()
            .find(|entry| entry.metadata.path == "skill/example/reference.md")
            .expect("materialized link path");
        assert_eq!(materialized.bytes, b"portable body");

        let external = tempfile::tempdir().expect("external");
        std::fs::write(external.path().join("secret.md"), "outside").expect("external file");
        let escaping = tempfile::tempdir().expect("escaping root");
        symlink(
            external.path().join("secret.md"),
            escaping.path().join("external.md"),
        )
        .expect("external symlink");
        let error = collect_directory(BundleRoot::Config, escaping.path(), &mut Vec::new())
            .expect_err("external target must be rejected");
        assert!(error.contains("outside exported root"), "{error}");
    }
}
