// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod settings;

use std::path::PathBuf;
use std::sync::Arc;

use loadout::download::queue::QueueManager;

fn main() {
    let settings_store = settings::SettingsStore::load(settings::settings_file_path());
    let initial_settings = settings_store.get();

    let download_dir = PathBuf::from(&initial_settings.download_dir);
    let _ = std::fs::create_dir_all(&download_dir);

    // `QueueManager::new` needs a Tokio runtime to be "current" on this
    // thread (it calls `tokio::runtime::Handle::try_current()` internally
    // to remember a handle for later scheduling); entering Tauri's own
    // global async runtime for the duration of construction satisfies
    // that without starting a second runtime.
    let queue_manager = {
        let runtime_handle = tauri::async_runtime::handle();
        let _guard = runtime_handle.inner().enter();
        Arc::new(QueueManager::new(
            download_dir,
            settings::queue_state_file_path(),
            initial_settings.max_concurrent,
        ))
    };
    queue_manager.set_default_speed_limit(initial_settings.default_speed_limit_bytes_per_sec);

    let settings_store = Arc::new(settings_store);

    tauri::Builder::default()
        .manage(queue_manager)
        .manage(settings_store)
        .plugin(tauri_plugin_opener::init())
        // Backs the Settings panel's download-folder "Change" button
        // (`commands::pick_download_folder`).
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::add_download,
            commands::list_downloads,
            commands::pause_download,
            commands::resume_download,
            commands::cancel_download,
            commands::remove_download,
            commands::remove_download_and_delete_file,
            commands::retry_download,
            commands::set_download_speed_limit,
            commands::get_settings,
            commands::set_settings,
            commands::pick_download_folder,
            commands::open_path_in_default_app,
            commands::reveal_path_in_file_manager,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
