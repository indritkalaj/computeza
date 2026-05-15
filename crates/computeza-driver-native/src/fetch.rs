//! Download + extract helper for managed component binaries.
//!
//! When `detect_bin_dir()` can't find a host-installed version of a
//! managed component (PostgreSQL on Windows, etc.), the installer falls
//! through to this module which:
//!
//! 1. Streams a known-good binary zip from the upstream vendor.
//! 2. Verifies the SHA-256 against a pinned checksum.
//! 3. Extracts to a stable cache directory under the component's
//!    `root_dir` (e.g. `%PROGRAMDATA%\Computeza\postgres\binaries\<v>\`).
//!
//! The cache is content-addressed by version, so re-running the
//! installer with the same bundle hits the existing extraction instead
//! of re-downloading.
//!
//! # Why pin a checksum
//!
//! TLS protects the bytes in transit, but not against a vendor-side
//! incident (compromised release server, swapped binary). Pinning the
//! SHA-256 in this source file means an attacker would need a code
//! change AND a server compromise; either alone is insufficient. The
//! pin is updated by hand when we bump versions; AGENTS.md will track
//! the audit trail.

use std::{
    io,
    path::{Path, PathBuf},
};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use tracing::{info, warn};

use crate::progress::{InstallPhase, ProgressHandle};

/// Format of the downloaded archive. Tells `fetch_and_extract` how to
/// unpack the payload after the SHA-256 check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.zip` (handled by the `zip` crate).
    Zip,
    /// `.tar.gz` / `.tgz` (handled by `tar` over `flate2::GzDecoder`).
    TarGz,
    /// `.tar.xz` (handled by `tar` over `liblzma::read::XzDecoder`).
    /// liblzma is compiled in with `features = ["static"]` so a virgin
    /// Linux host without xz-utils still works.
    TarXz,
    /// A raw binary, not an archive. Saved verbatim to
    /// `<cache>/<version>/<bin_subpath>/<filename>` and chmod 0755 on
    /// Unix. `bin_subpath` typically points to a `bin/` directory and
    /// the raw filename is taken from the URL's last segment.
    Raw,
}

/// One pinned upstream bundle: where to download it from, what version
/// it represents, the format of the archive, and the SHA-256 checksum
/// we verify after download.
#[derive(Clone, Debug)]
pub struct Bundle {
    /// Human-readable version label, used as the cache subdirectory
    /// name. Stable across re-runs so the cache hits.
    pub version: &'static str,
    /// Full HTTPS URL to fetch.
    pub url: &'static str,
    /// Archive format. See [`ArchiveKind`].
    pub kind: ArchiveKind,
    /// Hex-encoded SHA-256 of the downloaded file. `None` opts out of
    /// verification for v0.0.x when we don't yet have an authoritative
    /// checksum published by the vendor -- a TODO, not a permanent
    /// state.
    pub sha256: Option<&'static str>,
    /// After extraction, this relative path under the cache directory
    /// is the directory callers expect to contain the runnable
    /// binaries.
    pub bin_subpath: &'static str,
}

/// Errors raised by [`fetch_and_extract`].
#[derive(Debug, Error)]
pub enum FetchError {
    /// Filesystem / process I/O.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// HTTP download failed.
    #[error("download from {url}: {source}")]
    Download {
        /// URL we attempted to fetch.
        url: String,
        /// Underlying reqwest error.
        source: reqwest::Error,
    },
    /// Server returned a non-2xx response.
    #[error("download from {url} returned HTTP {status}")]
    BadStatus {
        /// URL we attempted to fetch.
        url: String,
        /// Status code returned.
        status: u16,
    },
    /// Downloaded file's SHA-256 did not match the pinned value.
    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// URL we fetched.
        url: String,
        /// Pinned hash we expected.
        expected: String,
        /// Hash we computed from the downloaded bytes.
        actual: String,
    },
    /// Zip extraction failed.
    #[error("extracting {path}: {message}")]
    Extract {
        /// Path of the zip we tried to extract.
        path: PathBuf,
        /// Error message from the zip library.
        message: String,
    },
}

/// Ensure `bundle` is extracted under `cache_root/<version>/` and return
/// the absolute path to the `bin` subdirectory. Idempotent: if the
/// extraction already exists, returns the path without re-downloading.
///
/// Layout:
/// ```text
/// cache_root/
///   <version>/
///     <bundle contents>/
///       <bin_subpath>/
///         postgres.exe, initdb.exe, psql.exe, ...
/// ```
pub async fn fetch_and_extract(
    cache_root: &Path,
    bundle: &Bundle,
    progress: &ProgressHandle,
) -> Result<PathBuf, FetchError> {
    let extracted_root = cache_root.join(bundle.version);
    let bin_dir = extracted_root.join(bundle.bin_subpath);
    let sentinel = extracted_root.join(".computeza-extracted");

    if fs::try_exists(&sentinel).await? {
        info!(
            version = bundle.version,
            cache = %extracted_root.display(),
            "binary bundle already extracted; skipping download"
        );
        progress.set_message(format!(
            "Using cached binaries at {}",
            extracted_root.display()
        ));
        return Ok(bin_dir);
    }

    fs::create_dir_all(&extracted_root).await?;
    let zip_path = extracted_root.join("download.zip");

    info!(
        url = bundle.url,
        version = bundle.version,
        target = %extracted_root.display(),
        "downloading binary bundle"
    );
    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!("Downloading {}", bundle.url));
    download_stream(bundle.url, &zip_path, progress).await?;

    if let Some(expected) = bundle.sha256 {
        progress.set_phase(InstallPhase::Verifying);
        progress.set_message("Verifying SHA-256");
        let actual = sha256_file(&zip_path).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            // Don't leave a half-trusted file on disk.
            let _ = fs::remove_file(&zip_path).await;
            return Err(FetchError::ChecksumMismatch {
                url: bundle.url.to_string(),
                expected: expected.to_string(),
                actual,
            });
        }
        info!(version = bundle.version, "bundle checksum OK");
    } else {
        warn!(
            url = bundle.url,
            "no SHA-256 pinned for this bundle; trusting TLS-only integrity. \
             TODO: pin a checksum before promoting beyond v0.0.x."
        );
    }

    info!(
        archive = %zip_path.display(),
        target = %extracted_root.display(),
        kind = ?bundle.kind,
        "extracting binary bundle"
    );
    progress.set_phase(InstallPhase::Extracting);
    progress.set_message(format!("Extracting to {}", extracted_root.display()));
    let archive_clone = zip_path.clone();
    let target_clone = extracted_root.clone();
    let kind = bundle.kind;
    let url = bundle.url.to_string();
    let bin_subpath = bundle.bin_subpath.to_string();
    tokio::task::spawn_blocking(move || {
        extract(kind, &archive_clone, &target_clone, &url, &bin_subpath)
    })
    .await
    .map_err(|e| FetchError::Io(io::Error::other(e)))??;

    // For zip/tar.gz we drop the downloaded archive once extracted.
    // For Raw bundles the "archive" IS the artifact -- extract()
    // already moved it to its final spot; no separate file to clean.
    if !matches!(bundle.kind, ArchiveKind::Raw) {
        let _ = fs::remove_file(&zip_path).await;
    }

    // Sentinel marks "this version is fully extracted; safe to reuse".
    fs::write(&sentinel, b"ok").await?;

    Ok(bin_dir)
}

async fn download_stream(
    url: &str,
    dest: &Path,
    progress: &ProgressHandle,
) -> Result<(), FetchError> {
    let resp = reqwest::Client::builder()
        // PostgreSQL bundles take a while; don't fail mid-stream on a
        // slow link. Per-chunk reads have their own timeouts inside
        // reqwest.
        .timeout(std::time::Duration::from_secs(60 * 30))
        .build()
        .map_err(|source| FetchError::Download {
            url: url.into(),
            source,
        })?
        .get(url)
        .send()
        .await
        .map_err(|source| FetchError::Download {
            url: url.into(),
            source,
        })?;
    if !resp.status().is_success() {
        return Err(FetchError::BadStatus {
            url: url.into(),
            status: resp.status().as_u16(),
        });
    }

    let total = resp.content_length();
    progress.set_bytes(0, total);

    let mut file = fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| FetchError::Download {
            url: url.into(),
            source,
        })?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        progress.set_bytes(downloaded, total);
    }
    file.flush().await?;
    Ok(())
}

async fn sha256_file(path: &Path) -> Result<String, FetchError> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn extract(
    kind: ArchiveKind,
    archive_path: &Path,
    dest: &Path,
    url: &str,
    bin_subpath: &str,
) -> Result<(), FetchError> {
    match kind {
        ArchiveKind::Zip => extract_zip(archive_path, dest),
        ArchiveKind::TarGz => extract_tar_gz(archive_path, dest),
        ArchiveKind::TarXz => extract_tar_xz(archive_path, dest),
        ArchiveKind::Raw => place_raw(archive_path, dest, url, bin_subpath),
    }
}

fn extract_zip(archive_path: &Path, dest: &Path) -> Result<(), FetchError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| FetchError::Extract {
        path: archive_path.to_path_buf(),
        message: e.to_string(),
    })?;
    archive.extract(dest).map_err(|e| FetchError::Extract {
        path: archive_path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, dest: &Path) -> Result<(), FetchError> {
    // Read the first 8 bytes so an "extract failed" can tell the
    // operator at a glance whether the downloaded file is even a
    // gzip stream, an HTML error page, or empty. The gzip magic
    // is 0x1f 0x8b -- anything else means the upstream URL didn't
    // actually serve a tar.gz (CDN redirect dropped, mirror down,
    // hot link block, etc.).
    let file_size = std::fs::metadata(archive_path).map(|m| m.len()).unwrap_or(0);
    let head: [u8; 8] = {
        let mut buf = [0u8; 8];
        if let Ok(mut f) = std::fs::File::open(archive_path) {
            use std::io::Read;
            let _ = f.read(&mut buf);
        }
        buf
    };
    if head[0] != 0x1f || head[1] != 0x8b {
        return Err(FetchError::Extract {
            path: archive_path.to_path_buf(),
            message: format!(
                "downloaded file is not a gzip stream (got {file_size} bytes; \
                 first 8 = {head:02x?}). The upstream URL may have redirected \
                 to an HTML error page, or the mirror is rate-limiting. \
                 Try `curl -I` on the bundle URL to confirm the response shape."
            ),
        });
    }
    let file = std::fs::File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest).map_err(|e| FetchError::Extract {
        path: archive_path.to_path_buf(),
        message: format!("{e} (file size: {file_size} bytes)"),
    })?;
    Ok(())
}

fn extract_tar_xz(archive_path: &Path, dest: &Path) -> Result<(), FetchError> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = liblzma::read::XzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest).map_err(|e| FetchError::Extract {
        path: archive_path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(())
}

/// Move the downloaded "archive" (actually a raw binary) into
/// `<dest>/<bin_subpath>/<filename>` and set executable permission
/// on Unix. The filename is taken from the URL's last path segment.
fn place_raw(
    archive_path: &Path,
    dest: &Path,
    url: &str,
    bin_subpath: &str,
) -> Result<(), FetchError> {
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("binary");
    let target_dir = dest.join(bin_subpath);
    std::fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(filename);
    std::fs::rename(archive_path, &target).or_else(|_| {
        // Cross-device rename can fail; fall back to copy + remove.
        std::fs::copy(archive_path, &target).map(|_| ())?;
        let _ = std::fs::remove_file(archive_path);
        Ok::<_, FetchError>(())
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_carries_pinned_metadata() {
        let b = Bundle {
            version: "17.2-3",
            url: "https://example.invalid/x.zip",
            kind: ArchiveKind::Zip,
            sha256: Some("deadbeef"),
            bin_subpath: "pgsql/bin",
        };
        assert_eq!(b.version, "17.2-3");
        assert_eq!(b.bin_subpath, "pgsql/bin");
        assert!(b.sha256.is_some());
    }
}
