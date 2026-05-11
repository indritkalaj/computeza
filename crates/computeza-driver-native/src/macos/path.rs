//! Cross-shell PATH registration for managed binaries on macOS.
//!
//! The macOS analogue of the Linux `/usr/local/bin` symlink approach.
//! Two complications vs Linux:
//!
//! 1. **Apple Silicon vs Intel**: Homebrew puts binaries under
//!    `/opt/homebrew/bin` on Apple Silicon and `/usr/local/bin` on
//!    Intel. Both are typically on the default $PATH. We pick the one
//!    whose parent directory is writable (after a sudo `install` step)
//!    by the installer.
//! 2. **`/etc/paths.d/`**: an alternative to symlinks that lets new
//!    login shells pick up the binary without modifying any individual
//!    rc file. We use it AS WELL AS the symlink so already-open shells
//!    work too (the symlink is the immediate-effect mechanism).

use std::{
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt};
use tracing::{debug, info};

/// Candidate symlink directories tried in order of preference.
const CANDIDATE_BIN_DIRS: &[&str] = &[
    "/usr/local/bin", // Intel default, also typically on Apple Silicon for system tools
    "/opt/homebrew/bin", // Apple Silicon Homebrew
];

/// Errors during PATH registration.
#[derive(Debug, Error)]
pub enum PathError {
    /// Filesystem operation failed.
    #[error("filesystem: {0}")]
    Io(#[from] io::Error),
    /// No writable candidate symlink directory was found.
    #[error("no writable symlink dir; tried: {0:?}")]
    NoWritableDir(Vec<PathBuf>),
    /// Binary referenced by the registration call does not exist.
    #[error("binary not found: {0}")]
    BinaryNotFound(PathBuf),
}

/// Register a single binary under `/{usr/local,opt/homebrew}/bin/computeza-<name>`
/// and drop a `/etc/paths.d/computeza-<name>` file pointing at the
/// binary's parent directory.
pub async fn register(name: &str, target_bin: &Path) -> Result<PathBuf, PathError> {
    if !fs::try_exists(target_bin).await? {
        return Err(PathError::BinaryNotFound(target_bin.to_path_buf()));
    }

    // 1. Symlink under whichever bin dir is writable.
    let bin_dir = pick_writable_bin_dir().await?;
    let link_path = bin_dir.join(format!("computeza-{name}"));
    let tmp = link_path.with_extension("tmp-computeza");
    let _ = fs::remove_file(&tmp).await;
    symlink(target_bin, &tmp).await?;
    fs::rename(&tmp, &link_path).await?;
    info!(link = %link_path.display(), target = %target_bin.display(), "registered macOS PATH symlink");

    // 2. /etc/paths.d/ entry pointing at the parent dir, so new login
    //    shells pick it up via path_helper(8).
    if let Some(parent) = target_bin.parent() {
        let paths_d = PathBuf::from("/etc/paths.d").join(format!("computeza-{name}"));
        match fs::create_dir_all("/etc/paths.d").await {
            Ok(()) => {
                if let Ok(mut f) = fs::File::create(&paths_d).await {
                    let _ = f.write_all(parent.to_string_lossy().as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                    let _ = f.sync_all().await;
                    debug!(file = %paths_d.display(), "wrote /etc/paths.d entry");
                }
            }
            Err(e) => debug!(error = %e, "could not create /etc/paths.d; relying on symlink only"),
        }
    }

    Ok(link_path)
}

/// Reverse of [`register`]: remove the symlink + /etc/paths.d entry if
/// they exist. Best-effort, never fails on "not present".
pub async fn unregister(name: &str) -> Result<(), PathError> {
    for d in CANDIDATE_BIN_DIRS {
        let link = PathBuf::from(d).join(format!("computeza-{name}"));
        match fs::remove_file(&link).await {
            Ok(()) => debug!(link = %link.display(), "removed PATH symlink"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(PathError::Io(e)),
        }
    }
    let paths_d = PathBuf::from("/etc/paths.d").join(format!("computeza-{name}"));
    let _ = fs::remove_file(&paths_d).await;
    Ok(())
}

async fn pick_writable_bin_dir() -> Result<PathBuf, PathError> {
    let mut tried = Vec::new();
    for d in CANDIDATE_BIN_DIRS {
        let path = PathBuf::from(d);
        tried.push(path.clone());
        if !fs::try_exists(&path).await.unwrap_or(false) {
            continue;
        }
        // Test writability by attempting to create + remove a sentinel file.
        let probe = path.join(".computeza-write-probe");
        if let Ok(mut f) = fs::File::create(&probe).await {
            let _ = f.write_all(b"").await;
            let _ = fs::remove_file(&probe).await;
            return Ok(path);
        }
    }
    Err(PathError::NoWritableDir(tried))
}

async fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    let target = target.to_path_buf();
    let link = link.to_path_buf();
    tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(target, link))
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
}
