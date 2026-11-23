// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod settings;

use std::path::PathBuf;
use std::sync::Arc;

use loadout::download::queue::QueueManager;
use loadout::extract::binaries::BinaryManager;
use loadout::extract::jobs::ExtractJobManager;
use loadout::server::{self, PairingState};
use tauri::Manager;

/// Shows, unminimizes, and focuses the main window. Called by the
/// single-instance callback below -- a second launch (e.g. from the Start
/// menu while the app is already running) hands off to this one instead
/// of starting a second process that would try to bind the same handoff
/// server port -- and by a new extract job's window-opener callback, so
/// the app surfaces itself when the extension starts an extraction while
/// it was in the background.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

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
    // Applies to every download added from now on -- via the UI *and* via
    // the handoff server -- since it lives inside `QueueManager` itself.
    queue_manager.set_default_speed_limit(initial_settings.default_speed_limit_bytes_per_sec);

    let settings_store = Arc::new(settings_store);

    // Local handoff HTTP server (pairing + `/downloads` + `/extract`):
    // shares the same `QueueManager` instance managed below, and persists
    // its paired-profile list alongside the other app-data files.
    let handoff_queue = Arc::clone(&queue_manager);
    let pairing_state = Arc::new(PairingState::new(
        settings::app_data_dir().join("paired_profiles.json"),
    ));

    // `ExtractJobManager` for the handoff server's `/extract` route. Built
    // here, alongside `queue_manager`, so `server::serve` has something
    // real to drive `/extract` with; its placeholder `on_new_job` callback
    // below is replaced with the real window-opener inside `.setup()`,
    // once an `AppHandle` actually exists.
    let binaries = Arc::new(BinaryManager::new(settings::app_data_dir().join("bin")));
    let extract_jobs = {
        let runtime_handle = tauri::async_runtime::handle();
        let _guard = runtime_handle.inner().enter();
        Arc::new(ExtractJobManager::new(
            PathBuf::from(&initial_settings.download_dir),
            Arc::clone(&binaries),
            Arc::new(|| {}),
            initial_settings.max_concurrent,
        ))
    };

    tauri::Builder::default()
        // Must be the first plugin registered (per the plugin's own docs):
        // a second launch (e.g. from the Start menu while the app is
        // already running) must hand off to this instance and exit before
        // anything else -- in particular before a second `QueueManager`/
        // handoff server could touch the same state file or port. The
        // second process never gets past this plugin's init; this
        // callback runs in the *first* process.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .manage(queue_manager)
        .manage(settings_store)
        .manage(Arc::clone(&binaries))
        .manage(Arc::clone(&extract_jobs))
        .manage(Arc::clone(&pairing_state))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // Backs the Settings panel's download-folder "Change" button
        // (`commands::pick_download_folder`).
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            // Replaces the placeholder `on_new_job` callback `extract_jobs`
            // was constructed with above (no `AppHandle` existed yet at
            // that point) with the real one, now that it does.
            let opener_handle = app.handle().clone();
            extract_jobs.set_window_opener(Arc::new(move || {
                show_main_window(&opener_handle);
            }));

            show_main_window(app.handle());

            tauri::async_runtime::spawn(server::serve(handoff_queue, pairing_state, extract_jobs));

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::add_download,
            commands::smart_add,
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
            commands::open_extensions_page,
            commands::open_extension_folder,
            commands::list_paired_profiles,
            commands::allow_new_pairing,
            commands::pairing_mode_remaining_seconds,
            commands::forget_paired_profile,
            commands::rename_paired_profile,
            commands::list_extract_jobs,
            commands::choose_extract_format,
            commands::cancel_extract_job,
            commands::retry_extract_job,
            commands::remove_extract_job,
            commands::remove_extract_job_and_delete_file,
            commands::check_ytdlp_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
