//! Computeza -- single binary entry point.
//!
//! Per spec section 2.1, this binary is three things:
//!
//! 1. **Installer** -- first-run wizard that lays down the entire data plane
//!    natively on the host operating system (`computeza install`).
//! 2. **Operator console** -- long-running server hosting the Leptos web UI,
//!    REST API, gRPC API, and the reconciliation loop (`computeza serve`).
//! 3. **Configurator** -- declarative state engine with a YAML representation
//!    suitable for git, CI, and IaC workflows (`computeza apply`, etc.).
//!
//! Every command surface here is GUI-equivalent: anything you can do from
//! this CLI you can also do from the web console (and vice versa). The CLI
//! exists for power users, scripting, and CI; the web console is the
//! primary interface (spec section 2.1, section 4.2, and the user mandate that
//! everything -- instances, clusters, users, permissions -- be reachable
//! from a GUI).
//!
//! User-facing strings route through [`computeza_i18n::Localizer`]. Clap's
//! derive macros require `&'static str` for help text, which is a known
//! limitation; the `Cli` struct below uses `command_help_*` keys as
//! placeholders that will be replaced by builder-pattern construction
//! reading from the localizer once the command surface stabilizes.

#![warn(missing_docs)]

use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use computeza_i18n::Localizer;

/// Computeza command-line entry point.
#[derive(Parser, Debug)]
#[command(
    name = "computeza",
    version,
    // about/long_about left unset so clap renders the short binary description;
    // localised banner is printed when no subcommand is given (see main()).
    disable_help_subcommand = true,
)]
struct Cli {
    /// Subcommand to run; if omitted, prints the welcome banner.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Top-level command surface. Each variant maps 1:1 to a section of the
/// web console (spec section 4.2).
#[derive(Subcommand, Debug)]
enum Command {
    /// First-run installer.
    Install {
        /// Component to install. v0.0.x ships only `postgres`.
        component: InstallComponent,
    },
    /// Start the operator console (web UI + reconciler loop).
    Serve {
        /// Address to bind the HTTP server to. Default `127.0.0.1:8400`
        /// per the port allocation in spec section 10.6.
        #[arg(long, default_value = "127.0.0.1:8400")]
        addr: SocketAddr,

        /// Path to the SQLite metadata store file. Default
        /// `./computeza-state.db` (relative to CWD). In production the
        /// installer drops this under /var/lib/computeza on Linux,
        /// /Library/Application Support/Computeza on macOS, and
        /// %PROGRAMDATA%\Computeza on Windows; the flag lets dev / test
        /// invocations point at a writable path without root.
        #[arg(long, default_value = "computeza-state.db")]
        state_db: String,
    },
    /// Show cluster status and reconciliation drift.
    Status {
        /// Address of the running operator console to probe.
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: String,
    },
    /// Show license tier, seat usage, activation health, expiry.
    License,
}

/// Components recognised by `computeza install`. Postgres has full
/// multi-OS coverage; the other 10 currently ship Linux-only drivers.
#[derive(clap::ValueEnum, Clone, Debug)]
enum InstallComponent {
    /// PostgreSQL.
    Postgres,
    /// Kanidm identity provider. Linux-only.
    Kanidm,
    /// Garage S3-compatible object storage. Linux-only.
    Garage,
    /// Lakekeeper Iceberg REST catalog. Linux-only.
    Lakekeeper,
    /// Apache XTable Iceberg<->Delta<->Hudi sync. Linux-only.
    Xtable,
    /// Databend columnar SQL engine. Linux-only.
    Databend,
    /// Qdrant vector store. Linux-only.
    Qdrant,
    /// Restate durable execution. Linux-only.
    Restate,
    /// GreptimeDB unified observability. Linux-only.
    Greptime,
    /// Grafana visualisation. Linux-only.
    Grafana,
    /// OpenFGA fine-grained authorization. Linux-only.
    Openfga,
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let l = Localizer::english();

    match cli.command {
        None => {
            println!("{}", l.t("welcome-banner"));
            println!("{}", l.t("welcome-help"));
        }
        Some(Command::Serve { addr, state_db }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let store = computeza_state::SqliteStore::open(&state_db)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "opening SqliteStore at {state_db}: {e}; \
                         pass --state-db <writable-path> to relocate (the default \
                         `./computeza-state.db` requires write permission in the CWD)"
                        )
                    })?;
                tracing::info!(state_db = %state_db, "metadata store ready");

                let mut state = computeza_ui_server::AppState::with_store(store);

                // Open the operator account file next to the metadata
                // store so login / setup / logout work on the next
                // serve invocation. The file is created (empty) on
                // first run; the operator hits /setup to mint the
                // first credential.
                let operator_path = std::path::Path::new(&state_db)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("operators.jsonl");
                let operators = computeza_ui_server::auth::OperatorFile::open(&operator_path)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "opening operator file at {}: {e}",
                            operator_path.display()
                        )
                    })?;
                if operators.is_empty().await {
                    tracing::warn!(
                        path = %operator_path.display(),
                        "no operator account exists yet; the first-boot setup form is open at /setup. \
                         Anyone with network access to this server can create the initial account -- \
                         bind 127.0.0.1 (the default) until /setup has been completed."
                    );
                } else {
                    tracing::info!(
                        path = %operator_path.display(),
                        "operator account(s) loaded; sign in at /login"
                    );
                }
                state = state.with_operators(operators);

                // Bootstrap the audit log. Key persists at
                // <state_db_parent>/audit.key (chmod 0600); without
                // persistence the signature chain breaks across
                // restarts and historic events become unverifiable.
                let parent = std::path::Path::new(&state_db)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let audit_key_path = parent.join("audit.key");
                let audit_log_path = parent.join("audit.jsonl");
                let audit_key = open_or_init_audit_key(&audit_key_path).await?;
                let audit = computeza_audit::AuditLog::open(&audit_log_path, audit_key)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "opening AuditLog at {}: {e}",
                            audit_log_path.display()
                        )
                    })?;
                tracing::info!(
                    audit_log = %audit_log_path.display(),
                    "audit log attached; sign-in, sign-out, and first-boot setup events are recorded here"
                );
                state = state.with_audit(audit);

                match open_secrets_store(&state_db).await? {
                    Some(s) => {
                        tracing::info!(
                            "secrets store attached; install paths that generate credentials \
                             (initial admin passwords, API tokens) will persist them encrypted"
                        );
                        state = state.with_secrets(s);
                    }
                    None => {
                        tracing::warn!(
                            "COMPUTEZA_SECRETS_PASSPHRASE not set; secrets store is NOT attached. \
                             Install paths that generate credentials will surface them on the \
                             result page only (one-time view; the operator must copy them out). \
                             Set COMPUTEZA_SECRETS_PASSPHRASE before `computeza serve` to enable \
                             encrypted persistence + the rotate-secrets UI."
                        );
                    }
                }

                // License envelope. Verified against the binary's
                // baked-in trusted root + the current time at boot;
                // failures fall back to Community mode (we log + keep
                // serving rather than crashing -- the control plane
                // must stay reachable so the operator can fix the
                // envelope).
                let license_path = parent.join("license.json");
                state = state.with_license_path(license_path.clone());
                match computeza_license::load_license_file(&license_path) {
                    Ok(None) => {
                        tracing::info!(
                            "no license envelope at {path}; running in Community mode. \
                             Activate one at /admin/license to unlock enterprise entitlements.",
                            path = license_path.display(),
                        );
                    }
                    Ok(Some(license)) => {
                        let root = computeza_license::trusted_root();
                        match license.verify(Some(&root), chrono::Utc::now()) {
                            Ok(()) => {
                                tracing::info!(
                                    license_id = %license.payload.id,
                                    tier = %license.payload.tier,
                                    seats = ?license.payload.seats,
                                    expires = %license.payload.not_after.to_rfc3339(),
                                    "license envelope verified; entitlements active"
                                );
                                state.set_license(Some(license)).await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    path = %license_path.display(),
                                    "license envelope on disk failed verification; falling back to Community mode. \
                                     Visit /admin/license to install a fresh envelope."
                                );
                                // Hold the license in state anyway so
                                // the UI can surface why it's invalid
                                // (the status helper returns an
                                // Invalid status; the kill-switch
                                // engages and the banner explains
                                // what the operator needs to do).
                                state.set_license(Some(license)).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %license_path.display(),
                            "could not read license envelope; running in Community mode. \
                             Check filesystem permissions on the path."
                        );
                    }
                }

                let store_for_tick = state
                    .store
                    .clone()
                    .expect("AppState::with_store always populates store");
                let secrets_for_tick = state.secrets.clone();
                tokio::spawn(reconcile_tick(store_for_tick, secrets_for_tick));
                computeza_ui_server::serve_with_state(addr, state).await
            })?;
        }
        Some(Command::Install { component }) => {
            install_component(component, &l)?;
        }
        Some(Command::Status { url }) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(status(&url, &l))?;
        }
        Some(Command::License) => println!("{}", l.t("cmd-license-todo")),
    }
    Ok(())
}

/// Probe the operator console at `url` and print a localized status
/// summary. v0.0.x just hits `/healthz`; future versions surface drift
/// per resource (spec section 4.4 drift indicators).
async fn status(url: &str, l: &Localizer) -> anyhow::Result<()> {
    let healthz = format!("{}/healthz", url.trim_end_matches('/'));
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?
        .get(&healthz)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            println!("{} ({url})", l.t("status-healthy"));
            Ok(())
        }
        Ok(r) => {
            println!("{}: HTTP {} ({url})", l.t("status-unhealthy"), r.status());
            std::process::exit(1);
        }
        Err(e) => {
            println!("{}: {e} ({url})", l.t("status-unreachable"));
            std::process::exit(1);
        }
    }
}

/// Dispatch `computeza install <component>` to the appropriate platform
/// install path. Postgres has full multi-OS coverage; the rest are
/// Linux-only today.
fn install_component(component: InstallComponent, l: &Localizer) -> anyhow::Result<()> {
    match component {
        InstallComponent::Postgres => install_postgres(l),
        InstallComponent::Kanidm => install_simple_linux("kanidm", l),
        InstallComponent::Garage => install_simple_linux("garage", l),
        InstallComponent::Lakekeeper => install_simple_linux("lakekeeper", l),
        InstallComponent::Xtable => install_simple_linux("xtable", l),
        InstallComponent::Databend => install_simple_linux("databend", l),
        InstallComponent::Qdrant => install_simple_linux("qdrant", l),
        InstallComponent::Restate => install_simple_linux("restate", l),
        InstallComponent::Greptime => install_simple_linux("greptime", l),
        InstallComponent::Grafana => install_simple_linux("grafana", l),
        InstallComponent::Openfga => install_simple_linux("openfga", l),
    }
}

/// Run a Linux single-binary-service install for the named component.
/// On non-Linux this is a clear-error stub until per-OS drivers land.
#[cfg(target_os = "linux")]
fn install_simple_linux(component: &str, _l: &Localizer) -> anyhow::Result<()> {
    use computeza_driver_native::linux;
    use computeza_driver_native::progress::ProgressHandle;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let progress = ProgressHandle::noop();
    let result = runtime.block_on(async move {
        match component {
            "kanidm" => linux::kanidm::install(linux::kanidm::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "garage" => linux::garage::install(linux::garage::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "lakekeeper" => {
                linux::lakekeeper::install(linux::lakekeeper::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "xtable" => linux::xtable::install(linux::xtable::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "databend" => {
                linux::databend::install(linux::databend::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "qdrant" => linux::qdrant::install(linux::qdrant::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "restate" => {
                linux::restate::install(linux::restate::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "greptime" => {
                linux::greptime::install(linux::greptime::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "grafana" => {
                linux::grafana::install(linux::grafana::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "openfga" => {
                linux::openfga::install(linux::openfga::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            other => Err(anyhow::anyhow!("unknown component: {other}")),
        }
    })?;
    println!(
        "{component} installed.\n  bin_dir: {}\n  port: {}",
        result.0, result.1
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_simple_linux(component: &str, _l: &Localizer) -> anyhow::Result<()> {
    anyhow::bail!(
        "`computeza install {component}` is currently implemented for Linux only. \
         The driver code is in `crates/computeza-driver-native/src/linux/{component}.rs`; \
         the Windows + macOS variants ship in follow-up commits. Postgres is the only \
         component with full multi-OS coverage so far."
    )
}

#[cfg(target_os = "linux")]
fn install_postgres(_l: &Localizer) -> anyhow::Result<()> {
    use computeza_driver_native::linux::postgres;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(postgres::install(postgres::InstallOptions::default()))?;
    println!(
        "PostgreSQL installed.\n  bin_dir: {}\n  data_dir: {}\n  unit: {}\n  port: {}",
        result.bin_dir.display(),
        result.data_dir.display(),
        result.unit_path.display(),
        result.port,
    );
    if let Some(link) = result.psql_symlink {
        println!("  psql:    {}", link.display());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_postgres(_l: &Localizer) -> anyhow::Result<()> {
    // PostgreSQL is Linux-only in v0.0.x to stay consistent with the
    // other 10 components. The macOS + Windows driver modules under
    // crates/computeza-driver-native/src/{macos,windows}/postgres.rs
    // are reference code from earlier iterations but no longer
    // reachable through the CLI or the wizard. Re-enable at v0.1+
    // when the macOS / Windows variants of the generic service
    // helper land and every component can be installed multi-OS
    // together.
    anyhow::bail!(
        "`computeza install postgres` is currently implemented for Linux only. \
         v0.0.x targets systemd-based Linux on x86_64 for the entire data plane. \
         macOS + Windows native install drivers ship in v0.1+. See README \
         'Platform support' for the supported distro list."
    )
}

/// Periodic in-process reconcile tick. Reads every `postgres-instance`
/// row from the metadata store and runs `observe()` against it,
/// persisting the result back via `with_state(store, name)`. The UI's
/// `/status` page then surfaces the live server version, last-observed
/// timestamp, and the green "Observing" badge.
///
/// v0.0.x only covers postgres-instance; the HTTP reconcilers
/// (kanidm/garage/lakekeeper/...) will join this loop once their
/// resource types are populated through the apply flow.
async fn reconcile_tick(
    store: std::sync::Arc<computeza_state::SqliteStore>,
    secrets: Option<std::sync::Arc<computeza_secrets::SecretsStore>>,
) {
    use computeza_core::reconciler::Context;
    use computeza_core::{NoOpDriver, Reconciler};
    use computeza_reconciler_postgres::{PostgresReconciler, PostgresSpec};
    use computeza_state::Store;

    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    // Don't fire immediately on startup -- give the install path or the
    // operator time to write the first row.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let ctx = Context::default();
    loop {
        tick.tick().await;
        let rows = match store.list("postgres-instance", None).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "reconcile_tick: failed to list postgres-instance resources; \
                     skipping this tick. Will retry on the next 30s interval."
                );
                continue;
            }
        };
        for sr in rows {
            let mut spec: PostgresSpec = match serde_json::from_value(sr.spec.clone()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        kind = %sr.key.kind,
                        name = %sr.key.name,
                        "reconcile_tick: spec did not deserialize as PostgresSpec; \
                         leaving status as Unknown. Inspect /resource/{}/{} and fix the spec.",
                        sr.key.kind, sr.key.name,
                    );
                    continue;
                }
            };
            // Resolve superuser_password_ref against the secrets store
            // before constructing the reconciler. Mirrors the
            // post-install observe path in ui-server so periodic and
            // post-install observations see the same password.
            computeza_ui_server::hydrate_postgres_password(&mut spec, secrets.as_deref()).await;
            let reconciler: PostgresReconciler<NoOpDriver> =
                PostgresReconciler::new(spec.endpoint.clone(), spec.superuser_password)
                    .with_state(store.clone(), &sr.key.name);
            // Observe is best-effort; the reconciler itself logs failures
            // and writes a `last_observe_failed=true` sentinel to the
            // store so the UI can render the orange "Failed" badge.
            let _ = reconciler.observe(&ctx).await;
        }
    }
}

/// Bootstrap the encrypted secrets store at startup.
///
/// Reads `COMPUTEZA_SECRETS_PASSPHRASE` from the environment; returns
/// `Ok(None)` (with a warning at the call site) when the var is unset
/// so a fresh `computeza serve` still boots without forcing the
/// operator to set up secrets immediately. When the passphrase IS set,
/// the salt and ciphertext live next to `state_db` (default CWD):
///
/// - `<state_db_parent>/computeza-secrets.salt` -- 16 random bytes,
///   generated on first run and stable thereafter.
/// - `<state_db_parent>/computeza-secrets.jsonl` -- AES-256-GCM
///   ciphertext lines, one per secret name.
///
/// Loss of the passphrase OR the salt makes existing ciphertext
/// unrecoverable; the operator should back both up.
async fn open_secrets_store(
    state_db_path: &str,
) -> anyhow::Result<Option<computeza_secrets::SecretsStore>> {
    let passphrase = match std::env::var("COMPUTEZA_SECRETS_PASSPHRASE") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(None),
    };

    let parent = std::path::Path::new(state_db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let salt_path = parent.join("computeza-secrets.salt");
    let store_path = parent.join("computeza-secrets.jsonl");

    let salt = ensure_secrets_salt(&salt_path).await?;
    let key = computeza_secrets::derive_kek_from_passphrase(passphrase.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("deriving secrets KEK: {e}"))?;
    let store = computeza_secrets::SecretsStore::open(&store_path, &key)
        .await
        .map_err(|e| anyhow::anyhow!("opening SecretsStore at {}: {e}", store_path.display()))?;
    tracing::info!(
        salt = %salt_path.display(),
        store = %store_path.display(),
        "secrets store opened"
    );
    Ok(Some(store))
}

/// Read the secrets salt from disk, or generate + persist a fresh
/// 16-byte random salt on first run. The salt is NOT secret on its own
/// (it's only useful in combination with the passphrase) but it MUST
/// be stable -- regenerating it would render every existing ciphertext
/// unreadable. First-time generation emits a prominent warn-level log
/// listing the files an operator must back up to keep secrets
/// recoverable.
async fn ensure_secrets_salt(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let salt_exists = tokio::fs::try_exists(path).await.unwrap_or(false);
    if salt_exists {
        let s = tokio::fs::read(path).await?;
        if s.len() >= 16 {
            return Ok(s);
        }
        tracing::warn!(
            path = %path.display(),
            len = s.len(),
            "secrets salt file present but shorter than 16 bytes; regenerating. \
             Existing ciphertext (if any) will become unreadable."
        );
    }
    use aes_gcm::aead::rand_core::RngCore;
    use aes_gcm::aead::OsRng;
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, salt).await?;

    let store_path_hint = path.with_file_name("computeza-secrets.jsonl");
    tracing::warn!(
        salt = %path.display(),
        ciphertext = %store_path_hint.display(),
        "Generated a NEW 16-byte secrets salt -- this is a first-run event. \
         To keep secrets recoverable across hardware migrations and disaster \
         recovery you MUST back up THREE things together: \
         (1) the salt file shown above, \
         (2) the encrypted ciphertext file (computeza-secrets.jsonl in the same \
         directory), and \
         (3) the COMPUTEZA_SECRETS_PASSPHRASE value. \
         Losing any one of these renders every stored secret permanently \
         unrecoverable -- there is no master recovery path by design."
    );
    Ok(salt.to_vec())
}

/// Open the audit signing key at `path`, or generate + persist a
/// fresh one on first run. The key persistence is what lets the
/// signature chain survive a server restart; without it the chain
/// would break each reboot and the audit log becomes unverifiable
/// past the most recent process lifetime.
///
/// The key file is `chmod 0600` on Unix; anyone with read access to
/// it can forge audit entries for past dates.
async fn open_or_init_audit_key(
    path: &std::path::Path,
) -> anyhow::Result<computeza_audit::AuditKey> {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        let bytes = tokio::fs::read(path).await?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("audit key at {} is not 32 bytes", path.display()))?;
        return Ok(computeza_audit::AuditKey::from_secret_bytes(arr));
    }
    let key = computeza_audit::AuditKey::generate();
    let bytes = key.to_secret_bytes();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = tokio::fs::metadata(path).await {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = tokio::fs::set_permissions(path, perms).await;
        }
    }
    tracing::warn!(
        path = %path.display(),
        "Generated a NEW ed25519 audit signing key (first-run event). \
         Back this file up alongside the secrets salt -- losing it means the \
         audit signature chain breaks at the next restart and historic events \
         become unverifiable."
    );
    Ok(key)
}

/// Configure the global tracing subscriber. Reads `RUST_LOG` for filtering
/// (defaults to `info`) and writes structured logs to stderr.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
