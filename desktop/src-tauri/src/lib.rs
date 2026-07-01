mod launch;

use std::sync::Mutex;

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_shell::{process::CommandChild, ShellExt};

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
      let (_rx, child) = app
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

      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
