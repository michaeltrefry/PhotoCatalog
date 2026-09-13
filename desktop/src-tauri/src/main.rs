#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use photocatalog::{application, preview};
use tauri::{Emitter, Manager};
use std::sync::atomic::Ordering;

fn main() {
    // The installed executable is also the isolated worker. Never initialize a
    // webview, dialogs or catalog owner in a worker process.
    match std::env::args_os().nth(1).as_deref() {
        Some(arg) if arg == "--preview-worker" => {
            if let Err(error) = preview::worker_main() {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
            return;
        }
        Some(arg) if arg == "--photo-export-worker" => {
            if let Err(error) = photocatalog::export_worker::export_worker_main() {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let bridge = application::Bridge::spawn(application::Config {
                worker_executable: std::env::current_exe()?,
                cache_root: Some(app.path().app_cache_dir()?),
                original_roots: Vec::new(),
                preview_policy: preview::PreviewPolicy::default(),
                preview_limits: preview::ServiceLimits::default(),
                limits: application::Limits::default(),
            })?;
            app.manage(commands::State::new(bridge));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::catalog_command,
            commands::catalog_cancel_operation,
            commands::catalog_choose_folder,
            commands::catalog_preview_bytes,
            commands::catalog_preview_release,
            commands::catalog_frontend_ready,
            commands::catalog_quit,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<commands::State>();
                if state.frontend_ready.load(Ordering::Acquire) && !state.quitting.load(Ordering::Acquire) {
                    api.prevent_close();
                    let _ = window.emit("catalog-close-requested", ());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Unable to start PhotoCatalog");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = &event {
            let state = app.state::<commands::State>();
            if state.frontend_ready.load(Ordering::Acquire) && !state.quitting.load(Ordering::Acquire) {
                api.prevent_exit();
                let _ = app.emit("catalog-close-requested", ());
            }
        }
        if matches!(event, tauri::RunEvent::Exit) {
            // Joins the owner after worker cleanup, before the process leaves.
            app.state::<commands::State>().bridge.shutdown();
        }
    });
}
