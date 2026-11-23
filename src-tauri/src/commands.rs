//! Tauri commands exposed to the frontend.
//!
//! Every command here is a thin wrapper around the shared `QueueManager` or
//! `SettingsStore`, taking/returning only JSON-serializable types so
//! `tauri::generate_handler!` can expose them directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use loadout::download::queue::QueueManager;
use loadout::download::types::{DownloadInfo, DownloadRequest};
use loadout::extract::binaries::BinaryManager;
use loadout::extract::jobs::{ExtractJobInfo, ExtractJobManager};
use loadout::server::{PAIRING_MODE_DURATION, PairedProfileInfo, PairingState};
use reqwest::header::{CONTENT_TYPE, RANGE, USER_AGENT};
use tauri::{Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
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

/// A real browser UA string, not this app's own -- several file-hosting
/// pages (pixeldrain, Google Drive, gofile, ...) serve a different (or no)
/// response to a request that doesn't look like a browser.
const SNIFF_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Short: this only gates which of two already-fast paths `smart_add`
/// takes, so a slow/unresponsive server should fall through to a decision
/// quickly rather than make pasting a link feel stuck.
const SNIFF_TIMEOUT: Duration = Duration::from_secs(6);

/// Whether a response's `Content-Type` is present and starts with
/// `text/html` -- `None` means "don't know" (header missing or not valid
/// UTF-8), distinct from `Some(false)` ("known, and it's not HTML"), so the
/// caller can tell "confirmed not a page" apart from "couldn't tell".
fn is_html_content_type(response: &reqwest::Response) -> Option<bool> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("text/html"))
}

/// Whether `url` looks like a page (needs `yt-dlp` extraction) rather than
/// a directly downloadable file -- its `Content-Type` starting with
/// `text/html`, same signal a browser itself uses to render a page instead
/// of offering a download.
///
/// Tries a `HEAD` request first; if that doesn't yield a usable
/// `Content-Type` -- the server rejects `HEAD` outright (`405`/`501`), or
/// (seen in the wild from pixeldrain's page: a `200` with no `Content-Type`
/// header at all) answers but omits it -- falls back to a ranged `GET`
/// (`Range: bytes=0-0`), which every site tested in developing this
/// actually sets it on.
///
/// Defaults to "looks like a page" (`true`) whenever neither attempt
/// resolves it -- a probe failure, a timeout, or both responses missing
/// `Content-Type`. `yt-dlp` fails a genuinely unsupported URL with a clear
/// "unsupported URL" error on the resulting extract-job row; treating an
/// ambiguous response as a direct file instead would just queue a doomed
/// download of whatever bytes that response happened to contain.
async fn looks_like_html_page(client: &reqwest::Client, url: &str) -> bool {
    let head = client
        .head(url)
        .header(USER_AGENT, SNIFF_USER_AGENT)
        .timeout(SNIFF_TIMEOUT)
        .send()
        .await;

    if let Ok(response) = head {
        if let Some(is_html) = is_html_content_type(&response) {
            return is_html;
        }
    }

    let ranged = client
        .get(url)
        .header(USER_AGENT, SNIFF_USER_AGENT)
        .header(RANGE, "bytes=0-0")
        .timeout(SNIFF_TIMEOUT)
        .send()
        .await;

    match ranged {
        Ok(response) => is_html_content_type(&response).unwrap_or(true),
        Err(_) => true,
    }
}

/// Backs the addbar's "paste a link, press enter": unlike `add_download`
/// (always a direct file), this works for anything the browser extension's
/// hover-to-download button does too -- YouTube/Vimeo/etc., but also
/// non-video file-hosting pages like pixeldrain, Google Drive, or gofile,
/// since `yt-dlp` has extractors for those as well, not just video sites.
///
/// Sniffs `url` first (see `looks_like_html_page`) and routes to whichever
/// of the two already-existing paths fits: a direct file goes straight to
/// `QueueManager::add` (the fast, resumable multi-segment downloader); a
/// page goes to `ExtractJobManager::probe`, the same call the local
/// server's `/extract` route makes for the extension's hover button.
/// Returns the new download's or extract job's id.
#[tauri::command]
pub async fn smart_add(
    url: String,
    queue: State<'_, Arc<QueueManager>>,
    extract_jobs: State<'_, Arc<ExtractJobManager>>,
) -> Result<String, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("URL must not be empty".to_string());
    }

    let client = reqwest::Client::new();
    if looks_like_html_page(&client, &url).await {
        Ok(extract_jobs.probe(url, HashMap::new()))
    } else {
        Ok(queue.add(DownloadRequest {
            url,
            filename: None,
            headers: HashMap::new(),
        }))
    }
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
/// `QueueManager`/`ExtractJobManager` immediately, so both apply to every
/// download/extraction added from now on (UI- or extension-added alike).
#[tauri::command]
pub fn set_settings(
    settings: SettingsDto,
    store: State<Arc<SettingsStore>>,
    queue: State<Arc<QueueManager>>,
    extract_jobs: State<Arc<ExtractJobManager>>,
) -> Result<(), String> {
    let default_speed_limit = settings.default_speed_limit_bytes_per_sec;
    let download_dir = std::path::PathBuf::from(&settings.download_dir);
    store.set(settings).map_err(|e| e.to_string())?;
    queue.set_default_speed_limit(default_speed_limit);
    queue.set_download_dir(download_dir.clone());
    extract_jobs.set_download_dir(download_dir);
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

/// Writes `chrome://extensions` to the OS clipboard, for the Browser
/// Companion Setup Guide's "OPEN PAGE" step.
///
/// This is deliberately clipboard-only. `chrome://extensions` has no
/// registered OS-level protocol handler on a typical system, so attempting
/// to open it via `tauri-plugin-opener`'s `Opener::open_url` is very likely
/// to trigger the OS's own "choose an app" dialog (on Windows,
/// `ShellExecuteExW`'s app-picker) instead of actually doing anything
/// useful -- and nothing in this environment has a real display to confirm
/// which approach actually works. Rather than pop that dialog on every use,
/// this command never calls `open_url` at all: it always writes the URL to
/// the clipboard via `tauri-plugin-clipboard-manager`'s
/// `Clipboard::write_text` and returns a confirmation string that
/// accurately describes only that, with no mention of "opening" or the
/// browser doing something automatically. The clipboard write's own
/// failure is the only real error case, surfaced as `Err`.
#[tauri::command]
pub async fn open_extensions_page(app: tauri::AppHandle) -> Result<String, String> {
    const EXTENSIONS_URL: &str = "chrome://extensions";

    app.clipboard()
        .write_text(EXTENSIONS_URL)
        .map(|_| {
            "Copied chrome://extensions — paste it into your browser's address bar.".to_string()
        })
        .map_err(|e| e.to_string())
}

/// Resolves the bundled `extension/` folder (packaged as an installer
/// resource -- see `tauri.conf.json`'s `bundle.resources`) and opens it in
/// the OS file explorer, for the Browser Companion Setup Guide's
/// "OPEN FOLDER" step.
#[tauri::command]
pub fn open_extension_folder(app: tauri::AppHandle) -> Result<(), String> {
    let folder = app
        .path()
        .resolve("extension", tauri::path::BaseDirectory::Resource)
        .map_err(|e| e.to_string())?;

    app.opener()
        .open_path(folder.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Returns the Settings panel's "PAIRED BROWSERS" list.
#[tauri::command]
pub fn list_paired_profiles(pairing: State<Arc<PairingState>>) -> Vec<PairedProfileInfo> {
    pairing.list()
}

/// Backs the Settings panel's "ALLOW NEW PAIRING" button: opens `/pair`
/// back up for `PAIRING_MODE_DURATION` and returns that duration (in
/// seconds) so the frontend's countdown starts from the same number this
/// actually used, rather than a second copy of the constant.
#[tauri::command]
pub fn allow_new_pairing(pairing: State<Arc<PairingState>>) -> u64 {
    pairing.allow_new_pairing(PAIRING_MODE_DURATION);
    PAIRING_MODE_DURATION.as_secs()
}

/// How many seconds pairing mode has left, or `None` if it's closed --
/// read when the Settings panel opens, so a live countdown already in
/// progress (started from an earlier visit to the panel) resumes showing
/// the right remaining time instead of silently resetting.
#[tauri::command]
pub fn pairing_mode_remaining_seconds(pairing: State<Arc<PairingState>>) -> Option<u64> {
    pairing.pairing_mode_remaining().map(|d| d.as_secs())
}

/// Removes one paired profile by id (the Settings panel's per-row FORGET
/// button) -- the browser itself still holds its now-invalid token until
/// it next tries to use it, exactly like the extension's own "Forget
/// Pairing"/`\/unpair` already works, just triggered from the app side
/// instead.
#[tauri::command]
pub fn forget_paired_profile(id: String, pairing: State<Arc<PairingState>>) -> Result<(), String> {
    if pairing.forget_by_id(&id) {
        Ok(())
    } else {
        Err(format!("no paired profile with id '{id}'"))
    }
}

/// Renames one paired profile (the Settings panel's per-row label field) --
/// cosmetic only, never affects the token or what it's allowed to do.
#[tauri::command]
pub fn rename_paired_profile(
    id: String,
    label: String,
    pairing: State<Arc<PairingState>>,
) -> Result<(), String> {
    if pairing.rename(&id, &label) {
        Ok(())
    } else {
        Err(format!("no paired profile with id '{id}'"))
    }
}

/// Returns the current snapshot of every tracked extract job (video/
/// social-platform URLs handed off from the browser extension).
#[tauri::command]
pub fn list_extract_jobs(jobs: State<Arc<ExtractJobManager>>) -> Vec<ExtractJobInfo> {
    jobs.list()
}

/// Starts the download for an extract job awaiting a format choice.
#[tauri::command]
pub fn choose_extract_format(
    id: String,
    format_label: String,
    jobs: State<Arc<ExtractJobManager>>,
) -> Result<(), String> {
    jobs.choose_format(&id, &format_label)
}

/// Kills an extract job's live subprocess (if any) and marks it canceled.
#[tauri::command]
pub fn cancel_extract_job(id: String, jobs: State<Arc<ExtractJobManager>>) -> Result<(), String> {
    jobs.cancel(&id);
    Ok(())
}

/// Re-probes a failed extract job from scratch, using its originally
/// captured source URL/headers.
#[tauri::command]
pub fn retry_extract_job(id: String, jobs: State<Arc<ExtractJobManager>>) -> Result<(), String> {
    jobs.retry(&id)
}

/// Removes a terminal-status (`Completed`/`Failed`/`Canceled`) extract job
/// from the tracked jobs list and drops its cached extraction data. Errors
/// (removing nothing) for a job that's missing or still live.
#[tauri::command]
pub fn remove_extract_job(id: String, jobs: State<Arc<ExtractJobManager>>) -> Result<(), String> {
    jobs.remove(&id)
}

/// Removes an extract job *and*, if it is `Completed`, deletes its
/// finished file from disk. The frontend only calls this after the user
/// has explicitly confirmed the deletion -- mirrors
/// `remove_download_and_delete_file` for ordinary downloads.
#[tauri::command]
pub fn remove_extract_job_and_delete_file(
    id: String,
    jobs: State<Arc<ExtractJobManager>>,
) -> Result<(), String> {
    jobs.remove_and_delete_file(&id)
}

/// Unconditionally re-fetches and overwrites `yt-dlp.exe`.
#[tauri::command]
pub async fn check_ytdlp_update(binaries: State<'_, Arc<BinaryManager>>) -> Result<String, String> {
    binaries
        .update_ytdlp()
        .await
        .map(|_| "yt-dlp updated to the latest version.".to_string())
        .map_err(|e| e.to_string())
}
