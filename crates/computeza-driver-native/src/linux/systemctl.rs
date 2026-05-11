//! Thin wrapper around the `systemctl` CLI.
//!
//! Going through the CLI rather than D-Bus keeps the dependency surface
//! tiny and matches what most operators expect to see when they `strace`
//! the installer. The CLI is also stable across systemd versions in a
//! way the D-Bus interface isn't always.

use std::io;

use thiserror::Error;
use tokio::process::Command;

/// systemctl-specific errors.
#[derive(Debug, Error)]
pub enum SystemctlError {
    /// Spawning the `systemctl` process failed (typically: not installed,
    /// not on PATH, or we're not on a systemd-managed host).
    #[error("spawning systemctl failed: {0}")]
    Spawn(io::Error),
    /// `systemctl` ran but returned a non-zero exit code.
    #[error("systemctl {args:?} exited with {code:?}: {stderr}")]
    NonZero {
        /// Arguments that were passed.
        args: Vec<String>,
        /// Exit code (None means signalled).
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
}

/// Run `systemctl <args>` and return Ok if it exits 0.
pub async fn run(args: &[&str]) -> Result<(), SystemctlError> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .await
        .map_err(SystemctlError::Spawn)?;
    if out.status.success() {
        return Ok(());
    }
    Err(SystemctlError::NonZero {
        args: owned,
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Convenience: `systemctl daemon-reload`.
pub async fn daemon_reload() -> Result<(), SystemctlError> {
    run(&["daemon-reload"]).await
}

/// Convenience: `systemctl enable --now <unit>`.
pub async fn enable_now(unit: &str) -> Result<(), SystemctlError> {
    run(&["enable", "--now", unit]).await
}

/// Convenience: `systemctl stop <unit>` (ignores 'unit not loaded' errors).
pub async fn stop(unit: &str) -> Result<(), SystemctlError> {
    run(&["stop", unit]).await
}
