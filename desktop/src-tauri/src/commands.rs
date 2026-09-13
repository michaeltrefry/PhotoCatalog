use photocatalog::{application::{Bridge, Cancellation, PreviewBytes, Reply, Request}, storage_volume::NativePath};
use serde::Serialize;
use std::{collections::HashMap, sync::{Mutex, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use tauri_plugin_dialog::DialogExt;

struct Operation {
    started: Instant,
    cancellation: Option<Cancellation>,
    canceled: bool,
}
pub struct State {
    pub bridge: Bridge,
    operations: Mutex<HashMap<String, Operation>>,
    handoffs: Mutex<HashMap<String, PreviewBytes>>,
    pub frontend_ready: AtomicBool,
    pub quitting: AtomicBool,
}
impl State {
    pub fn new(bridge: Bridge) -> Self { Self { bridge, operations: Mutex::new(HashMap::new()), handoffs: Mutex::new(HashMap::new()), frontend_ready: AtomicBool::new(false), quitting: AtomicBool::new(false) } }
}
fn valid_operation(operation: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(operation).map(|_| ()).map_err(|_| "Invalid operation identifier".into())
}

#[tauri::command]
pub async fn catalog_command(state: tauri::State<'_, State>, operation: String, request: Request) -> Result<Reply, String> {
    valid_operation(&operation)?;
    let closing = matches!(&request, Request::Close { .. });
    let pending = {
        let mut operations = state.operations.lock().map_err(|_| "Operation state unavailable")?;
        operations.retain(|_, value| value.cancellation.is_some() || value.started.elapsed() < Duration::from_secs(60));
        if operations.len() >= 128 && !operations.contains_key(&operation) { return Err("Too many active catalog operations".into()); }
        if operations.get(&operation).is_some_and(|value| value.cancellation.is_some()) { return Err("Operation identifier is already active".into()); }
        let pending = state.bridge.submit(request).map_err(|error| error.message)?;
        let canceled = operations.get(&operation).is_some_and(|value| value.canceled);
        let cancellation = pending.cancellation();
        if canceled { cancellation.cancel(); }
        operations.insert(operation.clone(), Operation { started: Instant::now(), cancellation: Some(cancellation), canceled });
        pending
    };
    let result = tauri::async_runtime::spawn_blocking(move || pending.recv()).await.map_err(|error| error.to_string());
    state.operations.lock().map_err(|_| "Operation state unavailable")?.remove(&operation);
    if closing { state.handoffs.lock().map_err(|_| "Preview state unavailable")?.clear(); }
    result
}

#[tauri::command]
pub fn catalog_cancel_operation(state: tauri::State<'_, State>, operation: String) -> Result<(), String> {
    valid_operation(&operation)?;
    let mut operations = state.operations.lock().map_err(|_| "Operation state unavailable")?;
    operations.retain(|_, value| value.cancellation.is_some() || value.started.elapsed() < Duration::from_secs(60));
    if let Some(value) = operations.get_mut(&operation) {
        value.canceled = true;
        if let Some(cancellation) = &value.cancellation { cancellation.cancel(); }
    } else if operations.len() < 128 {
        // Cancellation can arrive before the async command is polled.
        operations.insert(operation, Operation { started: Instant::now(), cancellation: None, canceled: true });
    } else { return Err("Too many active catalog operations".into()); }
    Ok(())
}

#[derive(Serialize)]
pub struct SelectedPath { path: NativePath, display: String }

#[tauri::command]
pub async fn catalog_choose_folder(app: tauri::AppHandle, create_catalog: bool) -> Result<Option<SelectedPath>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let selected = if create_catalog {
            app.dialog().file().set_title("Choose a name and location for the new catalog folder").set_file_name("PhotoCatalog").blocking_save_file()
        } else { app.dialog().file().set_title("Open a catalog folder").blocking_pick_folder() };
        selected.map(|file| {
            let path = file.into_path().map_err(|error| error.to_string())?;
            Ok(SelectedPath { path: NativePath::from_path(&path), display: path.to_string_lossy().into_owned() })
        }).transpose()
    }).await.map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn catalog_preview_bytes(state: tauri::State<'_, State>, catalog: String, ticket: String, handoff: String) -> Result<tauri::ipc::Response, String> {
    valid_operation(&handoff)?;
    let pending = state.bridge.preview_bytes(catalog, ticket.clone(), false).map_err(|error| error.message)?;
    let bytes = tauri::async_runtime::spawn_blocking(move || pending.recv()).await.map_err(|error| error.to_string())?.map_err(|error| error.message)?;
    let mut handoffs = state.handoffs.lock().map_err(|_| "Preview state unavailable")?;
    if handoffs.contains_key(&handoff) { return Err("Preview handoff is already active".into()); }
    // Hold the core reservation until the renderer acknowledges reception.
    // This IPC copy adds at most the core's aggregate 16 MiB transport allowance.
    let response = tauri::ipc::Response::new(bytes.bytes().to_vec());
    handoffs.insert(handoff, bytes);
    Ok(response)
}

#[tauri::command]
pub fn catalog_preview_release(state: tauri::State<'_, State>, handoff: String) -> Result<(), String> {
    state.handoffs.lock().map_err(|_| "Preview state unavailable")?.remove(&handoff);
    Ok(())
}

#[tauri::command]
pub fn catalog_frontend_ready(state: tauri::State<'_, State>) {
    state.frontend_ready.store(true, Ordering::Release);
}

#[tauri::command]
pub async fn catalog_quit(app: tauri::AppHandle, state: tauri::State<'_, State>) -> Result<(), String> {
    let bridge = state.bridge.clone();
    tauri::async_runtime::spawn_blocking(move || bridge.shutdown()).await.map_err(|error| error.to_string())?;
    state.handoffs.lock().map_err(|_| "Preview state unavailable")?.clear();
    state.quitting.store(true, Ordering::Release);
    app.exit(0);
    Ok(())
}
