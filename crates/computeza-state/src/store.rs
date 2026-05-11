//! The [`Store`] trait — backend-agnostic persistence of the platform's
//! desired-state metadata graph.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;

/// Unique key for a resource within the state store.
///
/// `workspace == None` means a cluster-scoped resource (the typical case
/// in single-tenant deployments per spec §3.6). Multi-tenant deployments
/// scope every resource to a workspace name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceKey {
    /// Resource kind (kebab-case, matches `Resource::kind()` in
    /// `computeza-core`).
    pub kind: String,
    /// Human-readable name, unique within the (kind, workspace) scope.
    pub name: String,
    /// Workspace scope; `None` means cluster-scoped.
    pub workspace: Option<String>,
}

impl ResourceKey {
    /// Construct a cluster-scoped key.
    #[must_use]
    pub fn cluster_scoped(kind: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            workspace: None,
        }
    }

    /// Construct a workspace-scoped key.
    #[must_use]
    pub fn workspace_scoped(
        kind: impl Into<String>,
        name: impl Into<String>,
        workspace: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            workspace: Some(workspace.into()),
        }
    }

    /// Render the key as a single human-readable string for log + error output.
    #[must_use]
    pub fn display(&self) -> String {
        match &self.workspace {
            Some(ws) => format!("{}/{}/{}", ws, self.kind, self.name),
            None => format!("{}/{}", self.kind, self.name),
        }
    }
}

/// One row read from the store. The spec/status payloads are opaque JSON
/// values; callers deserialize against their resource-specific types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredResource {
    /// Stable UUID assigned by the store on first save.
    pub uuid: Uuid,
    /// Key (kind/name/workspace).
    pub key: ResourceKey,
    /// Optimistic-concurrency revision; bumps by 1 on each save.
    pub revision: u64,
    /// User-declared desired state.
    pub spec: serde_json::Value,
    /// System-observed actual state, or null if not yet observed.
    pub status: Option<serde_json::Value>,
    /// Wall-clock of first save.
    pub created_at: DateTime<Utc>,
    /// Wall-clock of most recent save (spec or status).
    pub updated_at: DateTime<Utc>,
}

/// Backend-agnostic state-store contract.
///
/// Implementations: [`crate::SqliteStore`] (v0.0.x), and a Postgres
/// counterpart when HA-mode lands.
#[async_trait]
pub trait Store: Send + Sync {
    /// Create or update a resource.
    ///
    /// `expected_revision`:
    /// - `None`: create. Fails with `RevisionConflict` if the resource
    ///   already exists.
    /// - `Some(r)`: update only if the current revision is exactly `r`.
    ///   Fails with `RevisionConflict` otherwise.
    async fn save(
        &self,
        key: &ResourceKey,
        spec: &serde_json::Value,
        expected_revision: Option<u64>,
    ) -> Result<StoredResource>;

    /// Load a resource. Returns `None` if it does not exist.
    async fn load(&self, key: &ResourceKey) -> Result<Option<StoredResource>>;

    /// List every resource of the given kind in the given workspace
    /// scope. `workspace=None` lists cluster-scoped resources; pass a
    /// specific workspace for workspace-scoped lookup.
    async fn list(&self, kind: &str, workspace: Option<&str>) -> Result<Vec<StoredResource>>;

    /// Replace a resource's status. Does NOT bump the revision —
    /// reconciler-driven status updates are not user-initiated changes
    /// and shouldn't trigger spec-conflict errors elsewhere.
    async fn put_status(&self, key: &ResourceKey, status: &serde_json::Value) -> Result<()>;

    /// Delete a resource. `expected_revision` follows the same semantics
    /// as in `save`: `None` deletes unconditionally; `Some(r)` deletes
    /// only if the current revision is exactly `r`.
    async fn delete(&self, key: &ResourceKey, expected_revision: Option<u64>) -> Result<()>;
}
