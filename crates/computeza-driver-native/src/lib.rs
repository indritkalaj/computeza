//! Computeza native driver — installs and manages components as native OS services.
//!
//! This is the v1.0 driver per spec §3.2 Tier 2. It implements the
//! [`computeza_core::Driver`] trait against three host service managers:
//!
//! - **Linux** — systemd unit lifecycle (spec §10.3)
//! - **macOS** — launchd plist lifecycle (spec §10.3)
//! - **Windows** — Windows Services via SCM (spec §10.3)
//!
//! Resource limits, hardening directives (NoNewPrivileges, PrivateTmp,
//! sandboxing, Job Objects), per-component virtual service accounts, and
//! firewall rule management all live here. Multi-node HA without
//! Kubernetes (certificate distribution, peer discovery, Raft membership,
//! rolling upgrades — spec §10.5) is also implemented in this crate.
//!
//! Internally, the crate has three platform modules — `linux`, `macos`,
//! `windows` — selected via cfg gates. From the operator's perspective
//! the experience is identical on all three platforms; only the lowest
//! layer diverges.
//!
//! Scaffold stub. Implementation is pending; the platform sub-modules
//! (`linux` / `macos` / `windows`) will be added under cfg gates when the
//! native service-manager integration starts.

#![warn(missing_docs)]
