//! Computeza audit -- append-only signed audit log with hash chaining.
//!
//! Per spec section 3.5 every spec change is an event in the audit log; the
//! current spec is a projection of the event log. Per spec section 4.5 the same
//! log feeds the **Audit Evidence Pack** generator -- the killer feature
//! for regulated buyers -- and spec section 6.5 makes it the EU AI Act Article 12
//! record-keeping store for the agent runtime.
//!
//! # Guarantees
//!
//! - **Append-only**: events are written sequentially to a JSON-Lines file
//!   under O_APPEND-equivalent semantics. The library exposes no API to
//!   modify a written event.
//! - **Per-event signing**: every event is signed with the cluster's
//!   Ed25519 audit key. Tampering with a single event invalidates that
//!   event's signature.
//! - **Hash chaining**: every event carries `prev_digest`, the BLAKE3
//!   digest of the previous event's canonical bytes. Truncating, deleting,
//!   or reordering events breaks the chain on the very next verify pass.
//! - **Public key embedded**: evidence packs ship the public key
//!   alongside the events so an auditor can verify offline without
//!   trusting the operator's running system.
//!
//! # What v0.0.x ships
//!
//! - [`AuditEvent`] type + [`Action`] enum
//! - [`AuditKey`] keypair (random generation; load/save TBD)
//! - [`AuditLog`] writer that opens a file in append mode and signs each
//!   event, computing the chain head as it goes
//! - [`verify::verify_log`] reader that walks the file and re-validates
//!   every signature + chain link
//! - Unit tests covering happy path, chain breakage, signature breakage,
//!   and out-of-order detection
//!
//! Persistent key storage (file-backed with restrictive perms / HSM
//! integration) and the evidence-pack ZIP exporter ship in follow-ups.

#![warn(missing_docs)]

mod event;
mod key;
mod log;
pub mod verify;

pub use event::{Action, AuditEvent, AuditEventBody};
pub use key::AuditKey;
pub use log::AuditLog;
pub use verify::{verify_log, VerifyError};
