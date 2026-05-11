//! Thin wrapper around `launchctl`.
//!
//! macOS 10.11+ deprecated `launchctl load` / `unload` in favour of the
//! `bootstrap` / `bootout` verbs which take a domain target. For system
//! daemons that's `system` (literally), for user agents it's `gui/<uid>`.
//! We only manage system daemons, so the domain string is constant.

use std::io;

use thiserror::Error;
use tokio::process::Command;

/// Domain that owns Computeza-managed daemons.
pub const SYSTEM_DOMAIN: &str = "system";

/// launchctl-specific errors.
#[derive(Debug, Error)]
pub enum LaunchctlError {
    /// Spawning the `launchctl` process failed.
    #[error("spawning launchctl failed: {0}")]
    Spawn(io::Error),
    /// `launchctl` ran but returned a non-zero exit code.
    #[error("launchctl {args:?} exited with {code:?}: {stderr}")]
    NonZero {
        /// Arguments that were passed.
        args: Vec<String>,
        /// Exit code (None means signalled).
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
}

/// Run `launchctl <args>` and return Ok if it exits 0.
pub async fn run(args: &[&str]) -> Result<(), LaunchctlError> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let out = Command::new("launchctl")
        .args(args)
        .output()
        .await
        .map_err(LaunchctlError::Spawn)?;
    if out.status.success() {
        return Ok(());
    }
    Err(LaunchctlError::NonZero {
        args: owned,
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// `launchctl bootstrap system <plist_path>`. Idempotent in the sense
/// that bootstrapping an already-loaded service returns success on
/// modern launchctl; older versions error 17 ("file exists"), which we
/// translate to a clean Ok via [`bootstrap_idempotent`].
pub async fn bootstrap_system(plist_path: &str) -> Result<(), LaunchctlError> {
    run(&["bootstrap", SYSTEM_DOMAIN, plist_path]).await
}

/// As [`bootstrap_system`] but treats exit code 17 (file exists) as success.
pub async fn bootstrap_idempotent(plist_path: &str) -> Result<(), LaunchctlError> {
    match bootstrap_system(plist_path).await {
        Ok(()) => Ok(()),
        Err(LaunchctlError::NonZero { code: Some(17), .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `launchctl bootout system/<label>`. Translates "service not found"
/// into clean Ok so uninstall is idempotent.
pub async fn bootout(label: &str) -> Result<(), LaunchctlError> {
    let target = format!("{SYSTEM_DOMAIN}/{label}");
    match run(&["bootout", &target]).await {
        Ok(()) => Ok(()),
        Err(LaunchctlError::NonZero {
            code: Some(113), ..
        }) => Ok(()), // 113 = "Could not find service"
        Err(e) => Err(e),
    }
}

/// `launchctl kickstart -k system/<label>` -- ensures the service is
/// running, restarting it if it was already loaded.
pub async fn kickstart(label: &str) -> Result<(), LaunchctlError> {
    let target = format!("{SYSTEM_DOMAIN}/{label}");
    run(&["kickstart", "-k", &target]).await
}
