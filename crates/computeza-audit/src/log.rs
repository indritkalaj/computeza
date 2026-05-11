//! Append-only audit log writer.

use std::{io, path::PathBuf};

use chrono::Utc;
use serde::Serialize;
use thiserror::Error;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use uuid::Uuid;

use crate::{
    event::{Action, AuditEvent, AuditEventBody},
    key::AuditKey,
};

/// Errors from the audit log.
#[derive(Debug, Error)]
pub enum AuditError {
    /// I/O on the underlying log file.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// JSON serialisation failed (should never happen for our types).
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// An append-only signed audit log backed by a JSON-Lines file.
///
/// One file per cluster lifetime; rotation is a follow-up feature (sealed
/// segments + roll-over signature so verifying a rotated log is still
/// chain-checkable end-to-end).
pub struct AuditLog {
    inner: Mutex<Inner>,
}

struct Inner {
    file: tokio::fs::File,
    key: AuditKey,
    next_seq: u64,
    /// Hex BLAKE3 digest of the last successfully appended event's
    /// `body_bytes || signature_bytes`. Empty for an empty log.
    chain_head: String,
    /// Path is kept for diagnostic / debug output only.
    #[allow(dead_code)]
    path: PathBuf,
}

impl AuditLog {
    /// Open (creating if absent) the audit log at `path` and bind it to
    /// the given signing key. If the file already contains events, the
    /// next-seq counter and chain head are recovered by reading the file.
    pub async fn open(path: impl Into<PathBuf>, key: AuditKey) -> Result<Self, AuditError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Recover state from any existing file.
        let (next_seq, chain_head) = recover_state(&path).await?;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.create(true).append(true).read(false);
        // On Unix create the file with restrictive mode so it's not
        // world-readable. Audit logs contain every administrative action
        // on the cluster; treat them as sensitive.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path).await?;
        // If the file pre-existed, the mode= above had no effect; tighten
        // it now. Best-effort: ignore errors if we can't chmod (e.g. file
        // owned by another user).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = tokio::fs::metadata(&path).await {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = tokio::fs::set_permissions(&path, perms).await;
            }
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                key,
                next_seq,
                chain_head,
                path,
            }),
        })
    }

    /// Append a single event. Returns the assigned sequence number and
    /// the new chain head.
    pub async fn append(
        &self,
        actor: impl Into<String>,
        action: Action,
        resource: Option<String>,
        detail: serde_json::Value,
    ) -> Result<AppendOutcome, AuditError> {
        let mut g = self.inner.lock().await;

        let body = AuditEventBody {
            id: Uuid::new_v4(),
            seq: g.next_seq,
            timestamp: Utc::now(),
            prev_digest: g.chain_head.clone(),
            actor: actor.into(),
            action,
            resource,
            detail,
        };
        let body_bytes = body.canonical_bytes()?;
        let signature = g.key.sign(&body_bytes);
        let event = AuditEvent {
            body,
            signature: signature.clone(),
        };
        let line = serde_json::to_vec(&event)?;

        g.file.write_all(&line).await?;
        g.file.write_all(b"\n").await?;
        g.file.sync_data().await?;

        // Advance the chain head and seq counter only after the bytes are
        // durably on disk.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&body_bytes);
        hasher.update(signature.as_bytes());
        let new_head = hasher.finalize().to_hex().to_string();
        g.chain_head = new_head.clone();
        let seq = g.next_seq;
        g.next_seq += 1;

        Ok(AppendOutcome {
            seq,
            chain_head: new_head,
            event,
        })
    }

    /// Public-key fingerprint (base64). Useful for evidence-pack headers.
    pub async fn verifying_key_b64(&self) -> String {
        let g = self.inner.lock().await;
        g.key.verifying_key_b64()
    }
}

/// Returned by [`AuditLog::append`] for callers that want to chain
/// downstream effects on the new chain head.
#[derive(Debug, Clone, Serialize)]
pub struct AppendOutcome {
    /// Assigned sequence number (0-based, monotonic within this log).
    pub seq: u64,
    /// Hex BLAKE3 digest of the just-written event. Becomes the next
    /// event's `prev_digest`.
    pub chain_head: String,
    /// The full event as written, in case the caller wants to forward
    /// it (telemetry / replication).
    pub event: AuditEvent,
}

async fn recover_state(path: &std::path::Path) -> Result<(u64, String), AuditError> {
    if !tokio::fs::try_exists(path).await? {
        return Ok((0, String::new()));
    }
    let bytes = tokio::fs::read(path).await?;
    if bytes.is_empty() {
        return Ok((0, String::new()));
    }
    let mut next_seq: u64 = 0;
    let mut chain_head = String::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let ev: AuditEvent = serde_json::from_slice(line)?;
        // Recompute the chain head from the body bytes + signature bytes
        // so a partially-corrupt last line doesn't poison recovery.
        let body_bytes = ev.body.canonical_bytes()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&body_bytes);
        hasher.update(ev.signature.as_bytes());
        chain_head = hasher.finalize().to_hex().to_string();
        next_seq = ev.body.seq + 1;
    }
    Ok((next_seq, chain_head))
}
