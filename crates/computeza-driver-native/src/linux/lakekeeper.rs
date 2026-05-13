//! Lakekeeper Iceberg REST catalog. Linux install path.
//!
//! Lakekeeper is stateless on disk; its persistent state lives in
//! PostgreSQL. The driver provisions a dedicated `lakekeeper`
//! postgres role + database on first install and wires the
//! connection string plus a randomly-generated secrets-encryption
//! key into the systemd unit via `Environment=` directives.
//!
//! Flow on a fresh host (postgres MUST already be installed -- the
//! install wizard sequences postgres before lakekeeper):
//!
//! 1. Generate / load credentials. The first run generates a random
//!    db password + encryption key and persists both at
//!    `<root>/db-credentials.json` (mode 0600). Subsequent runs
//!    re-read the file so re-installs don't lock anyone out by
//!    rotating the postgres password underneath them.
//! 2. Provision the postgres role + database via
//!    `sudo -u postgres psql`. Idempotent: the role-creation SQL is
//!    wrapped in a `DO $$...IF NOT EXISTS...$$` block so re-runs
//!    are no-ops; the database CREATE tolerates "already exists"
//!    errors.
//! 3. Hand off to the shared `service::install_service` with the
//!    bundle + env vars filled in. The shared template lays down
//!    the systemd unit with
//!    `Environment="ICEBERG_REST__PG_DATABASE_URL=..."` and
//!    `Environment="ICEBERG_REST__PG_ENCRYPTION_KEY=..."`.
//! 4. Push the db password as a generated credential so the
//!    operator captures it on the install-result page (one-shot
//!    view + JSON download) -- consistent with other components
//!    that emit credentials.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::{fs, process::Command};

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::{GeneratedCredential, ProgressHandle},
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-lakekeeper.service";
pub const DEFAULT_PORT: u16 = 8181;

// Verified May 2026 against the GitHub Releases API. Lakekeeper
// ships gnu rather than musl builds.
const LAKEKEEPER_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "0.12.2",
        url: "https://github.com/lakekeeper/lakekeeper/releases/download/v0.12.2/lakekeeper-x86_64-unknown-linux-gnu.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "0.11.6",
        url: "https://github.com/lakekeeper/lakekeeper/releases/download/v0.11.6/lakekeeper-x86_64-unknown-linux-gnu.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
];

#[must_use]
pub fn available_versions() -> &'static [Bundle] {
    LAKEKEEPER_BUNDLES
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub root_dir: PathBuf,
    pub port: u16,
    pub unit_name: String,
    pub version: Option<String>,
    /// Postgres host the lakekeeper backing store lives on. Defaults
    /// to the loopback address where our managed postgres binds.
    pub pg_host: String,
    /// Port of the postgres server (matches the install wizard's
    /// `--port` for the postgres step; default 5432).
    pub pg_port: u16,
    /// Postgres database name. Defaults to `lakekeeper`; the driver
    /// creates it on first install.
    pub pg_database: String,
    /// Postgres role name. Defaults to `lakekeeper`; the driver
    /// creates this role on first install.
    pub pg_user: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/lakekeeper"),
            port: DEFAULT_PORT,
            unit_name: UNIT_NAME.into(),
            version: None,
            pg_host: "127.0.0.1".into(),
            pg_port: 5432,
            pg_database: "lakekeeper".into(),
            pg_user: "lakekeeper".into(),
        }
    }
}

/// Persisted credentials for the lakekeeper backing store. Read
/// back on every install so re-runs preserve the password instead
/// of rotating it underneath the running service. The file lives
/// at `<root>/db-credentials.json` with mode 0600.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LakekeeperCreds {
    /// Password for the `lakekeeper` postgres role.
    db_password: String,
    /// Symmetric key Lakekeeper uses to encrypt secret material it
    /// persists in postgres (warehouse credentials, S3 keys, etc.).
    /// Loss of this key means stored secrets become unrecoverable;
    /// we persist it alongside the db password so disaster recovery
    /// can resurrect a backup install.
    encryption_key: String,
}

pub async fn install(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    fs::create_dir_all(&opts.root_dir).await?;
    let creds = load_or_generate_credentials(&opts.root_dir).await?;

    progress.set_message(format!(
        "Provisioning postgres role + database for lakekeeper on {}:{}",
        opts.pg_host, opts.pg_port
    ));
    provision_postgres(&opts.pg_user, &opts.pg_database, &creds.db_password).await?;

    let pg_url = format!(
        "postgres://{user}:{pwd}@{host}:{port}/{db}",
        user = opts.pg_user,
        pwd = creds.db_password,
        host = opts.pg_host,
        port = opts.pg_port,
        db = opts.pg_database,
    );
    let env: Vec<(String, String)> = vec![
        ("ICEBERG_REST__PG_DATABASE_URL".into(), pg_url),
        (
            "ICEBERG_REST__PG_ENCRYPTION_KEY".into(),
            creds.encryption_key.clone(),
        ),
    ];

    // Surface the credentials on the install-result page (+ the
    // one-shot JSON download). The encryption key is the more
    // critical of the two from a disaster-recovery standpoint;
    // we list both so operators back them up together.
    progress.push_credential(GeneratedCredential {
        component: "lakekeeper".into(),
        label: "postgres role password".into(),
        value: creds.db_password.clone(),
        username: Some(opts.pg_user.clone()),
        secret_ref: Some("lakekeeper/postgres-password".into()),
    });
    progress.push_credential(GeneratedCredential {
        component: "lakekeeper".into(),
        label: "secrets-encryption key (KEEP BACKED UP -- lost key = lost stored secrets)".into(),
        value: creds.encryption_key.clone(),
        username: None,
        secret_ref: Some("lakekeeper/encryption-key".into()),
    });

    let bundle = pick_bundle(opts.version.as_deref()).clone();
    let args = vec!["serve".into()];
    service::install_service(
        ServiceInstall {
            component: "lakekeeper",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "lakekeeper",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: None,
            cli_symlink: None,
            env,
        },
        progress,
    )
    .await
}

#[derive(Clone, Debug)]
pub struct UninstallOptions {
    pub root_dir: PathBuf,
    pub unit_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/lakekeeper"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("lakekeeper", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/lakekeeper");
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-lakekeeper".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: None,
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => LAKEKEEPER_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&LAKEKEEPER_BUNDLES[0]),
        None => &LAKEKEEPER_BUNDLES[0],
    }
}

/// Load `<root>/db-credentials.json` if present, otherwise generate
/// a fresh password + encryption key and persist them. The file is
/// 0600-mode (owner-only) on Unix.
///
/// Persisting matters because re-installing lakekeeper rotates the
/// postgres password underneath the role. If we regenerated on
/// every run, every re-install would silently rotate the password
/// and any external lakekeeper client carrying a cached copy would
/// stop working.
async fn load_or_generate_credentials(root: &Path) -> Result<LakekeeperCreds, ServiceError> {
    let path = root.join("db-credentials.json");
    if let Ok(bytes) = fs::read(&path).await {
        if let Ok(creds) = serde_json::from_slice::<LakekeeperCreds>(&bytes) {
            tracing::info!(
                path = %path.display(),
                "reusing lakekeeper db credentials from disk"
            );
            return Ok(creds);
        }
        // File present but malformed -- log and fall through to
        // regenerate. The bad file gets overwritten.
        tracing::warn!(
            path = %path.display(),
            "lakekeeper db-credentials.json was unparseable; regenerating"
        );
    }

    let creds = LakekeeperCreds {
        db_password: generate_hex_secret(32),
        encryption_key: generate_hex_secret(32),
    };
    let json = serde_json::to_vec_pretty(&creds)
        .map_err(|e| ServiceError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, &json).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = tokio::fs::set_permissions(&path, perms).await;
    }
    tracing::info!(
        path = %path.display(),
        "generated fresh lakekeeper db credentials"
    );
    Ok(creds)
}

/// Render `n` bytes of CSPRNG output as a hex string. Used for both
/// the postgres password (only [0-9a-f], so safe to embed verbatim
/// in SQL literals) and the secrets-encryption key.
fn generate_hex_secret(n: usize) -> String {
    use aes_gcm::aead::rand_core::RngCore;
    use aes_gcm::aead::OsRng;
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    hex::encode(&buf)
}

/// Provision the `lakekeeper` postgres role + database, idempotent.
///
/// Runs as the `postgres` OS user (via `sudo -u postgres psql`) so
/// peer auth on the local Unix socket is used -- no postgres
/// superuser password needed. Two psql calls:
///
/// 1. A `DO $$ BEGIN ... END $$` block that creates the role with
///    the supplied password on first run and ALTERs the password
///    on subsequent runs. Either way the row exists with the right
///    password when we return.
/// 2. A bare `CREATE DATABASE` that tolerates "already exists"
///    (SQLSTATE 42P04) by matching on the error text. Postgres
///    doesn't permit `CREATE DATABASE` inside a transaction block
///    so a `DO $$` wrapper isn't an option there.
async fn provision_postgres(
    user: &str,
    database: &str,
    password: &str,
) -> Result<(), ServiceError> {
    // password is hex (generate_hex_secret), so [0-9a-f] only --
    // no single quotes or backslashes possible. Safe to embed
    // literally in the SQL string.
    debug_assert!(
        password.bytes().all(|b| b.is_ascii_hexdigit()),
        "lakekeeper db password must be hex-only to avoid SQL escaping"
    );
    let role_sql = format!(
        "DO $$ BEGIN \
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{user}') THEN \
                CREATE ROLE \"{user}\" LOGIN PASSWORD '{password}'; \
            ELSE \
                ALTER ROLE \"{user}\" WITH PASSWORD '{password}'; \
            END IF; \
        END $$;"
    );
    let role_out = Command::new("sudo")
        .arg("-u")
        .arg("postgres")
        .arg("psql")
        .arg("--no-psqlrc")
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .arg("-c")
        .arg(&role_sql)
        .output()
        .await?;
    if !role_out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "creating/altering postgres role `{user}` failed (exit {:?}). Common causes: (1) postgres is not yet installed or not running -- install it first via /install/postgres; (2) the `postgres` OS user does not exist (apt postgres creates it as a side effect); (3) the lakekeeper install is not running as root, so `sudo -u postgres` cannot pivot. Full stderr:\n{}",
            role_out.status.code(),
            String::from_utf8_lossy(&role_out.stderr)
        ))));
    }

    let db_out = Command::new("sudo")
        .arg("-u")
        .arg("postgres")
        .arg("psql")
        .arg("--no-psqlrc")
        .arg("-c")
        .arg(&format!("CREATE DATABASE \"{database}\" OWNER \"{user}\";"))
        .output()
        .await?;
    if !db_out.status.success() {
        let stderr = String::from_utf8_lossy(&db_out.stderr);
        // SQLSTATE 42P04 = duplicate_database. Postgres surfaces
        // this as "already exists" in English; we match the
        // substring for distro-locale robustness (most operators
        // run en_US postgres but psql honours LC_MESSAGES).
        if !stderr.contains("already exists") {
            return Err(ServiceError::Io(std::io::Error::other(format!(
                "creating postgres database `{database}` failed (exit {:?}). Full stderr:\n{stderr}",
                db_out.status.code(),
            ))));
        }
        tracing::info!(
            database = database,
            "postgres database already exists; reusing"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_hex_secret_is_hex_only_and_correct_length() {
        for n in [8, 16, 32, 64] {
            let s = generate_hex_secret(n);
            assert_eq!(
                s.len(),
                n * 2,
                "hex encoding doubles length: {n} bytes -> {} chars",
                n * 2
            );
            assert!(
                s.bytes().all(|b| b.is_ascii_hexdigit()),
                "generated secret must be hex-only (no SQL-escaping needed): {s}"
            );
        }
    }

    #[test]
    fn generate_hex_secret_is_unique_across_calls() {
        let a = generate_hex_secret(32);
        let b = generate_hex_secret(32);
        assert_ne!(a, b, "CSPRNG should not produce the same string twice");
    }

    #[tokio::test]
    async fn load_or_generate_creates_file_on_first_call_and_reuses_on_second() {
        let dir = std::env::temp_dir().join(format!(
            "computeza-test-lakekeeper-creds-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let first = load_or_generate_credentials(&dir).await.unwrap();
        let second = load_or_generate_credentials(&dir).await.unwrap();
        assert_eq!(
            first.db_password, second.db_password,
            "second call should reuse the persisted db password"
        );
        assert_eq!(
            first.encryption_key, second.encryption_key,
            "second call should reuse the persisted encryption key"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_or_generate_writes_credentials_file_at_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "computeza-test-lakekeeper-perms-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let _ = load_or_generate_credentials(&dir).await.unwrap();
        let meta = std::fs::metadata(dir.join("db-credentials.json")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "db-credentials.json must be owner-only; was {mode:o}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
