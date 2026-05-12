//! Linux-specific implementations.
//!
//! Compiled only on `target_os = "linux"`. systemd is assumed as the init
//! system; we don't pretend to support OpenRC / runit / sysvinit for v1.0.

// The linux driver modules are internal install plumbing -- pub items
// here exist so other modules in this crate (and tests) can call them,
// not because they're a documented external surface. Matches the
// existing precedent in `windows/kanidm.rs`. The crate-level
// `#![warn(missing_docs)]` still applies to all the cross-platform
// API surfaces (fetch::, detect::, os_detect::, progress::, ...).
#![allow(missing_docs)]

pub mod path;
pub mod postgres;
pub mod service;
pub mod systemctl;

// Component drivers (single-binary services built on top of
// `service::install_service`). Each module is a thin spec table.
pub mod databend;
pub mod garage;
pub mod grafana;
pub mod greptime;
pub mod kanidm;
pub mod lakekeeper;
pub mod openfga;
pub mod qdrant;
pub mod restate;
pub mod xtable;
