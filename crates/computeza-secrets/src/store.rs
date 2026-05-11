//! [`SecretsStore`] -- file-backed encrypted secret storage.

use std::{collections::HashMap, path::PathBuf};

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    AeadCore, Aes256Gcm, Key, Nonce,
};
use base64ct::{Base64, Encoding};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncWriteExt, sync::RwLock};
use tracing::debug;

use crate::{
    error::{Result, SecretsError},
    kek::MasterKey,
};

/// AES-256-GCM encrypted secret on disk.
///
/// The wire format is a single JSON object per line:
///
/// ```text
/// {"name":"postgres/superuser","nonce":"<b64>","ciphertext":"<b64>"}
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
struct EncryptedEntry {
    name: String,
    nonce: String,
    ciphertext: String,
}

/// Encrypted file-backed secret store. Cheap to clone (Arc-shared under
/// the hood); pass clones into reconcilers as needed.
#[derive(Clone)]
pub struct SecretsStore {
    inner: std::sync::Arc<Inner>,
}

struct Inner {
    cipher: Aes256Gcm,
    cache: RwLock<HashMap<String, EncryptedEntry>>,
    path: PathBuf,
}

impl SecretsStore {
    /// Open the store at `path`, decrypting on demand. Creates an empty
    /// file if `path` doesn't exist. Returns an error only if the file
    /// exists but is unreadable or malformed.
    pub async fn open(path: impl Into<PathBuf>, key: &MasterKey) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
        let cache = if fs::try_exists(&path).await? {
            let raw = fs::read(&path).await?;
            let mut map = HashMap::new();
            for line in raw.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let entry: EncryptedEntry = serde_json::from_slice(line)?;
                map.insert(entry.name.clone(), entry);
            }
            map
        } else {
            HashMap::new()
        };
        debug!(path = %path.display(), entries = cache.len(), "opened secrets store");
        Ok(Self {
            inner: std::sync::Arc::new(Inner {
                cipher,
                cache: RwLock::new(cache),
                path,
            }),
        })
    }

    /// Store (or replace) a secret by name. The value is encrypted before
    /// it touches disk; only the name and the ciphertext are persisted.
    pub async fn put(&self, name: &str, value: &str) -> Result<()> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .inner
            .cipher
            .encrypt(&nonce, value.as_bytes())
            .map_err(|e| SecretsError::Crypto(e.to_string()))?;
        let entry = EncryptedEntry {
            name: name.to_string(),
            nonce: Base64::encode_string(&nonce),
            ciphertext: Base64::encode_string(&ciphertext),
        };
        let mut cache = self.inner.cache.write().await;
        cache.insert(name.to_string(), entry);
        self.write_file(&cache).await?;
        Ok(())
    }

    /// Look up a secret by name. Returns `Ok(None)` if absent.
    pub async fn get(&self, name: &str) -> Result<Option<SecretString>> {
        let cache = self.inner.cache.read().await;
        let Some(entry) = cache.get(name) else {
            return Ok(None);
        };
        let nonce_bytes =
            Base64::decode_vec(&entry.nonce).map_err(|e| SecretsError::Base64(e.to_string()))?;
        let ciphertext = Base64::decode_vec(&entry.ciphertext)
            .map_err(|e| SecretsError::Base64(e.to_string()))?;
        if nonce_bytes.len() != 12 {
            return Err(SecretsError::Crypto(format!(
                "nonce must be 12 bytes, got {}",
                nonce_bytes.len()
            )));
        }
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = self
            .inner
            .cipher
            .decrypt(nonce, ciphertext.as_ref())
            .map_err(|e| SecretsError::Crypto(e.to_string()))?;
        let s = String::from_utf8(plaintext)
            .map_err(|e| SecretsError::Crypto(format!("non-utf8 plaintext: {e}")))?;
        Ok(Some(SecretString::from(s)))
    }

    /// Delete a secret. Returns `NotFound` if there was nothing to delete.
    pub async fn delete(&self, name: &str) -> Result<()> {
        let mut cache = self.inner.cache.write().await;
        if cache.remove(name).is_none() {
            return Err(SecretsError::NotFound(name.into()));
        }
        self.write_file(&cache).await?;
        Ok(())
    }

    /// List every stored secret name. Useful for the operator console's
    /// "Secrets" page (names visible, values gated behind a privileged
    /// reveal).
    pub async fn list_names(&self) -> Result<Vec<String>> {
        let cache = self.inner.cache.read().await;
        let mut v: Vec<String> = cache.keys().cloned().collect();
        v.sort();
        Ok(v)
    }

    /// Rewrite the entire file atomically.
    async fn write_file(&self, cache: &HashMap<String, EncryptedEntry>) -> Result<()> {
        // Atomic write: tmp file + rename. Stable order so diffs stay readable.
        let tmp = self.inner.path.with_extension("tmp-computeza-secrets");
        let mut f = fs::File::create(&tmp).await?;
        let mut entries: Vec<&EncryptedEntry> = cache.values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in entries {
            let line = serde_json::to_vec(e)?;
            f.write_all(&line).await?;
            f.write_all(b"\n").await?;
        }
        f.sync_all().await?;
        drop(f);
        // On Unix, lock the tmp file down before rename so the destination
        // never appears world-readable (even briefly). Values are
        // AES-GCM ciphertext, but names are clear and could leak which
        // services are deployed; treat the file as sensitive.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tmp).await?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&tmp, perms).await?;
        }
        fs::rename(&tmp, &self.inner.path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kek::MasterKey;
    use secrecy::ExposeSecret;

    fn key() -> MasterKey {
        MasterKey::from_bytes(&[42u8; 32]).unwrap()
    }

    async fn fresh_store() -> (tempfile::TempDir, SecretsStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.jsonl");
        let s = SecretsStore::open(&path, &key()).await.unwrap();
        (dir, s)
    }

    #[tokio::test]
    async fn round_trip_one_secret() {
        let (_dir, s) = fresh_store().await;
        s.put("postgres/superuser", "hunter2").await.unwrap();
        let got = s.get("postgres/superuser").await.unwrap().unwrap();
        assert_eq!(got.expose_secret(), "hunter2");
    }

    #[tokio::test]
    async fn missing_returns_none() {
        let (_dir, s) = fresh_store().await;
        assert!(s.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_removes() {
        let (_dir, s) = fresh_store().await;
        s.put("x", "v").await.unwrap();
        s.delete("x").await.unwrap();
        assert!(s.get("x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_names_sorted() {
        let (_dir, s) = fresh_store().await;
        s.put("b", "v").await.unwrap();
        s.put("a", "v").await.unwrap();
        s.put("c", "v").await.unwrap();
        assert_eq!(s.list_names().await.unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn reopen_preserves_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.jsonl");
        {
            let s = SecretsStore::open(&path, &key()).await.unwrap();
            s.put("k", "v").await.unwrap();
        }
        let s = SecretsStore::open(&path, &key()).await.unwrap();
        assert_eq!(s.get("k").await.unwrap().unwrap().expose_secret(), "v");
    }

    #[tokio::test]
    async fn wrong_key_fails_to_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.jsonl");
        {
            let s = SecretsStore::open(&path, &key()).await.unwrap();
            s.put("k", "v").await.unwrap();
        }
        let bad = MasterKey::from_bytes(&[1u8; 32]).unwrap();
        let s = SecretsStore::open(&path, &bad).await.unwrap();
        let err = s.get("k").await.unwrap_err();
        assert!(matches!(err, SecretsError::Crypto(_)));
    }

    #[tokio::test]
    async fn ciphertext_on_disk_does_not_contain_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.jsonl");
        let s = SecretsStore::open(&path, &key()).await.unwrap();
        s.put("k", "supersecret-plaintext").await.unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("supersecret-plaintext"),
            "plaintext leaked into on-disk file: {raw}"
        );
    }

    #[tokio::test]
    async fn passphrase_derivation_is_deterministic() {
        let salt = b"computeza-test-salt!";
        let k1 = crate::kek::derive_kek_from_passphrase(b"correct horse", salt).unwrap();
        let k2 = crate::kek::derive_kek_from_passphrase(b"correct horse", salt).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes());
        let k3 = crate::kek::derive_kek_from_passphrase(b"battery staple", salt).unwrap();
        assert_ne!(k1.as_bytes(), k3.as_bytes());
    }
}
