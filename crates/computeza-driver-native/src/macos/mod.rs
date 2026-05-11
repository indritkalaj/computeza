//! macOS-specific implementations.
//!
//! Compiled only on `target_os = "macos"`. launchd is the service
//! manager (no systemd). System daemons live under
//! `/Library/LaunchDaemons/`; per-user agents under
//! `~/Library/LaunchAgents/`. Computeza-managed services are always
//! system daemons (they need to outlive the operator's login session),
//! so we write plists to the system path and `launchctl bootstrap` them
//! into the `system` domain.

pub mod launchctl;
pub mod path;
pub mod postgres;
