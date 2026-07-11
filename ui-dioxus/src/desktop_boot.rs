//! Desktop bootstrap: fold the Tauri `desktop/` wrapper's job (pick workspace → launch a local
//! `otto serve` sidecar → auto-connect) into the one Dioxus crate. Fixed port 8787, generated
//! token. This whole module is desktop-only — it is `mod`-gated behind `#[cfg(feature =
//! "desktop")]` in `main.rs`, so it never compiles into (or is referenced by) the web build.
use std::process::{Child, Command};

use crate::net::url::LaunchParams;

/// Pick a workspace folder, spawn `otto serve` there, and return the sidecar guard + connect
/// params. Returns `None` if the user cancels the folder picker, or if the sidecar fails to
/// spawn (e.g. `otto` is not on `PATH` and `OTTO_BIN` is unset).
///
/// Uses `rfd::AsyncFileDialog` rather than the blocking `rfd::FileDialog` — the blocking dialog
/// parks the calling OS thread until the user responds, which would stall the async executor
/// (and, transitively, every other `use_future`/`spawn` task in the app) if called directly from
/// one. The async dialog cooperates with the executor instead, so `boot()` is itself `async` and
/// must be driven from a `spawn`/`use_future`, never called synchronously.
pub async fn boot() -> Option<(SidecarGuard, LaunchParams)> {
    let handle = rfd::AsyncFileDialog::new()
        .set_title("Choose a workspace folder")
        .pick_folder()
        .await?;
    let root = handle.path().to_path_buf();
    let token = uuid::Uuid::new_v4().to_string();
    // Spawn the sidecar. `otto` must be on PATH (or point OTTO_BIN at it); mirrors desktop/'s
    // sidecar contract. Fixed port 8787 to match the LaunchParams the web path also uses.
    let child = Command::new(std::env::var("OTTO_BIN").unwrap_or_else(|_| "otto".into()))
        .arg("serve")
        .arg("--port")
        .arg("8787")
        .arg("--root")
        .arg(&root)
        .env("OTTO_TOKEN", &token)
        .spawn()
        .ok()?;
    Some((
        SidecarGuard(child),
        LaunchParams {
            ws: "ws://127.0.0.1:8787".into(),
            token,
        },
    ))
}

/// Owns the sidecar `Child`. A bare `std::process::Child` does **not** terminate its process on
/// drop — only an explicit `.kill()` does — so without this wrapper the `otto serve` sidecar
/// would be silently orphaned (left running) whenever the value holding it is dropped. This
/// guard is stored in a `Signal` in `app.rs` so it lives for the app's lifetime and its `Drop`
/// fires (best-effort `kill()` + `wait()` to reap the zombie) when that signal's value is torn
/// down.
///
/// Caveat (unverified without a live desktop run): whether Dioxus's desktop shutdown path
/// actually drops root-scope signal values before the process exits is not something this
/// compile-only gate can confirm — see the Task 13 report.
pub struct SidecarGuard(pub Child);

impl Drop for SidecarGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SidecarGuard::drop` must not panic even if the child already exited on its own (e.g. the
    /// sidecar crashed before the guard was dropped) — `kill()` on an already-reaped/exited child
    /// returns `Err`, which the `Drop` impl deliberately swallows via `let _ =`.
    #[test]
    fn drop_does_not_panic_on_already_exited_child() {
        let mut child = Command::new("true")
            .spawn()
            .expect("spawning `true` must succeed on any POSIX test host");
        // Let it actually exit before we drop the guard, so `kill()` races a dead pid.
        let _ = child.wait();
        drop(SidecarGuard(child));
    }
}
