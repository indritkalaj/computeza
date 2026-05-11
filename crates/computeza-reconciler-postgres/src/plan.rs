//! Plan computation for the PostgreSQL reconciler.
//!
//! `plan()` diffs desired vs actual database lists and produces a typed
//! [`PostgresPlan`]. The plan is data; `apply()` is what executes it. This
//! split is deliberate -- it lets us unit-test the diff logic without a
//! database, and lets future "dry-run" / "what-if" UI surfaces render the
//! plan to a user before any changes hit the wire.

use serde::{Deserialize, Serialize};

use crate::resource::DatabaseSpec;

/// One change the reconciler intends to apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatabaseChange {
    /// Create this database.
    Create(DatabaseSpec),
    /// Drop a database that exists on the server but is not in the spec.
    /// Only emitted when `spec.prune` is true.
    Drop {
        /// Name of the database to drop.
        name: String,
    },
}

/// Plan returned by [`crate::PostgresReconciler::plan`]. Empty plan means
/// the system is already converged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostgresPlan {
    /// Ordered list of changes. Creates come before drops so a partial
    /// apply leaves the system in a more-converged, not less-converged,
    /// state.
    pub changes: Vec<DatabaseChange>,
}

impl PostgresPlan {
    /// True when there is nothing to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Reserved database names on every PostgreSQL server. The reconciler
/// never touches these -- including when `prune` is true.
pub(crate) const SYSTEM_DATABASES: &[&str] = &["template0", "template1", "postgres"];

/// Compute the plan to converge `actual` to `desired`.
///
/// `actual` is the names of databases that exist on the server now (as
/// returned by [`crate::PostgresStatus::databases`]). `desired` is the
/// list from [`crate::PostgresSpec::databases`]. `prune` controls whether
/// extras in `actual` are dropped.
pub fn compute_plan(desired: &[DatabaseSpec], actual: &[String], prune: bool) -> PostgresPlan {
    let mut changes = Vec::new();

    // Creates: anything in `desired` not present in `actual`.
    for db in desired {
        if !actual.iter().any(|a| a == &db.name) {
            changes.push(DatabaseChange::Create(db.clone()));
        }
    }

    // Drops: anything in `actual` (and not a system db) absent from `desired`,
    // gated on `prune`.
    if prune {
        for name in actual {
            if SYSTEM_DATABASES.contains(&name.as_str()) {
                continue;
            }
            if !desired.iter().any(|d| &d.name == name) {
                changes.push(DatabaseChange::Drop { name: name.clone() });
            }
        }
    }

    PostgresPlan { changes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(name: &str) -> DatabaseSpec {
        DatabaseSpec {
            name: name.into(),
            owner: None,
            encoding: None,
        }
    }

    #[test]
    fn empty_diff_when_converged() {
        let plan = compute_plan(
            &[db("analytics"), db("marketing")],
            &["analytics".into(), "marketing".into()],
            false,
        );
        assert!(plan.is_empty(), "expected empty plan, got {plan:?}");
    }

    #[test]
    fn creates_missing_databases() {
        let plan = compute_plan(
            &[db("analytics"), db("marketing")],
            &["analytics".into()],
            false,
        );
        assert_eq!(plan.changes, vec![DatabaseChange::Create(db("marketing"))]);
    }

    #[test]
    fn no_drops_without_prune() {
        let plan = compute_plan(
            &[db("analytics")],
            &["analytics".into(), "stale".into()],
            false,
        );
        assert!(
            plan.is_empty(),
            "prune=false should never emit drops, got {plan:?}"
        );
    }

    #[test]
    fn drops_extras_when_pruning() {
        let plan = compute_plan(
            &[db("analytics")],
            &["analytics".into(), "stale".into()],
            true,
        );
        assert_eq!(
            plan.changes,
            vec![DatabaseChange::Drop {
                name: "stale".into()
            }]
        );
    }

    #[test]
    fn never_drops_system_databases_even_with_prune() {
        let plan = compute_plan(
            &[],
            &[
                "template0".into(),
                "template1".into(),
                "postgres".into(),
                "user_db".into(),
            ],
            true,
        );
        assert_eq!(
            plan.changes,
            vec![DatabaseChange::Drop {
                name: "user_db".into()
            }],
            "system databases must never appear in drops"
        );
    }

    #[test]
    fn creates_come_before_drops() {
        let plan = compute_plan(&[db("new")], &["old".into()], true);
        assert_eq!(
            plan.changes,
            vec![
                DatabaseChange::Create(db("new")),
                DatabaseChange::Drop { name: "old".into() },
            ],
            "creates should precede drops so partial apply leaves system more-converged"
        );
    }
}
