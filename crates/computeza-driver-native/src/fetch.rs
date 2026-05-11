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

/// One pinned upstream bundle: where to download it from, what version
/// it represents, and the SHA-256 checksum we verify after download.
#[derive(Clone, Debug)]
pub struct Bundle {
    /// Human-readable version label, used as the cache subdirectory
    /// name. Stable across re-runs so the cache hits.
    pub version: &'static str,
    /// Full HTTPS URL to fetch. Must be a zip archive.
    pub url: &'static str,
    /// Hex-encoded SHA-256 of the zip. `None` opts out of verification
    /// for v0.0.x when we don't yet have an authoritative checksum
    /// published by the vendor -- a TODO, not a permanent state.
    pub sha256: Option<&'static str>,
    /// After extraction, this relative path under the cache directory
    /// is what callers actually want (typically `pgsql/bin`).
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
pub async fn fetch_and_extract(cache_root: &Path, bundle: &Bundle) -> Result<PathBuf, FetchError> {
    let extracted_root = cache_root.join(bundle.version);
    let bin_dir = extracted_root.join(bundle.bin_subpath);
    let sentinel = extracted_root.join(".computeza-extracted");

    if fs::try_exists(&sentinel).await? {
        info!(
            version = bundle.version,
            cache = %extracted_root.display(),
            "binary bundle already extracted; skipping download"
        );
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
    download_stream(bundle.url, &zip_path).await?;

    if let Some(expected) = bundle.sha256 {
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
        "extracting binary bundle"
    );
    let zip_clone = zip_path.clone();
    let target_clone = extracted_root.clone();
    tokio::task::spawn_blocking(move || extract_zip(&zip_clone, &target_clone))
        .await
        .map_err(|e| FetchError::Io(io::Error::other(e)))??;

    // Drop the zip; the extracted tree is what we keep.
    let _ = fs::remove_file(&zip_path).await;

    // Sentinel marks "this version is fully extracted; safe to reuse".
    fs::write(&sentinel, b"ok").await?;

    Ok(bin_dir)
}

async fn download_stream(url: &str, dest: &Path) -> Result<(), FetchError> {
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

    let mut file = fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| FetchError::Download {
            url: url.into(),
            source,
        })?;
        file.write_all(&chunk).await?;
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

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), FetchError> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| FetchError::Extract {
        path: zip_path.to_path_buf(),
        message: e.to_string(),
    })?;
    archive.extract(dest).map_err(|e| FetchError::Extract {
        path: zip_path.to_path_buf(),
        message: e.to_string(),
    })?;
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
            sha256: Some("deadbeef"),
            bin_subpath: "pgsql/bin",
        };
        assert_eq!(b.version, "17.2-3");
        assert_eq!(b.bin_subpath, "pgsql/bin");
        assert!(b.sha256.is_some());
    }
}
