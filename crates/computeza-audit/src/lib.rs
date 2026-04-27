//! Computeza audit — append-only signed audit log.
//!
//! Per spec §3.5 every spec change is an event in the audit log; the current
//! spec is a projection of the event log. Per spec §4.5 the audit log feeds
//! the **Audit Evidence Pack** generator — the killer feature for regulated
//! buyers — which exports tamper-evident archives with Ed25519-signed
//! manifests, chained back to a root key in our HSM (spec §15.7 layer 5).
//!
//! This crate also provides the EU AI Act Article 12 record-keeping that the
//! agent runtime depends on (spec §6.5): every retrieval, generation, and
//! tool invocation by an agent is journaled here as a side effect.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
