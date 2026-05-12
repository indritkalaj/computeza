//! Windows-specific implementations.
//!
//! Compiled only on `target_os = "windows"`. Windows Services via SCM
//! (Service Control Manager) is the service manager; we drive it
//! through `sc.exe` rather than the native Win32 API to keep the dep
//! surface tiny. PATH registration uses PowerShell's
//! `[Environment]::SetEnvironmentVariable` -- simpler than direct
//! registry edits, and it broadcasts WM_SETTINGCHANGE for us.

pub mod path;
pub mod postgres;
pub mod sc;

pub mod kanidm;
