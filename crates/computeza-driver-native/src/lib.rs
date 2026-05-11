//! Computeza native driver — installs and manages components as native OS services.
//!
//! v1.0 driver per spec §3.2 Tier 2. Implements [`computeza_core::Driver`]
//! against the host's service manager:
//!
//! - **Linux** — systemd unit lifecycle (spec §10.3)
//! - **macOS** — launchd plist lifecycle (spec §10.3)
//! - **Windows** — Windows Services via SCM (spec §10.3)
//!
//! From the operator's perspective the experience is identical on all
//! three platforms; only the lowest layer diverges. Cross-platform PATH
//! registration for any managed binary that exposes a CLI users may
//! invoke directly is also this crate's responsibility (see CLAUDE.md
//! rule §4).
//!
//! v0.0.x ships the Linux Postgres install path as the first end-to-end
//! demonstration of the autonomous-installer mandate (spec §2.1). macOS
//! and Windows implementations of the same surface follow.

#![warn(missing_docs)]

#[cfg(target_os = "linux")]
pub mod linux;
