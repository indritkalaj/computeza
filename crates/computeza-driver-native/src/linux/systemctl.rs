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
///
/// On failure, the returned `NonZero` error carries a `stderr` that
/// includes both systemctl's own message AND the last 30 journal
/// lines for the unit. Reason: systemctl just prints "control
/// process exited" + "see journalctl"; the actual cause lives in
/// the journal, and the operator hitting the install button has
/// no way to run journalctl themselves. We splice it in here so
/// the failure page surfaces the real reason.
pub async fn enable_now(unit: &str) -> Result<(), SystemctlError> {
    match run(&["enable", "--now", unit]).await {
        Ok(()) => Ok(()),
        Err(SystemctlError::NonZero { args, code, stderr }) => {
            let journal_tail = journal_tail_for_unit(unit).await;
            let enriched = if journal_tail.is_empty() {
                stderr
            } else {
                format!("{stderr}\n\n--- journalctl -u {unit} -n 30 ---\n{journal_tail}")
            };
            Err(SystemctlError::NonZero {
                args,
                code,
                stderr: enriched,
            })
        }
        Err(other) => Err(other),
    }
}

/// Convenience: `systemctl stop <unit>` (ignores 'unit not loaded' errors).
pub async fn stop(unit: &str) -> Result<(), SystemctlError> {
    run(&["stop", unit]).await
}

/// Convenience: `systemctl reset-failed <unit>`. Clears the failure
/// state of a unit that's been restart-looping so a freshly-rewritten
/// systemd unit gets a clean slate to start from. Best-effort: a
/// unit that isn't loaded or isn't in a failed state still exits 0.
pub async fn reset_failed(unit: &str) -> Result<(), SystemctlError> {
    run(&["reset-failed", unit]).await
}

/// Tail the systemd journal for a single unit. Used to enrich the
/// `enable_now` failure path so the operator doesn't have to ssh in
/// and run journalctl themselves. Best-effort: a missing journalctl
/// or a permissioned-out read returns an empty string rather than
/// raising; the surrounding context is already an error.
async fn journal_tail_for_unit(unit: &str) -> String {
    journal_tail(unit, 30).await
}

/// Public variant exposed to per-component drivers so they can
/// enrich their own failure paths (typically a `wait_for_ready`
/// timeout where systemctl returned 0 but the daemon crashed
/// during startup). Same best-effort semantics as
/// [`journal_tail_for_unit`].
///
/// `lines` is the value passed to `journalctl -n` (clamp it to
/// something reasonable -- 30 to 100 is the sane range).
#[must_use]
pub async fn journal_tail(unit: &str, lines: u32) -> String {
    let out = Command::new("journalctl")
        .arg("-u")
        .arg(unit)
        .arg("-n")
        .arg(lines.to_string())
        .arg("--no-pager")
        .arg("--output=cat")
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}
