//! Resource — the unit of declarative state.
//!
//! Per spec §3.5, every managed thing in Computeza is a Resource: it has
//! an identity (UUID + scoped name), a spec (user-declared desired state),
//! a status (system-observed actual state), a history (the audit log
//! projection), and a lineage (upstream dependencies and downstream
//! dependents).
//!
//! The `Resource` trait below captures the structural contract; concrete
//! resource types (`Warehouse`, `User`, `Pipeline`, `Bucket`, …) live in
//! the crates that own them.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// Identity of a resource — stable UUID plus a human-readable name unique
/// within the resource's parent scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct ResourceId {
    /// Stable identifier; never reused across resource lifetimes.
    pub uuid: Uuid,
    /// Human-readable name, unique within the parent scope.
    pub name: String,
    /// Resource kind discriminator (e.g. "warehouse", "user", "pipeline").
    pub kind: String,
}

/// Versioning metadata for optimistic concurrency.
///
/// Edits supply the `revision` they read; the state store rejects writes
/// where the persisted revision has advanced. The UI surfaces the resulting
/// `Conflict` error as a three-way merge dialog (spec §3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct Revision(pub u64);

/// Common metadata every resource carries alongside its spec/status.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Metadata {
    /// Resource identity.
    pub id: ResourceId,
    /// Optimistic-concurrency revision.
    pub revision: Revision,
    /// When the spec was first created.
    pub created_at: DateTime<Utc>,
    /// When the spec was last updated.
    pub updated_at: DateTime<Utc>,
    /// Workspace scope (relevant for multi-tenant deployments — spec §3.6).
    pub workspace: Option<String>,
}

/// Marker trait implemented by every resource type the platform manages.
///
/// `Spec` is the user-declared desired state; `Status` is the system-observed
/// actual state. Both must be (de)serializable so they can round-trip through
/// the state store, GitOps YAML, and the audit log.
pub trait Resource: Send + Sync + 'static {
    /// User-declared desired state.
    type Spec: Serialize + DeserializeOwned + Send + Sync + Clone;
    /// System-observed actual state.
    type Status: Serialize + DeserializeOwned + Send + Sync + Clone;

    /// Resource kind discriminator. Stable identifier, kebab-case.
    fn kind() -> &'static str;
}
