use std::fs::{self, File};
use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

use ::self_update::{Release, ReleaseAsset, ReleaseUpdate};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use sha2::{Digest as _, Sha256};

use crate::SelfUpdateArgs;

const REPOSITORY_OWNER: &str = "sunerpy";
const REPOSITORY_NAME: &str = "zuno";
const BINARY_NAME: &str = "zuno";
const CHECKSUM_ASSET: &str = "SHA256SUMS";

pub(crate) fn execute(args: &SelfUpdateArgs) -> Result<(), String> {
    execute_inner(args).map_err(|error| error.to_string())
}

fn execute_inner(args: &SelfUpdateArgs) -> Result<(), UpdateError> {
    let platform = ReleasePlatform::for_host()?;
    let token = resolve_github_token(|name| std::env::var(name).ok());
    let source = GithubReleaseSource::new(platform.target, token)?;
    let explicit_tag = args.tag.as_deref().map(normalize_tag).transpose()?;
    let release = match explicit_tag.as_deref() {
        Some(tag) => source.tagged(tag)?,
        None => source.latest()?,
    };
    validate_version(release.version())?;

    let current = env!("CARGO_PKG_VERSION");
    let relation = version_relation(current, release.version())?;
    if args.check {
        print_check_result(current, release.version(), relation);
        return Ok(());
    }
    if !args.force && explicit_tag.is_none() && relation != VersionRelation::Newer {
        print_no_update_result(current, release.version(), relation);
        return Ok(());
    }

    let archive_name = archive_asset_name(release.version(), platform);
    let archive = exact_asset(&release, &archive_name)?;
    let checksums = exact_asset(&release, CHECKSUM_ASSET)?;
    let executable = std::env::current_exe().map_err(UpdateError::LocateExecutable)?;

    println!("Zuno self-update");
    println!("  current: {current} ({})", executable.display());
    println!("  release: {}", release.version());
    println!("  target:  {}", platform.target);
    println!("  asset:   {archive_name}");
    println!("  verify:  {CHECKSUM_ASSET} (SHA-256)");

    if !args.yes && !confirm_replace()? {
        println!("Update cancelled.");
        return Ok(());
    }

    let temporary = tempfile::TempDir::new().map_err(UpdateError::TemporaryDirectory)?;
    let checksum_path = temporary.path().join(CHECKSUM_ASSET);
    let archive_path = temporary.path().join(&archive_name);
    source.download(checksums, &checksum_path)?;
    source.download(archive, &archive_path)?;

    let manifest = fs::read_to_string(&checksum_path).map_err(|source| UpdateError::ReadFile {
        path: checksum_path.clone(),
        source,
    })?;
    let expected = checksum_for(&manifest, &archive_name)?;
    verify_checksum(&archive_path, &expected)?;
    println!("Verified {archive_name} against {CHECKSUM_ASSET}.");

    let extracted = temporary.path().join("extracted");
    fs::create_dir(&extracted).map_err(|source| UpdateError::CreateDirectory {
        path: extracted.clone(),
        source,
    })?;
    ::self_update::Extract::from_source(&archive_path)
        .extract_file(&extracted, platform.executable)
        .map_err(|source| UpdateError::Extract {
            asset: archive_name.clone(),
            detail: source.to_string(),
        })?;
    let replacement = extracted.join(platform.executable);
    validate_replacement(&replacement)?;

    self_replace::self_replace(&replacement).map_err(|source| UpdateError::Replace {
        executable,
        detail: source.to_string(),
    })?;
    println!("Updated Zuno to {}.", release.version());
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReleasePlatform {
    target: &'static str,
    extension: &'static str,
    executable: &'static str,
}

impl ReleasePlatform {
    fn for_host() -> Result<Self, UpdateError> {
        Self::from_parts(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn from_parts(os: &str, arch: &str) -> Result<Self, UpdateError> {
        match (os, arch) {
            ("linux", "x86_64") => Ok(Self {
                target: "x86_64-unknown-linux-musl",
                extension: "tar.gz",
                executable: BINARY_NAME,
            }),
            ("linux", "aarch64") => Ok(Self {
                target: "aarch64-unknown-linux-musl",
                extension: "tar.gz",
                executable: BINARY_NAME,
            }),
            ("macos", "x86_64") => Ok(Self {
                target: "x86_64-apple-darwin",
                extension: "tar.gz",
                executable: BINARY_NAME,
            }),
            ("macos", "aarch64") => Ok(Self {
                target: "aarch64-apple-darwin",
                extension: "tar.gz",
                executable: BINARY_NAME,
            }),
            ("windows", "x86_64") => Ok(Self {
                target: "x86_64-pc-windows-msvc",
                extension: "zip",
                executable: "zuno.exe",
            }),
            ("windows", "aarch64") => Ok(Self {
                target: "aarch64-pc-windows-msvc",
                extension: "zip",
                executable: "zuno.exe",
            }),
            _ => Err(UpdateError::UnsupportedPlatform {
                os: os.to_owned(),
                arch: arch.to_owned(),
            }),
        }
    }
}

fn archive_asset_name(version: &str, platform: ReleasePlatform) -> String {
    format!(
        "{BINARY_NAME}-{version}-{}.{}",
        platform.target, platform.extension
    )
}

struct GithubReleaseSource {
    backend: Box<dyn ReleaseUpdate>,
    token: Option<String>,
}

impl GithubReleaseSource {
    fn new(target: &str, token: Option<String>) -> Result<Self, UpdateError> {
        let mut builder = ::self_update::backends::github::Update::configure();
        builder
            .repo_owner(REPOSITORY_OWNER)
            .repo_name(REPOSITORY_NAME)
            .target(target)
            .bin_name(BINARY_NAME)
            .current_version(env!("CARGO_PKG_VERSION"))
            .show_output(false);
        if let Some(token) = token.as_deref() {
            builder.auth_token(token);
        }
        let backend = builder.build().map_err(|source| UpdateError::Configure {
            detail: source.to_string(),
        })?;
        Ok(Self {
            backend: Box::new(backend),
            token,
        })
    }

    fn latest(&self) -> Result<Release, UpdateError> {
        let releases = self
            .backend
            .get_latest_release()
            .map_err(|source| github_error("querying the latest GitHub release", source))?;
        releases
            .latest()
            .cloned()
            .ok_or(UpdateError::MissingLatestRelease)
    }

    fn tagged(&self, tag: &str) -> Result<Release, UpdateError> {
        self.backend
            .get_release_version(tag)
            .map_err(|source| github_error("querying the requested GitHub release", source))
    }

    fn download(&self, asset: &ReleaseAsset, destination: &Path) -> Result<(), UpdateError> {
        let mut file = File::create(destination).map_err(|source| UpdateError::CreateFile {
            path: destination.to_path_buf(),
            source,
        })?;
        let mut download = ::self_update::Download::from_url(asset.download_url());
        download
            .show_download_progress(io::stderr().is_terminal())
            .request_header(ACCEPT, HeaderValue::from_static("application/octet-stream"));
        if let Some(token) = self.token.as_deref() {
            let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|source| {
                UpdateError::AuthorizationHeader {
                    detail: source.to_string(),
                }
            })?;
            download.request_header(AUTHORIZATION, value);
        }
        download
            .download_to(&mut file)
            .map_err(|source| UpdateError::Download {
                asset: asset.name().to_owned(),
                detail: source.to_string(),
            })?;
        file.sync_all().map_err(|source| UpdateError::WriteFile {
            path: destination.to_path_buf(),
            source,
        })
    }
}

fn github_error(action: &'static str, source: ::self_update::errors::Error) -> UpdateError {
    UpdateError::Github {
        action,
        detail: source.to_string(),
    }
}

fn resolve_github_token(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"].into_iter().find_map(|name| {
        get(name).and_then(|value| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        })
    })
}

fn normalize_tag(tag: &str) -> Result<String, UpdateError> {
    let trimmed = tag.trim();
    let version = trimmed.strip_prefix('v').unwrap_or(trimmed);
    validate_version(version)?;
    Ok(format!("v{version}"))
}

fn validate_version(version: &str) -> Result<(), UpdateError> {
    ::self_update::version::bump_is_greater(version, version)
        .map(|_| ())
        .map_err(|source| UpdateError::InvalidVersion {
            version: version.to_owned(),
            detail: source.to_string(),
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionRelation {
    Newer,
    Equal,
    Older,
}

fn version_relation(current: &str, candidate: &str) -> Result<VersionRelation, UpdateError> {
    if ::self_update::version::bump_is_greater(current, candidate).map_err(|source| {
        UpdateError::InvalidVersion {
            version: candidate.to_owned(),
            detail: source.to_string(),
        }
    })? {
        return Ok(VersionRelation::Newer);
    }
    if ::self_update::version::bump_is_greater(candidate, current).map_err(|source| {
        UpdateError::InvalidVersion {
            version: current.to_owned(),
            detail: source.to_string(),
        }
    })? {
        return Ok(VersionRelation::Older);
    }
    Ok(VersionRelation::Equal)
}

fn print_check_result(current: &str, release: &str, relation: VersionRelation) {
    match relation {
        VersionRelation::Newer => {
            println!("Zuno {current} -> {release} is available.");
            println!("Run `zuno self-update` to install it.");
        }
        VersionRelation::Equal => println!("Zuno {current} is up to date."),
        VersionRelation::Older => {
            println!("Zuno {current} is newer than the latest release {release}.")
        }
    }
}

fn print_no_update_result(current: &str, release: &str, relation: VersionRelation) {
    match relation {
        VersionRelation::Newer => unreachable!("newer releases are installable"),
        VersionRelation::Equal => println!("Zuno {current} is already up to date."),
        VersionRelation::Older => {
            println!(
                "Zuno {current} is newer than the latest release {release}; no update applied."
            )
        }
    }
}

fn exact_asset<'a>(
    release: &'a Release,
    expected_name: &str,
) -> Result<&'a ReleaseAsset, UpdateError> {
    let mut matches = release
        .assets()
        .iter()
        .filter(|asset| asset.name() == expected_name);
    let Some(asset) = matches.next() else {
        return Err(UpdateError::MissingAsset {
            version: release.version().to_owned(),
            asset: expected_name.to_owned(),
        });
    };
    if matches.next().is_some() {
        return Err(UpdateError::DuplicateAsset {
            version: release.version().to_owned(),
            asset: expected_name.to_owned(),
        });
    }
    Ok(asset)
}

fn checksum_for(manifest: &str, asset: &str) -> Result<String, UpdateError> {
    let mut found = None;
    for (index, line) in manifest.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let digest = fields.next().unwrap_or_default();
        let filename = fields.next().unwrap_or_default().trim_start_matches('*');
        if fields.next().is_some() || filename.is_empty() {
            return Err(UpdateError::MalformedChecksumLine { line: index + 1 });
        }
        if filename != asset {
            continue;
        }
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UpdateError::InvalidChecksum {
                asset: asset.to_owned(),
            });
        }
        if found.replace(digest.to_ascii_lowercase()).is_some() {
            return Err(UpdateError::DuplicateChecksum {
                asset: asset.to_owned(),
            });
        }
    }
    found.ok_or_else(|| UpdateError::MissingChecksum {
        asset: asset.to_owned(),
    })
}

fn verify_checksum(path: &Path, expected: &str) -> Result<(), UpdateError> {
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch {
            asset: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<download>")
                .to_owned(),
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = File::open(path).map_err(|source| UpdateError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| UpdateError::ReadFile {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn confirm_replace() -> Result<bool, UpdateError> {
    if !io::stdin().is_terminal() {
        return Err(UpdateError::ConfirmationRequired);
    }
    print!("Replace the current Zuno executable? [y/N] ");
    io::stdout()
        .flush()
        .map_err(UpdateError::WriteConfirmation)?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(UpdateError::ReadConfirmation)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn validate_replacement(path: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| UpdateError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(UpdateError::InvalidReplacement {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(UpdateError::ReplacementNotExecutable {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum UpdateError {
    #[error("self-update is unavailable on {os}/{arch}: no matching Zuno release target")]
    UnsupportedPlatform { os: String, arch: String },
    #[error("could not configure the GitHub release updater: {detail}")]
    Configure { detail: String },
    #[error("GitHub returned no latest release")]
    MissingLatestRelease,
    #[error(
        "{action}: {detail}\n\nhint: for a private repository, set GITHUB_TOKEN or GH_TOKEN; for example:\n  GH_TOKEN=\"$(gh auth token)\" zuno self-update"
    )]
    Github {
        action: &'static str,
        detail: String,
    },
    #[error("invalid release version `{version}`: {detail}")]
    InvalidVersion { version: String, detail: String },
    #[error("release {version} does not contain required asset `{asset}`")]
    MissingAsset { version: String, asset: String },
    #[error("release {version} contains duplicate asset `{asset}`")]
    DuplicateAsset { version: String, asset: String },
    #[error("could not create update file {}: {source}", path.display())]
    CreateFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write update file {}: {source}", path.display())]
    WriteFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read update file {}: {source}", path.display())]
    ReadFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not create update directory {}: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not create a temporary update directory: {0}")]
    TemporaryDirectory(io::Error),
    #[error("invalid GitHub authorization header: {detail}")]
    AuthorizationHeader { detail: String },
    #[error(
        "could not download release asset `{asset}`: {detail}\n\nhint: check proxy/no_proxy and GITHUB_TOKEN or GH_TOKEN"
    )]
    Download { asset: String, detail: String },
    #[error("line {line} in SHA256SUMS is malformed")]
    MalformedChecksumLine { line: usize },
    #[error("SHA256SUMS has no digest for `{asset}`")]
    MissingChecksum { asset: String },
    #[error("SHA256SUMS contains duplicate digests for `{asset}`")]
    DuplicateChecksum { asset: String },
    #[error("SHA256SUMS contains an invalid SHA-256 digest for `{asset}`")]
    InvalidChecksum { asset: String },
    #[error(
        "checksum mismatch for `{asset}`: expected {expected}, downloaded {actual}; executable was not replaced"
    )]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("could not extract `{asset}`: {detail}")]
    Extract { asset: String, detail: String },
    #[error("release archive did not produce a non-empty regular executable at {}", path.display())]
    InvalidReplacement { path: PathBuf },
    #[cfg(unix)]
    #[error("release executable is not marked executable: {}", path.display())]
    ReplacementNotExecutable { path: PathBuf },
    #[error("could not locate the running Zuno executable: {0}")]
    LocateExecutable(io::Error),
    #[error(
        "could not atomically replace {}: {detail}\n\nhint: install Zuno in a writable PATH directory or rerun with the privileges that own this file",
        executable.display()
    )]
    Replace { executable: PathBuf, detail: String },
    #[error("self-update requires confirmation on a terminal; rerun with --yes")]
    ConfirmationRequired,
    #[error("could not write the self-update confirmation: {0}")]
    WriteConfirmation(io::Error),
    #[error("could not read the self-update confirmation: {0}")]
    ReadConfirmation(io::Error),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset::new(name, format!("https://example.invalid/{name}"))
    }

    fn release(assets: Vec<ReleaseAsset>) -> Release {
        Release::builder()
            .version("0.2.0")
            .assets(assets)
            .build()
            .expect("release fixture")
    }

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn release_targets_match_the_published_matrix() {
        let cases = [
            ("linux", "x86_64", "x86_64-unknown-linux-musl", "tar.gz"),
            ("linux", "aarch64", "aarch64-unknown-linux-musl", "tar.gz"),
            ("macos", "x86_64", "x86_64-apple-darwin", "tar.gz"),
            ("macos", "aarch64", "aarch64-apple-darwin", "tar.gz"),
            ("windows", "x86_64", "x86_64-pc-windows-msvc", "zip"),
            ("windows", "aarch64", "aarch64-pc-windows-msvc", "zip"),
        ];
        for (os, arch, target, extension) in cases {
            let platform = ReleasePlatform::from_parts(os, arch).expect("supported platform");
            assert_eq!(platform.target, target);
            assert_eq!(platform.extension, extension);
        }
        assert!(ReleasePlatform::from_parts("freebsd", "x86_64").is_err());
    }

    #[test]
    fn linux_asset_uses_the_musl_release_even_for_a_host_build() {
        let platform = ReleasePlatform::from_parts("linux", "x86_64").expect("linux x86 release");
        assert_eq!(
            archive_asset_name("0.2.0", platform),
            "zuno-0.2.0-x86_64-unknown-linux-musl.tar.gz"
        );
    }

    #[test]
    fn exact_asset_never_accepts_a_substring_or_duplicate() {
        let candidate = release(vec![
            asset("zuno-0.2.0-x86_64-unknown-linux-musl.tar.gz.sig"),
            asset("SHA256SUMS"),
        ]);
        assert!(exact_asset(&candidate, "zuno-0.2.0-x86_64-unknown-linux-musl.tar.gz").is_err());

        let duplicate = release(vec![asset("SHA256SUMS"), asset("SHA256SUMS")]);
        assert!(matches!(
            exact_asset(&duplicate, "SHA256SUMS"),
            Err(UpdateError::DuplicateAsset { .. })
        ));
    }

    #[test]
    fn github_token_priority_is_deterministic_and_trimmed() {
        let get = getter(&[("GITHUB_TOKEN", "  primary  "), ("GH_TOKEN", "fallback")]);
        assert_eq!(resolve_github_token(get).as_deref(), Some("primary"));

        let get = getter(&[("GITHUB_TOKEN", " "), ("GH_TOKEN", " fallback ")]);
        assert_eq!(resolve_github_token(get).as_deref(), Some("fallback"));
        assert_eq!(resolve_github_token(getter(&[])), None);
    }

    #[test]
    fn tags_are_normalized_and_validated() {
        assert_eq!(normalize_tag("0.2.0").expect("bare tag"), "v0.2.0");
        assert_eq!(normalize_tag(" v0.2.0 ").expect("prefixed tag"), "v0.2.0");
        assert!(normalize_tag("latest").is_err());
        assert!(normalize_tag("").is_err());
    }

    #[test]
    fn version_relation_distinguishes_upgrade_equal_and_newer_local_builds() {
        assert_eq!(
            version_relation("0.1.0", "0.2.0").expect("versions"),
            VersionRelation::Newer
        );
        assert_eq!(
            version_relation("0.2.0", "0.2.0").expect("versions"),
            VersionRelation::Equal
        );
        assert_eq!(
            version_relation("0.3.0", "0.2.0").expect("versions"),
            VersionRelation::Older
        );
    }

    #[test]
    fn checksum_manifest_requires_one_exact_valid_digest() {
        let digest = "a".repeat(64);
        let manifest = format!(
            "{digest}  other.tar.gz\n{digest}  zuno-0.2.0-x86_64-unknown-linux-musl.tar.gz\n"
        );
        assert_eq!(
            checksum_for(&manifest, "zuno-0.2.0-x86_64-unknown-linux-musl.tar.gz")
                .expect("checksum"),
            digest
        );

        assert!(checksum_for("abcd  zuno.tar.gz\n", "zuno.tar.gz").is_err());
        assert!(
            checksum_for(
                &format!("{digest}  zuno.tar.gz\n{digest}  zuno.tar.gz\n"),
                "zuno.tar.gz"
            )
            .is_err()
        );
        assert!(checksum_for(&format!("{digest}  other.tar.gz\n"), "zuno.tar.gz").is_err());
    }

    #[test]
    fn downloaded_bytes_must_match_the_release_checksum() {
        let temporary = tempfile::TempDir::new().expect("temporary directory");
        let archive = temporary.path().join("zuno.tar.gz");
        let mut file = File::create(&archive).expect("archive fixture");
        file.write_all(b"verified release bytes")
            .expect("write fixture");
        file.sync_all().expect("sync fixture");

        let expected = sha256_file(&archive).expect("hash fixture");
        verify_checksum(&archive, &expected).expect("matching checksum");

        fs::write(&archive, b"tampered release bytes").expect("tamper fixture");
        assert!(matches!(
            verify_checksum(&archive, &expected),
            Err(UpdateError::ChecksumMismatch { .. })
        ));
    }
}
