//! macOS analogue of `linux::service`: shared install/uninstall
//! helpers for single-binary services. Drives launchd via the
//! [`crate::macos::launchctl`] wrapper instead of systemctl.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tokio::{fs, net::TcpStream, time::sleep};
use tracing::{info, warn};

use super::{launchctl, path as pathmod};
use crate::{
    fetch::{self, Bundle, FetchError},
    progress::{InstallPhase, ProgressHandle},
};

#[derive(Clone, Debug)]
pub struct ServiceInstall {
    pub component: &'static str,
    pub root_dir: PathBuf,
    pub bundle: Bundle,
    pub binary_name: &'static str,
    pub args: Vec<String>,
    pub port: u16,
    /// launchd label (e.g. `com.computeza.kanidm`). The plist is
    /// written to `/Library/LaunchDaemons/<label>.plist`.
    pub label: String,
    pub config: Option<ConfigFile>,
    pub cli_symlink: Option<CliSymlink>,
}

#[derive(Clone, Debug)]
pub struct ConfigFile {
    pub filename: String,
    pub contents: String,
}

#[derive(Clone, Debug)]
pub struct CliSymlink {
    pub short_name: &'static str,
    pub binary_name: &'static str,
}

#[derive(Clone, Debug)]
pub struct InstalledService {
    pub bin_dir: PathBuf,
    pub plist_path: PathBuf,
    pub port: u16,
    pub cli_symlink: Option<PathBuf>,
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

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Launchctl(#[from] launchctl::LaunchctlError),
    #[error("expected binary {binary:?} not found under {}", bin_dir.display())]
    BinaryMissing { binary: String, bin_dir: PathBuf },
    #[error("service did not become ready on port {port} within {timeout_secs}s")]
    NotReady { port: u16, timeout_secs: u64 },
}

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
    let bin_dir = fetch::fetch_and_extract(&cache_root, &opts.bundle, progress).await?;
    if !fs::try_exists(bin_dir.join(opts.binary_name))
        .await
        .unwrap_or(false)
    {
        return Err(ServiceError::BinaryMissing {
            binary: opts.binary_name.into(),
            bin_dir: bin_dir.clone(),
        });
    }

    if let Some(cfg) = &opts.config {
        progress.set_message(format!(
            "Writing config {}/{}",
            opts.root_dir.display(),
            cfg.filename
        ));
        fs::create_dir_all(&opts.root_dir).await?;
        fs::write(opts.root_dir.join(&cfg.filename), &cfg.contents).await?;
    }

    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Writing launchd plist for {}", opts.label));
    let bin_path = bin_dir.join(opts.binary_name);
    let plist_body = launchd_plist(&opts.label, &bin_path, &opts.args, &opts.root_dir);
    let plist_path = PathBuf::from("/Library/LaunchDaemons").join(format!("{}.plist", opts.label));
    fs::write(&plist_path, &plist_body).await?;
    info!(plist = %plist_path.display(), "wrote launchd plist");

    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Loading {} into launchd", opts.label));
    launchctl::bootstrap_idempotent(plist_path.to_string_lossy().as_ref()).await?;
    launchctl::kickstart(&opts.label).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for port {} to accept connections",
        opts.port
    ));
    wait_for_port("127.0.0.1", opts.port, Duration::from_secs(30)).await?;

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
        plist_path,
        port: opts.port,
        cli_symlink,
    })
}

pub async fn uninstall_service(
    component: &str,
    root_dir: &Path,
    label: &str,
    cli_short_name: Option<&str>,
) -> Result<Uninstalled, ServiceError> {
    let mut out = Uninstalled::default();

    if let Err(e) = launchctl::bootout(label).await {
        out.warn(format!("launchctl bootout system/{label}: {e}"));
    } else {
        out.ok(format!("bootout system/{label}"));
    }
    let plist_path = PathBuf::from("/Library/LaunchDaemons").join(format!("{label}.plist"));
    if fs::try_exists(&plist_path).await.unwrap_or(false) {
        match fs::remove_file(&plist_path).await {
            Ok(()) => out.ok(format!("removed plist {}", plist_path.display())),
            Err(e) => out.warn(format!("removing plist {}: {e}", plist_path.display())),
        }
    }
    let data_dir = root_dir.join("data");
    if fs::try_exists(&data_dir).await.unwrap_or(false) {
        match fs::remove_dir_all(&data_dir).await {
            Ok(()) => out.ok(format!("removed data dir {}", data_dir.display())),
            Err(e) => out.warn(format!("removing data dir {}: {e}", data_dir.display())),
        }
    }
    if let Some(name) = cli_short_name {
        if let Err(e) = pathmod::unregister(name).await {
            out.warn(format!("removing {name} symlink: {e}"));
        } else {
            out.ok(format!("removed computeza-{name} symlink"));
        }
    }
    let _ = component;
    Ok(out)
}

fn launchd_plist(label: &str, bin_path: &Path, args: &[String], root_dir: &Path) -> String {
    let mut arg_strs = String::new();
    arg_strs.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&bin_path.to_string_lossy())
    ));
    for a in args {
        arg_strs.push_str(&format!("        <string>{}</string>\n", xml_escape(a)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
             <key>Label</key>\n    <string>{label}</string>\n\
             <key>ProgramArguments</key>\n    <array>\n{args}    </array>\n\
             <key>WorkingDirectory</key>\n    <string>{root}</string>\n\
             <key>RunAtLoad</key>\n    <true/>\n\
             <key>KeepAlive</key>\n    <true/>\n\
         </dict>\n\
         </plist>\n",
        label = xml_escape(label),
        args = arg_strs,
        root = xml_escape(&root_dir.to_string_lossy()),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn wait_for_port(host: &str, port: u16, timeout: Duration) -> Result<(), ServiceError> {
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
