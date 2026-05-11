//! Cross-shell PATH registration on Windows.
//!
//! Two-pronged approach:
//!
//! 1. **`.cmd` shim** at `%PROGRAMFILES%\Computeza\bin\computeza-<name>.cmd`
//!    that forwards to the real binary. The shim is a 1-line text file
//!    so creating it doesn't need NTFS-symlink developer mode.
//! 2. **Machine PATH** entry for `%PROGRAMFILES%\Computeza\bin` added
//!    via PowerShell's `[Environment]::SetEnvironmentVariable` -- this
//!    writes the registry and broadcasts WM_SETTINGCHANGE so new shells
//!    pick the change up without a logout.
//!
//! Both steps need elevated permissions (writing under Program Files,
//! editing machine env vars). The installer runs elevated via UAC.

use std::{
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt, process::Command};
use tracing::{debug, info};

/// Errors during PATH registration.
#[derive(Debug, Error)]
pub enum PathError {
    /// Filesystem operation failed (typically: not running elevated).
    #[error("filesystem: {0}")]
    Io(#[from] io::Error),
    /// PowerShell failed to update machine PATH.
    #[error("powershell PATH update failed (exit {code:?}): {stderr}")]
    PowershellFailed {
        /// Exit code from powershell.exe.
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
    /// Binary referenced by the registration call does not exist.
    #[error("binary not found: {0}")]
    BinaryNotFound(PathBuf),
    /// %PROGRAMFILES% env var was unreadable.
    #[error("PROGRAMFILES env var not set")]
    NoProgramFiles,
}

/// Where shims live.
fn shim_root() -> Result<PathBuf, PathError> {
    let pf = std::env::var("PROGRAMFILES").map_err(|_| PathError::NoProgramFiles)?;
    Ok(PathBuf::from(pf).join("Computeza").join("bin"))
}

/// Register a binary by name: writes
/// `%PROGRAMFILES%\Computeza\bin\computeza-<name>.cmd` that forwards to
/// `target_bin`, and adds the shim root to machine PATH.
pub async fn register(name: &str, target_bin: &Path) -> Result<PathBuf, PathError> {
    if !fs::try_exists(target_bin).await? {
        return Err(PathError::BinaryNotFound(target_bin.to_path_buf()));
    }
    let root = shim_root()?;
    fs::create_dir_all(&root).await?;

    let shim_path = root.join(format!("computeza-{name}.cmd"));
    // 1-line forwarding shim. `%*` forwards all CLI args verbatim.
    // The double-quotes around the target handle paths with spaces.
    let body = format!("@\"{}\" %*\r\n", target_bin.display());
    let mut f = fs::File::create(&shim_path).await?;
    f.write_all(body.as_bytes()).await?;
    f.sync_all().await?;
    info!(shim = %shim_path.display(), target = %target_bin.display(), "wrote PATH shim");

    add_to_machine_path(&root).await?;

    Ok(shim_path)
}

/// Reverse of [`register`]: remove the .cmd shim if present. The
/// machine-PATH entry is left in place (other Computeza-managed
/// binaries may still rely on it); a separate `unregister_path_root`
/// call removes it.
pub async fn unregister(name: &str) -> Result<(), PathError> {
    let root = shim_root()?;
    let shim_path = root.join(format!("computeza-{name}.cmd"));
    match fs::remove_file(&shim_path).await {
        Ok(()) => {
            debug!(shim = %shim_path.display(), "removed PATH shim");
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PathError::Io(e)),
    }
}

/// Add `dir` to the machine PATH if it isn't already present.
async fn add_to_machine_path(dir: &Path) -> Result<(), PathError> {
    let dir_str = dir.to_string_lossy().into_owned();
    // PowerShell one-liner: read machine PATH, add if absent, write back.
    // We use the `-Command` invocation rather than -EncodedCommand for
    // clarity; the script is small enough that escaping is manageable.
    let script = format!(
        r#"$cur = [Environment]::GetEnvironmentVariable('Path', 'Machine'); \
           if ($cur -notlike "*{dir}*") {{ \
             [Environment]::SetEnvironmentVariable('Path', "$cur;{dir}", 'Machine') \
           }}"#,
        dir = dir_str.replace('"', "`\"")
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .await
        .map_err(PathError::Io)?;
    if !out.status.success() {
        return Err(PathError::PowershellFailed {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    info!(dir = %dir.display(), "ensured machine PATH contains shim root");
    Ok(())
}
