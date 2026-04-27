//! OpenFGA reconciler.
//!
//! Manages fine-grained authorization per spec §7.12 — authorization
//! model, tuples, namespace assignment. Communicates over OpenFGA gRPC.
//! OpenFGA implements the Zanzibar paper better than any Rust alternative
//! as of 2026 (spec §2.2 "On non-Rust dependencies"); we run it in Go.
//! Stateless behind LB; backed by HA Postgres.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
