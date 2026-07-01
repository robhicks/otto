mod launch;

use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_shell::{
  process::{CommandChild, CommandEvent},
  ShellExt,
};

/// Holds the spawned sidecar's handle so it can be killed on app exit (Task 8). `None` until
/// the sidecar spawns, and stays `None` if the user cancels the folder picker (nothing to kill).
struct SidecarHandle(Mutex<Option<CommandChild>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_shell::init())
    .plugin(tauri_plugin_dialog::init())
    .manage(SidecarHandle(Mutex::new(None)))
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      let Some(folder) = app.dialog().file().blocking_pick_folder() else {
        // No workspace chosen on launch — nothing to serve. Exit cleanly rather than show
        // an empty, disconnected window.
        app.handle().exit(0);
        return Ok(());
      };
      let root = folder
        .into_path()
        .map_err(|e| format!("chosen folder is not a filesystem path: {e}"))?;

      let token = uuid::Uuid::new_v4().to_string();
      let (rx, child) = app
        .shell()
        .sidecar("otto")
        .map_err(|e| e.to_string())?
        .args([
          "serve",
          "--root",
          &root.to_string_lossy(),
          "--port",
          "8787",
        ])
        .env("OTTO_TOKEN", &token)
        .spawn()
        .map_err(|e| e.to_string())?;
      app
        .state::<SidecarHandle>()
        .0
        .lock()
        .unwrap()
        .replace(child);

      watch_for_readiness(app.handle().clone(), rx, token);

      Ok(())
    })
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|app_handle, event| {
      if let tauri::RunEvent::ExitRequested { .. } = event {
        if let Some(child) = app_handle
          .state::<SidecarHandle>()
          .0
          .lock()
          .unwrap()
          .take()
        {
          let _ = child.kill();
        }
      }
    });
}

/// Watches the sidecar's output for otto serve's readiness line (with a 5s timeout), then
/// navigates the main window to the bootstrap URL — or shows an error dialog on failure/timeout.
fn watch_for_readiness(
  app: AppHandle,
  mut rx: tauri::async_runtime::Receiver<CommandEvent>,
  token: String,
) {
  tauri::async_runtime::spawn(async move {
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
      while let Some(event) = rx.recv().await {
        match event {
          CommandEvent::Stderr(line) => {
            let text = String::from_utf8_lossy(&line);
            if launch::is_ready_line(&text) {
              return Ok(());
            }
          }
          CommandEvent::Terminated(payload) => {
            return Err(format!(
              "otto serve exited before starting (code {:?})",
              payload.code
            ));
          }
          _ => {}
        }
      }
      Err("otto serve's output stream closed unexpectedly".to_string())
    })
    .await;

    match outcome {
      Ok(Ok(())) => {
        let target = launch::build_launch_url("ws://127.0.0.1:8787", &token);
        if let Some(window) = app.get_webview_window("main") {
          // `target` is relative (`index.html?...`), so it must be resolved against the
          // webview's current base URL before `navigate` — `Url::parse` rejects a relative
          // URL outright ("relative URL without a base").
          let joined = window
            .url()
            .map_err(|e| e.to_string())
            .and_then(|base| base.join(&target).map_err(|e| e.to_string()));
          match joined {
            Ok(url) => {
              let _ = window.navigate(url);
            }
            Err(e) => show_startup_error(&app, &format!("invalid launch URL: {e}")),
          }
        }
      }
      Ok(Err(message)) => show_startup_error(&app, &message),
      Err(_) => show_startup_error(&app, "otto serve did not start within 5 seconds"),
    }
  });
}

fn show_startup_error(app: &AppHandle, message: &str) {
  app
    .dialog()
    .message(message)
    .title("otto failed to start")
    .kind(MessageDialogKind::Error)
    .blocking_show();
}
