//! Wrapper around `sc.exe` -- the Windows Service Control Manager CLI.
//!
//! `sc.exe` has a famously persnickety argument format: keys MUST be
//! followed by `=` AND a space (`binPath= "..."` not `binPath="..."`).
//! We hide that wart behind typed helpers.

use std::io;

use thiserror::Error;
use tokio::process::Command;

/// sc.exe-specific errors.
#[derive(Debug, Error)]
pub enum ScError {
    /// Spawning the `sc.exe` process failed.
    #[error("spawning sc.exe failed: {0}")]
    Spawn(io::Error),
    /// `sc.exe` ran but returned a non-zero exit code.
    #[error("sc {args:?} exited with {code:?}: {stdout}")]
    NonZero {
        /// Arguments that were passed.
        args: Vec<String>,
        /// Exit code (None means signalled).
        code: Option<i32>,
        /// Captured stdout (sc.exe writes to stdout on error too).
        stdout: String,
    },
}

/// Run `sc.exe <args>` and return Ok if it exits 0.
pub async fn run(args: &[&str]) -> Result<(), ScError> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let out = Command::new("sc.exe")
        .args(args)
        .output()
        .await
        .map_err(ScError::Spawn)?;
    if out.status.success() {
        return Ok(());
    }
    Err(ScError::NonZero {
        args: owned,
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    })
}

/// Configuration for a Windows Service we create via `sc.exe create`.
pub struct ServiceSpec<'a> {
    /// Internal service name (no spaces).
    pub name: &'a str,
    /// Human-readable display name shown in services.msc.
    pub display_name: &'a str,
    /// Fully-quoted command line: `"C:\path\to\exe.exe" arg1 "arg with spaces"`.
    pub bin_path: &'a str,
    /// `auto` (boot), `demand` (manual), `disabled`, etc.
    pub start: &'a str,
}

/// `sc create` the service. Returns Ok if the service already exists with
/// the same configuration -- callers expecting idempotent install can
/// follow up with `sc config` if they need to update fields.
pub async fn create(spec: &ServiceSpec<'_>) -> Result<(), ScError> {
    // sc.exe argument quirk: each key must be followed by `= ` (equals
    // then space). The space goes in the SAME argv slot as the equals
    // sign, hence the leading "" formatting below.
    let args = [
        "create",
        spec.name,
        "binPath=",
        spec.bin_path,
        "DisplayName=",
        spec.display_name,
        "start=",
        spec.start,
    ];
    match run(&args).await {
        Ok(()) => Ok(()),
        // 1073 = ERROR_SERVICE_EXISTS
        Err(ScError::NonZero {
            code: Some(1073), ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `sc start <name>`. Returns Ok if the service is already running.
pub async fn start(name: &str) -> Result<(), ScError> {
    match run(&["start", name]).await {
        Ok(()) => Ok(()),
        // 1056 = ERROR_SERVICE_ALREADY_RUNNING
        Err(ScError::NonZero {
            code: Some(1056), ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `sc stop <name>`. Returns Ok if the service is already stopped or absent.
pub async fn stop(name: &str) -> Result<(), ScError> {
    match run(&["stop", name]).await {
        Ok(()) => Ok(()),
        // 1062 = ERROR_SERVICE_NOT_ACTIVE; 1060 = ERROR_SERVICE_DOES_NOT_EXIST
        Err(ScError::NonZero {
            code: Some(1062 | 1060),
            ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `sc delete <name>`. Returns Ok if the service does not exist.
pub async fn delete(name: &str) -> Result<(), ScError> {
    match run(&["delete", name]).await {
        Ok(()) => Ok(()),
        Err(ScError::NonZero {
            code: Some(1060), ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}
