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
///
/// The `phase` / `bytes_*` / `message` fields always reflect the
/// *currently-running component*. For a single-component install
/// (the per-component pages at `/install/<slug>`) that's the whole
/// story. For a multi-component install (the unified `/install`
/// flow) `components` carries the per-slug breakdown -- pending /
/// running / done / failed -- and the top-level fields track the
/// component whose `state == Running`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InstallProgress {
    /// Which coarse step the installer is on.
    pub phase: InstallPhase,
    /// Latest message line from the driver, for the wizard's "what's
    /// happening" subtitle.
    pub message: String,
    /// Full append-only log of every status line the driver produced
    /// (each `set_message` call adds one entry). The wizard renders
    /// this in an expandable scrollable panel below the progress bar
    /// so the operator can see exactly what happened during a slow
    /// step like `cargo install kanidmd` or the 270MB postgres
    /// bundle download.
    pub log: Vec<String>,
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
    /// Per-component state for a multi-component install (the unified
    /// `/install` flow). Empty for single-component installs. Order
    /// matches the install dispatch order so the wizard renders a
    /// stable top-to-bottom checklist.
    #[serde(default)]
    pub components: Vec<ComponentProgress>,
    /// Generated credentials for this install run (initial admin
    /// passwords, API tokens, ...). Never serialized to JSON -- the
    /// polling endpoint must not leak plaintext secrets -- and drained
    /// after the operator views the result page exactly once.
    #[serde(skip)]
    pub generated_credentials: Vec<GeneratedCredential>,
    /// Cached copy of [`Self::generated_credentials`] stashed at result-
    /// page render time so a single follow-up call to
    /// `GET /install/credentials.json/{job_id}` can drain + return the
    /// same bag as a downloadable JSON document. Drained on first
    /// download so the file is truly one-shot, matching the view-once
    /// contract of the on-page table.
    #[serde(skip)]
    pub credentials_for_download: Option<Vec<GeneratedCredential>>,
    /// True when this progress record represents an UNINSTALL flow
    /// (rollback or per-component teardown) rather than an install.
    /// The wizard chrome flips labels accordingly: "Installing" →
    /// "Uninstalling", "Install completed" → "Uninstall completed",
    /// the result page calls render_uninstall_result, etc.
    #[serde(default)]
    pub is_rollback: bool,
}

/// One credential generated by the installer that the operator needs
/// to capture (e.g. an initial admin password). Always plaintext --
/// the encrypted persistence is the secrets store's job. Lives only
/// in [`InstallProgress::generated_credentials`] until the result
/// page renders, then gets drained so it can't be displayed twice.
#[derive(Clone, Debug)]
pub struct GeneratedCredential {
    /// Which component the credential belongs to (e.g. "postgres").
    pub component: String,
    /// Label for the credential (e.g. "superuser password",
    /// "initial admin password").
    pub label: String,
    /// Plaintext credential value.
    pub value: String,
    /// Optional username / role the credential pairs with.
    pub username: Option<String>,
    /// Optional reference into the secrets store
    /// (e.g. "postgres/admin-password") so the operator can recover
    /// the credential via the rotate / view UI once it lands.
    pub secret_ref: Option<String>,
}

/// Per-component progress entry inside [`InstallProgress::components`]
/// for a multi-component install. The wizard renders one row per
/// entry, highlighting whichever is currently `Running`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ComponentProgress {
    /// Component slug (e.g. "postgres", "kanidm") -- matches the
    /// dispatch table in the UI server's `install_all_handler`.
    pub slug: String,
    /// Coarse state: pending, running, done, or failed.
    pub state: ComponentState,
    /// Multi-line success summary for this component (set when
    /// transitioning to `Done`).
    pub summary: Option<String>,
    /// Error chain for this component (set when transitioning to
    /// `Failed`). Other components after this one stay `Pending`.
    pub error: Option<String>,
}

/// Per-component state inside a multi-component install. Values are
/// kebab-case in JSON so the front-end can use them as CSS classes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentState {
    /// Not started yet -- the dispatcher hasn't reached this slug.
    #[default]
    Pending,
    /// Currently installing; the parent [`InstallProgress`]'s
    /// `phase`, `bytes_*`, `message` reflect this component's
    /// step-by-step progress.
    Running,
    /// Install finished successfully; `summary` is populated.
    Done,
    /// Install failed; `error` is populated and no further
    /// components were attempted.
    Failed,
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

    /// Update the "what's happening" subtitle, and append the same
    /// line to the install log so the wizard's expandable log panel
    /// shows the full history of status messages. Skips appending
    /// when the message exactly matches the previous one (avoids
    /// runaway duplicates during a long download tick loop).
    pub fn set_message(&self, msg: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            let s = msg.into();
            if g.log.last().map(|l| l.as_str()) != Some(s.as_str()) {
                g.log.push(s.clone());
            }
            g.message = s;
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
    /// this point the wizard stops polling. Appends a terminal
    /// "Done." line to the log so the expandable log panel always
    /// ends with a clear closing line.
    pub fn finish_success(&self, summary: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            g.phase = InstallPhase::Done;
            g.success_summary = Some(summary.into());
            g.completed = true;
            g.finished_at = Some(Utc::now());
            g.log.push("Done.".into());
        }
    }

    /// Mark the task failed with the given error chain. Appends a
    /// terminal "Failed: <error>" line so the operator sees both the
    /// last status message and the final reason in the log.
    pub fn finish_failure(&self, error: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            let err = error.into();
            g.phase = InstallPhase::Failed;
            g.error = Some(err.clone());
            g.completed = true;
            g.finished_at = Some(Utc::now());
            g.log.push(format!("Failed: {err}"));
        }
    }

    // -- multi-component helpers (unified install) --
    //
    // The unified `/install` POST runs N installs sequentially. Each
    // calls into a per-component driver that uses the same handle.
    // These helpers track per-component status so the wizard can
    // render a stable N-row checklist with the current one highlighted.

    /// Initialize the per-component checklist for a multi-component
    /// install. Every slug starts in `Pending`. Idempotent: calling
    /// twice replaces the list.
    pub fn init_components(&self, slugs: &[&str]) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            g.components = slugs
                .iter()
                .map(|s| ComponentProgress {
                    slug: (*s).to_string(),
                    state: ComponentState::Pending,
                    summary: None,
                    error: None,
                })
                .collect();
        }
    }

    /// Mark the given slug as the currently-running component. Resets
    /// the per-current-component bytes counter so a previous
    /// component's tail-end download counters don't bleed into the
    /// next one's view.
    pub fn start_component(&self, slug: &str) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            if let Some(c) = g.components.iter_mut().find(|c| c.slug == slug) {
                c.state = ComponentState::Running;
            }
            g.bytes_downloaded = 0;
            g.total_bytes = None;
        }
    }

    /// Mark the given slug as finished with the given summary. Does
    /// NOT mark the whole job complete -- the orchestrator decides
    /// when to call [`finish_success`] / [`finish_failure`] for the
    /// run as a whole.
    pub fn finish_component(&self, slug: &str, summary: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            if let Some(c) = g.components.iter_mut().find(|c| c.slug == slug) {
                c.state = ComponentState::Done;
                c.summary = Some(summary.into());
            }
        }
    }

    /// Mark the given slug as failed with the given error chain. The
    /// orchestrator typically calls this and then [`finish_failure`]
    /// to mark the whole run failed.
    pub fn fail_component(&self, slug: &str, error: impl Into<String>) {
        if let Some(m) = &self.inner {
            let mut g = m.lock().unwrap();
            if let Some(c) = g.components.iter_mut().find(|c| c.slug == slug) {
                c.state = ComponentState::Failed;
                c.error = Some(error.into());
            }
        }
    }

    /// Record a generated credential for later one-time display on
    /// the result page. The plaintext lives in memory only -- the
    /// JSON polling endpoint strips it via `#[serde(skip)]`.
    pub fn push_credential(&self, c: GeneratedCredential) {
        if let Some(m) = &self.inner {
            m.lock().unwrap().generated_credentials.push(c);
        }
    }

    /// Drain and return the generated credentials, leaving the
    /// in-memory list empty. The result page calls this on the first
    /// GET after completion so a subsequent refresh of the same URL
    /// does not re-display them.
    pub fn drain_credentials(&self) -> Vec<GeneratedCredential> {
        if let Some(m) = &self.inner {
            std::mem::take(&mut m.lock().unwrap().generated_credentials)
        } else {
            Vec::new()
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
    fn multi_component_lifecycle_records_per_slug_state() {
        let state = Arc::new(Mutex::new(InstallProgress::default()));
        let h = ProgressHandle::new(state);

        h.init_components(&["postgres", "openfga", "kanidm"]);
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.components.len(), 3);
        assert!(snap
            .components
            .iter()
            .all(|c| c.state == ComponentState::Pending));

        h.start_component("postgres");
        h.set_bytes(50, Some(100));
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.components[0].state, ComponentState::Running);
        assert_eq!(snap.components[1].state, ComponentState::Pending);
        assert_eq!(snap.bytes_downloaded, 50);

        h.finish_component("postgres", "bin_dir: /var/lib/computeza/postgres/bin");
        // start_component resets the byte counters so a stale value
        // from the previous component doesn't leak.
        h.start_component("openfga");
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.components[0].state, ComponentState::Done);
        assert!(snap.components[0].summary.is_some());
        assert_eq!(snap.components[1].state, ComponentState::Running);
        assert_eq!(snap.bytes_downloaded, 0);
        assert!(snap.total_bytes.is_none());

        h.fail_component("openfga", "boom");
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.components[1].state, ComponentState::Failed);
        assert_eq!(snap.components[1].error.as_deref(), Some("boom"));
        assert_eq!(snap.components[2].state, ComponentState::Pending);
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
