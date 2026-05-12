//! Linux-specific implementations.
//!
//! Compiled only on `target_os = "linux"`. systemd is assumed as the init
//! system; we don't pretend to support OpenRC / runit / sysvinit for v1.0.

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
