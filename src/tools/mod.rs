//! Version-pinned GitHub release downloads with a persistent, platform-specific cache.
//!
//! Downloads are blocking. They are never executed by this module. Implement [`Tool`]
//! to add another release layout, or pass a custom [`Release`] to [`ToolCache::ensure_release`].

mod builtin;
pub use builtin::BuiltinTool;

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use fs2::FileExt;
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_EXTRACTED: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    /// Rust target environment (`gnu`, `musl`, `msvc`, or empty on macOS).
    pub env: String,
}

impl Platform {
    pub fn host() -> Self {
        Self {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            env: if cfg!(target_env = "musl") {
                "musl"
            } else if cfg!(target_env = "msvc") {
                "msvc"
            } else if cfg!(target_env = "gnu") {
                "gnu"
            } else {
                ""
            }
            .into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ArchiveFormat {
    Binary,
    TarGz,
}

#[derive(Clone, Debug)]
pub struct Release {
    /// GitHub `owner/repository`.
    pub repository: String,
    /// Exact release tag; `latest` is deliberately not supported.
    pub tag: String,
    /// Exact asset filename in the release.
    pub asset: String,
    /// Relative executable path inside the archive (or destination for a raw binary).
    pub executable: PathBuf,
    pub format: ArchiveFormat,
    /// Optional trusted SHA-256 of the downloaded asset, as hexadecimal.
    /// Otherwise the GitHub asset digest is checked when GitHub provides one.
    pub sha256: Option<String>,
}

pub trait Tool {
    fn release(&self, version: &str, platform: &Platform) -> Result<Release>;
}

/// A downloader using the OS cache directory, or an explicitly supplied directory.
/// Cache population is serialized per release using a filesystem lock. Only complete
/// installations are published, by renaming a temporary directory on the same filesystem.
pub struct ToolCache {
    root: PathBuf,
    client: Client,
    github_token: Option<String>,
}

impl ToolCache {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            root: root.into(),
            client: Client::builder()
                .user_agent(concat!("cargo-sycamore/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(300))
                .https_only(true)
                .build()?,
            github_token: None,
        })
    }

    pub fn system() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "cargo-sycamore")
            .context("could not determine a user cache directory")?;
        Self::new(dirs.cache_dir().join("tools"))
    }

    /// Authenticate GitHub API requests to avoid the low anonymous rate limit.
    /// The token is not sent on asset downloads or stored in the cache.
    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self
    }

    pub fn ensure(&self, tool: &impl Tool, version: &str) -> Result<PathBuf> {
        validate_segment(version)?;
        ensure!(version != "latest", "an explicit tool version is required");
        self.ensure_release(&tool.release(version, &Platform::host())?)
    }

    pub fn ensure_release(&self, release: &Release) -> Result<PathBuf> {
        self.install(release, |destination| self.download(release, destination))
            .with_context(|| {
                format!(
                    "installing {} from {} at {}",
                    release.asset, release.repository, release.tag
                )
            })
    }

    fn install(
        &self,
        release: &Release,
        download: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        release.validate()?;
        // Include the entire definition, so different repositories, target assets,
        // extraction layouts, and trusted checksums cannot share an installation.
        let key = format!("{:x}", Sha256::digest(format!("{release:?}")));
        fs::create_dir_all(&self.root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(format!("{key}.lock")))?;
        FileExt::lock_exclusive(&lock)?;
        let installed = self.root.join(&key);
        let executable = installed.join(&release.executable);
        if is_executable(&executable) && installed.join(".complete").is_file() {
            return Ok(executable);
        }
        // Recover an incomplete/corrupt entry, while holding its lock.
        if installed.exists() {
            fs::remove_dir_all(&installed)?;
        }
        let staging = tempfile::tempdir_in(&self.root)?;
        let asset = staging.path().join("download");
        download(&asset)?;
        if let Some(digest) = &release.sha256 {
            verify_sha256(&asset, digest)?;
        }
        let unpacked = staging.path().join("unpacked");
        fs::create_dir(&unpacked)?;
        match release.format {
            ArchiveFormat::Binary => {
                let output = unpacked.join(&release.executable);
                fs::create_dir_all(output.parent().context("missing executable parent")?)?;
                fs::copy(&asset, output)?;
            }
            ArchiveFormat::TarGz => extract_tar_gz(&asset, &unpacked)?,
        }
        let output = unpacked.join(&release.executable);
        ensure!(
            fs::symlink_metadata(&output).is_ok_and(|m| m.is_file()),
            "release does not contain a regular executable at {}",
            release.executable.display()
        );
        ensure!(output.metadata()?.len() > 0, "release executable is empty");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output, fs::Permissions::from_mode(0o755))?;
        }
        File::create(unpacked.join(".complete"))?.sync_all()?;
        fs::rename(unpacked, &installed)?;
        Ok(executable)
    }

    fn download(&self, release: &Release, destination: &Path) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/tags/{}",
            release.repository, release.tag
        );
        let mut request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = &self.github_token {
            request = request.bearer_auth(token);
        }
        let metadata: GithubRelease = request.send()?.error_for_status()?.json()?;
        let asset = metadata
            .assets
            .into_iter()
            .find(|asset| asset.name == release.asset)
            .with_context(|| {
                format!(
                    "asset '{}' is unavailable for release '{}'",
                    release.asset, release.tag
                )
            })?;
        ensure!(
            asset.size <= MAX_DOWNLOAD,
            "release asset exceeds download size limit"
        );
        let url = reqwest::Url::parse(&asset.browser_download_url)?;
        ensure!(
            url.scheme() == "https" && url.host_str() == Some("github.com"),
            "unexpected GitHub asset URL"
        );
        let response = self.client.get(url).send()?.error_for_status()?;
        let mut file = File::create(destination)?;
        copy_limited(response, &mut file, MAX_DOWNLOAD)?;
        file.flush()?;
        ensure!(
            file.metadata()?.len() == asset.size,
            "release asset download is truncated or has an unexpected size"
        );
        if let Some(digest) = asset.digest {
            let digest = digest
                .strip_prefix("sha256:")
                .context("unsupported GitHub asset digest algorithm")?;
            verify_sha256(destination, digest)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

impl Release {
    fn validate(&self) -> Result<()> {
        let parts: Vec<_> = self.repository.split('/').collect();
        ensure!(parts.len() == 2, "repository must be owner/repository");
        for part in parts {
            validate_segment(part)?;
        }
        validate_segment(&self.tag)?;
        ensure!(self.tag != "latest", "an explicit release tag is required");
        validate_segment(&self.asset)?;
        validate_path(&self.executable)?;
        ensure!(
            self.executable != Path::new(".complete"),
            "reserved executable path"
        );
        if let Some(digest) = &self.sha256 {
            validate_digest(digest)?;
        }
        Ok(())
    }
}

fn validate_segment(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b)),
        "invalid release name or version: {value:?}"
    );
    Ok(())
}

fn validate_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_)))
            && !path.to_string_lossy().contains(['\\', ':']),
        "unsafe release path: {}",
        path.display()
    );
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256 digest"
    );
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    validate_digest(expected)?;
    let mut hash = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    ensure!(
        format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected),
        "SHA-256 checksum mismatch"
    );
    Ok(())
}

fn copy_limited(reader: impl Read, writer: &mut impl Write, limit: u64) -> Result<()> {
    let copied = io::copy(&mut reader.take(limit + 1), writer)?;
    ensure!(copied <= limit, "release exceeds size limit");
    Ok(())
}

fn extract_tar_gz(asset: &Path, destination: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(asset)?));
    let mut total = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path: PathBuf = entry
            .path()?
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect();
        let kind = entry.header().entry_type();
        // A leading `./` is common in tar archives; a root directory entry is harmless.
        if path.as_os_str().is_empty() && kind.is_dir() {
            continue;
        }
        // Never materialize symlinks, hard links, devices, or traversal paths.
        // Preserve the rest of the archive, including Binaryen's runtime libraries.
        validate_path(&path)?;
        ensure!(
            kind.is_file() || kind.is_dir(),
            "unsupported archive entry: {}",
            path.display()
        );
        total = total
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(
            total <= MAX_EXTRACTED,
            "archive exceeds extraction size limit"
        );
        ensure!(
            entry.unpack_in(destination)?,
            "archive entry escapes destination"
        );
    }
    // Tar stops at its end marker, before gzip necessarily validates CRC32/ISIZE.
    // Drain the bounded remainder to force trailer validation before publishing.
    copy_limited(archive.into_inner(), &mut io::sink(), MAX_EXTRACTED)?;
    Ok(())
}

#[cfg(test)]
mod tests;
