//! Reconciler trait — the convergence loop that powers every Computeza
//! component integration.
//!
//! Per spec §3.3, every reconciler observes actual state, compares it to
//! desired state, computes the minimum set of operations required to
//! converge them, and applies those operations through the appropriate
//! [`Driver`](crate::driver::Driver). This trait captures that contract.
//!
//! Implementations live in the `computeza-reconciler-*` crates.

use async_trait::async_trait;

use crate::{driver::Driver, error::Result, health::Health, resource::Resource};

/// Per-reconcile execution context. Carries everything a reconciler needs
/// from the platform without that needing to be a global.
///
/// Concrete fields (audit log handle, secret store, telemetry emitter,
/// vended-credential issuer, lineage emitter) are added as the supporting
/// crates land. Today this is just a marker.
#[derive(Debug, Default)]
pub struct Context {}

/// What a reconciler observed when it last queried the managed component.
///
/// Each reconciler defines its own `ActualState` shape. The reconciler
/// trait is parametric over it via the `Reconciler::Resource::Status`
/// associated type.
pub type ActualState<R> = <R as Resource>::Status;

/// Description of the changes a reconciler intends to apply to converge
/// actual state to desired state. Concrete plan shapes live with each
/// reconciler; the type parameter on [`Reconciler::Plan`] makes that explicit.
#[derive(Debug, Default)]
pub struct PlanMarker;

/// What happened when a plan was applied. Returned to the audit layer.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Whether any change was actually applied (false means the system was
    /// already converged).
    pub changed: bool,
    /// Human-readable summary; not user-facing, audit-log-only.
    pub summary: String,
}

/// The convergence loop. One impl per managed component.
///
/// This trait mirrors spec §3.3's pseudocode literally; future revisions
/// may add hooks for compensation, dry-run, and partial-failure recovery,
/// but those are additive.
#[async_trait]
pub trait Reconciler: Send + Sync {
    /// The resource type this reconciler manages.
    type Resource: Resource;
    /// The driver this reconciler dispatches deployment operations through.
    type Driver: Driver;
    /// The plan shape this reconciler computes during `plan`.
    type Plan: Send + Sync;

    /// Observe the actual state of the managed component.
    async fn observe(&self, ctx: &Context) -> Result<ActualState<Self::Resource>>;

    /// Compute the minimum set of operations needed to converge actual to
    /// desired state.
    async fn plan(
        &self,
        desired: &<Self::Resource as Resource>::Spec,
        actual: &ActualState<Self::Resource>,
    ) -> Result<Self::Plan>;

    /// Apply the plan via the driver.
    async fn apply(
        &self,
        ctx: &Context,
        plan: Self::Plan,
        driver: &Self::Driver,
    ) -> Result<Outcome>;

    /// Health snapshot used for the drift indicator.
    async fn health(&self, ctx: &Context) -> Result<Health>;
}
