//! Progress reporting for long-running install operations.
//!
//! The Windows PostgreSQL install path can take 1-10 minutes -- most
//! of that is the EDB binary download. A single blocking HTTP request
//! would hold the operator's browser hostage; instead the UI server
//! spawns the install as a background tokio task and tracks progress
//! through one of these [`ProgressHandle`]s.
//!
//! Driver code calls `handle.set_phase(...)`, `handle.set_bytes(...)`,
//! `handle.set_message(...)`; the UI server reads the same snapshot
//! and serializes it for the wizard's JS poller. The handle is cheap
//! to clone and the no-op variant has zero allocations, so call sites
//! that don't care (CLI, tests) can pass `ProgressHandle::noop()`
//! without paying any cost.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Coarse-grained phase of an install. The wizard renders one row per
/// phase; values are kebab-case in JSON so the front-end can use them
/// directly as CSS classes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallPhase {
    /// Task spawned but not yet started.
    #[default]
    Queued,
    /// Looking for a pre-existing PostgreSQL install or a previously-
    /// downloaded bundle in our cache.
    DetectingBinaries,
    /// Streaming the binary bundle zip from the upstream vendor. Bytes
    /// counters in [`InstallProgress`] are most meaningful here.
    Downloading,
    /// Verifying the SHA-256 of the downloaded zip against the pinned
    /// value (or warning that no pin exists).
    Verifying,
    /// Unpacking the downloaded zip to the cache directory.
    Extracting,
    /// Running initdb to create the cluster's data dir.
    Initdb,
    /// Registering the OS service (Windows SCM, systemd, launchd).
    RegisteringService,
    /// Asking the service manager to start the daemon.
    StartingService,
    /// Polling the daemon's TCP port until it accepts connections.
    WaitingForReady,
    /// Adding the CLI tools (psql) to PATH.
    RegisteringPath,
    /// All steps completed successfully.
    Done,
    /// One of the steps failed; see `error`.
    Failed,
}

impl InstallPhase {
    /// Human-readable label for the wizard's phase row. English only;
    /// localization moves through `computeza-i18n` once the wizard's
    /// progress strings are added to the bundle.
    pub fn label(&self) -> &'static str {
        match self {
            InstallPhase::Queued => "Queued",
            InstallPhase::DetectingBinaries => "Detecting existing install",
            InstallPhase::Downloading => "Downloading binaries",
            InstallPhase::Verifying => "Verifying checksum",
            InstallPhase::Extracting => "Extracting archive",
            InstallPhase::Initdb => "Initializing data directory",
            InstallPhase::RegisteringService => "Registering service",
            InstallPhase::StartingService => "Starting service",
            InstallPhase::WaitingForReady => "Waiting for readiness",
            InstallPhase::RegisteringPath => "Registering PATH",
            InstallPhase::Done => "Done",
            InstallPhase::Failed => "Failed",
        }
    }
}

/// Current snapshot of an in-flight install. Serialised verbatim by
/// the UI server's `/api/install/job/{id}` endpoint and polled by the
/// wizard's JS.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InstallProgress {
    /// Which coarse step the installer is on.
    pub phase: InstallPhase,
    /// Latest message line from the driver, for the wizard's "what's
    /// happening" subtitle.
    pub message: String,
    /// Bytes pulled from the network so far (only meaningful during
    /// [`InstallPhase::Downloading`]).
    pub bytes_downloaded: u64,
    /// Total bytes for the current download. `None` when the server
    /// did not send a Content-Length header.
    pub total_bytes: Option<u64>,
    /// True once the task has either finished or failed -- the wizard
    /// uses this to stop polling.
    pub completed: bool,
    /// Human-readable error chain when the task failed.
    pub error: Option<String>,
    /// Multi-line summary written by the installer when the task
    /// succeeded (bin_dir, data_dir, service name, port, etc.).
    pub success_summary: Option<String>,
    /// Wall-clock when the task transitioned out of `Queued`.
    pub started_at: Option<DateTime<Utc>>,
    /// Wall-clock when the task completed (success or failure).
    pub finished_at: Option<DateTime<Utc>>,
}

impl InstallProgress {
    /// Compute a 0.0-1.0 progress ratio for the current phase. The
    /// download phase derives from bytes; all other phases return 0.5
    /// (indeterminate -- shows the bar as "moving") until they finish.
    pub fn phase_ratio(&self) -> f64 {
        match self.phase {
            InstallPhase::Downloading => match (self.total_bytes, self.bytes_downloaded) {
                (Some(total), down) if total > 0 => (down as f64 / total as f64).clamp(0.0, 1.0),
                _ => 0.0,
            },
            InstallPhase::Done => 1.0,
            InstallPhase::Failed => 0.0,
            _ => 0.5,
        }
    }
}

/// Handle threaded into the driver. Cheap to clone; the no-op variant
/// is also `Send + Sync` and does not allocate, so call sites that
/// don't care (CLI, tests) can pass [`ProgressHandle::noop`] freely.
#[derive(Clone, Default)]
pub struct ProgressHandle {
    inner: Option<Arc<Mutex<InstallProgress>>>,
}

impl ProgressHandle {
    /// Build a handle that drops every update. Use for CLI and tests.
    pub fn noop() -> Self {
        Self { inner: None }
    }

    /// Build a handle backed by the given shared state. The UI server
    /// keeps one of these per job and reads it from the polling
    /// endpoint.
    pub fn new(state: Arc<Mutex<InstallProgress>>) -> Self {
        Self { inner: Some(state) }
    }

    /// Get a clone of the current snapshot. Returns `None` for no-op
    /// handles.
    pub fn snapshot(&self) -> Option<InstallProgress> {
        self.inner.as_ref().map(|m| m.lock().unwrap().clone())
    }

    /// Update the coarse phase. Idempotent: setting the same phase
    /// twice is a no-op.
    pub fn set_phase(&self, phase: InstallPhase) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            if g.phase == phase {
                return;
            }
            g.phase = phase;
            if g.started_at.is_none() && phase != InstallPhase::Queued {
                g.started_at = Some(Utc::now());
            }
        }
    }

    /// Update the "what's happening" subtitle.
    pub fn set_message(&self, msg: impl Into<String>) {
        if let Some(m) = &self.inner {
            m.lock().unwrap().message = msg.into();
        }
    }

    /// Update the download counters. `total` may be `None` if the
    /// server omitted Content-Length.
    pub fn set_bytes(&self, downloaded: u64, total: Option<u64>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            g.bytes_downloaded = downloaded;
            if total.is_some() {
                g.total_bytes = total;
            }
        }
    }

    /// Mark the task complete with the given success summary. After
    /// this point the wizard stops polling.
    pub fn finish_success(&self, summary: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            g.phase = InstallPhase::Done;
            g.success_summary = Some(summary.into());
            g.completed = true;
            g.finished_at = Some(Utc::now());
        }
    }

    /// Mark the task failed with the given error chain.
    pub fn finish_failure(&self, error: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            g.phase = InstallPhase::Failed;
            g.error = Some(error.into());
            g.completed = true;
            g.finished_at = Some(Utc::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_handle_drops_updates() {
        let h = ProgressHandle::noop();
        h.set_phase(InstallPhase::Downloading);
        h.set_bytes(100, Some(1000));
        h.set_message("hello");
        assert!(h.snapshot().is_none());
    }

    #[test]
    fn real_handle_records_updates() {
        let state = Arc::new(Mutex::new(InstallProgress::default()));
        let h = ProgressHandle::new(state.clone());
        h.set_phase(InstallPhase::Downloading);
        h.set_bytes(100, Some(1000));
        h.set_message("hello");
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.phase, InstallPhase::Downloading);
        assert_eq!(snap.bytes_downloaded, 100);
        assert_eq!(snap.total_bytes, Some(1000));
        assert_eq!(snap.message, "hello");
        assert!(snap.started_at.is_some());
        assert!(!snap.completed);
    }

    #[test]
    fn phase_ratio_is_proportional_during_download() {
        let mut p = InstallProgress {
            phase: InstallPhase::Downloading,
            bytes_downloaded: 250,
            total_bytes: Some(1000),
            ..Default::default()
        };
        assert!((p.phase_ratio() - 0.25).abs() < 1e-9);
        p.bytes_downloaded = 1500;
        assert_eq!(p.phase_ratio(), 1.0); // clamped
    }

    #[test]
    fn finish_success_sets_completed() {
        let state = Arc::new(Mutex::new(InstallProgress::default()));
        let h = ProgressHandle::new(state);
        h.finish_success("bin_dir: C:\\foo");
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.phase, InstallPhase::Done);
        assert!(snap.completed);
        assert_eq!(snap.success_summary.as_deref(), Some("bin_dir: C:\\foo"));
        assert!(snap.finished_at.is_some());
    }
}
