//! Full-restart helper: arm a detached relaunch onto the same exe, tear down
//! the owned DSH tree, then exit so the fresh instance re-runs the whole
//! startup chain.
//!
//! The launch-time exe self-update and the plugin-package sync that once
//! lived here were removed (audit 2026-09): they had no call sites since the
//! "no independent Release channel" rework (see lib.rs setup comments) — dead
//! code whose download path went through public mirrors with a size-only
//! integrity fallback, plus a `minimumReleaseAge=0` cooldown bypass. The
//! tray「检查更新(DSH)」menu item was removed too (audit 2026-09): its bare
//! `dsh --version` never resolved in the GUI PATH, and the manual
//! `npm install -g @deepseek-ai/dsh@latest` remains the official update path.

use std::path::Path;

use tauri::AppHandle;

use crate::dsh;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Tray/panel「前后端重启」: arm the detached relaunch helper onto the
/// same exe, tear down the owned DSH tree, then exit. The fresh instance
/// re-runs the whole startup chain (shell + DSH backend) — the go-to when a
/// wedged plugin leaves even the webchat unusable. Only exits when the
/// helper is armed, so a failed arm never turns a restart into a quit.
pub fn restart_app(app: &AppHandle) {
    if let Ok(exe) = tauri::utils::platform::current_exe() {
        if relaunch_app(&exe) {
            // Stop the backend unconditionally (owned tree AND any attached
            // external listener on 3080) — a "complete restart" that leaves
            // an old backend behind keeps freshly installed plugins in
            // 「重启后生效」 limbo forever (2026-08-19 report).
            crate::dsh::stop_backend(app);
            app.exit(0);
        }
    }
}

/// Relaunch the app onto the freshly swapped exe. A detached helper waits for
/// this process to exit (releasing the single-instance lock), then starts the
/// new exe. The exit skips DSH teardown on purpose: a running webchat backend
/// stays up and the new instance attaches to it instead of respawning.
/// Returns whether the helper armed successfully.
fn relaunch_app(exe: &Path) -> bool {
    let mut command = std::process::Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `ping -n 4` is the quote-proof, PATH-proof ~3s delay (cmd's
        // builtin `timeout` loses to a GNU timeout.exe on some PATHs);
        // `start "" "path"` is the safe launcher for spaced paths. After /S
        // strips the outer quotes the helper reads:
        //   ping -n 4 127.0.0.1 >nul & start "" "C:\...\app.exe"
        command.raw_arg(format!(
            "/S /C \"ping -n 4 127.0.0.1 >nul & start \"\" \"{}\"\"",
            exe.display()
        ));
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        command
            .arg("-c")
            .arg(format!("sleep 3 && '{}'", exe.display()));
    }
    match command.spawn() {
        Ok(_) => {
            log_line("[dsh-desktop] relaunch helper armed (starts the new exe in ~3s)");
            true
        }
        Err(e) => {
            log_line(&format!(
                "[dsh-desktop] relaunch helper failed ({e}); the new version activates on next manual launch"
            ));
            false
        }
    }
}

/// Append one line to the shared shell log beside the exe updater's output.
/// Routes through dsh's timestamped writer so the log tab colors uniformly;
/// the `[dsh-desktop] ` prefix stays as the source tag.
fn log_line(line: &str) {
    crate::dsh::log_write(crate::dsh::LogLevel::Info, line);
}
