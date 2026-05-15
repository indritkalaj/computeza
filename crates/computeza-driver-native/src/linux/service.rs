//! Shared install/uninstall helpers for single-binary services on Linux.
//!
//! Most managed components (kanidm, garage, qdrant, restate, openfga,
//! grafana, greptime, databend, lakekeeper) are single-binary services:
//! the install path is essentially "download binary, drop a systemd
//! unit, start it". This module factors that out so each component
//! lands as ~80 lines of configuration rather than ~300 lines of
//! reimplementation.
//!
//! Postgres is the special case (initdb, pg_hba, role bootstrap) and
//! keeps its own bespoke driver in `linux::postgres`.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tokio::{fs, net::TcpStream, time::sleep};
use tracing::{info, warn};

use super::{path as pathmod, systemctl};
use crate::{
    fetch::{self, Bundle, FetchError},
    progress::{InstallPhase, ProgressHandle},
};

/// Component-agnostic install configuration. The component-specific
/// driver constructs one of these from its CLI/UI inputs and hands it
/// to [`install_service`].
#[derive(Clone, Debug)]
pub struct ServiceInstall {
    /// Display name used in log lines (e.g. "kanidm", "garage").
    pub component: &'static str,
    /// Subdirectory under `/var/lib/computeza/` that owns the binary
    /// cache + data dir. Typically the component name.
    pub root_dir: PathBuf,
    /// Bundle to fetch + extract.
    pub bundle: Bundle,
    /// Name of the binary inside `bundle.bin_subpath` to launch.
    pub binary_name: &'static str,
    /// Additional CLI args for the service's `ExecStart` line. Wrapped
    /// into the systemd unit verbatim.
    pub args: Vec<String>,
    /// TCP port the service binds. Used for the readiness probe.
    pub port: u16,
    /// systemd unit name including the `.service` suffix.
    pub unit_name: String,
    /// Optional config-file body. Written under `root_dir/<config_filename>`.
    pub config: Option<ConfigFile>,
    /// Name of the CLI tool to symlink into `/usr/local/bin/computeza-<name>`.
    /// None means no PATH registration.
    pub cli_symlink: Option<CliSymlink>,
    /// Environment variables to set on the service's systemd unit via
    /// `Environment=` lines. Used by components that consume their
    /// configuration through env vars rather than (or in addition to)
    /// a config file -- e.g. lakekeeper's `LAKEKEEPER__PG_DATABASE_URL`.
    /// Values are written verbatim into the unit; the driver is
    /// responsible for shell-safe quoting if needed (typically not
    /// needed because systemd parses Environment= line-by-line and
    /// double-quotes inside values are literal).
    pub env: Vec<(String, String)>,
    /// Pre-start commands to run before ExecStart. Each entry is an
    /// argv tail (without the binary path) that systemd executes
    /// sequentially in its own `ExecStartPre=` line. Used by
    /// components that need a first-time setup step before serving
    /// -- e.g. lakekeeper's `migrate` (schema creation) before
    /// `serve`. The same binary as ExecStart is used for each
    /// pre-start; pass a different one by including it as args[0]
    /// in the entry's vec if needed (uncommon).
    ///
    /// systemd's semantics: any non-zero exit aborts the unit
    /// startup, so a failing migrate will surface as a clean
    /// "service failed to start" with the migrate stderr in the
    /// journal (and our journal-tail enrichment relays it to the
    /// install-result page).
    pub exec_start_pre_args: Vec<Vec<String>>,
}

/// One config file laid down before service start.
#[derive(Clone, Debug)]
pub struct ConfigFile {
    /// Filename under `root_dir`. The full path becomes
    /// `<root_dir>/<filename>`.
    pub filename: String,
    /// Verbatim contents.
    pub contents: String,
    /// Idempotency policy. `true` (the default for most drivers)
    /// overwrites any existing config -- safe for files the
    /// driver fully owns. `false` writes only when the path
    /// doesn't exist, preserving operator edits across re-installs.
    /// Drivers whose config is meant as a starting-point template
    /// (databend, future grafana.ini, ...) should set this to
    /// `false`.
    pub overwrite_if_present: bool,
}

/// PATH-shim registration for a CLI that ships in the bundle.
#[derive(Clone, Debug)]
pub struct CliSymlink {
    /// `computeza-<short_name>` is what gets dropped into `/usr/local/bin/`.
    pub short_name: &'static str,
    /// Name of the binary in `bundle.bin_subpath` to point the symlink at.
    pub binary_name: &'static str,
}

/// Result of a successful [`install_service`].
#[derive(Clone, Debug)]
pub struct InstalledService {
    /// Cache directory the binary tree was extracted to.
    pub bin_dir: PathBuf,
    /// Path the systemd unit was written to.
    pub unit_path: PathBuf,
    /// Port the service is now listening on.
    pub port: u16,
    /// Optional PATH symlink created.
    pub cli_symlink: Option<PathBuf>,
}

/// Generic install pipeline. Mirrors the postgres flow at a higher
/// level of abstraction:
///
/// 1. Resolve the binary directory (download bundle if needed).
/// 2. Write the config file under root_dir, if provided.
/// 3. Write a systemd unit at `/etc/systemd/system/<unit_name>`.
/// 4. daemon-reload + enable --now.
/// 5. Wait for the TCP port.
/// 6. Optionally register a `/usr/local/bin/computeza-<short_name>` symlink.
pub async fn install_service(
    opts: ServiceInstall,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message(format!(
        "Fetching {} {} binaries",
        opts.component, opts.bundle.version
    ));
    let cache_root = opts.root_dir.join("binaries");
    let initial_bin_dir = fetch::fetch_and_extract(&cache_root, &opts.bundle, progress).await?;
    // The bundle's `bin_subpath` is a best-effort guess at where the
    // binary lives inside the tarball; vendors frequently rename the
    // top-level directory across versions (e.g.
    // `greptime-linux-amd64-v1.0.1/` vs `greptime/`), which would
    // make the static guess miss. If the binary isn't at the
    // expected path, scan one level deeper for it. This is the
    // bundled-binary equivalent of garage's `find_cargo_root` --
    // tolerate any reasonable nesting without hand-coding per-version
    // bin_subpath values.
    let bin_dir = locate_binary_dir(&initial_bin_dir, opts.binary_name)
        .await
        .ok_or_else(|| ServiceError::BinaryMissing {
            binary: opts.binary_name.into(),
            bin_dir: initial_bin_dir.clone(),
        })?;

    if let Some(cfg) = &opts.config {
        fs::create_dir_all(&opts.root_dir).await?;
        let target = opts.root_dir.join(&cfg.filename);
        let already_present = fs::try_exists(&target).await.unwrap_or(false);
        if already_present && !cfg.overwrite_if_present {
            progress.set_message(format!(
                "Keeping operator-edited config at {} (re-install does not clobber)",
                target.display()
            ));
            info!(path = %target.display(), "config preserved (overwrite_if_present=false)");
        } else {
            progress.set_message(format!(
                "Writing config {}/{}",
                opts.root_dir.display(),
                cfg.filename
            ));
            fs::write(&target, &cfg.contents).await?;
        }
    }

    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering systemd unit {}", opts.unit_name));
    let bin_path = bin_dir.join(opts.binary_name);
    let args_str = opts.args.join(" ");
    let unit_body = systemd_unit(
        opts.component,
        &bin_path,
        &args_str,
        &opts.root_dir,
        &opts.env,
        &opts.exec_start_pre_args,
    );
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    fs::write(&unit_path, &unit_body).await?;
    info!(unit = %unit_path.display(), "wrote systemd unit");
    systemctl::daemon_reload().await?;

    // Stop the unit before enabling --now. Reason: `enable --now`
    // is a no-op if the service is already active OR in a
    // restart loop. `daemon-reload` re-reads the unit file from
    // disk, but the *running* process keeps using the in-memory
    // unit it was started with -- including the OLD `Environment=`
    // lines, OLD `ExecStart`, etc. A re-install therefore writes
    // a corrected unit to disk that the actually-running daemon
    // never sees.
    //
    // Best-effort: a missing-unit `stop` returns non-zero
    // harmlessly. The subsequent `enable --now` always starts the
    // service afresh, picking up every change in the rewritten
    // unit.
    let _ = systemctl::stop(&opts.unit_name).await;

    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    systemctl::enable_now(&opts.unit_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for port {} to accept connections",
        opts.port
    ));
    // On timeout, splice the systemd journal tail into the error
    // so the install-result page surfaces the actual daemon-side
    // failure (config-schema mismatch, missing native lib, port
    // collision, ...) instead of the generic "did not become
    // ready" string. Every driver that goes through
    // install_service benefits without per-driver wiring.
    if let Err(e) = wait_for_port("127.0.0.1", opts.port, Duration::from_secs(30)).await {
        if matches!(e, ServiceError::NotReady { .. }) {
            let tail = systemctl::journal_tail(&opts.unit_name, 60).await;
            if !tail.is_empty() {
                return Err(ServiceError::Io(io::Error::other(format!(
                    "{} did not bind 127.0.0.1:{} within 30s. Journal tail (most recent {} lines from `journalctl -u {}`):\n\n{tail}",
                    opts.component,
                    opts.port,
                    tail.lines().count(),
                    opts.unit_name,
                ))));
            }
        }
        return Err(e);
    }

    let cli_symlink = if let Some(cli) = &opts.cli_symlink {
        progress.set_phase(InstallPhase::RegisteringPath);
        progress.set_message(format!("Registering CLI {}", cli.short_name));
        match pathmod::register(cli.short_name, &bin_dir.join(cli.binary_name)).await {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(error = %e, "PATH registration failed; install otherwise complete");
                None
            }
        }
    } else {
        None
    };

    Ok(InstalledService {
        bin_dir,
        unit_path,
        port: opts.port,
        cli_symlink,
    })
}

/// Component-agnostic uninstall pipeline mirroring `install_service`.
///
/// Best-effort and idempotent: every step swallows "already gone" errors.
pub async fn uninstall_service(
    component: &str,
    root_dir: &Path,
    unit_name: &str,
    cli_short_name: Option<&str>,
) -> Result<Uninstalled, ServiceError> {
    let mut out = Uninstalled::default();

    if let Err(e) = systemctl::stop(unit_name).await {
        out.warn(format!("systemctl stop {unit_name}: {e}"));
    } else {
        out.ok(format!("stopped {unit_name}"));
    }
    if let Err(e) = systemctl::run(&["disable", unit_name]).await {
        out.warn(format!("systemctl disable {unit_name}: {e}"));
    } else {
        out.ok(format!("disabled {unit_name}"));
    }
    let unit_path = PathBuf::from("/etc/systemd/system").join(unit_name);
    if fs::try_exists(&unit_path).await.unwrap_or(false) {
        match fs::remove_file(&unit_path).await {
            Ok(()) => out.ok(format!("removed unit file {}", unit_path.display())),
            Err(e) => out.warn(format!("removing unit file {}: {e}", unit_path.display())),
        }
    }
    // Older releases shipped a sibling drop-in dir
    // /etc/systemd/system/{unit}.d/; clean that too if present so
    // re-installs start with a clean override stack.
    let dropin_dir = PathBuf::from("/etc/systemd/system").join(format!("{unit_name}.d"));
    if fs::try_exists(&dropin_dir).await.unwrap_or(false) {
        match fs::remove_dir_all(&dropin_dir).await {
            Ok(()) => out.ok(format!("removed drop-in dir {}", dropin_dir.display())),
            Err(e) => out.warn(format!("removing drop-in dir {}: {e}", dropin_dir.display())),
        }
    }
    // systemd may still hold a failed-unit reference even after the
    // file is gone; reset-failed clears it so the next install
    // doesn't trip the "already exists in failed state" check.
    if let Err(e) = systemctl::run(&["reset-failed", unit_name]).await {
        // Not fatal -- most often this just means the unit was
        // never in a failed state. Log at info level.
        tracing::info!(unit = %unit_name, error = %e, "uninstall: reset-failed (informational)");
    }
    if let Err(e) = systemctl::daemon_reload().await {
        out.warn(format!("daemon-reload: {e}"));
    } else {
        out.ok("daemon-reload");
    }

    // Wipe the entire component root_dir (binaries, configs, data,
    // logs, snapshot dirs, lock files, ...). Previously we only
    // removed root_dir/data which left binaries/<version>/, the
    // generated *.toml, and per-component subdirs like meta/, raft/,
    // metadata/ behind -- visible as "residuals" after uninstall.
    if fs::try_exists(root_dir).await.unwrap_or(false) {
        match fs::remove_dir_all(root_dir).await {
            Ok(()) => out.ok(format!("removed component dir {}", root_dir.display())),
            Err(e) => out.warn(format!("removing {}: {e}", root_dir.display())),
        }
    }

    // systemd RuntimeDirectory: /run/{component} is auto-created at
    // unit-start time and auto-removed at unit-stop time, but if the
    // unit crashed we sometimes see a leftover. Best-effort sweep.
    let runtime_dir = PathBuf::from("/run").join(component);
    if fs::try_exists(&runtime_dir).await.unwrap_or(false) {
        match fs::remove_dir_all(&runtime_dir).await {
            Ok(()) => out.ok(format!("removed runtime dir {}", runtime_dir.display())),
            Err(e) => {
                // /run is tmpfs -- failures here usually mean
                // another process is holding a file open. Don't
                // fail the uninstall over it.
                tracing::info!(dir = %runtime_dir.display(), error = %e, "uninstall: could not remove runtime dir (informational)");
            }
        }
    }

    if let Some(name) = cli_short_name {
        if let Err(e) = pathmod::unregister(name).await {
            out.warn(format!("removing {name} symlink: {e}"));
        } else {
            out.ok(format!("removed /usr/local/bin/computeza-{name}"));
        }
    }

    // Best-effort: if /var/lib/computeza is now empty (every
    // component uninstalled), remove the parent so the operator
    // can verify "no residuals" with a single `ls`. We only remove
    // when truly empty -- never `remove_dir_all` on the shared
    // parent, since other components may still be installed.
    if let Some(parent) = root_dir.parent() {
        if parent.ends_with("computeza") {
            if let Ok(mut entries) = fs::read_dir(parent).await {
                if entries.next_entry().await.ok().flatten().is_none() {
                    match fs::remove_dir(parent).await {
                        Ok(()) => out.ok(format!("removed empty parent {}", parent.display())),
                        Err(e) => tracing::info!(parent = %parent.display(), error = %e, "uninstall: parent not empty / could not remove (informational)"),
                    }
                }
            }
        }
    }

    Ok(out)
}

#[derive(Clone, Debug, Default)]
pub struct Uninstalled {
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
}

impl Uninstalled {
    pub fn ok(&mut self, msg: impl Into<String>) {
        self.steps.push(msg.into());
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

/// Errors raised by [`install_service`] / [`uninstall_service`].
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Systemctl(#[from] systemctl::SystemctlError),
    #[error("expected binary {binary:?} not found under {}", bin_dir.display())]
    BinaryMissing { binary: String, bin_dir: PathBuf },
    #[error("service did not become ready on port {port} within {timeout_secs}s")]
    NotReady { port: u16, timeout_secs: u64 },
}

fn systemd_unit(
    component: &str,
    bin_path: &Path,
    args: &str,
    root_dir: &Path,
    env: &[(String, String)],
    exec_start_pre_args: &[Vec<String>],
) -> String {
    // WorkingDirectory=<root>: many daemons (qdrant, kanidm,
    // probably others) resolve runtime paths -- snapshot temp
    // dirs, init-flag dot-files, log files -- relative to CWD.
    // systemd's default CWD is `/`, which is read-only under the
    // ProtectSystem=strict sandbox, so those relative writes
    // crash the daemon at startup. Setting WorkingDirectory to
    // the component's root_dir means every `./foo` inside the
    // daemon's code lands somewhere we already have in
    // ReadWritePaths.
    //
    // RuntimeDirectory=<component>: matches the postgres / kanidm
    // forward-compat. systemd mints /run/<component>/ for socket
    // / lock / pid files; cheap to include even when unused.
    //
    // Environment= lines: emitted one per `env` entry. Used by
    // components like lakekeeper that consume their config via env
    // vars (postgres connection string, encryption key, ...). We
    // quote values with double quotes; systemd reads the value
    // verbatim between the quotes, so embedded single quotes /
    // shell metacharacters are fine. Embedded double quotes in
    // values would need escaping; today's call sites don't supply
    // any, and the function asserts on that in debug builds.
    //
    // CRITICAL: each line MUST start flush-left in the resulting
    // string. systemd treats a line that begins with whitespace
    // as a continuation of the previous directive (per
    // systemd.syntax), so an indented `Environment="K2=V2"` would
    // be folded into the previous line's value rather than parsed
    // as a new directive. That's exactly the bug that broke the
    // lakekeeper install -- the second Environment line was being
    // absorbed into the first one and ICEBERG_REST__PG_DATABASE_URL
    // never reached the daemon.
    //
    // The surrounding format! string uses Rust's `\<newline>`
    // line-continuation which consumes the newline plus any
    // following whitespace, so its lines render flush-left. The
    // env block doesn't get that treatment because it's
    // interpolated; we have to emit `\n` (only) at line ends
    // ourselves.
    let env_block: String = env
        .iter()
        .map(|(k, v)| {
            debug_assert!(
                !v.contains('"'),
                "Environment value for {k} contains a double quote which would break systemd parsing"
            );
            format!("Environment=\"{k}={v}\"\n")
        })
        .collect();

    // ExecStartPre block: one line per pre-step. systemd runs them
    // sequentially before ExecStart; any non-zero exit aborts the
    // unit startup. Same flush-left rule as Environment= (leading
    // whitespace would fold into the previous directive).
    let pre_block: String = exec_start_pre_args
        .iter()
        .map(|args| format!("ExecStartPre={} {}\n", bin_path.display(), args.join(" ")))
        .collect();
    format!(
        "[Unit]\n\
         Description=Computeza-managed {component}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={root}\n\
         RuntimeDirectory={component}\n\
         RuntimeDirectoryMode=0755\n\
         {env_block}{pre_block}ExecStart={bin} {args}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         ReadWritePaths={root}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        component = component,
        bin = bin_path.display(),
        args = args,
        root = root_dir.display(),
        env_block = env_block,
        pre_block = pre_block,
    )
}

/// Locate a binary inside a freshly-extracted bundle, tolerating
/// the per-version nesting that several upstreams use (e.g.
/// `greptime-linux-amd64-v1.0.1/greptime`, `grafana-v13.0.1/bin/grafana`).
///
/// Strategy:
/// 1. If `<start>/<binary>` exists, return `start` (the common case
///    where the bundle's `bin_subpath` was correct).
/// 2. Otherwise, scan `start`'s direct children. If exactly one is a
///    directory AND it contains the binary, return that subdir.
/// 3. Otherwise, recurse one more level (the grafana case:
///    `grafana-v13.0.1/bin/grafana` is two levels deep from the
///    extraction root).
/// 4. Give up after two levels -- deeper nesting suggests a
///    misconfigured bundle and should fail loudly rather than be
///    silently auto-detected.
async fn locate_binary_dir(start: &Path, binary_name: &str) -> Option<PathBuf> {
    // Walk up until we find an existing directory. Mis-pinned
    // bin_subpaths land us in a non-existent path; the parent
    // (the extraction root) always exists post-fetch_and_extract,
    // so we recover by scanning from the closest existing
    // ancestor. This makes the scanner self-healing against
    // future tarball-layout drift (e.g. grafana renamed
    // `grafana-v13.0.1/` to `grafana-13.0.1/` between our pin
    // and the actual upstream release).
    let mut effective_start = start.to_path_buf();
    while !fs::try_exists(&effective_start).await.unwrap_or(false) {
        match effective_start.parent() {
            Some(p) if p != effective_start => effective_start = p.to_path_buf(),
            _ => return None,
        }
    }

    if fs::try_exists(effective_start.join(binary_name))
        .await
        .unwrap_or(false)
    {
        return Some(effective_start);
    }
    // Scan one level deep.
    if let Some(level1) = single_subdir_containing(&effective_start, binary_name).await {
        return Some(level1);
    }
    // Scan two levels deep -- check each direct subdir.
    let mut entries = fs::read_dir(&effective_start).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_type().await.ok()?.is_dir() {
            continue;
        }
        if let Some(level2) = single_subdir_containing(&entry.path(), binary_name).await {
            return Some(level2);
        }
    }
    None
}

/// List `dir`'s subdirectories; if any contains a file named
/// `binary_name`, return the path to that subdirectory. Returns
/// `None` if zero or multiple matches (we don't want to silently
/// pick the wrong one).
async fn single_subdir_containing(dir: &Path, binary_name: &str) -> Option<PathBuf> {
    let mut entries = fs::read_dir(dir).await.ok()?;
    let mut hits = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_type().await.ok()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if fs::try_exists(path.join(binary_name))
            .await
            .unwrap_or(false)
        {
            hits.push(path);
        }
    }
    if hits.len() == 1 {
        Some(hits.into_iter().next().unwrap())
    } else {
        None
    }
}

pub(super) async fn wait_for_port(host: &str, port: u16, timeout: Duration) -> Result<(), ServiceError> {
    let deadline = Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    loop {
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::NotReady {
                port,
                timeout_secs: timeout.as_secs(),
            });
        }
        sleep(Duration::from_millis(500)).await;
    }
}

// Suppress the `Pin` warning we don't actually need here.
#[allow(unused_imports)]
use tokio::io::AsyncWriteExt as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_emits_each_environment_directive_flush_left() {
        // Regression net for the lakekeeper bug: the env_block
        // emitter used to terminate each Environment= line with
        // `\n         ` (matching the surrounding format! string's
        // 9-space continuation indent). systemd treats leading
        // whitespace as line continuation, so the SECOND env var
        // was being folded into the first one's value -- the
        // daemon never saw ICEBERG_REST__PG_DATABASE_URL as a
        // discrete env var and lakekeeper died with "A connection
        // string or postgres host must be provided" through 500+
        // restart attempts. The fix: terminate each line with
        // just `\n` so the next directive starts in column 0.
        let bin = PathBuf::from("/var/lib/computeza/lakekeeper/binaries/0.12.2/lakekeeper");
        let root = PathBuf::from("/var/lib/computeza/lakekeeper");
        let env = vec![
            (
                "ICEBERG_REST__PG_DATABASE_URL".to_string(),
                "postgres://u:p@h:5432/d".to_string(),
            ),
            (
                "ICEBERG_REST__PG_ENCRYPTION_KEY".to_string(),
                "deadbeef".to_string(),
            ),
        ];
        let unit = systemd_unit("lakekeeper", &bin, "serve", &root, &env, &[]);
        // Each Environment= line must start in column 0 (no
        // leading whitespace). We split on \n and look for any
        // line that starts with whitespace AND contains
        // "Environment=" -- the regression test fails loudly if
        // ANY such line exists.
        for (i, line) in unit.lines().enumerate() {
            if line.contains("Environment=") {
                assert!(
                    !line.starts_with(char::is_whitespace),
                    "line {i} starts with whitespace: {line:?}\nsystemd would parse this as a continuation of the previous directive. Each Environment= line must be flush-left.\nFull unit:\n{unit}"
                );
            }
        }
        // Belt-and-braces: assert both directive names appear at
        // the start of a line.
        assert!(
            unit.contains(
                "\nEnvironment=\"ICEBERG_REST__PG_DATABASE_URL=postgres://u:p@h:5432/d\"\n"
            ),
            "first env var must render as its own flush-left line"
        );
        assert!(
            unit.contains("\nEnvironment=\"ICEBERG_REST__PG_ENCRYPTION_KEY=deadbeef\"\n"),
            "second env var must render as its own flush-left line"
        );
        // And ExecStart must immediately follow the last env line,
        // also flush-left.
        assert!(
            unit.contains(
                "\nExecStart=/var/lib/computeza/lakekeeper/binaries/0.12.2/lakekeeper serve\n"
            ),
            "ExecStart must render flush-left after the env block"
        );
    }

    #[test]
    fn systemd_unit_with_empty_env_omits_environment_lines() {
        let bin = PathBuf::from("/var/lib/computeza/openfga/binaries/1.15.1/openfga");
        let root = PathBuf::from("/var/lib/computeza/openfga");
        let unit = systemd_unit("openfga", &bin, "run --inmem", &root, &[], &[]);
        assert!(!unit.contains("Environment="));
        assert!(unit.contains(
            "\nExecStart=/var/lib/computeza/openfga/binaries/1.15.1/openfga run --inmem\n"
        ));
        assert!(!unit.contains("ExecStartPre="));
    }

    #[test]
    fn systemd_unit_emits_exec_start_pre_lines_flush_left_before_execstart() {
        // Lakekeeper-shaped regression: migrate must run before
        // serve. Each ExecStartPre= line must (a) be flush-left
        // and (b) appear in the unit BEFORE ExecStart= so systemd
        // sequences them correctly.
        let bin = PathBuf::from("/var/lib/computeza/lakekeeper/binaries/0.12.2/lakekeeper");
        let root = PathBuf::from("/var/lib/computeza/lakekeeper");
        let pre: &[Vec<String>] = &[vec!["migrate".to_string()]];
        let unit = systemd_unit("lakekeeper", &bin, "serve", &root, &[], pre);
        assert!(
            unit.contains(
                "\nExecStartPre=/var/lib/computeza/lakekeeper/binaries/0.12.2/lakekeeper migrate\n"
            ),
            "ExecStartPre line must render flush-left with the full bin path + args.\nUnit:\n{unit}"
        );
        // Order check: ExecStartPre must come before ExecStart.
        let pre_idx = unit.find("ExecStartPre=").expect("ExecStartPre missing");
        let start_idx = unit.find("ExecStart=").expect("ExecStart missing");
        assert!(
            pre_idx < start_idx,
            "ExecStartPre must appear before ExecStart in the unit (systemd sequences them in declaration order)"
        );
    }
}
