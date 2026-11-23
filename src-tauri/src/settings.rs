//! Persisted user settings, stored as `settings.json` in the app data dir.
//!
//! `max_concurrent` is read once by `QueueManager` at construction time
//! (`main.rs`), so a change here via `set_settings` only takes effect on
//! the next app restart -- there is no live-resize path for it.
//! `default_speed_limit_bytes_per_sec` and `download_dir` have no such
//! restriction: `main.rs` seeds both into `QueueManager`/`ExtractJobManager`
//! at startup, and `commands::set_settings` pushes every change into both
//! live (via `QueueManager::set_default_speed_limit`/`set_download_dir` and
//! `ExtractJobManager::set_download_dir`), applied to every download/
//! extraction added from now on -- including ones handed off by the
//! extension through the local server, not just UI-added ones.
//! `default_speed_limit_bytes_per_sec`'s `0` means "no limit", same as an
//! empty value.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Must match `identifier` in `tauri.conf.json`.
///
/// `app_data_dir()` below mirrors Tauri's own `PathResolver::app_data_dir`
/// (`dirs::data_dir().join(identifier)`) by hand, rather than going through
/// an `AppHandle`, because `QueueManager` -- which needs `download_dir` --
/// is constructed in `main()` before `tauri::Builder` is built (and thus
/// before any `AppHandle` exists).
const APP_IDENTIFIER: &str = "com.loadout.desktop";

const SETTINGS_FILE_NAME: &str = "settings.json";
const QUEUE_STATE_FILE_NAME: &str = "queue_state.json";

/// JSON-serializable settings, as sent to/from the frontend and persisted
/// to disk verbatim (field names are not renamed, so the on-disk JSON and
/// the JS-side object use these exact snake_case keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsDto {
    pub download_dir: String,
    pub max_concurrent: usize,
    pub default_speed_limit_bytes_per_sec: Option<u64>,
    /// Whether a short chime plays when a download/extract job reaches
    /// `Completed` (frontend-only effect: `src/app.js` checks this on each
    /// poll, nothing server-side reacts to it).
    #[serde(default = "default_true")]
    pub download_chime: bool,
    /// Whether a `Failed` download/extract job is automatically retried
    /// (frontend-only, same as `download_chime`: `src/app.js` calls the
    /// same `retry_download`/`retry_extract_job` commands a manual retry
    /// does, capped per row so a job that keeps failing doesn't retry
    /// forever).
    #[serde(default)]
    pub auto_retry_failed: bool,
    /// Whether the app launches at OS login. Unlike the two fields above,
    /// this has a real backend effect: `commands::set_autostart` both
    /// persists it here and calls `tauri_plugin_autostart` live, and
    /// `main.rs` applies whatever was last persisted on every startup.
    #[serde(default = "default_true")]
    pub start_at_login: bool,
}

// `#[serde(default)]`'s field-level form (used for `auto_retry_failed`
// above) only ever supplies `bool`'s own default (`false`); these two
// fields need `true` instead, for settings.json files written before
// this field existed -- deserializing one of those must not silently
// flip a setting a user never touched.
fn default_true() -> bool {
    true
}

impl Default for SettingsDto {
    fn default() -> Self {
        let download_dir = dirs::download_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();

        Self {
            download_dir,
            max_concurrent: 3,
            default_speed_limit_bytes_per_sec: None,
            download_chime: true,
            auto_retry_failed: false,
            start_at_login: true,
        }
    }
}

/// Resolves the app data directory the same way Tauri's own
/// `app_data_dir()` path resolver would (see `APP_IDENTIFIER`'s doc
/// comment for why this is computed by hand instead).
pub fn app_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_IDENTIFIER)
}

/// Path to the persisted settings file.
pub fn settings_file_path() -> PathBuf {
    app_data_dir().join(SETTINGS_FILE_NAME)
}

/// Path to the queue manager's persisted state file.
pub fn queue_state_file_path() -> PathBuf {
    app_data_dir().join(QUEUE_STATE_FILE_NAME)
}

/// Thread-safe, disk-backed settings store, managed as Tauri state.
pub struct SettingsStore {
    path: PathBuf,
    current: Mutex<SettingsDto>,
}

impl SettingsStore {
    /// Loads settings from `path`, falling back to [`SettingsDto::default`]
    /// if the file doesn't exist yet or fails to parse -- same
    /// "no usable prior state == fresh start" convention the queue
    /// manager's own `load_state` uses.
    pub fn load(path: PathBuf) -> Self {
        let current = fs::read_to_string(&path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();

        Self {
            path,
            current: Mutex::new(current),
        }
    }

    /// Returns a clone of the current settings.
    pub fn get(&self) -> SettingsDto {
        self.current.lock().unwrap().clone()
    }

    /// Persists `settings` to disk and updates the in-memory copy.
    pub fn set(&self, settings: SettingsDto) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(&settings)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&self.path, json)?;

        *self.current.lock().unwrap() = settings;
        Ok(())
    }
}
