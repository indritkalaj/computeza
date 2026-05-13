//! Release-directory + `current` symlink swap pattern for
//! source-built and bundled-binary components.
//!
//! The naive "replace `<root>/bin/<binary>` in place" pattern that
//! the early drivers used had three problems:
//!
//! 1. `Text file busy` (ETXTBSY) when the binary is mmap'd by a
//!    running process. We mitigated this with `systemctl stop` +
//!    atomic rename, but rollback was still hand-rolled.
//! 2. No version history on disk. Once a release was overwritten,
//!    rolling back required rebuilding from an older git tag.
//! 3. No pre-flight check on the new binary. If `cargo build`
//!    produced a binary that segfaults at startup, the installer
//!    swapped it in anyway and the operator saw an obscure
//!    systemd-side failure.
//!
//! This module ships the release-directory pattern that
//! Capistrano, AWS CodeDeploy, dpkg-update-alternatives, and
//! Kubernetes Deployments have all converged on:
//!
//! ```text
//! /var/lib/computeza/<component>/
//! |-- releases/
//! |   |-- 20260513T08-00-00-v1.10.1/
//! |   |   |-- kanidmd                 <- binary
//! |   |   |-- src -> ../../src/...    <- optional: pointer at the cached source tree
//! |   |   |-- manifest.json           <- {version, built_at, sha256?}
//! |   |-- 20260514T11-22-33-v1.10.2/
//! |   |   `-- ...
//! |-- current -> releases/20260514T11-22-33-v1.10.2
//! |-- src/                            <- source-tree cache (shared across releases)
//! |-- data/                           <- runtime state (untouched by releases)
//! `-- config.toml                     <- operator-managed (untouched by releases)
//! ```
//!
//! The driver writes the new binary into a fresh release dir,
//! optionally runs a pre-flight probe (`<new>/binary --version`),
//! atomic-swaps the `current` symlink, and prunes old releases
//! down to a configurable retention count. systemd ExecStart
//! always references `<root>/current/<binary>`, so the daemon
//! always sees the active release no matter how many times the
//! operator re-installs.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{fs, process::Command};

/// Errors raised while creating, swapping, or pruning releases.
#[derive(Debug, Error)]
pub enum ReleaseError {
    /// Filesystem I/O failure -- creating dirs, writing manifests,
    /// renaming symlinks. The wrapped [`std::io::Error`] carries
    /// the underlying cause and (when available) the offending
    /// path.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The pre-flight probe (`<binary> <args>`) returned a non-zero
    /// exit code. Carries the full stderr so the operator can see
    /// why the new binary failed before we swap it in.
    #[error(
        "pre-flight probe failed for {binary} (exit {code:?}); aborting before symlink swap so the previous release stays current. Full stderr:\n{stderr}"
    )]
    PreflightFailed {
        /// Binary that was probed (relative to the release dir).
        binary: String,
        /// Exit code reported by the probe (None if the process was
        /// signalled before exiting normally).
        code: Option<i32>,
        /// Captured stderr from the probe invocation.
        stderr: String,
    },
}

/// A freshly-allocated release directory under
/// `<root>/releases/<id>`. The driver writes its binary + any
/// per-release artefacts inside `dir`, then calls
/// [`make_current`] to atomic-swap the active release.
#[derive(Clone, Debug)]
pub struct Release {
    /// Component root (e.g. `/var/lib/computeza/kanidm`).
    pub root: PathBuf,
    /// Release identifier: ISO-8601-ish timestamp + version.
    /// Lexicographically sortable so `ls releases/` gives chrono
    /// order without any extra metadata.
    pub id: String,
    /// Full path to the release directory:
    /// `<root>/releases/<id>`. Created (empty) before this
    /// constructor returns.
    pub dir: PathBuf,
}

/// Metadata written at `<release_dir>/manifest.json` so future
/// installs and audit tooling can identify which version a given
/// release directory holds without parsing the directory name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// Upstream version string (e.g. `"1.10.1"`).
    pub version: String,
    /// When the release was laid down. Same TZ semantics as the
    /// rest of the audit log: stored UTC, rendered in the
    /// operator's local TZ in the UI.
    pub built_at: chrono::DateTime<Utc>,
    /// Optional SHA-256 of the primary binary, when the upstream
    /// publishes one (or when we compute it locally for source
    /// builds). Hex-encoded.
    pub binary_sha256: Option<String>,
}

/// Allocate a fresh release directory under `<root>/releases/`.
/// The id format is `YYYYMMDDTHHMMSS-<version>` so `ls
/// releases/` gives lexicographic chrono order. The release dir
/// is created empty; the caller populates it.
pub async fn new_release(root: &Path, version: &str) -> Result<Release, ReleaseError> {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S");
    let id = format!("{timestamp}-v{version}");
    let dir = root.join("releases").join(&id);
    fs::create_dir_all(&dir).await?;
    Ok(Release {
        root: root.to_path_buf(),
        id,
        dir,
    })
}

/// Write the release manifest to `<release_dir>/manifest.json`.
/// Drivers call this after their binary + auxiliary files are in
/// place, just before the symlink swap.
pub async fn write_manifest(
    release: &Release,
    manifest: &ReleaseManifest,
) -> Result<(), ReleaseError> {
    let path = release.dir.join("manifest.json");
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| ReleaseError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, &json).await?;
    Ok(())
}

/// Pre-flight probe: run a binary inside the release dir with the
/// given args. Used to verify that a freshly-built binary at
/// least loads its dynamic libraries and parses its `--version`
/// (or `--help`) flag before we atomic-swap it into `current`.
///
/// We deliberately don't run the daemon's actual `serve`-style
/// subcommand because that would bind a port and we'd race the
/// already-running release. `--version` is the canonical
/// "minimal startup check" that ELF / library / glibc-version
/// problems will trip on.
pub async fn preflight_probe(
    release: &Release,
    binary: &str,
    args: &[&str],
) -> Result<(), ReleaseError> {
    let path = release.dir.join(binary);
    let out = Command::new(&path).args(args).output().await?;
    if !out.status.success() {
        return Err(ReleaseError::PreflightFailed {
            binary: binary.into(),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Atomic-swap `<root>/current` to point at this release.
///
/// On Linux `rename(2)` is atomic when both paths are on the same
/// filesystem. We write a temporary symlink at
/// `<root>/current.new` and rename it over `<root>/current`; the
/// kernel guarantees that any process reading `<root>/current` at
/// any instant sees either the old or new target, never an empty
/// or half-written entry.
pub async fn make_current(release: &Release) -> Result<(), ReleaseError> {
    let current = release.root.join("current");
    let staging = release.root.join("current.new");
    // The symlink target is relative so the release directory
    // tree is relocatable (an operator copying the install root
    // to a new path won't break the link).
    let target = Path::new("releases").join(&release.id);

    // Best-effort cleanup of a stale staging symlink from a prior
    // crashed install. We don't care if remove fails.
    let _ = fs::remove_file(&staging).await;

    // Create the new symlink, then rename it over the live one.
    // tokio doesn't have an async symlink helper; we use the
    // blocking stdlib variant inside spawn_blocking to keep the
    // executor responsive on slow filesystems.
    let staging_clone = staging.clone();
    let target_clone = target.clone();
    tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(&target_clone, &staging_clone))
        .await
        .map_err(|e| ReleaseError::Io(std::io::Error::other(e.to_string())))??;

    fs::rename(&staging, &current).await?;
    Ok(())
}

/// Retention: list every release directory under
/// `<root>/releases/`, sort lexicographically (== chronologically
/// thanks to the id format), and remove all but the most recent
/// `keep`. Returns the paths that were actually deleted so the
/// driver can surface them via tracing.
///
/// The release currently pointed at by `<root>/current` is never
/// deleted, even if it's older than the cut-off. That guards
/// against the edge case where the operator rolled back to an
/// older release and the next install would otherwise prune the
/// active version.
pub async fn prune_releases(root: &Path, keep: usize) -> Result<Vec<PathBuf>, ReleaseError> {
    let releases_dir = root.join("releases");
    if !fs::try_exists(&releases_dir).await.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let current_target = current_release(root).await;

    let mut entries: Vec<PathBuf> = Vec::new();
    let mut iter = fs::read_dir(&releases_dir).await?;
    while let Ok(Some(entry)) = iter.next_entry().await {
        if entry.file_type().await.ok().is_some_and(|t| t.is_dir()) {
            entries.push(entry.path());
        }
    }
    entries.sort();

    if entries.len() <= keep {
        return Ok(Vec::new());
    }
    let prune_count = entries.len() - keep;
    let mut removed = Vec::new();
    for path in entries.into_iter().take(prune_count) {
        if current_target
            .as_ref()
            .is_some_and(|c| path.file_name() == c.file_name())
        {
            // Active release; never prune.
            continue;
        }
        if fs::remove_dir_all(&path).await.is_ok() {
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Resolve `<root>/current` and return the release-id segment
/// (e.g. `releases/20260513T08-00-00-v1.10.1`). Returns `None`
/// when the symlink doesn't exist (= no release ever activated).
pub async fn current_release(root: &Path) -> Option<PathBuf> {
    let current = root.join("current");
    fs::read_link(&current).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "computeza-test-release-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn new_release_creates_a_versioned_dir() {
        let root = tempdir("new");
        let r = new_release(&root, "1.10.1").await.unwrap();
        assert!(r.dir.exists(), "release dir must be created");
        assert!(r.id.contains("v1.10.1"));
        assert!(r.id.starts_with(char::is_numeric));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn make_current_swaps_the_symlink_atomically() {
        let root = tempdir("swap");
        let r1 = new_release(&root, "1.0.0").await.unwrap();
        make_current(&r1).await.unwrap();
        let current_target = std::fs::read_link(root.join("current")).unwrap();
        assert!(
            current_target.to_string_lossy().contains("v1.0.0"),
            "current should point at the v1.0.0 release; got {}",
            current_target.display()
        );

        // Swap to a newer release; symlink should re-point.
        let r2 = new_release(&root, "1.1.0").await.unwrap();
        make_current(&r2).await.unwrap();
        let current_target = std::fs::read_link(root.join("current")).unwrap();
        assert!(
            current_target.to_string_lossy().contains("v1.1.0"),
            "current should follow the swap to v1.1.0; got {}",
            current_target.display()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prune_releases_keeps_n_most_recent_plus_current() {
        let root = tempdir("prune");
        // Plant five releases.
        let mut ids = Vec::new();
        for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.4.0"] {
            // Tiny sleep so the timestamp portion of the id is
            // distinct (resolution is seconds).
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            let r = new_release(&root, v).await.unwrap();
            ids.push(r.id);
        }
        // Make the OLDEST current so the prune respects it.
        let oldest = root.join("releases").join(&ids[0]);
        let _ = tokio::task::spawn_blocking({
            let oldest_clone = oldest.clone();
            let current = root.join("current");
            move || {
                let relative =
                    std::path::Path::new("releases").join(oldest_clone.file_name().unwrap());
                std::os::unix::fs::symlink(relative, current)
            }
        })
        .await
        .unwrap();

        let removed = prune_releases(&root, 2).await.unwrap();
        // 5 releases, keep 2 → would prune 3, but the oldest is
        // current, so only 2 actually get removed.
        assert_eq!(
            removed.len(),
            2,
            "should prune the two non-current oldest, keeping current + 2 newest"
        );
        // The current release must still exist on disk.
        assert!(
            oldest.exists(),
            "active release must NEVER be pruned even when it's older than the cut-off"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_probe_fails_loudly_for_nonzero_exit() {
        let root = tempdir("preflight");
        let r = new_release(&root, "0.0.1").await.unwrap();
        // Lay down a "binary" that exits non-zero.
        let fake = r.dir.join("daemon");
        std::fs::write(&fake, "#!/bin/sh\necho boom >&2\nexit 7\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = preflight_probe(&r, "daemon", &["--version"])
            .await
            .expect_err("non-zero exit must surface as a ReleaseError");
        match err {
            ReleaseError::PreflightFailed { code, stderr, .. } => {
                assert_eq!(code, Some(7));
                assert!(stderr.contains("boom"));
            }
            other => panic!("expected PreflightFailed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn write_manifest_round_trips_through_json() {
        let root = tempdir("manifest");
        let r = new_release(&root, "2.0.0").await.unwrap();
        let m = ReleaseManifest {
            version: "2.0.0".into(),
            built_at: Utc::now(),
            binary_sha256: Some("deadbeef".into()),
        };
        write_manifest(&r, &m).await.unwrap();
        let bytes = std::fs::read(r.dir.join("manifest.json")).unwrap();
        let parsed: ReleaseManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.version, "2.0.0");
        assert_eq!(parsed.binary_sha256.as_deref(), Some("deadbeef"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
