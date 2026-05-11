//! Audit-log verification.
//!
//! Reads a JSON-Lines audit log produced by [`crate::AuditLog`] and
//! re-validates: every event signature against the supplied verifying
//! key, every sequence number monotonically increasing from 0, and every
//! `prev_digest` matching the previous event's body+signature digest.
//!
//! This is what the evidence-pack exporter calls to certify "this log is
//! intact" before zipping it for an auditor.

use std::path::Path;

use ed25519_dalek::VerifyingKey;
use thiserror::Error;
use tokio::fs;

use crate::{event::AuditEvent, key::AuditKey};

/// Errors during verification. Each carries the seq it failed on for
/// pinpointable evidence-pack diagnostics.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// Underlying file read failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A line wasn't valid JSON.
    #[error("malformed JSON at line {line}: {detail}")]
    MalformedJson {
        /// 1-based line number in the log file.
        line: usize,
        /// Underlying serde_json error message.
        detail: String,
    },
    /// Sequence numbers are not monotonic from 0.
    #[error("sequence out of order at line {line}: expected {expected}, got {actual}")]
    OutOfOrder {
        /// 1-based line number.
        line: usize,
        /// Expected seq.
        expected: u64,
        /// Actual seq.
        actual: u64,
    },
    /// Hash chain broken: prev_digest doesn't match the prior event's digest.
    #[error("chain broken at seq {seq}: expected prev_digest {expected:?}, got {actual:?}")]
    ChainBroken {
        /// Sequence number of the offending event.
        seq: u64,
        /// Digest we computed from the prior event.
        expected: String,
        /// Digest the offending event carries.
        actual: String,
    },
    /// Ed25519 signature failed to verify against the supplied key.
    #[error("signature invalid at seq {seq}")]
    BadSignature {
        /// Sequence number of the offending event.
        seq: u64,
    },
}

/// Verification result for callers that want the count of events checked.
#[derive(Debug, Clone, Copy)]
pub struct VerifyOutcome {
    /// How many events were verified successfully.
    pub events_verified: u64,
    /// The terminal chain head — pass this to evidence-pack signers as
    /// the "log tip at export time" reference.
    pub final_chain_head: [u8; 32],
}

/// Verify a JSON-Lines audit log at `path` against the verifying key.
pub async fn verify_log(
    path: impl AsRef<Path>,
    verifying: &VerifyingKey,
) -> Result<VerifyOutcome, VerifyError> {
    let bytes = fs::read(path).await?;
    let mut prev_head = String::new();
    let mut final_head_bytes = [0u8; 32];
    let mut events_verified: u64 = 0;

    // `expected_seq` and `i` advance in lockstep starting at 0; using
    // .zip(0_u64..) lets clippy stop worrying about an explicit counter.
    let lines = bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .enumerate()
        .zip(0_u64..);
    for ((i, line), expected_seq) in lines {
        let line_no = i + 1;
        let event: AuditEvent =
            serde_json::from_slice(line).map_err(|e| VerifyError::MalformedJson {
                line: line_no,
                detail: e.to_string(),
            })?;
        if event.body.seq != expected_seq {
            return Err(VerifyError::OutOfOrder {
                line: line_no,
                expected: expected_seq,
                actual: event.body.seq,
            });
        }
        if event.body.prev_digest != prev_head {
            return Err(VerifyError::ChainBroken {
                seq: event.body.seq,
                expected: prev_head,
                actual: event.body.prev_digest,
            });
        }
        let body_bytes = event
            .body
            .canonical_bytes()
            .map_err(|e| VerifyError::MalformedJson {
                line: line_no,
                detail: e.to_string(),
            })?;
        if !AuditKey::verify(verifying, &body_bytes, &event.signature) {
            return Err(VerifyError::BadSignature {
                seq: event.body.seq,
            });
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(&body_bytes);
        hasher.update(event.signature.as_bytes());
        let h = hasher.finalize();
        prev_head = h.to_hex().to_string();
        final_head_bytes = *h.as_bytes();
        // `expected_seq` advances automatically as the loop iterates the
        // 0_u64.. zip; the count of verified events is `expected_seq + 1`
        // at the end of the body. We capture that via `events_verified`.
        events_verified = expected_seq + 1;
    }
    Ok(VerifyOutcome {
        events_verified,
        final_chain_head: final_head_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{event::Action, log::AuditLog};
    use serde_json::json;
    use std::path::PathBuf;

    async fn fresh_log() -> (tempfile::TempDir, PathBuf, AuditKey) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let key = AuditKey::from_secret([7u8; 32]);
        (dir, path, key)
    }

    #[tokio::test]
    async fn empty_log_verifies() {
        let (_dir, path, key) = fresh_log().await;
        let log = AuditLog::open(&path, AuditKey::from_secret([7u8; 32]))
            .await
            .unwrap();
        drop(log);
        let verifying = AuditKey::verifying_key_from_b64(&key.verifying_key_b64()).unwrap();
        let r = verify_log(&path, &verifying).await.unwrap();
        assert_eq!(r.events_verified, 0);
    }

    #[tokio::test]
    async fn three_events_round_trip() {
        let (_dir, path, key) = fresh_log().await;
        {
            let log = AuditLog::open(&path, AuditKey::from_secret([7u8; 32]))
                .await
                .unwrap();
            log.append(
                "system",
                Action::Reconciled,
                Some("postgres-instance/x".into()),
                json!({"changed": true}),
            )
            .await
            .unwrap();
            log.append(
                "user:indrit",
                Action::UserAction,
                None,
                json!({"clicked": "Save"}),
            )
            .await
            .unwrap();
            log.append("system", Action::Authn, None, json!({"ok": true}))
                .await
                .unwrap();
        }
        let verifying = AuditKey::verifying_key_from_b64(&key.verifying_key_b64()).unwrap();
        let r = verify_log(&path, &verifying).await.unwrap();
        assert_eq!(r.events_verified, 3);
    }

    #[tokio::test]
    async fn tampered_signature_detected() {
        let (_dir, path, key) = fresh_log().await;
        {
            let log = AuditLog::open(&path, AuditKey::from_secret([7u8; 32]))
                .await
                .unwrap();
            log.append("system", Action::Reconciled, None, json!({}))
                .await
                .unwrap();
        }
        // Flip one byte of the signature on disk.
        let mut bytes = tokio::fs::read(&path).await.unwrap();
        // Find the last `"` before `}` and flip the byte before it.
        let pos = bytes.iter().rposition(|b| *b == b'"').unwrap();
        bytes[pos - 1] = bytes[pos - 1].wrapping_add(1);
        tokio::fs::write(&path, &bytes).await.unwrap();

        let verifying = AuditKey::verifying_key_from_b64(&key.verifying_key_b64()).unwrap();
        let err = verify_log(&path, &verifying).await.unwrap_err();
        assert!(
            matches!(err, VerifyError::BadSignature { .. }),
            "expected BadSignature, got {err:?}"
        );
    }

    #[tokio::test]
    async fn truncating_an_event_breaks_chain() {
        let (_dir, path, key) = fresh_log().await;
        {
            let log = AuditLog::open(&path, AuditKey::from_secret([7u8; 32]))
                .await
                .unwrap();
            log.append("system", Action::Reconciled, None, json!({"n": 1}))
                .await
                .unwrap();
            log.append("system", Action::Reconciled, None, json!({"n": 2}))
                .await
                .unwrap();
            log.append("system", Action::Reconciled, None, json!({"n": 3}))
                .await
                .unwrap();
        }
        // Delete the middle event by reading, splitting, and rewriting.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.remove(1);
        tokio::fs::write(&path, format!("{}\n", lines.join("\n")))
            .await
            .unwrap();

        let verifying = AuditKey::verifying_key_from_b64(&key.verifying_key_b64()).unwrap();
        let err = verify_log(&path, &verifying).await.unwrap_err();
        assert!(
            matches!(
                err,
                VerifyError::OutOfOrder { .. } | VerifyError::ChainBroken { .. }
            ),
            "expected OutOfOrder or ChainBroken, got {err:?}"
        );
    }

    #[tokio::test]
    async fn reopen_resumes_sequence_and_chain() {
        let (_dir, path, key) = fresh_log().await;
        {
            let log = AuditLog::open(&path, AuditKey::from_secret([7u8; 32]))
                .await
                .unwrap();
            log.append("system", Action::Reconciled, None, json!({}))
                .await
                .unwrap();
        }
        // Open again with the SAME key; append two more events.
        {
            let log = AuditLog::open(&path, AuditKey::from_secret([7u8; 32]))
                .await
                .unwrap();
            let a = log
                .append("system", Action::Reconciled, None, json!({}))
                .await
                .unwrap();
            assert_eq!(a.seq, 1, "second open should resume at seq=1");
            let b = log
                .append("system", Action::Reconciled, None, json!({}))
                .await
                .unwrap();
            assert_eq!(b.seq, 2);
        }
        let verifying = AuditKey::verifying_key_from_b64(&key.verifying_key_b64()).unwrap();
        let r = verify_log(&path, &verifying).await.unwrap();
        assert_eq!(r.events_verified, 3);
    }
}
