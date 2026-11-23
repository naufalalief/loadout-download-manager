use std::io::Write;
use std::path::Path;

use super::types::DownloadInfo;

/// Writes `downloads` to `state_file` as JSON, creating any missing parent
/// directories first.
///
/// The write is atomic with respect to readers and crashes: the JSON is
/// written to a sibling temp file (`<state_file>.tmp`), flushed to disk,
/// and only then renamed over `state_file` (a same-directory rename, which
/// replaces the target atomically on both Windows and Unix). The live
/// state file is never truncated in place, so a crash or a concurrent
/// reader can only ever observe either the previous complete state or the
/// new complete state -- never a half-written file that `load_state` would
/// reject (silently losing the whole queue).
///
/// This function does not serialize concurrent callers against each other
/// (two callers would race on the shared temp file); `QueueManager` routes
/// every save through a single persist lock for that.
///
/// Deliberately serializes only [`DownloadInfo`] (id, url, filename, paths,
/// byte counts, status, error) -- never the in-memory-only per-download
/// headers/cookies side channel that `QueueManager` keeps separately from
/// this list. Those must never be written to disk or logged.
pub fn save_state(state_file: &Path, downloads: &[DownloadInfo]) -> std::io::Result<()> {
    if let Some(parent) = state_file.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(downloads)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let mut tmp_os = state_file.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_os);

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp, state_file)
}

/// Reads and parses `state_file`'s `DownloadInfo` list.
///
/// Returns an empty list if the file doesn't exist yet or its contents
/// can't be parsed -- callers treat "no usable prior state" the same as
/// "fresh start" rather than failing to construct. This function does not
/// apply any crash-recovery status rewriting; that's `QueueManager::new`'s
/// job, so that `load_state` itself stays a plain, literal read.
pub fn load_state(state_file: &Path) -> Vec<DownloadInfo> {
    std::fs::read_to_string(state_file)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}
