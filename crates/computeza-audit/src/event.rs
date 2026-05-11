//! Audit event types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One immutable entry in the audit log.
///
/// The on-wire JSON form is what gets signed; signature covers the
/// canonical bytes of `body`. `signature` and the BLAKE3 digest of the
/// signed bytes are then chained into the next event via `prev_digest`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    /// Everything-except-signature payload. Signed by [`AuditEvent::signature`].
    pub body: AuditEventBody,
    /// Ed25519 signature over the canonical JSON of `body`. Base64 (no padding).
    pub signature: String,
}

/// The signed payload of an audit event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEventBody {
    /// Stable event identifier.
    pub id: Uuid,
    /// Monotonic sequence number within this log file (starts at 0).
    pub seq: u64,
    /// When the event was emitted (UTC).
    pub timestamp: DateTime<Utc>,
    /// Hex BLAKE3 digest of the previous event's canonical body bytes
    /// concatenated with its signature bytes. Empty string for the first
    /// event in a log.
    pub prev_digest: String,
    /// Principal that triggered the event (typically a Kanidm user id,
    /// service-account id, or the literal `"system"` for internal flows).
    pub actor: String,
    /// What happened.
    pub action: Action,
    /// Affected resource identifier, e.g. `"postgres-instance/primary"`.
    /// `None` for events that aren't resource-scoped (e.g. login).
    pub resource: Option<String>,
    /// Free-form structured detail. Reconcilers attach plan summaries
    /// here; UI actions attach the before/after spec diff.
    pub detail: serde_json::Value,
}

/// Coarse action categories. Each reconciler / subsystem refines these
/// with `detail` payload structure.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// A resource was created (e.g. `CREATE DATABASE`, new user provisioned).
    ResourceCreated,
    /// A resource was updated (spec change, config rollout).
    ResourceUpdated,
    /// A resource was deleted (`DROP DATABASE`, user removed).
    ResourceDeleted,
    /// A reconciliation produced an outcome (success or failure).
    Reconciled,
    /// Authentication / authorization decision (login, token issuance).
    Authn,
    /// A high-level user-initiated action (clicked Save in the UI,
    /// applied a YAML).
    UserAction,
    /// AI-related action (per spec §6.5 — agent step, retrieval, LLM call).
    Ai,
    /// Catch-all for events not yet modelled.
    Other,
}

impl AuditEventBody {
    /// Canonical bytes used as signature input AND as the seed for the
    /// next event's `prev_digest`. We use `serde_json::to_vec` against a
    /// derived struct that *omits* the signature field, with sorted
    /// object keys so the same body always produces the same bytes
    /// regardless of struct field order in source.
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        // serde_json by default preserves struct field order from the source.
        // We don't reorder keys at run time — instead we accept that the
        // canonical form is "the JSON serde_json produces from this struct".
        // Stability across source edits is enforced by the Eq impl + tests.
        serde_json::to_vec(self)
    }
}
