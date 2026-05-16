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
    // Studio workspace files: operator-authored SQL / Python /
    // arbitrary text snippets. `path` is the slash-separated tree
    // location ("/sql/finance/customer-summary.sql"); folders are
    // implicit from path segments. Stored as TEXT so we can index +
    // diff cleanly; binary files belong in object storage, not
    // here.
    r#"CREATE TABLE IF NOT EXISTS studio_files (
        id TEXT PRIMARY KEY NOT NULL,
        path TEXT NOT NULL UNIQUE,
        content TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )"#,
    r#"CREATE INDEX IF NOT EXISTS studio_files_by_path ON studio_files(path)"#,
    // Per-file revision history. Every studio_files_update that
    // changes the content snapshots the prior content here so
    // operators can browse / restore older versions Databricks-
    // style. Append-only; never pruned automatically (operators
    // can drop rows by hand or via a future retention setting).
    r#"CREATE TABLE IF NOT EXISTS studio_file_revisions (
        id TEXT PRIMARY KEY NOT NULL,
        file_id TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at TEXT NOT NULL,
        FOREIGN KEY (file_id) REFERENCES studio_files(id) ON DELETE CASCADE
    )"#,
    r#"CREATE INDEX IF NOT EXISTS studio_file_revisions_by_file ON studio_file_revisions(file_id, created_at DESC)"#,
    // Soft-delete: trashed_at is the RFC3339 UTC timestamp when the
    // file was moved to .Trash. NULL means "live". The .Trash UI
    // surfaces these for the 30-day retention window; a background
    // sweep on startup hard-deletes any row with trashed_at older
    // than the retention cutoff.
    r#"ALTER TABLE studio_files ADD COLUMN trashed_at TEXT"#,
    r#"CREATE INDEX IF NOT EXISTS studio_files_by_trash ON studio_files(trashed_at)"#,
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
            // ALTER TABLE ADD COLUMN isn't idempotent in SQLite (no
            // "IF NOT EXISTS"); on re-open the column already exists
            // and the statement errors with "duplicate column name".
            // Treat that specific error as success so the migration
            // stays idempotent. Any other error is real and propagates.
            if let Err(e) = sqlx::query(stmt).execute(&pool).await {
                let msg = e.to_string();
                if msg.contains("duplicate column name") {
                    continue;
                }
                return Err(e.into());
            }
        }
        debug!(path, "opened sqlite state store");
        Ok(Self { pool })
    }

    // ============================================================
    // Studio files (workspace file browser)
    // ============================================================
    //
    // CRUD on the studio_files table. Inherent methods on
    // SqliteStore rather than a trait so the Postgres backend can
    // adopt the same shape later without touching every reconciler
    // that uses the generic Store trait.

    /// List every file, ordered by path. The flat list is what the
    /// UI tree-builder folds into a hierarchy. Cheap because the
    /// table has a path index + studio rarely accumulates more than
    /// a handful of files (operator-authored, not log-volume).
    pub async fn studio_files_list(&self) -> Result<Vec<StudioFile>> {
        // Default view excludes soft-deleted files. The .Trash UI
        // surfaces them via studio_files_list_trash().
        let rows = sqlx::query(
            "SELECT id, path, content, created_at, updated_at, trashed_at \
             FROM studio_files WHERE trashed_at IS NULL ORDER BY path",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(studio_file_from_row).collect()
    }

    /// List trashed files, newest-trashed first. Used by the .Trash
    /// view to surface restore/permanent-delete actions.
    pub async fn studio_files_list_trash(&self) -> Result<Vec<StudioFile>> {
        let rows = sqlx::query(
            "SELECT id, path, content, created_at, updated_at, trashed_at \
             FROM studio_files WHERE trashed_at IS NOT NULL ORDER BY trashed_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(studio_file_from_row).collect()
    }

    /// Move a file to .Trash (soft delete). Returns true on success,
    /// false if the file didn't exist. Idempotent on already-trashed
    /// rows (just refreshes the timestamp).
    pub async fn studio_files_trash(&self, id: &str) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let n = sqlx::query("UPDATE studio_files SET trashed_at = ?1 WHERE id = ?2")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// Restore a trashed file back into the workspace. Returns false
    /// if the file didn't exist or wasn't trashed.
    pub async fn studio_files_restore(&self, id: &str) -> Result<bool> {
        let n = sqlx::query(
            "UPDATE studio_files SET trashed_at = NULL \
             WHERE id = ?1 AND trashed_at IS NOT NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Hard-delete trashed files older than `retention_days` and any
    /// row passed explicitly via the `force` slice. Returns the count
    /// of rows actually deleted. Called from the studio shutdown /
    /// startup sweeper and from the "Empty trash" button.
    pub async fn studio_files_purge_trash(
        &self,
        retention_days: i64,
        force_ids: &[String],
    ) -> Result<u64> {
        let cutoff = (Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
        let mut total = sqlx::query(
            "DELETE FROM studio_files WHERE trashed_at IS NOT NULL AND trashed_at < ?1",
        )
        .bind(&cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        for id in force_ids {
            let n =
                sqlx::query("DELETE FROM studio_files WHERE id = ?1 AND trashed_at IS NOT NULL")
                    .bind(id)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
            total += n;
        }
        Ok(total)
    }

    /// Fetch one file by id. Returns Ok(None) on miss -- callers
    /// surface that as a 404; an outright Err only fires on real
    /// SQLite trouble.
    pub async fn studio_files_get(&self, id: &str) -> Result<Option<StudioFile>> {
        let row = sqlx::query(
            "SELECT id, path, content, created_at, updated_at \
             FROM studio_files WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(studio_file_from_row).transpose()
    }

    /// Resolve a path to its file. Used by import (overwrite-if-exists)
    /// and by the studio editor's "save by name" path.
    pub async fn studio_files_get_by_path(&self, path: &str) -> Result<Option<StudioFile>> {
        let row = sqlx::query(
            "SELECT id, path, content, created_at, updated_at \
             FROM studio_files WHERE path = ?1",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        row.map(studio_file_from_row).transpose()
    }

    /// Create a new file. Returns the populated record. Fails with
    /// PathConflict if the path already exists -- callers can choose
    /// to update-in-place instead.
    pub async fn studio_files_create(&self, path: &str, content: &str) -> Result<StudioFile> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO studio_files (id, path, content, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
        )
        .bind(&id)
        .bind(path)
        .bind(content)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(StudioFile {
            id,
            path: path.to_string(),
            content: content.to_string(),
            created_at: now.clone(),
            updated_at: now,
            trashed_at: None,
        })
    }

    /// Update content and/or path. Either field is optional. Returns
    /// Ok(None) when the id doesn't exist. When `content` changes
    /// and differs from the current value, the prior content is
    /// snapshotted into `studio_file_revisions` so operators can
    /// browse / restore older versions later.
    pub async fn studio_files_update(
        &self,
        id: &str,
        path: Option<&str>,
        content: Option<&str>,
    ) -> Result<Option<StudioFile>> {
        let existing = match self.studio_files_get(id).await? {
            Some(f) => f,
            None => return Ok(None),
        };
        let now = Utc::now().to_rfc3339();
        let new_path = path.unwrap_or(&existing.path);
        let new_content = content.unwrap_or(&existing.content);

        // Snapshot the prior content only when:
        //   1. The content is actually changing.
        //   2. The prior content is non-empty (skip the no-op initial
        //      revision for blank files).
        if let Some(c) = content {
            if c != existing.content && !existing.content.is_empty() {
                let rev_id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO studio_file_revisions (id, file_id, content, created_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .bind(&rev_id)
                .bind(id)
                .bind(&existing.content)
                .bind(&existing.updated_at)
                .execute(&self.pool)
                .await?;
            }
        }

        sqlx::query(
            "UPDATE studio_files SET path = ?1, content = ?2, updated_at = ?3 \
             WHERE id = ?4",
        )
        .bind(new_path)
        .bind(new_content)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(Some(StudioFile {
            id: id.to_string(),
            path: new_path.to_string(),
            content: new_content.to_string(),
            created_at: existing.created_at,
            updated_at: now,
            trashed_at: existing.trashed_at,
        }))
    }

    /// List revisions for a file, newest first. Each row carries
    /// the content as it was BEFORE the update that produced the
    /// next-newer revision (or the current file, for the first row).
    pub async fn studio_files_list_revisions(
        &self,
        file_id: &str,
    ) -> Result<Vec<StudioFileRevision>> {
        let rows = sqlx::query(
            "SELECT id, content, created_at \
             FROM studio_file_revisions \
             WHERE file_id = ?1 \
             ORDER BY created_at DESC",
        )
        .bind(file_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| StudioFileRevision {
                id: r.get::<String, _>("id"),
                file_id: file_id.to_string(),
                content: r.get::<String, _>("content"),
                created_at: r.get::<String, _>("created_at"),
            })
            .collect())
    }

    /// Fetch a single revision by id (for restore / preview).
    pub async fn studio_files_get_revision(
        &self,
        revision_id: &str,
    ) -> Result<Option<StudioFileRevision>> {
        let row = sqlx::query(
            "SELECT id, file_id, content, created_at \
             FROM studio_file_revisions WHERE id = ?1",
        )
        .bind(revision_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| StudioFileRevision {
            id: r.get::<String, _>("id"),
            file_id: r.get::<String, _>("file_id"),
            content: r.get::<String, _>("content"),
            created_at: r.get::<String, _>("created_at"),
        }))
    }

    /// Delete by id. Returns Ok(true) if a row was removed, Ok(false)
    /// otherwise -- callers may choose to surface either as 200 OK.
    pub async fn studio_files_delete(&self, id: &str) -> Result<bool> {
        let n = sqlx::query("DELETE FROM studio_files WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }
}

/// One row of the studio_files table -- a single operator-authored
/// SQL / Python / text snippet.
#[derive(Clone, Debug)]
pub struct StudioFile {
    /// Stable UUID. Used in URLs (?open=<id>&active=<id>) so renames
    /// don't break bookmarks.
    pub id: String,
    /// Slash-separated tree location, e.g. "/sql/finance/customers.sql".
    /// UNIQUE in SQLite -- two files can't share a path.
    pub path: String,
    /// File body. Text-only; binary belongs in object storage.
    pub content: String,
    /// RFC3339 string; the UI parses lazily for display.
    pub created_at: String,
    /// RFC3339 string.
    pub updated_at: String,
    /// Soft-delete marker. None = live; Some(rfc3339) = in .Trash
    /// since that timestamp. Files older than the retention window
    /// (30 days) are hard-deleted by the startup sweeper.
    pub trashed_at: Option<String>,
}

/// One row of the studio_file_revisions table -- a snapshot of a
/// file's prior content. Each row was the file's content right
/// before the update that created the next-newer revision.
#[derive(Clone, Debug)]
pub struct StudioFileRevision {
    pub id: String,
    pub file_id: String,
    pub content: String,
    pub created_at: String,
}

fn studio_file_from_row(row: sqlx::sqlite::SqliteRow) -> Result<StudioFile> {
    // trashed_at is read defensively -- older callers that select
    // without the column still work because try_get returns Err on
    // missing column, which we map to None.
    let trashed_at: Option<String> = row.try_get("trashed_at").ok().flatten();
    Ok(StudioFile {
        id: row.try_get("id")?,
        path: row.try_get("path")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        trashed_at,
    })
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
