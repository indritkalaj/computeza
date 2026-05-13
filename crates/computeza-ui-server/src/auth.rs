//! Operator account + session-cookie authentication.
//!
//! The operator console is a single-tenant trust boundary: there is
//! one (or a small set of) operators who can drive the install /
//! uninstall / rotate flows, and everyone else (LAN guests, marketing
//! visitors) hits the public landing page at `/`. This module wires
//! the credential + session primitives that gate the operator
//! surfaces.
//!
//! # Surfaces
//!
//! - [`OperatorFile`] -- JSONL file on disk, one operator per line.
//!   Each line carries the username (plaintext) and an Argon2id PHC
//!   password hash. Lives at `<state_db_parent>/operators.jsonl`.
//!   Decoupled from the encrypted [`SecretsStore`] so operators can
//!   log in even when `COMPUTEZA_SECRETS_PASSPHRASE` is unset (the
//!   first-boot setup flow doesn't require the secrets store).
//!
//! - [`SessionStore`] -- in-memory `HashMap<session_id, Session>`
//!   keyed by 128-bit random ids. Each session carries the operator
//!   username, a per-session CSRF token, and a creation timestamp.
//!   Lost on server restart -- operators just re-log-in. Persistent
//!   sessions are a v0.1+ ask.
//!
//! - Cookie helpers ([`session_cookie_header`], [`session_id_from_request`])
//!   -- emit and read the `computeza_session` cookie. Plain random id
//!   in the cookie value (no HMAC needed -- the id is the secret);
//!   `HttpOnly`, `SameSite=Strict`, `Path=/`.
//!
//! - Password helpers ([`hash_password`], [`verify_password`]) --
//!   thin wrappers over `argon2::password_hash` with the default
//!   Argon2id parameters from the crate (m=19MiB, t=2, p=1 in 0.5.x;
//!   matches OWASP 2025 baseline).

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Errors returned by the auth layer. Anything that propagates to a
/// handler renders as a clean failure page; nothing here surfaces a
/// raw error to the operator's browser.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Filesystem read/write failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Operator file on disk could not be parsed.
    #[error("operator file is malformed: {0}")]
    BadFile(String),
    /// Argon2 hashing or verification raised an error.
    #[error("argon2: {0}")]
    Argon2(String),
    /// The username + password combination did not match any operator.
    #[error("invalid credentials")]
    BadCredentials,
    /// Attempted to create an operator with a username that already
    /// exists. The setup handler refuses to overwrite.
    #[error("operator already exists: {0}")]
    AlreadyExists(String),
}

/// A single operator record persisted in [`OperatorFile`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperatorRecord {
    /// Operator's login name. ASCII alphanumeric + `_-.` only, max 64
    /// chars. Enforced at create-time.
    pub username: String,
    /// Argon2id PHC string for the password. Contains the salt + the
    /// chosen parameters so verification works without external state.
    pub password_hash: String,
    /// Wall-clock when the operator was created. Surfaced on the
    /// audit row eventually; not exposed via the cookie.
    pub created_at: DateTime<Utc>,
    /// Group memberships -- references into the built-in
    /// [`BUILTIN_GROUPS`] table. Existing operators created before
    /// the RBAC layer landed deserialize with this defaulted to
    /// `["admins"]` so they retain full access.
    #[serde(default = "default_groups")]
    pub groups: Vec<String>,
}

fn default_groups() -> Vec<String> {
    vec!["admins".to_string()]
}

/// Coarse permission categories. Routes are gated by membership in
/// one of these; the role-to-permission mapping lives in
/// [`BUILTIN_GROUPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Read-only surfaces: status, audit, resource detail, components.
    Read,
    /// Operational mutations: install, uninstall, rotate, delete.
    Write,
    /// Administrative actions: manage operator accounts + group
    /// memberships, view + rotate secrets at /admin/*.
    Manage,
}

impl Permission {
    /// Label rendered on the group-permissions read-only page. Kept
    /// minimal so the localized i18n surface doesn't need to grow a
    /// per-permission key set in v0.0.x.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Permission::Read => "Read",
            Permission::Write => "Write",
            Permission::Manage => "Manage",
        }
    }
}

/// Built-in group definitions. v0.0.x ships three: `admins` (full),
/// `operators` (Read + Write, no operator management), and `viewers`
/// (Read only). Custom groups + custom permission mixes are a v0.1+
/// extension.
pub const BUILTIN_GROUPS: &[(&str, &[Permission])] = &[
    (
        "admins",
        &[Permission::Read, Permission::Write, Permission::Manage],
    ),
    ("operators", &[Permission::Read, Permission::Write]),
    ("viewers", &[Permission::Read]),
];

/// Whether `group_name` is a built-in group recognized by the
/// permission layer. Unknown groups deserialize cleanly but contribute
/// no permissions -- the operator effectively has no access.
#[must_use]
pub fn is_known_group(group_name: &str) -> bool {
    BUILTIN_GROUPS.iter().any(|(n, _)| *n == group_name)
}

/// Compute the union of permissions across a list of group names.
/// Unknown group names are silently skipped (operators with only
/// unknown groups end up with no permissions and can't access the
/// console).
#[must_use]
pub fn permissions_for_groups(group_names: &[String]) -> std::collections::HashSet<Permission> {
    let mut out = std::collections::HashSet::new();
    for g in group_names {
        if let Some((_, perms)) = BUILTIN_GROUPS.iter().find(|(n, _)| *n == g.as_str()) {
            for p in *perms {
                out.insert(*p);
            }
        }
    }
    out
}

/// Map a request `(method, path)` onto the permission the route
/// requires. Returns `None` for paths the middleware should bypass
/// (public surfaces are already handled by [`is_public_path`]). Every
/// other authenticated request gets a permission requirement; the
/// permission middleware extracts the operator's groups, computes
/// effective permissions, and rejects with 403 when the required
/// permission isn't in the set.
#[must_use]
pub fn required_permission_for(method: &str, path: &str) -> Option<Permission> {
    // /logout is allowed for any signed-in operator regardless of
    // their effective permissions -- otherwise a Viewer who can't
    // even sign out would be stuck.
    if path == "/logout" {
        return Some(Permission::Read);
    }
    // /admin/* requires Manage for both reads and writes.
    if path.starts_with("/admin/") {
        return Some(Permission::Manage);
    }
    // Read methods: Read is enough.
    if method == "GET" || method == "HEAD" {
        return Some(Permission::Read);
    }
    // Everything else (POST / PUT / DELETE) mutates state.
    Some(Permission::Write)
}

/// JSONL-backed operator account store.
///
/// One operator per line. Reads scan the entire file on each load
/// (operator counts are tiny in practice; no need for indexing).
/// Writes append, then atomically replace the file on rewrite.
#[derive(Clone)]
pub struct OperatorFile {
    path: Arc<PathBuf>,
    cache: Arc<RwLock<Vec<OperatorRecord>>>,
}

impl OperatorFile {
    /// Open the operator file at `path`, reading any existing records
    /// into the in-memory cache. Creates a fresh empty store if the
    /// file does not exist.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let records = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let raw = tokio::fs::read(&path).await?;
            let mut out = Vec::new();
            for line in raw.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let rec: OperatorRecord =
                    serde_json::from_slice(line).map_err(|e| AuthError::BadFile(e.to_string()))?;
                out.push(rec);
            }
            out
        } else {
            Vec::new()
        };
        tracing::debug!(
            path = %path.display(),
            count = records.len(),
            "opened operator file"
        );
        Ok(Self {
            path: Arc::new(path),
            cache: Arc::new(RwLock::new(records)),
        })
    }

    /// True when no operators are registered yet. The first-boot
    /// setup flow tests this to decide whether to allow the public
    /// `/setup` form.
    pub async fn is_empty(&self) -> bool {
        self.cache.read().await.is_empty()
    }

    /// Verify a username + password combination. Returns the matching
    /// [`OperatorRecord`] on success.
    ///
    /// Implementation note: when the username does not exist we still
    /// run an Argon2id verification against a dummy hash so the total
    /// time-to-respond does not betray which usernames are registered.
    pub async fn verify(
        &self,
        username: &str,
        password: &str,
    ) -> Result<OperatorRecord, AuthError> {
        let records = self.cache.read().await;
        let found = records.iter().find(|r| r.username == username).cloned();
        drop(records);
        let stored_hash = match &found {
            Some(rec) => rec.password_hash.clone(),
            None => DUMMY_HASH.to_string(),
        };
        let ok = verify_password(password, &stored_hash)?;
        match (ok, found) {
            (true, Some(rec)) => Ok(rec),
            _ => Err(AuthError::BadCredentials),
        }
    }

    /// Append a new operator with the given group memberships.
    /// `groups` is validated against [`BUILTIN_GROUPS`]; unknown
    /// names are rejected so a typo doesn't accidentally leave the
    /// operator without any permissions.
    ///
    /// Pass `&["admins"]` as the groups argument for the first-boot
    /// operator (`/setup`) so they retain full access.
    pub async fn create(
        &self,
        username: &str,
        password: &str,
        groups: &[String],
    ) -> Result<(), AuthError> {
        validate_username(username)?;
        if password.len() < 12 {
            return Err(AuthError::Argon2(
                "password must be at least 12 characters".into(),
            ));
        }
        for g in groups {
            if !is_known_group(g) {
                return Err(AuthError::Argon2(format!("unknown group: {g}")));
            }
        }
        if groups.is_empty() {
            return Err(AuthError::Argon2(
                "an operator must be a member of at least one group".into(),
            ));
        }
        let mut records = self.cache.write().await;
        if records.iter().any(|r| r.username == username) {
            return Err(AuthError::AlreadyExists(username.to_string()));
        }
        let hash = hash_password(password)?;
        let rec = OperatorRecord {
            username: username.to_string(),
            password_hash: hash,
            created_at: Utc::now(),
            groups: groups.to_vec(),
        };
        records.push(rec.clone());
        self.write_atomic(&records).await?;
        tracing::info!(
            username = %username,
            groups = ?groups,
            "operator account created"
        );
        Ok(())
    }

    /// List every operator. Used by the /admin/operators page.
    pub async fn list(&self) -> Vec<OperatorRecord> {
        self.cache.read().await.clone()
    }

    /// Look up an operator by username. Returns `None` when no
    /// matching record exists. Used by the permission middleware on
    /// every authenticated request to map the session's username
    /// back onto its effective group memberships.
    pub async fn get(&self, username: &str) -> Option<OperatorRecord> {
        self.cache
            .read()
            .await
            .iter()
            .find(|r| r.username == username)
            .cloned()
    }

    /// Replace the group memberships for an existing operator.
    /// Returns `BadCredentials` (treated as a generic "not found"
    /// for the UI) if the username doesn't exist.
    pub async fn set_groups(&self, username: &str, groups: &[String]) -> Result<(), AuthError> {
        for g in groups {
            if !is_known_group(g) {
                return Err(AuthError::Argon2(format!("unknown group: {g}")));
            }
        }
        if groups.is_empty() {
            return Err(AuthError::Argon2(
                "an operator must be a member of at least one group".into(),
            ));
        }
        let mut records = self.cache.write().await;
        let Some(rec) = records.iter_mut().find(|r| r.username == username) else {
            return Err(AuthError::BadCredentials);
        };
        rec.groups = groups.to_vec();
        self.write_atomic(&records).await?;
        tracing::info!(username = %username, groups = ?groups, "operator group memberships updated");
        Ok(())
    }

    /// Delete an operator account. Returns `BadCredentials` when no
    /// matching record exists. The caller is responsible for
    /// preventing the last admin from being removed -- that policy
    /// lives in the handler.
    pub async fn delete(&self, username: &str) -> Result<(), AuthError> {
        let mut records = self.cache.write().await;
        let before = records.len();
        records.retain(|r| r.username != username);
        if records.len() == before {
            return Err(AuthError::BadCredentials);
        }
        self.write_atomic(&records).await?;
        tracing::info!(username = %username, "operator account deleted");
        Ok(())
    }

    async fn write_atomic(&self, records: &[OperatorRecord]) -> Result<(), AuthError> {
        let tmp = self.path.with_extension("computeza-tmp");
        let mut buf = Vec::new();
        for rec in records {
            let line = serde_json::to_vec(rec).map_err(|e| AuthError::BadFile(e.to_string()))?;
            buf.extend_from_slice(&line);
            buf.push(b'\n');
        }
        tokio::fs::write(&tmp, &buf).await?;
        tokio::fs::rename(&tmp, self.path.as_ref()).await?;
        Ok(())
    }
}

/// Validation rule for new operator usernames. ASCII alphanumeric,
/// underscore, hyphen, dot. 1-64 chars. The same regex would let
/// `..` through; we additionally disallow leading dots so the
/// username never looks like a hidden file path.
fn validate_username(username: &str) -> Result<(), AuthError> {
    if username.is_empty() || username.len() > 64 {
        return Err(AuthError::Argon2(
            "username must be between 1 and 64 characters".into(),
        ));
    }
    if username.starts_with('.') {
        return Err(AuthError::Argon2(
            "username must not start with a dot".into(),
        ));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(AuthError::Argon2(
            "username must be ASCII alphanumeric, underscore, hyphen, or dot only".into(),
        ));
    }
    Ok(())
}

/// Hash a password under Argon2id with a fresh random 16-byte salt
/// and the crate's default parameters. Output is a PHC string that
/// embeds the salt + parameters so verification is self-contained.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::default();
    argon
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Argon2(e.to_string()))
}

/// Verify a password against a stored PHC hash. Constant-time
/// comparison is provided by `argon2`'s verifier internally.
pub fn verify_password(password: &str, stored_hash: &str) -> Result<bool, AuthError> {
    let parsed = match PasswordHash::new(stored_hash) {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Argon2(e.to_string())),
    }
}

/// A stable PHC string used to keep verification timing constant
/// when a username does not exist. The corresponding plaintext is
/// not known to anyone -- this is a freshly-generated hash with the
/// salt pinned at build time so the value is identical across
/// processes (so cache effects don't leak the username-exists bit).
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlfc2FsdF8xMjM0NQ$ZHVtbXlfaGFzaF92YWx1ZV9zaXh0eWZvdXJfYnl0ZXNfZm9yX2NvbnN0YW50X3RpbWVfdmVyaWZ5";

/// In-process session record. Created on successful `POST /login` and
/// referenced by the cookie on every subsequent request.
#[derive(Clone, Debug)]
pub struct Session {
    /// Authenticated operator username.
    pub username: String,
    /// Per-session CSRF token. Embedded as a hidden input on every
    /// state-changing form and verified by the middleware on every
    /// POST.
    pub csrf_token: String,
    /// Wall-clock when the session was issued.
    pub created_at: DateTime<Utc>,
}

/// In-memory session table. Cheap to clone (Arc-shared). Sessions
/// live for the lifetime of the process -- a server restart forces
/// every operator to re-sign-in. Acceptable for v0.0.x; persistent
/// sessions are a v0.1+ ask.
#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl SessionStore {
    /// Construct an empty session table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a new session for `username`. Returns the cookie value
    /// (a 32-char hex session id) the caller embeds in `Set-Cookie`.
    pub async fn create(&self, username: impl Into<String>) -> String {
        let session_id = random_id_hex(32);
        let csrf_token = random_id_hex(32);
        let session = Session {
            username: username.into(),
            csrf_token,
            created_at: Utc::now(),
        };
        self.inner.write().await.insert(session_id.clone(), session);
        session_id
    }

    /// Look up a session by cookie value.
    pub async fn get(&self, session_id: &str) -> Option<Session> {
        self.inner.read().await.get(session_id).cloned()
    }

    /// Destroy a session. Called from `POST /logout`.
    pub async fn destroy(&self, session_id: &str) {
        self.inner.write().await.remove(session_id);
    }
}

/// Generate `n` characters of hex from CSPRNG bytes. `n` must be
/// even; we panic in debug on odd values to surface caller mistakes.
fn random_id_hex(n: usize) -> String {
    use aes_gcm::aead::rand_core::RngCore;
    use aes_gcm::aead::OsRng;
    debug_assert!(n % 2 == 0, "random_id_hex needs an even length");
    let mut buf = vec![0u8; n / 2];
    OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(n);
    for b in &buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Cookie name. Stable so a server restart's freshly-issued cookie
/// still maps onto the operator's existing browser store after they
/// log in again.
pub const SESSION_COOKIE_NAME: &str = "computeza_session";

/// Format a `Set-Cookie` header value for a freshly-issued session.
/// `HttpOnly` keeps the JS poller (and any future XSS) from reading
/// the cookie; `SameSite=Strict` blocks cross-site form posts; the
/// `Path=/` keeps the cookie scoped to the operator console.
pub fn session_cookie_header(session_id: &str) -> String {
    format!("{SESSION_COOKIE_NAME}={session_id}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400")
}

/// Format a `Set-Cookie` header value that clears the session cookie.
/// Used by `POST /logout`.
pub fn clear_session_cookie_header() -> String {
    format!("{SESSION_COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Extract the session id from a `Cookie:` header. Returns `None`
/// when the header is missing or does not carry our cookie name.
pub fn session_id_from_cookies(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE_NAME}=")) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Path prefixes that bypass the auth middleware entirely. Anything
/// not matching one of these prefixes requires a valid session.
pub const PUBLIC_PATH_PREFIXES: &[&str] = &[
    // Landing page
    "/login",
    "/setup",
    "/healthz",
    "/static/",
    "/favicon.ico",
    "/components",
    // Operator install guide -- public so prospective buyers / new
    // operators can read prereqs + step-by-step setup before signing
    // in to anything. Content is reference material; no per-tenant
    // state leaks.
    "/install-guide",
    // License-status read endpoint feeding the banner JS injected
    // into every page (including unauthenticated /login + landing).
    // Returns coarse-grained status only; safe to expose unauth.
    "/api/license/status",
];

/// Whether a request path is in the public set. The single `/` is
/// special-cased so subpaths like `/install` don't accidentally
/// match a `/` prefix.
pub fn is_public_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    PUBLIC_PATH_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Render a hidden CSRF input. The value is left empty -- the inline
/// JS in `render_shell` reads the [`CSRF_COOKIE_NAME`] cookie on form
/// submit and fills every `input[name=csrf_token]` from there. This
/// keeps renderer signatures simple (no csrf_token parameter to
/// thread through every form) while still binding the token to the
/// session via the cookie.
#[must_use]
pub const fn csrf_input() -> &'static str {
    r#"<input type="hidden" name="csrf_token" value="" />"#
}

/// Cookie name carrying the per-session CSRF token (non-HttpOnly so
/// the inline JS can read it). Set alongside the session cookie on
/// every login and cleared on logout.
pub const CSRF_COOKIE_NAME: &str = "computeza_csrf";

/// Format a `Set-Cookie` value for the CSRF cookie. Non-HttpOnly so
/// the inline JS in `render_shell` can read it; SameSite=Strict
/// blocks cross-site reads via document.cookie just like the session
/// cookie.
pub fn csrf_cookie_header(token: &str) -> String {
    format!("{CSRF_COOKIE_NAME}={token}; Path=/; SameSite=Strict; Max-Age=86400")
}

/// Clearing variant of [`csrf_cookie_header`], paired with the
/// session-cookie clear in `POST /logout`.
pub fn clear_csrf_cookie_header() -> String {
    format!("{CSRF_COOKIE_NAME}=; Path=/; SameSite=Strict; Max-Age=0")
}

/// Constant-time check that `provided` matches the session's token.
/// Both are random 32-char hex strings; we still avoid early-exit
/// comparisons so a side-channel can't recover the token byte by byte.
#[must_use]
pub fn csrf_tokens_match(provided: &str, session_token: &str) -> bool {
    if provided.len() != session_token.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (a, b) in provided.bytes().zip(session_token.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Paths that bypass the CSRF middleware specifically (separate from
/// the auth-middleware bypass). The login / setup POSTs operate
/// without an established session, so there's no CSRF token to bind
/// against -- the protection there comes from SameSite=Strict + the
/// fact that a successful POST grants only an authenticated session
/// (no privileged side effects).
pub const CSRF_EXEMPT_POST_PATHS: &[&str] = &["/login", "/setup"];

/// Whether a POST path is exempt from CSRF verification.
#[must_use]
pub fn is_csrf_exempt(path: &str) -> bool {
    CSRF_EXEMPT_POST_PATHS.contains(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let h = hash_password("hunter2hunter2").unwrap();
        assert!(verify_password("hunter2hunter2", &h).unwrap());
        assert!(!verify_password("wrong-password", &h).unwrap());
    }

    #[test]
    fn verify_password_returns_false_on_malformed_hash() {
        // We treat malformed hashes as "bad credentials" rather than
        // an error so an operator can't probe the system by sending
        // garbage values.
        assert!(!verify_password("anything", "not-a-real-phc-string").unwrap());
    }

    #[test]
    fn validate_username_rejects_bad_inputs() {
        assert!(validate_username("").is_err());
        assert!(validate_username(".hidden").is_err());
        assert!(validate_username("has space").is_err());
        assert!(validate_username("has/slash").is_err());
        assert!(validate_username(&"a".repeat(65)).is_err());
        assert!(validate_username("admin").is_ok());
        assert!(validate_username("ops-team_01").is_ok());
        assert!(validate_username("a.b").is_ok());
    }

    #[test]
    fn session_cookie_header_carries_expected_attributes() {
        let h = session_cookie_header("abc123");
        assert!(h.starts_with("computeza_session=abc123;"));
        assert!(h.contains("HttpOnly"));
        assert!(h.contains("SameSite=Strict"));
        assert!(h.contains("Path=/"));
        assert!(h.contains("Max-Age=86400"));
    }

    #[test]
    fn session_id_parses_out_of_cookie_header() {
        assert_eq!(
            session_id_from_cookies("computeza_session=xyz789"),
            Some("xyz789".into())
        );
        assert_eq!(
            session_id_from_cookies("other=foo; computeza_session=xyz789; third=bar"),
            Some("xyz789".into())
        );
        assert_eq!(session_id_from_cookies("computeza_session="), None);
        assert_eq!(session_id_from_cookies("other=foo"), None);
        assert_eq!(session_id_from_cookies(""), None);
    }

    #[test]
    fn random_id_hex_is_hex_and_unique() {
        let a = random_id_hex(32);
        let b = random_id_hex(32);
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn is_public_path_classifies_known_routes() {
        assert!(is_public_path("/"));
        assert!(is_public_path("/login"));
        assert!(is_public_path("/setup"));
        assert!(is_public_path("/healthz"));
        assert!(is_public_path("/static/computeza.css"));
        assert!(is_public_path("/favicon.ico"));
        assert!(is_public_path("/components"));
        assert!(is_public_path("/install-guide"));
        assert!(!is_public_path("/install"));
        assert!(!is_public_path("/status"));
        assert!(!is_public_path("/state"));
        assert!(!is_public_path("/admin/secrets"));
        assert!(!is_public_path("/resource/postgres-instance/local"));
        // / does NOT prefix-match other paths because of the special-case.
        assert!(!is_public_path("/install/postgres"));
    }

    #[tokio::test]
    async fn operator_file_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "computeza-test-ops-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("operators.jsonl");
        let store = OperatorFile::open(&path).await.unwrap();
        assert!(store.is_empty().await);
        store
            .create("admin", "hunter2hunter2", &["admins".to_string()])
            .await
            .unwrap();
        assert!(!store.is_empty().await);

        // verify happy path
        let rec = store.verify("admin", "hunter2hunter2").await.unwrap();
        assert_eq!(rec.username, "admin");

        // bad password
        assert!(matches!(
            store.verify("admin", "wrong-password").await,
            Err(AuthError::BadCredentials)
        ));

        // unknown user
        assert!(matches!(
            store.verify("nobody", "hunter2hunter2").await,
            Err(AuthError::BadCredentials)
        ));

        // duplicate create rejects
        assert!(matches!(
            store
                .create("admin", "hunter2hunter2", &["admins".to_string()])
                .await,
            Err(AuthError::AlreadyExists(_))
        ));

        // Re-open from disk to verify persistence.
        drop(store);
        let store2 = OperatorFile::open(&path).await.unwrap();
        assert!(!store2.is_empty().await);
        store2.verify("admin", "hunter2hunter2").await.unwrap();

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn permissions_for_groups_unions_across_memberships() {
        let perms = permissions_for_groups(&["viewers".to_string(), "operators".to_string()]);
        assert!(perms.contains(&Permission::Read));
        assert!(perms.contains(&Permission::Write));
        assert!(!perms.contains(&Permission::Manage));
    }

    #[test]
    fn permissions_for_groups_skips_unknown_groups() {
        let perms =
            permissions_for_groups(&["not-a-real-group".to_string(), "viewers".to_string()]);
        assert!(perms.contains(&Permission::Read));
        assert!(!perms.contains(&Permission::Write));
        assert!(!perms.contains(&Permission::Manage));
    }

    #[test]
    fn permissions_for_groups_admins_has_everything() {
        let perms = permissions_for_groups(&["admins".to_string()]);
        assert!(perms.contains(&Permission::Read));
        assert!(perms.contains(&Permission::Write));
        assert!(perms.contains(&Permission::Manage));
    }

    #[test]
    fn required_permission_routes_admin_through_manage() {
        assert_eq!(
            required_permission_for("GET", "/admin/operators"),
            Some(Permission::Manage)
        );
        assert_eq!(
            required_permission_for("POST", "/admin/operators"),
            Some(Permission::Manage)
        );
        assert_eq!(
            required_permission_for("POST", "/admin/secrets/postgres/admin-password/rotate"),
            Some(Permission::Manage)
        );
    }

    #[test]
    fn required_permission_routes_read_for_get_write_for_post() {
        assert_eq!(
            required_permission_for("GET", "/status"),
            Some(Permission::Read)
        );
        assert_eq!(
            required_permission_for("GET", "/audit"),
            Some(Permission::Read)
        );
        assert_eq!(
            required_permission_for("POST", "/install"),
            Some(Permission::Write)
        );
        assert_eq!(
            required_permission_for("POST", "/install/job/abc/rollback"),
            Some(Permission::Write)
        );
        // /logout always allowed for anyone signed in (even viewers).
        assert_eq!(
            required_permission_for("POST", "/logout"),
            Some(Permission::Read)
        );
    }

    #[test]
    fn is_known_group_only_matches_builtin() {
        assert!(is_known_group("admins"));
        assert!(is_known_group("operators"));
        assert!(is_known_group("viewers"));
        assert!(!is_known_group("custom-group"));
        assert!(!is_known_group(""));
    }

    #[tokio::test]
    async fn operator_file_set_groups_and_delete() {
        let dir = std::env::temp_dir().join(format!(
            "computeza-test-rbac-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("operators.jsonl");
        let store = OperatorFile::open(&path).await.unwrap();
        store
            .create("alice", "alicepassword!", &["admins".to_string()])
            .await
            .unwrap();
        store
            .create("bob", "bobpassword12", &["viewers".to_string()])
            .await
            .unwrap();
        assert_eq!(store.list().await.len(), 2);

        // set_groups happy path
        store
            .set_groups("bob", &["operators".to_string()])
            .await
            .unwrap();
        let bob = store.get("bob").await.unwrap();
        assert_eq!(bob.groups, vec!["operators"]);

        // unknown group rejected
        assert!(matches!(
            store.set_groups("bob", &["not-a-group".to_string()]).await,
            Err(AuthError::Argon2(_))
        ));

        // empty groups rejected
        assert!(matches!(
            store.set_groups("bob", &[]).await,
            Err(AuthError::Argon2(_))
        ));

        // delete happy path
        store.delete("bob").await.unwrap();
        assert_eq!(store.list().await.len(), 1);
        assert!(store.get("bob").await.is_none());

        // delete unknown returns BadCredentials
        assert!(matches!(
            store.delete("nobody").await,
            Err(AuthError::BadCredentials)
        ));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn csrf_tokens_match_is_constant_time_and_correct() {
        let token = "deadbeefcafebabe1234567890abcdef";
        assert!(csrf_tokens_match(token, token));
        assert!(!csrf_tokens_match("short", token));
        assert!(!csrf_tokens_match(token, "short"));
        assert!(!csrf_tokens_match(
            "deadbeefcafebabe1234567890abcdee", // last char differs
            token
        ));
        assert!(!csrf_tokens_match("", token));
        assert!(!csrf_tokens_match(token, ""));
        assert!(csrf_tokens_match("", "")); // both empty matches but middleware rejects empty separately
    }

    #[test]
    fn csrf_input_renders_empty_value() {
        let s = csrf_input();
        assert!(s.contains(r#"name="csrf_token""#));
        assert!(s.contains(r#"value="""#));
        assert!(s.contains(r#"type="hidden""#));
    }

    #[test]
    fn csrf_cookie_header_carries_no_httponly() {
        // The CSRF cookie must be readable by JS so the form-fill
        // script can use it. Differs from the session cookie, which
        // is HttpOnly.
        let h = csrf_cookie_header("abc-token");
        assert!(h.starts_with("computeza_csrf=abc-token"));
        assert!(!h.contains("HttpOnly"));
        assert!(h.contains("SameSite=Strict"));
        assert!(h.contains("Max-Age=86400"));
    }

    #[test]
    fn is_csrf_exempt_only_allows_login_setup() {
        assert!(is_csrf_exempt("/login"));
        assert!(is_csrf_exempt("/setup"));
        assert!(!is_csrf_exempt("/logout"));
        assert!(!is_csrf_exempt("/install"));
        assert!(!is_csrf_exempt("/admin/secrets/foo/rotate"));
    }

    #[tokio::test]
    async fn session_store_create_get_destroy() {
        let store = SessionStore::new();
        let id = store.create("admin").await;
        let sess = store.get(&id).await.expect("session exists");
        assert_eq!(sess.username, "admin");
        assert_eq!(sess.csrf_token.len(), 32);
        store.destroy(&id).await;
        assert!(store.get(&id).await.is_none());
    }
}
