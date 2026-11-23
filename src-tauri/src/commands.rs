//! Tauri commands exposed to the frontend.
//!
//! Every command here is a thin wrapper around the shared `QueueManager` or
//! `SettingsStore`, taking/returning only JSON-serializable types so
//! `tauri::generate_handler!` can expose them directly.

use std::collections::HashMap;
use std::sync::Arc;

use loadout::download::queue::QueueManager;
use loadout::download::types::{DownloadInfo, DownloadRequest};
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::settings::SettingsDto;
use crate::settings::SettingsStore;

/// Queues a new download and returns its generated id.
///
/// The current default speed limit is applied by `QueueManager::add`
/// itself (see `QueueManager::set_default_speed_limit`).
#[tauri::command]
pub fn add_download(
    url: String,
    filename: Option<String>,
    queue: State<Arc<QueueManager>>,
) -> Result<String, String> {
    if url.trim().is_empty() {
        return Err("URL must not be empty".to_string());
    }

    let req = DownloadRequest {
        url,
        filename,
        headers: HashMap::new(),
    };
    Ok(queue.add(req))
}

/// Returns the current snapshot of every tracked download.
#[tauri::command]
pub fn list_downloads(queue: State<Arc<QueueManager>>) -> Vec<DownloadInfo> {
    queue.list()
}

/// Pauses a currently-downloading entry. A no-op if it isn't downloading.
#[tauri::command]
pub fn pause_download(id: String, queue: State<Arc<QueueManager>>) -> Result<(), String> {
    queue.pause(&id);
    Ok(())
}

/// Resumes a paused entry. A no-op if it isn't paused.
#[tauri::command]
pub fn resume_download(id: String, queue: State<Arc<QueueManager>>) -> Result<(), String> {
    queue.resume(&id);
    Ok(())
}

/// Cancels an entry, discarding any partial download.
#[tauri::command]
pub fn cancel_download(id: String, queue: State<Arc<QueueManager>>) -> Result<(), String> {
    queue.cancel(&id);
    Ok(())
}

/// Removes an entry from the queue entirely. Never deletes a `Completed`
/// entry's finished file -- see `remove_download_and_delete_file` for that.
#[tauri::command]
pub fn remove_download(id: String, queue: State<Arc<QueueManager>>) -> Result<(), String> {
    queue.remove(&id);
    Ok(())
}

/// Removes an entry from the queue *and*, if it is `Completed`, deletes its
/// finished file from disk. The frontend only calls this after the user
/// has explicitly confirmed the deletion.
#[tauri::command]
pub fn remove_download_and_delete_file(
    id: String,
    queue: State<Arc<QueueManager>>,
) -> Result<(), String> {
    queue.remove_and_delete_file(&id);
    Ok(())
}

/// Requeues a `Failed` entry, resuming from whatever partial progress it
/// kept. A no-op for any other status.
#[tauri::command]
pub fn retry_download(id: String, queue: State<Arc<QueueManager>>) -> Result<(), String> {
    queue.retry(&id);
    Ok(())
}

/// Sets (or clears, with `None`) a per-download speed limit.
#[tauri::command]
pub fn set_download_speed_limit(
    id: String,
    bytes_per_sec: Option<u64>,
    queue: State<Arc<QueueManager>>,
) -> Result<(), String> {
    queue.set_speed_limit(&id, bytes_per_sec);
    Ok(())
}

/// Returns the current persisted settings.
#[tauri::command]
pub fn get_settings(settings: State<Arc<SettingsStore>>) -> SettingsDto {
    settings.get()
}

/// Opens the OS's native folder-picker for the Settings panel's download
/// folder field, replacing manual path-typing. Blocks this command's own
/// thread while the dialog is open (fine -- Tauri commands run off the
/// main thread by default). `None` covers both "the user canceled" and
/// "the picked location isn't representable as a plain path" -- either
/// way, the frontend leaves the existing field value alone, exactly like
/// the user backing out of a native "choose a folder" dialog normally
/// would.
#[tauri::command]
pub fn pick_download_folder(app: tauri::AppHandle) -> Option<String> {
    let picked = app.dialog().file().blocking_pick_folder()?;
    picked
        .into_path()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Persists new settings.
///
/// `max_concurrent` changes here only take effect the next time the app
/// starts (see `settings.rs`'s doc comment). `default_speed_limit_bytes_per_sec`
/// and `download_dir` are additionally pushed into the running
/// `QueueManager` immediately, so both apply to every download added from
/// now on.
#[tauri::command]
pub fn set_settings(
    settings: SettingsDto,
    store: State<Arc<SettingsStore>>,
    queue: State<Arc<QueueManager>>,
) -> Result<(), String> {
    let default_speed_limit = settings.default_speed_limit_bytes_per_sec;
    let download_dir = std::path::PathBuf::from(&settings.download_dir);
    store.set(settings).map_err(|e| e.to_string())?;
    queue.set_default_speed_limit(default_speed_limit);
    queue.set_download_dir(download_dir);
    Ok(())
}

/// Opens `path` with the OS's default app for it -- backs the queue row
/// context menu's "Open file" action and double-click-to-open, both gated
/// in the frontend to a `Completed` row's own `dest_path`, so in practice
/// `path` always names a real, finished download.
#[tauri::command]
pub fn open_path_in_default_app(app: tauri::AppHandle, path: String) -> Result<(), String> {
    app.opener().open_path(path, None::<&str>).map_err(|e| e.to_string())
}

/// Opens `path`'s containing folder in the OS file explorer with `path`
/// itself selected -- backs the queue row context menu's "Open file
/// location" action, same gating as `open_path_in_default_app`.
#[tauri::command]
pub fn reveal_path_in_file_manager(app: tauri::AppHandle, path: String) -> Result<(), String> {
    app.opener().reveal_item_in_dir(path).map_err(|e| e.to_string())
}
