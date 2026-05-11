//! SQLite implementation of [`Store`].
//!
//! Single-file, single-node. Per spec section 3.1 this is what runs in
//! single-replica deployments; HA deployments use a Postgres backend
//! implementing the same [`Store`] trait (forthcoming).

use async_trait::async_trait;
use chrono::Utc;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use tracing::debug;
use uuid::Uuid;

use crate::{
    error::{Result, StateError},
    store::{ResourceKey, Store, StoredResource},
};

/// Embedded schema. Applied idempotently on every `open()`. Statements
/// are kept separate because `sqlx::query` only executes the first
/// statement in a multi-statement string.
const SCHEMA_STATEMENTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS resources (
        uuid TEXT PRIMARY KEY NOT NULL,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        workspace TEXT NOT NULL DEFAULT '',
        revision INTEGER NOT NULL,
        spec_json TEXT NOT NULL,
        status_json TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(kind, name, workspace)
    )"#,
    r#"CREATE INDEX IF NOT EXISTS resources_by_kind ON resources(kind)"#,
];

/// SQLite-backed state store.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) a SQLite database at `path`. The file is
    /// created with `mode=rwc` and standard sqlx pool defaults.
    ///
    /// For tests, pass `":memory:"` to get an in-memory database.
    pub async fn open(path: &str) -> Result<Self> {
        // In-memory SQLite databases are PER-CONNECTION, so a pool with
        // multiple connections would have N independent databases. For the
        // test path we pin max_connections=1; file-backed stores use the
        // normal pool size.
        let (opts, max_conns) = if path == ":memory:" {
            (SqliteConnectOptions::new().in_memory(true), 1u32)
        } else {
            (
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
                8u32,
            )
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(max_conns)
            .connect_with(opts)
            .await?;
        for stmt in SCHEMA_STATEMENTS {
            sqlx::query(stmt).execute(&pool).await?;
        }
        debug!(path, "opened sqlite state store");
        Ok(Self { pool })
    }
}

/// Convert `Option<String>` workspace to the on-disk representation: the
/// store uses '' (empty string) for cluster-scope so the UNIQUE index can
/// include it (SQLite's UNIQUE treats NULLs as distinct from each other).
fn ws_to_db(ws: Option<&str>) -> &str {
    ws.unwrap_or("")
}

/// Reverse of [`ws_to_db`].
fn ws_from_db(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn rfc3339_now() -> String {
    Utc::now().to_rfc3339()
}

#[async_trait]
impl Store for SqliteStore {
    async fn save(
        &self,
        key: &ResourceKey,
        spec: &serde_json::Value,
        expected_revision: Option<u64>,
    ) -> Result<StoredResource> {
        let mut tx = self.pool.begin().await?;
        let ws = ws_to_db(key.workspace.as_deref());

        // Look up existing row inside the transaction so the
        // expected-revision check is atomic w.r.t. concurrent writers.
        let existing: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT uuid, revision, created_at FROM resources \
             WHERE kind = ? AND name = ? AND workspace = ?",
        )
        .bind(&key.kind)
        .bind(&key.name)
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?;

        let now = rfc3339_now();
        let spec_text = serde_json::to_string(spec)?;

        let (uuid, revision, created_at) = match (existing, expected_revision) {
            (None, None) => {
                // Create.
                let uuid = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO resources (uuid, kind, name, workspace, revision, spec_json, status_json, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, 1, ?, NULL, ?, ?)",
                )
                .bind(uuid.to_string())
                .bind(&key.kind)
                .bind(&key.name)
                .bind(ws)
                .bind(&spec_text)
                .bind(&now)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                (uuid, 1u64, now.clone())
            }
            (None, Some(_)) => {
                return Err(StateError::NotFound(key.display()));
            }
            (Some((_uuid_s, _rev, _created)), None) => {
                return Err(StateError::RevisionConflict {
                    key: key.display(),
                    expected: None,
                    found: Some(_rev as u64),
                });
            }
            (Some((uuid_s, current_rev, created)), Some(expected)) => {
                if (current_rev as u64) != expected {
                    return Err(StateError::RevisionConflict {
                        key: key.display(),
                        expected: Some(expected),
                        found: Some(current_rev as u64),
                    });
                }
                let new_rev = current_rev + 1;
                sqlx::query(
                    "UPDATE resources SET spec_json = ?, revision = ?, updated_at = ? \
                     WHERE kind = ? AND name = ? AND workspace = ?",
                )
                .bind(&spec_text)
                .bind(new_rev)
                .bind(&now)
                .bind(&key.kind)
                .bind(&key.name)
                .bind(ws)
                .execute(&mut *tx)
                .await?;
                let uuid = Uuid::parse_str(&uuid_s).map_err(|_| {
                    StateError::Sqlx(sqlx::Error::Protocol("invalid uuid in store".into()))
                })?;
                (uuid, new_rev as u64, created)
            }
        };

        tx.commit().await?;

        Ok(StoredResource {
            uuid,
            key: key.clone(),
            revision,
            spec: spec.clone(),
            status: None, // not loaded; caller can refetch via load()
            created_at: parse_rfc3339(&created_at)?,
            updated_at: parse_rfc3339(&now)?,
        })
    }

    async fn load(&self, key: &ResourceKey) -> Result<Option<StoredResource>> {
        let ws = ws_to_db(key.workspace.as_deref());
        let row = sqlx::query(
            "SELECT uuid, revision, spec_json, status_json, created_at, updated_at, workspace \
             FROM resources \
             WHERE kind = ? AND name = ? AND workspace = ?",
        )
        .bind(&key.kind)
        .bind(&key.name)
        .bind(ws)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(row_to_stored(&r, &key.kind, &key.name)?)),
        }
    }

    async fn list(&self, kind: &str, workspace: Option<&str>) -> Result<Vec<StoredResource>> {
        let ws = ws_to_db(workspace);
        let rows = sqlx::query(
            "SELECT uuid, name, revision, spec_json, status_json, created_at, updated_at, workspace \
             FROM resources WHERE kind = ? AND workspace = ? ORDER BY name",
        )
        .bind(kind)
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let name: String = r.try_get("name")?;
                row_to_stored(r, kind, &name)
            })
            .collect()
    }

    async fn put_status(&self, key: &ResourceKey, status: &serde_json::Value) -> Result<()> {
        let ws = ws_to_db(key.workspace.as_deref());
        let now = rfc3339_now();
        let status_text = serde_json::to_string(status)?;
        let n = sqlx::query(
            "UPDATE resources SET status_json = ?, updated_at = ? \
             WHERE kind = ? AND name = ? AND workspace = ?",
        )
        .bind(&status_text)
        .bind(&now)
        .bind(&key.kind)
        .bind(&key.name)
        .bind(ws)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(StateError::NotFound(key.display()));
        }
        Ok(())
    }

    async fn delete(&self, key: &ResourceKey, expected_revision: Option<u64>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let ws = ws_to_db(key.workspace.as_deref());
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT revision FROM resources WHERE kind = ? AND name = ? AND workspace = ?",
        )
        .bind(&key.kind)
        .bind(&key.name)
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?;
        match (existing, expected_revision) {
            (None, _) => Err(StateError::NotFound(key.display())),
            (Some((rev,)), Some(expected)) if (rev as u64) != expected => {
                Err(StateError::RevisionConflict {
                    key: key.display(),
                    expected: Some(expected),
                    found: Some(rev as u64),
                })
            }
            _ => {
                sqlx::query("DELETE FROM resources WHERE kind = ? AND name = ? AND workspace = ?")
                    .bind(&key.kind)
                    .bind(&key.name)
                    .bind(ws)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }
}

fn parse_rfc3339(s: &str) -> Result<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            StateError::Sqlx(sqlx::Error::Protocol(format!(
                "bad timestamp in store: {e}"
            )))
        })
}

fn row_to_stored(r: &sqlx::sqlite::SqliteRow, kind: &str, name: &str) -> Result<StoredResource> {
    let uuid_s: String = r.try_get("uuid")?;
    let revision: i64 = r.try_get("revision")?;
    let spec_json: String = r.try_get("spec_json")?;
    let status_json: Option<String> = r.try_get("status_json")?;
    let created_at: String = r.try_get("created_at")?;
    let updated_at: String = r.try_get("updated_at")?;
    let workspace: String = r.try_get("workspace")?;
    let uuid = Uuid::parse_str(&uuid_s)
        .map_err(|_| StateError::Sqlx(sqlx::Error::Protocol("invalid uuid in store".into())))?;
    let spec: serde_json::Value = serde_json::from_str(&spec_json)?;
    let status = match status_json {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    };
    Ok(StoredResource {
        uuid,
        key: ResourceKey {
            kind: kind.to_string(),
            name: name.to_string(),
            workspace: ws_from_db(&workspace),
        },
        revision: revision as u64,
        spec,
        status,
        created_at: parse_rfc3339(&created_at)?,
        updated_at: parse_rfc3339(&updated_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn store() -> SqliteStore {
        SqliteStore::open(":memory:").await.unwrap()
    }

    fn key() -> ResourceKey {
        ResourceKey::cluster_scoped("postgres-instance", "primary")
    }

    #[tokio::test]
    async fn create_then_load() {
        let s = store().await;
        let k = key();
        let saved = s.save(&k, &json!({"port": 5432}), None).await.unwrap();
        assert_eq!(saved.revision, 1);
        let loaded = s.load(&k).await.unwrap().unwrap();
        assert_eq!(loaded.spec, json!({"port": 5432}));
        assert_eq!(loaded.revision, 1);
    }

    #[tokio::test]
    async fn duplicate_create_is_revision_conflict() {
        let s = store().await;
        let k = key();
        s.save(&k, &json!({}), None).await.unwrap();
        let err = s.save(&k, &json!({}), None).await.unwrap_err();
        assert!(matches!(err, StateError::RevisionConflict { .. }));
    }

    #[tokio::test]
    async fn update_with_correct_revision_bumps_revision() {
        let s = store().await;
        let k = key();
        s.save(&k, &json!({"v": 1}), None).await.unwrap();
        let r2 = s.save(&k, &json!({"v": 2}), Some(1)).await.unwrap();
        assert_eq!(r2.revision, 2);
        let loaded = s.load(&k).await.unwrap().unwrap();
        assert_eq!(loaded.spec, json!({"v": 2}));
    }

    #[tokio::test]
    async fn update_with_stale_revision_conflicts() {
        let s = store().await;
        let k = key();
        s.save(&k, &json!({"v": 1}), None).await.unwrap();
        s.save(&k, &json!({"v": 2}), Some(1)).await.unwrap();
        let err = s.save(&k, &json!({"v": 3}), Some(1)).await.unwrap_err();
        assert!(matches!(err, StateError::RevisionConflict { .. }));
    }

    #[tokio::test]
    async fn put_status_does_not_bump_revision() {
        let s = store().await;
        let k = key();
        s.save(&k, &json!({}), None).await.unwrap();
        s.put_status(&k, &json!({"healthy": true})).await.unwrap();
        let loaded = s.load(&k).await.unwrap().unwrap();
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.status, Some(json!({"healthy": true})));
    }

    #[tokio::test]
    async fn list_returns_all_of_kind() {
        let s = store().await;
        s.save(
            &ResourceKey::cluster_scoped("postgres-instance", "a"),
            &json!({}),
            None,
        )
        .await
        .unwrap();
        s.save(
            &ResourceKey::cluster_scoped("postgres-instance", "b"),
            &json!({}),
            None,
        )
        .await
        .unwrap();
        s.save(
            &ResourceKey::cluster_scoped("kanidm-instance", "x"),
            &json!({}),
            None,
        )
        .await
        .unwrap();
        let pg = s.list("postgres-instance", None).await.unwrap();
        assert_eq!(pg.len(), 2);
        assert_eq!(pg[0].key.name, "a");
        assert_eq!(pg[1].key.name, "b");
    }

    #[tokio::test]
    async fn workspace_scoping_isolates_rows() {
        let s = store().await;
        let cluster_key = ResourceKey::cluster_scoped("postgres-instance", "primary");
        let ws_key = ResourceKey::workspace_scoped("postgres-instance", "primary", "tenant-a");
        s.save(&cluster_key, &json!({"scope": "cluster"}), None)
            .await
            .unwrap();
        s.save(&ws_key, &json!({"scope": "tenant"}), None)
            .await
            .unwrap();
        assert_eq!(
            s.load(&cluster_key).await.unwrap().unwrap().spec["scope"],
            "cluster"
        );
        assert_eq!(
            s.load(&ws_key).await.unwrap().unwrap().spec["scope"],
            "tenant"
        );
    }

    #[tokio::test]
    async fn delete_with_correct_revision_removes_row() {
        let s = store().await;
        let k = key();
        s.save(&k, &json!({}), None).await.unwrap();
        s.delete(&k, Some(1)).await.unwrap();
        assert!(s.load(&k).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_with_stale_revision_conflicts() {
        let s = store().await;
        let k = key();
        s.save(&k, &json!({"v": 1}), None).await.unwrap();
        s.save(&k, &json!({"v": 2}), Some(1)).await.unwrap();
        let err = s.delete(&k, Some(1)).await.unwrap_err();
        assert!(matches!(err, StateError::RevisionConflict { .. }));
    }
}
