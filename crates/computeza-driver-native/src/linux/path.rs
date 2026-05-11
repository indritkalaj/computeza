//! Cross-shell PATH registration for managed binaries on Linux.
//!
//! When the installer lays down a component that exposes a user-facing
//! CLI (`psql`, `kanidm`, `garage`), we want that CLI on `$PATH` for
//! every login shell on the host. The two clean ways to do that:
//!
//! 1. Drop a script into `/etc/profile.d/` — picked up by bash/zsh/sh
//!    at next login. Doesn't affect already-open shells; doesn't work
//!    with fish without an extra `conf.d` drop.
//! 2. Drop a symlink into `/usr/local/bin/` (which is on every Linux
//!    distro's default `$PATH`). Affects every shell immediately, no
//!    re-login needed.
//!
//! We use option 2 — symlinks — because of the immediate-effect property.
//! Symlinks are prefixed with `computeza-` to avoid colliding with
//! distro-shipped binaries of the same name (e.g. system `psql` from a
//! package install).
//!
//! Uninstall is symmetric: the same function with `enabled=false`
//! removes any symlinks it previously created.

use std::{
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use tokio::fs;
use tracing::{debug, info};

/// Standard location for our symlinks. Every Linux distro has this on the
/// default `$PATH`.
const TARGET_DIR: &str = "/usr/local/bin";

/// Errors during PATH registration.
#[derive(Debug, Error)]
pub enum PathError {
    /// File system operation failed (typically: not running as root).
    #[error("filesystem: {0}")]
    Io(#[from] io::Error),
    /// A binary referenced by the registration call does not exist on disk.
    #[error("binary not found: {0}")]
    BinaryNotFound(PathBuf),
}

/// Register a single binary on `$PATH` via a symlink under
/// `/usr/local/bin/computeza-<name>`.
///
/// Idempotent: if the symlink already exists and points at the right
/// target, nothing happens. If it exists but points somewhere else, it
/// is repointed atomically.
pub async fn register(name: &str, target_bin: &Path) -> Result<PathBuf, PathError> {
    if !fs::try_exists(target_bin).await? {
        return Err(PathError::BinaryNotFound(target_bin.to_path_buf()));
    }
    let link_path = PathBuf::from(TARGET_DIR).join(format!("computeza-{name}"));

    // Atomic re-link: write to a tempname, then rename over.
    let tmp = link_path.with_extension("tmp-computeza");
    // Best-effort remove if a leftover exists.
    let _ = fs::remove_file(&tmp).await;
    symlink(target_bin, &tmp).await?;
    fs::rename(&tmp, &link_path).await?;
    info!(link = %link_path.display(), target = %target_bin.display(), "registered PATH symlink");
    Ok(link_path)
}

/// Reverse of [`register`]: remove the symlink if it exists. Returns Ok
/// even if there was nothing to remove.
pub async fn unregister(name: &str) -> Result<(), PathError> {
    let link_path = PathBuf::from(TARGET_DIR).join(format!("computeza-{name}"));
    match fs::remove_file(&link_path).await {
        Ok(()) => {
            debug!(link = %link_path.display(), "removed PATH symlink");
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PathError::Io(e)),
    }
}

#[cfg(unix)]
async fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    let target = target.to_path_buf();
    let link = link.to_path_buf();
    tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(target, link))
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
}
