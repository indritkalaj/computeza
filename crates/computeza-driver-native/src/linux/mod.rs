//! Linux-specific implementations.
//!
//! Compiled only on `target_os = "linux"`. systemd is assumed as the init
//! system; we don't pretend to support OpenRC / runit / sysvinit for v1.0.

pub mod path;
pub mod postgres;
pub mod systemctl;
