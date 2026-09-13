use photocatalog::{application::{Bridge, Cancellation, PreviewBytes, Reply, Request}, storage_volume::NativePath};
use serde::{Deserialize, Serialize};
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
            app.dialog().file().set_title("Choose a name and location for the new catalog folder").set_file_name("LensWorks").blocking_save_file()
        } else { app.dialog().file().set_title("Open a catalog folder").blocking_pick_folder() };
        selected.map(|file| {
            let path = file.into_path().map_err(|error| error.to_string())?;
            Ok(SelectedPath { path: NativePath::from_path(&path), display: path.to_string_lossy().into_owned() })
        }).transpose()
    }).await.map_err(|error| error.to_string())?
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationPurpose { Originals, BackupBundle, NewBackup, NewRestore, RelinkFolder, RelinkOriginal, ExportDirectory, ExportProfile, LightroomNewWorkbench, LightroomWorkbench, LightroomCaptureStaging, LightroomDiscoveryRoot, LightroomSourceCatalog, LightroomCaptureEvidence, LightroomNewCapture, LightroomNewSeal, LightroomApprovalDestination, LightroomNewApprovalDestination }

#[tauri::command]
pub async fn catalog_choose_location(app: tauri::AppHandle, purpose: LocationPurpose) -> Result<Option<SelectedPath>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dialog = app.dialog().file();
        let selected = match purpose {
            LocationPurpose::LightroomNewWorkbench => dialog.set_title("Choose a new Lightroom inspection folder").set_file_name("Lightroom Inspection").blocking_save_file(),
            LocationPurpose::LightroomWorkbench => dialog.set_title("Open an existing Lightroom inspection folder").blocking_pick_folder(),
            LocationPurpose::LightroomCaptureStaging => dialog.set_title("Choose the folder for temporary Lightroom capture files").blocking_pick_folder(),
            LocationPurpose::LightroomDiscoveryRoot => dialog.set_title("Choose a folder containing Lightroom catalogs").blocking_pick_folder(),
            LocationPurpose::LightroomSourceCatalog => dialog.set_title("Choose the Lightroom catalog to inspect").add_filter("Lightroom catalogs", &["lrcat"]).blocking_pick_file(),
            LocationPurpose::LightroomCaptureEvidence => dialog.set_title("Open a retained Lightroom capture").blocking_pick_folder(),
            LocationPurpose::LightroomNewCapture => dialog.set_title("Choose a new Lightroom capture folder").set_file_name("Lightroom Capture").blocking_save_file(),
            LocationPurpose::LightroomNewSeal => dialog.set_title("Choose a new Lightroom selection folder").set_file_name("Lightroom Selection").blocking_save_file(),
            LocationPurpose::LightroomApprovalDestination => dialog.set_title("Choose an existing LensWorks destination").blocking_pick_folder(),
            LocationPurpose::LightroomNewApprovalDestination => dialog.set_title("Choose a new LensWorks destination").set_file_name("LensWorks Import").blocking_save_file(),
            LocationPurpose::ExportDirectory => dialog.set_title("Choose the export destination folder").blocking_pick_folder(),
            LocationPurpose::ExportProfile => dialog.set_title("Choose an RGB output profile").add_filter("ICC profiles", &["icc", "icm"]).blocking_pick_file(),
            LocationPurpose::RelinkFolder => dialog.set_title("Locate the moved originals folder").blocking_pick_folder(),
            LocationPurpose::RelinkOriginal => dialog.set_title("Locate the original photo or metadata sidecar").blocking_pick_file(),
            LocationPurpose::Originals => dialog.set_title("Add photographs from a folder").blocking_pick_folder(),
            LocationPurpose::BackupBundle => dialog.set_title("Choose a LensWorks backup folder").blocking_pick_folder(),
            LocationPurpose::NewBackup => dialog.set_title("Choose a new backup folder").set_file_name("LensWorks Backup").blocking_save_file(),
            LocationPurpose::NewRestore => dialog.set_title("Choose a new restored catalog folder").set_file_name("LensWorks Restored").blocking_save_file(),
        };
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
    tauri::async_runtime::spawn_blocking(move || bridge.try_shutdown()).await.map_err(|error| error.to_string())?.map_err(|error| error.message)?;
    state.handoffs.lock().map_err(|_| "Preview state unavailable")?.clear();
    state.quitting.store(true, Ordering::Release);
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod location_tests {
    use super::LocationPurpose;
    #[test]
    fn export_picker_purposes_have_distinct_native_admission() {
        assert!(matches!(serde_json::from_str::<LocationPurpose>("\"export_directory\"").unwrap(), LocationPurpose::ExportDirectory));
        assert!(matches!(serde_json::from_str::<LocationPurpose>("\"export_profile\"").unwrap(), LocationPurpose::ExportProfile));
        assert!(serde_json::from_str::<LocationPurpose>("\"export_destination_file\"").is_err());
    }
}
