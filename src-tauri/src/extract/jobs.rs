//! Drives `yt-dlp` subprocesses to probe a page URL for downloadable
//! formats and then download the chosen one, tracking each such
//! "extract job" entirely in memory (see the plan's Global Constraints:
//! extract jobs are never persisted, unlike `QueueManager`'s queue).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use uuid::Uuid;

use super::binaries::BinaryManager;
use crate::download::queue::now_unix;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractStatus {
    Probing,
    AwaitingFormatChoice,
    Downloading,
    Completed,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractFormatOption {
    pub label: String,
    pub format_selector: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractJobInfo {
    pub id: String,
    pub source_url: String,
    pub title: String,
    pub status: ExtractStatus,
    pub formats: Vec<ExtractFormatOption>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
    /// The finished file's absolute path, set only once `status` reaches
    /// `Completed` (see `Inner::complete`) -- backs the frontend's "Open
    /// file"/"Open file location" context-menu actions and double-click-
    /// to-open, which only ever show for a `Completed` row.
    pub final_path: Option<String>,
    /// Unix seconds when this job was created (probe started).
    pub created_at: i64,
}

/// In-memory-only side channel holding exactly what's needed to (re-)run a
/// job's subprocesses: the original page URL and the headers captured for
/// it, plus (once a probe has succeeded) the raw `--dump-single-json`
/// stdout that produced its `formats`. Kept separate from `ExtractJobInfo`
/// (the same shape `QueueManager` uses for its own `headers` side channel)
/// so it's never accidentally swept into `ExtractJobInfo` if that type
/// gains `Deserialize`/persistence later. Retained until the job reaches
/// `Completed` or `Canceled`, so a `Failed` job can still be retried.
///
/// `probe_json` is what lets `choose_format` never re-probe the live page:
/// per the plan's Global Constraints and the design doc's decision #4,
/// probing and downloading both have to come from that one cached
/// extraction, not two independent `yt-dlp` invocations against the (by
/// then possibly different) live page. The download subprocess is handed
/// this cached JSON via `--load-info-json` instead of the page URL -- see
/// `Inner::run_download`.
struct ExtractionCache {
    source_url: String,
    headers: HashMap<String, String>,
    probe_json: Option<String>,
}

/// Where `ExtractJobManager` gets its `yt-dlp`/`ffmpeg` paths from: the
/// real `BinaryManager` (which fetches them on demand) in production, or a
/// pair of fixed paths a test points at its own stub fixture.
enum BinarySource {
    Managed(Arc<BinaryManager>),
    Fixed { ytdlp: PathBuf, ffmpeg: PathBuf },
}

/// Orchestrates `yt-dlp` probe/download subprocesses for "extract" jobs
/// (video/social-platform URLs handed off from the browser extension),
/// entirely separately from `QueueManager`'s plain-file download queue.
///
/// `ExtractJobManager` is a thin handle: all of its state lives in
/// `Inner`, shared with whatever background probe/download task is
/// currently running for a given job id.
pub struct ExtractJobManager {
    inner: Arc<Inner>,
}

struct Inner {
    /// Where a finished extraction lands. A `Mutex`, not a plain `PathBuf`,
    /// so `ExtractJobManager::set_download_dir` can change it live -- a
    /// job already probing/downloading when that's called keeps using
    /// whatever `output_template` `run_download` already built from the
    /// value in effect when IT started.
    download_dir: Mutex<PathBuf>,
    binaries: BinarySource,

    /// Swappable so `ExtractJobManager::set_window_opener` can replace the
    /// placeholder no-op installed at construction time (no `AppHandle`
    /// exists yet that early in `main()`) with the real window-opener
    /// callback once one is available, from inside `.setup()`.
    on_new_job: Mutex<Arc<dyn Fn() + Send + Sync>>,

    /// The tracked jobs, keyed by id.
    jobs: Mutex<HashMap<String, ExtractJobInfo>>,

    /// See [`ExtractionCache`].
    cache: Mutex<HashMap<String, ExtractionCache>>,

    /// The live subprocess for a job, present only while it's alive, so
    /// `cancel()` can kill it. Removed once the task driving it observes
    /// it has exited (or, if `cancel()` got there first, by `cancel()`
    /// itself).
    children: Mutex<HashMap<String, Child>>,

    /// Captured at construction time so the synchronous `probe`/
    /// `choose_format`/`retry` methods can spawn background work. `None`
    /// when no Tokio runtime is current at construction (mirrors
    /// `QueueManager::Inner::runtime`).
    runtime: Option<tokio::runtime::Handle>,

    /// Caps how many `yt-dlp` probe/download subprocesses run at once,
    /// across every job. Unbounded concurrency here means every HARVEST
    /// click (and every REPLANT) fires its own `yt-dlp` process
    /// immediately, hammering YouTube with simultaneous requests from this
    /// one IP -- a burst pattern real browsers never produce, and one
    /// YouTube's own anti-abuse heuristics respond to by degrading or
    /// throttling the connection (observed live: formats missing stream
    /// URLs, downloads stuck at 0 B/s, or an outright failed probe).
    /// Reuses the same number as `QueueManager`'s "Crops growing at once"
    /// setting, acquired right before each subprocess actually spawns (see
    /// `run_probe`/`run_download`) so excess jobs simply wait their turn
    /// instead of running in parallel.
    concurrency: tokio::sync::Semaphore,
}

impl ExtractJobManager {
    pub fn new(
        download_dir: PathBuf,
        binaries: Arc<BinaryManager>,
        on_new_job: Arc<dyn Fn() + Send + Sync>,
        max_concurrent: usize,
    ) -> Self {
        Self::construct(
            download_dir,
            BinarySource::Managed(binaries),
            on_new_job,
            max_concurrent,
        )
    }

    #[doc(hidden)]
    pub fn new_with_binaries(
        download_dir: PathBuf,
        ytdlp_path: PathBuf,
        ffmpeg_path: PathBuf,
        on_new_job: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::construct(
            download_dir,
            BinarySource::Fixed {
                ytdlp: ytdlp_path,
                ffmpeg: ffmpeg_path,
            },
            on_new_job,
            // Tests drive jobs one at a time and assert on exact timing
            // (e.g. a 10s `/slow` stub job plus a `cancel()` mid-flight);
            // a real limit here would make them about queuing, not the
            // behavior each actually tests. `usize::MAX` itself overflows
            // `tokio::sync::Semaphore`'s own internal permit cap, so use
            // its `MAX_PERMITS` constant instead.
            tokio::sync::Semaphore::MAX_PERMITS,
        )
    }

    fn construct(
        download_dir: PathBuf,
        binaries: BinarySource,
        on_new_job: Arc<dyn Fn() + Send + Sync>,
        max_concurrent: usize,
    ) -> Self {
        let inner = Arc::new(Inner {
            download_dir: Mutex::new(download_dir),
            binaries,
            on_new_job: Mutex::new(on_new_job),
            jobs: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::try_current().ok(),
            concurrency: tokio::sync::Semaphore::new(max_concurrent.max(1)),
        });
        Self { inner }
    }

    /// Returns a snapshot of every tracked job.
    pub fn list(&self) -> Vec<ExtractJobInfo> {
        self.inner.jobs.lock().unwrap().values().cloned().collect()
    }

    /// Replaces the callback invoked whenever a new job is created (see
    /// [`Self::probe`]). Used to install the real window-opener once an
    /// `AppHandle` exists, replacing the inert placeholder passed to
    /// `new`/`new_with_binaries` at construction time.
    pub fn set_window_opener(&self, opener: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.on_new_job.lock().unwrap() = opener;
    }

    /// Starts probing `source_url` in the background; returns the new
    /// job's id immediately (status `Probing`).
    pub fn probe(&self, source_url: String, headers: HashMap<String, String>) -> String {
        let id = Uuid::new_v4().to_string();

        self.inner.jobs.lock().unwrap().insert(
            id.clone(),
            ExtractJobInfo {
                id: id.clone(),
                source_url: source_url.clone(),
                title: String::new(),
                status: ExtractStatus::Probing,
                formats: Vec::new(),
                downloaded_bytes: 0,
                total_bytes: None,
                error: None,
                final_path: None,
                created_at: now_unix(),
            },
        );
        self.inner.cache.lock().unwrap().insert(
            id.clone(),
            ExtractionCache {
                source_url: source_url.clone(),
                headers: headers.clone(),
                probe_json: None,
            },
        );

        (self.inner.on_new_job.lock().unwrap())();

        self.inner.spawn_probe(id.clone(), source_url, headers);

        id
    }

    /// Starts the download for `id` using `format_label`. The download
    /// subprocess is driven entirely from the cached probe result (never
    /// the live page again -- see `ExtractionCache::probe_json`).
    pub fn choose_format(&self, id: &str, format_label: &str) -> Result<(), String> {
        let format_selector = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            let job = jobs
                .get_mut(id)
                .ok_or_else(|| format!("no extract job with id '{id}'"))?;

            if job.status != ExtractStatus::AwaitingFormatChoice {
                return Err(format!(
                    "extract job '{id}' is not awaiting a format choice"
                ));
            }

            let option = job
                .formats
                .iter()
                .find(|f| f.label == format_label)
                .ok_or_else(|| {
                    format!("format '{format_label}' is not available for extract job '{id}'")
                })?;
            let format_selector = option.format_selector.clone();

            job.status = ExtractStatus::Downloading;
            format_selector
        };

        let (probe_json, headers) = {
            let cache = self.inner.cache.lock().unwrap();
            let entry = cache
                .get(id)
                .ok_or_else(|| format!("no cached extraction data for extract job '{id}'"))?;
            let probe_json = entry.probe_json.clone().ok_or_else(|| {
                format!("no cached probe result for extract job '{id}'")
            })?;
            (probe_json, entry.headers.clone())
        };

        self.inner
            .spawn_download(id.to_string(), probe_json, headers, format_selector);

        Ok(())
    }

    /// Changes where a finished extraction lands, effective immediately --
    /// mirrors `QueueManager::set_download_dir`. A job already probing or
    /// downloading when this is called is unaffected (its `output_template`
    /// was already built from the value in effect when it started).
    pub fn set_download_dir(&self, dir: PathBuf) {
        *self.inner.download_dir.lock().unwrap() = dir;
    }

    /// Kills the job's live subprocess (if any) and marks it `Canceled`.
    /// A no-op if `id` is already in a terminal status.
    pub fn cancel(&self, id: &str) {
        let became_canceled = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            match jobs.get_mut(id) {
                Some(job)
                    if !matches!(
                        job.status,
                        ExtractStatus::Completed | ExtractStatus::Failed | ExtractStatus::Canceled
                    ) =>
                {
                    job.status = ExtractStatus::Canceled;
                    true
                }
                _ => false,
            }
        };

        if !became_canceled {
            return;
        }

        // The job's lifetime has definitively ended: it can never be
        // retried from `Canceled`, so the cache no longer needs to be kept.
        self.inner.cache.lock().unwrap().remove(id);

        if let Some(mut child) = self.inner.children.lock().unwrap().remove(id) {
            kill_process_tree(&mut child);
        }
    }

    /// Re-probes a `Failed` job from scratch, using its originally
    /// captured `source_url`/headers. A no-op (returns an error) for any
    /// other status.
    pub fn retry(&self, id: &str) -> Result<(), String> {
        {
            let mut jobs = self.inner.jobs.lock().unwrap();
            let job = jobs
                .get_mut(id)
                .ok_or_else(|| format!("no extract job with id '{id}'"))?;
            if job.status != ExtractStatus::Failed {
                return Err(format!("extract job '{id}' is not in a failed state"));
            }
            job.status = ExtractStatus::Probing;
            job.error = None;
            job.title = String::new();
            job.formats.clear();
        }

        let (source_url, headers) = {
            let cache = self.inner.cache.lock().unwrap();
            let entry = cache
                .get(id)
                .ok_or_else(|| format!("no cached extraction data for extract job '{id}'"))?;
            (entry.source_url.clone(), entry.headers.clone())
        };

        self.inner.spawn_probe(id.to_string(), source_url, headers);

        Ok(())
    }

    /// Removes a terminal-status job (`Completed`, `Failed`, or
    /// `Canceled`) from the tracked jobs map and drops its
    /// `ExtractionCache` entry, if any. Returns an error (removing
    /// nothing) for a job that's missing, or still live (`Probing`,
    /// `AwaitingFormatChoice`, `Downloading`) -- unlike ordinary HTTP
    /// downloads, extract jobs have no persisted queue row to fall back
    /// on, so letting a live job be removed out from under its own
    /// background task would leave that task updating a job id no
    /// longer in `jobs`.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut jobs = self.inner.jobs.lock().unwrap();
        let job = jobs
            .get(id)
            .ok_or_else(|| format!("no extract job with id '{id}'"))?;

        if !matches!(
            job.status,
            ExtractStatus::Completed | ExtractStatus::Failed | ExtractStatus::Canceled
        ) {
            return Err(format!(
                "extract job '{id}' is not in a terminal status"
            ));
        }

        jobs.remove(id);
        drop(jobs);

        self.inner.cache.lock().unwrap().remove(id);

        Ok(())
    }

    /// Same as [`Self::remove`], but if the job is `Completed`, also
    /// deletes its finished file from disk. Meant to be called only after
    /// the frontend has explicitly confirmed that deletion -- mirrors
    /// `QueueManager::remove_and_delete_file`. For any other terminal
    /// status this is identical to `remove` (there is no finished file to
    /// delete).
    pub fn remove_and_delete_file(&self, id: &str) -> Result<(), String> {
        let final_path = {
            let jobs = self.inner.jobs.lock().unwrap();
            let job = jobs
                .get(id)
                .ok_or_else(|| format!("no extract job with id '{id}'"))?;
            (job.status == ExtractStatus::Completed)
                .then(|| job.final_path.clone())
                .flatten()
        };

        self.remove(id)?;

        if let Some(path) = final_path {
            let _ = std::fs::remove_file(&path);
        }

        Ok(())
    }
}

impl Inner {
    async fn resolve_ytdlp(&self) -> Result<PathBuf, String> {
        match &self.binaries {
            BinarySource::Managed(manager) => {
                manager.ensure_ytdlp().await.map_err(|e| e.to_string())
            }
            BinarySource::Fixed { ytdlp, .. } => Ok(ytdlp.clone()),
        }
    }

    async fn resolve_ffmpeg(&self) -> Result<PathBuf, String> {
        match &self.binaries {
            BinarySource::Managed(manager) => {
                manager.ensure_ffmpeg().await.map_err(|e| e.to_string())
            }
            BinarySource::Fixed { ffmpeg, .. } => Ok(ffmpeg.clone()),
        }
    }

    fn spawn_probe(self: &Arc<Self>, id: String, source_url: String, headers: HashMap<String, String>) {
        let Some(handle) = self.runtime.clone() else {
            return;
        };
        let inner = Arc::clone(self);
        handle.spawn(async move {
            inner.run_probe(id, source_url, headers).await;
        });
    }

    fn spawn_download(
        self: &Arc<Self>,
        id: String,
        probe_json: String,
        headers: HashMap<String, String>,
        format_selector: String,
    ) {
        let Some(handle) = self.runtime.clone() else {
            return;
        };
        let inner = Arc::clone(self);
        handle.spawn(async move {
            inner
                .run_download(id, probe_json, headers, format_selector)
                .await;
        });
    }

    /// Marks `id` `Failed` with `message`, unless it's already `Canceled`
    /// (cancellation always wins). Leaves the extraction cache alone so a
    /// `retry()` can still use it. Always clears any live-subprocess entry
    /// for `id`.
    fn fail(&self, id: &str, message: impl Into<String>) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            if job.status == ExtractStatus::Canceled {
                return;
            }
            job.status = ExtractStatus::Failed;
            job.error = Some(message.into());
        }
        drop(jobs);
        self.children.lock().unwrap().remove(id);
    }

    /// Marks `id` `Completed`, unless it's already `Canceled`. The job's
    /// lifetime has now definitively ended, so its extraction cache is
    /// forgotten. `final_path` is the finished file's real absolute path
    /// (after `rename_to_friendly_name`), stored on the job so the frontend
    /// can open it or reveal it in the file manager.
    fn complete(&self, id: &str, final_path: String) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            if job.status == ExtractStatus::Canceled {
                return;
            }
            job.status = ExtractStatus::Completed;
            job.error = None;
            job.final_path = Some(final_path);
            if let Some(total) = job.total_bytes {
                job.downloaded_bytes = total;
            }
        }
        drop(jobs);
        self.cache.lock().unwrap().remove(id);
        self.children.lock().unwrap().remove(id);
    }

    /// Renames the just-downloaded file at `actual_path` (named after the
    /// video's id -- see `run_download`'s `output_template` comment for
    /// why) to a human-readable, title-based name in the same directory,
    /// and returns the path the file actually ends up at. Best-effort: a
    /// video that downloaded completely and correctly never fails a job
    /// over a cosmetic rename, so any error here is just logged, leaving
    /// (and returning) the file under its id-based name instead.
    async fn rename_to_friendly_name(&self, id: &str, actual_path: &Path) -> PathBuf {
        let title = self.jobs.lock().unwrap().get(id).map(|j| j.title.clone()).unwrap_or_default();
        if title.is_empty() {
            return actual_path.to_path_buf();
        }
        let (Some(ext), Some(dir)) = (
            actual_path.extension().and_then(|e| e.to_str()),
            actual_path.parent(),
        ) else {
            return actual_path.to_path_buf();
        };
        let friendly_path = dir.join(format!("{}.{ext}", sanitize_windows_filename(&title)));
        if friendly_path == actual_path {
            return actual_path.to_path_buf();
        }
        match tokio::fs::rename(actual_path, &friendly_path).await {
            Ok(()) => friendly_path,
            Err(e) => {
                eprintln!(
                    "extract job {id}: keeping id-based filename {actual_path:?} -- cosmetic rename to {friendly_path:?} failed: {e}"
                );
                actual_path.to_path_buf()
            }
        }
    }

    /// Whether `id` has already been flipped to `Canceled` by `cancel()`
    /// (which can race a background task that's just finished reading a
    /// killed subprocess's now-closed pipes).
    fn is_canceled(&self, id: &str) -> bool {
        self.jobs.lock().unwrap().get(id).map(|j| j.status) == Some(ExtractStatus::Canceled)
    }

    async fn run_probe(self: Arc<Self>, id: String, source_url: String, headers: HashMap<String, String>) {
        let ytdlp = match self.resolve_ytdlp().await {
            Ok(path) => path,
            Err(e) => {
                self.fail(&id, e);
                return;
            }
        };

        let mut cmd = Command::new(&ytdlp);
        hide_console_window(&mut cmd);
        // `--no-playlist` only stops a `watch?v=X&list=Y`-shaped URL from
        // expanding into its playlist; it does nothing for a URL that IS
        // fundamentally a playlist/channel/feed page (e.g. a channel's
        // main page). `--flat-playlist` covers that case: for a
        // playlist-shaped URL it returns a fast, shallow list of entries
        // instead of fully extracting every item on the page (which could
        // take minutes before failing anyway, since a playlist result has
        // no top-level `formats` array for `extract_formats` to find).
        cmd.arg("--no-playlist")
            .arg("--flat-playlist")
            .arg("--dump-single-json");
        add_youtube_client_fallback_args(&mut cmd);
        add_header_args(&mut cmd, &headers);
        cmd.arg(&source_url);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Waits its turn if `max_concurrent` probes/downloads are already
        // running -- see `Inner::concurrency`'s doc comment. Held until
        // this function returns, so it covers the whole subprocess's
        // lifetime, not just the spawn.
        let Ok(_permit) = self.concurrency.acquire().await else {
            return;
        };
        if self.is_canceled(&id) {
            return;
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.fail(&id, format!("failed to launch yt-dlp: {e}"));
                return;
            }
        };

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        self.children.lock().unwrap().insert(id.clone(), child);
        if self.is_canceled(&id)
            && let Some(mut child) = self.children.lock().unwrap().remove(&id)
        {
            kill_process_tree(&mut child);
        }

        let stdout_fut = async {
            let mut buf = Vec::new();
            if let Some(stdout) = stdout.as_mut() {
                let _ = stdout.read_to_end(&mut buf).await;
            }
            buf
        };
        let stderr_fut = async {
            let mut buf = Vec::new();
            if let Some(stderr) = stderr.as_mut() {
                let _ = stderr.read_to_end(&mut buf).await;
            }
            buf
        };
        let (stdout_buf, stderr_buf) = tokio::join!(stdout_fut, stderr_fut);

        let removed_child = self.children.lock().unwrap().remove(&id);
        let exit_status = match removed_child {
            Some(mut child) => child.wait().await.ok(),
            None => None,
        };

        if self.is_canceled(&id) {
            return;
        }

        if !exit_status.map(|s| s.success()).unwrap_or(false) {
            let message = first_nonempty_stderr_line(&stderr_buf)
                .unwrap_or_else(|| "Unsupported or unrecognized URL".to_string());
            eprintln!(
                "extract job {id}: probe failed (exit={exit_status:?}): {message}\nfull stderr:\n{}",
                String::from_utf8_lossy(&stderr_buf)
            );
            self.fail(&id, message);
            return;
        }

        let stdout_str = String::from_utf8_lossy(&stdout_buf);
        let json: Value = match serde_json::from_str(&stdout_str) {
            Ok(json) => json,
            Err(_) => {
                self.fail(&id, "Unsupported or unrecognized URL".to_string());
                return;
            }
        };

        // yt-dlp's `generic` extractor (used for any page it has no
        // dedicated extractor for -- pixeldrain, gofile, mediafire, most
        // file-hosting pages `smart_add` reaches) often reports the bare
        // URL id as `title` rather than the page's real title, even when
        // the page's own `<title>` tag has it (confirmed live against a
        // pixeldrain page: yt-dlp returned "3ez5YZmr", the page's own
        // `<title>` was "Otakudesu.io_KnH.S3--08_Mkv1080pv2.mkv ~
        // pixeldrain"). A dedicated extractor (YouTube, Vimeo, ...) doesn't
        // have this problem -- it builds `title` from the site's real
        // metadata -- so this only reads the page itself for the "generic"
        // case, not every probe. `rename_to_friendly_name` uses this same
        // `title` as the saved file's name, so a bad one here means a
        // bad filename on disk, not just a cosmetic display issue.
        let is_generic_extractor =
            json.get("extractor_key").and_then(|v| v.as_str()) == Some("Generic");
        let title = if is_generic_extractor {
            fetch_page_title(&source_url)
                .await
                .or_else(|| json.get("title").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string()))
                .unwrap_or_else(|| source_url.clone())
        } else {
            json.get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| source_url.clone())
        };

        let formats = extract_formats(&json);
        if formats.is_empty() {
            self.fail(&id, "Unsupported or unrecognized URL".to_string());
            return;
        }

        let probe_json = stdout_str.into_owned();

        // Cached *before* the status flip below, so that by the time any
        // caller can observe `AwaitingFormatChoice` (and call
        // `choose_format`), the cached JSON it needs is already in place
        // -- see `ExtractionCache::probe_json`'s doc comment.
        if let Some(entry) = self.cache.lock().unwrap().get_mut(&id) {
            entry.probe_json = Some(probe_json.clone());
        }

        // A single format isn't a choice -- go straight to downloading it,
        // the same path `choose_format` takes once a user picks the only
        // option, instead of parking in `AwaitingFormatChoice` for a click
        // that was never a real decision. Seen live via `smart_add` against
        // non-video file-hosting pages (pixeldrain, gofile, ...): yt-dlp
        // resolves each to exactly one plain format (see `extract_formats`'s
        // "Download"/`best` fallback).
        if let [only] = formats.as_slice() {
            let format_selector = only.format_selector.clone();
            {
                let mut jobs = self.jobs.lock().unwrap();
                match jobs.get_mut(&id) {
                    Some(job) if job.status != ExtractStatus::Canceled => {
                        job.title = title;
                        job.formats = formats.clone();
                        job.status = ExtractStatus::Downloading;
                    }
                    _ => return,
                }
            }
            self.spawn_download(id, probe_json, headers, format_selector);
            return;
        }

        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(&id)
            && job.status != ExtractStatus::Canceled
        {
            job.title = title;
            job.formats = formats;
            job.status = ExtractStatus::AwaitingFormatChoice;
        }
    }

    async fn run_download(
        self: Arc<Self>,
        id: String,
        probe_json: String,
        headers: HashMap<String, String>,
        format_selector: String,
    ) {
        let ytdlp = match self.resolve_ytdlp().await {
            Ok(path) => path,
            Err(e) => {
                self.fail(&id, e);
                return;
            }
        };
        let ffmpeg = match self.resolve_ffmpeg().await {
            Ok(path) => path,
            Err(e) => {
                self.fail(&id, e);
                return;
            }
        };
        let ffmpeg_dir = ffmpeg
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        // Deliberately `%(id)s`, not `%(title)s`: confirmed by direct
        // reproduction (manually re-running yt-dlp against a title
        // containing a Windows-illegal character like `|`) that the REAL
        // file yt-dlp writes uses its own Windows-safe Unicode
        // substitution (`|` -> the fullwidth `｜`), but the string it
        // reports via `--print after_move:filepath` below can silently
        // drop that same substituted character instead -- the two no
        // longer name the same file byte-for-byte. Neither the console's
        // codepage nor Python's own IO/locale-encoding env vars
        // (`PYTHONIOENCODING`, `PYTHONUTF8`) changed this; it reproduces
        // identically either way. The `tokio::fs::metadata` check right
        // below trusts that printed string literally, so this mismatch
        // made a perfectly good download fail as "produced no file" --
        // this is what that was. A video id is always a plain
        // filesystem-safe ASCII string yt-dlp never needs to re-sanitize,
        // so there's nothing for the print and the real file to disagree
        // about. `rename_to_friendly_name` below renames the result to
        // something human-readable afterwards, best-effort.
        let output_template = self.download_dir.lock().unwrap().join("%(id)s.%(ext)s");

        // The cached probe JSON, handed to yt-dlp via `--load-info-json`
        // instead of the page URL: per the plan's Global Constraints and
        // the design doc's decision #4, choosing a format must never
        // re-probe the live page, only ever use what the one earlier
        // probe already extracted. If the info embedded in that JSON
        // (e.g. signed stream URLs) has since expired, yt-dlp itself will
        // fail here and the job ends `Failed` with that error -- a stale
        // cache is a clean, surfaced failure, never a silent re-probe.
        // Stripped of `webpage_url`/`original_url` before being written --
        // see `strip_live_reprobe_keys`'s doc comment for why.
        let sanitized_json = strip_live_reprobe_keys(&probe_json);

        let info_json_path = std::env::temp_dir().join(format!("extract-job-{id}.json"));
        if let Err(e) = tokio::fs::write(&info_json_path, &sanitized_json).await {
            self.fail(
                &id,
                format!("failed to write cached extraction info: {e}"),
            );
            return;
        }

        let mut cmd = Command::new(&ytdlp);
        hide_console_window(&mut cmd);
        cmd.arg("--no-playlist")
            .arg("--load-info-json")
            .arg(&info_json_path)
            .arg("-f")
            .arg(&format_selector)
            .arg("--merge-output-format")
            .arg("mp4")
            .arg("--ffmpeg-location")
            .arg(&ffmpeg_dir)
            .arg("--newline")
            // `--print` implies `--quiet` in real yt-dlp, which would
            // otherwise suppress the `[download] NN.N%` progress lines
            // this loop below depends on.
            .arg("--progress")
            .arg("--print")
            .arg("after_move:filepath")
            .arg("-o")
            .arg(&output_template);
        add_youtube_client_fallback_args(&mut cmd);
        add_header_args(&mut cmd, &headers);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Waits its turn if `max_concurrent` probes/downloads are already
        // running -- see `Inner::concurrency`'s doc comment. Held until
        // this function returns, so it covers the whole subprocess's
        // lifetime, not just the spawn.
        let Ok(_permit) = self.concurrency.acquire().await else {
            let _ = tokio::fs::remove_file(&info_json_path).await;
            return;
        };
        if self.is_canceled(&id) {
            let _ = tokio::fs::remove_file(&info_json_path).await;
            return;
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = tokio::fs::remove_file(&info_json_path).await;
                self.fail(&id, format!("failed to launch yt-dlp: {e}"));
                return;
            }
        };

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        self.children.lock().unwrap().insert(id.clone(), child);
        if self.is_canceled(&id)
            && let Some(mut child) = self.children.lock().unwrap().remove(&id)
        {
            kill_process_tree(&mut child);
        }

        let id_for_stdout = id.clone();
        let inner_for_stdout = Arc::clone(&self);
        let stdout_fut = async move {
            let mut final_path: Option<String> = None;
            let mut raw_line_count: u32 = 0;
            let mut non_progress_lines: Vec<String> = Vec::new();
            if let Some(stdout) = stdout.take() {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    raw_line_count += 1;
                    if let Some((downloaded_bytes, total_bytes)) = parse_progress_line(&line) {
                        let mut jobs = inner_for_stdout.jobs.lock().unwrap();
                        if let Some(job) = jobs.get_mut(&id_for_stdout) {
                            job.downloaded_bytes = downloaded_bytes;
                            job.total_bytes = Some(total_bytes);
                        }
                    } else if !line.trim().is_empty() {
                        final_path = Some(line.trim().to_string());
                        non_progress_lines.push(line.trim().to_string());
                    }
                }
            }
            (final_path, raw_line_count, non_progress_lines)
        };

        let stderr_fut = async {
            let mut last_line: Option<String> = None;
            if let Some(stderr) = stderr.take() {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        last_line = Some(trimmed.to_string());
                    }
                }
            }
            last_line
        };

        let ((final_path, raw_line_count, non_progress_lines), last_stderr_line) =
            tokio::join!(stdout_fut, stderr_fut);

        let removed_child = self.children.lock().unwrap().remove(&id);
        let exit_status = match removed_child {
            Some(mut child) => child.wait().await.ok(),
            None => None,
        };

        // Best-effort: this is a scratch file we wrote ourselves above,
        // never anything the user cares about.
        let _ = tokio::fs::remove_file(&info_json_path).await;

        if self.is_canceled(&id) {
            return;
        }

        match (exit_status.map(|s| s.success()).unwrap_or(false), final_path) {
            (true, Some(path)) => match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.len() > 0 => {
                    let final_path = self.rename_to_friendly_name(&id, Path::new(&path)).await;
                    self.complete(&id, final_path.to_string_lossy().into_owned());
                }
                // yt-dlp can exit 0 and still print a final path via
                // `--print after_move:filepath` without ever writing real
                // bytes to it -- seen in practice with YouTube's
                // "SABR-only" streaming formats, which have no real stream
                // behind them. Reporting this as a success would ship a
                // broken 0-byte file with no indication anything went
                // wrong; treating it as a failure instead gives the user
                // something to act on (retry, or pick a different format).
                metadata_result => {
                    eprintln!(
                        "extract job {id}: download exited 0 and printed path {path:?}, but metadata check was {metadata_result:?} (not a usable file)\nlast stderr line: {last_stderr_line:?}\nstdout lines seen: {raw_line_count}, non-progress lines: {non_progress_lines:?}"
                    );
                    self.fail(
                        &id,
                        "Download finished but produced no file -- this video's stream format may not be available right now.".to_string(),
                    );
                }
            },
            _ => {
                let message = last_stderr_line
                    .unwrap_or_else(|| "Unsupported or unrecognized URL".to_string());
                eprintln!(
                    "extract job {id}: download failed (exit={exit_status:?}): {message}\nstdout lines seen: {raw_line_count}, non-progress lines: {non_progress_lines:?}"
                );
                self.fail(&id, message);
            }
        }
    }
}

/// Forces yt-dlp's YouTube extractor to try the normal `web` player client
/// first and fall back to `android` if that fails.
///
/// Reproduced live (not just suspected): when a request carries a header
/// that *looks* like a real, logged-in YouTube session (cookies named
/// `LOGIN_INFO`/`SID`/etc. -- content, not just presence, is what triggers
/// it), yt-dlp's default client selection switches to clients (`tv`,
/// `web_embedded`) whose signature-challenge ("n challenge") solving is
/// currently broken upstream, leaving only image-only "formats" and a hard
/// failure -- entirely independent of whether a JS runtime (Deno/Node) is
/// installed. `web,android` was verified (via direct yt-dlp invocations
/// against a real video, once with a synthetic logged-in-shaped cookie) to
/// return the full format list exactly as `default` alone does for a
/// logged-out request, with `android` only ever needed as the fallback.
/// Since this browser extension's entire point is handing yt-dlp the
/// user's real session cookies (to reach member-only/private videos), a
/// logged-in cookie reaching yt-dlp here is the common case, not an edge
/// case -- so this must be unconditional, not opt-in.
///
/// Scoped to the `youtube` extractor only; yt-dlp ignores `--extractor-args`
/// for extractors it doesn't name, so this is a harmless no-op for every
/// other site `yt-dlp` supports.
fn add_youtube_client_fallback_args(cmd: &mut Command) {
    cmd.arg("--extractor-args").arg("youtube:player_client=default,android");
}

/// Prevents a visible console window from flashing up for yt-dlp (and, since
/// it has no console of its own to inherit, the ffmpeg subprocess yt-dlp
/// itself spawns via `--ffmpeg-location`) on Windows. No-op on other
/// platforms, where spawned processes never get their own console window.
fn hide_console_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        // `tokio::process::Command` provides this as an inherent method on
        // Windows directly -- no `std::os::windows::process::CommandExt`
        // import needed (and importing it anyway triggers an unused-import
        // warning, since method resolution never reaches for the trait).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Kills `child` and, on Windows, every process it has spawned -- not just
/// `child` itself. `yt-dlp` spawns its own `ffmpeg` subprocess (via
/// `--ffmpeg-location`) to merge formats that need it, and that `ffmpeg`
/// process is `yt-dlp`'s OS-level child, not ours: `Child::start_kill()`
/// only terminates the process we directly spawned, leaving `ffmpeg`
/// orphaned and free to keep writing the output file for as long as it
/// takes to finish merging on its own. That's why a canceled job's partial
/// file could still be locked by a process that "cancel" supposedly ended.
/// `taskkill /T` kills the whole process tree rooted at `child`'s pid, so
/// `ffmpeg` goes down with it. Falls back to `start_kill()` if the pid is
/// unavailable or spawning `taskkill` itself fails, and is a plain
/// `start_kill()` on non-Windows platforms, which this app does not ship a
/// process-tree-kill equivalent for.
fn kill_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Some(pid) = child.id() {
            let spawned = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if spawned.is_ok() {
                return;
            }
        }
    }
    let _ = child.start_kill();
}

/// Adds `--referer ...`, `--user-agent ...` for each of `headers`'
/// `Referer`/`User-Agent` entries, omitting a flag entirely when its
/// captured value is empty (same convention as the browser extension's
/// header-capture fix).
///
/// Deliberately never forwards `headers`' `Cookie` entry to yt-dlp.
/// Reproduced live: a request carrying a real, logged-in YouTube session
/// cookie makes yt-dlp's YouTube extractor pick clients whose signature
/// challenge ("n challenge") solving is currently broken upstream, and the
/// `android` client that would otherwise dodge that bug refuses to run at
/// all once cookies are present ("Skipping client android since it does
/// not support cookies") -- so there is no known client-selection
/// workaround left once a real session cookie is in the request, unlike
/// `add_youtube_client_fallback_args` below, which only has to dodge the
/// *browser's own, cookie-free default* client selection. Public videos
/// (the common case) need no cookie at all, so dropping it trades away
/// only private/members-only/age-restricted extraction, recoverable later
/// if/when yt-dlp's upstream n-challenge handling for authenticated
/// sessions is fixed (surfaces automatically via the "Check for yt-dlp
/// update" button, no code change needed here).
fn add_header_args(cmd: &mut Command, headers: &HashMap<String, String>) {
    if let Some(referer) = non_empty(headers, "Referer") {
        cmd.arg("--referer").arg(referer);
    }
    if let Some(user_agent) = non_empty(headers, "User-Agent") {
        cmd.arg("--user-agent").arg(user_agent);
    }
}

fn non_empty<'a>(headers: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    headers.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
}

/// Strips `webpage_url`/`original_url` from a cached `--dump-single-json`
/// document before it's written to the `--load-info-json` temp file used
/// by the download step.
///
/// This closes a real gap in the "never silently re-probe the live page"
/// guarantee (design doc decision #4): `yt-dlp`'s own `--load-info-json`
/// handling has an internal fallback where, if processing the cached info
/// fails (e.g. a signed stream URL embedded in it has expired), it catches
/// that error and transparently re-extracts `webpage_url` live instead --
/// exactly the silent wrong-video-substitution risk this phase exists to
/// prevent, just moved one level down into `yt-dlp` itself. Removing both
/// URL fields removes `yt-dlp`'s only way to construct a URL for that
/// fallback, so a stale/invalid cache surfaces as an outright failure
/// instead.
///
/// Falls back to returning `json` unchanged if it doesn't parse as a JSON
/// object (shouldn't happen for anything that passed through
/// `run_probe`'s own `serde_json::from_str` check, but this is best-effort
/// sanitization, not the primary correctness path).
/// Replaces characters illegal in a Windows filename, and trims trailing
/// dots/spaces (also illegal there), with `_`. Used only for the cosmetic
/// post-download rename in `Inner::rename_to_friendly_name` -- yt-dlp's
/// own Windows-safe substitution is deliberately never used for the file
/// it writes itself, see `run_download`'s `output_template` comment.
fn sanitize_windows_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Best-effort fetch of `url`'s own `<title>` tag text, HTML-unescaped and
/// trimmed. `None` on any failure (network error, timeout, no `<title>`
/// found) -- this is a cosmetic-title fallback, never worth failing a probe
/// over.
///
/// Reads at most [`PAGE_TITLE_FETCH_MAX_BYTES`] (a `<title>` tag is always
/// near the top of an HTML document), via a ranged `GET` most servers honor
/// even for a page that doesn't support real partial content -- either way,
/// the body is truncated to that cap again below before searching it.
async fn fetch_page_title(url: &str) -> Option<String> {
    const PAGE_TITLE_FETCH_MAX_BYTES: usize = 65_536;
    const PAGE_TITLE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);
    const PAGE_TITLE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, PAGE_TITLE_USER_AGENT)
        .header(
            reqwest::header::RANGE,
            format!("bytes=0-{}", PAGE_TITLE_FETCH_MAX_BYTES - 1),
        )
        .timeout(PAGE_TITLE_FETCH_TIMEOUT)
        .send()
        .await
        .ok()?;

    let bytes = response.bytes().await.ok()?;
    let capped = &bytes[..bytes.len().min(PAGE_TITLE_FETCH_MAX_BYTES)];
    let html = String::from_utf8_lossy(capped);

    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")? + "<title".len();
    let open_end = lower[start..].find('>')? + start + 1;
    let close = lower[open_end..].find("</title>")? + open_end;

    let raw = html[open_end..close].trim();
    if raw.is_empty() {
        return None;
    }
    Some(unescape_html_entities(raw))
}

/// Replaces the handful of HTML entities a `<title>` tag realistically
/// contains. Not a general HTML-entity decoder (no numeric `&#NNNN;`
/// support) -- good enough for a cosmetic fallback title, not worth a new
/// dependency for.
fn unescape_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn strip_live_reprobe_keys(json: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(json) else {
        return json.to_string();
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove("webpage_url");
        obj.remove("original_url");
    }
    serde_json::to_string(&value).unwrap_or_else(|_| json.to_string())
}

/// Builds `formats` from a `--dump-single-json` document: every entry
/// whose `vcodec` isn't `"none"`, deduplicated by `height` (first seen per
/// height wins), sorted by height descending.
///
/// Falls back to one plain "Download" option (yt-dlp's own `best` selector)
/// when `formats` exists and is non-empty but nothing in it carries a
/// resolution -- seen from `smart_add`'s testing against pixeldrain, whose
/// page yt-dlp's generic extractor resolves to a single direct-file format
/// with no `height` at all. That shape isn't unique to pixeldrain -- any
/// non-video file host (gofile, mediafire, ...) or an audio-only source
/// extracts the same way -- so without this fallback every one of them
/// would fail as "unrecognized URL" despite yt-dlp having resolved it
/// correctly. A `formats` key that's missing entirely (e.g. a
/// `--flat-playlist` result) is a different shape and still returns empty,
/// same as before.
fn extract_formats(json: &Value) -> Vec<ExtractFormatOption> {
    let Some(entries) = json.get("formats").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    if entries.is_empty() {
        return Vec::new();
    }

    let mut seen = std::collections::HashSet::new();
    let mut heights: Vec<u64> = Vec::new();
    for entry in entries {
        if entry.get("vcodec").and_then(|v| v.as_str()) == Some("none") {
            continue;
        }
        let Some(height) = entry.get("height").and_then(|v| v.as_u64()) else {
            continue;
        };
        if height > 0 && seen.insert(height) {
            heights.push(height);
        }
    }
    heights.sort_unstable_by(|a, b| b.cmp(a));

    if heights.is_empty() {
        return vec![ExtractFormatOption {
            label: "Download".to_string(),
            format_selector: "best".to_string(),
        }];
    }

    heights
        .into_iter()
        .map(|height| ExtractFormatOption {
            label: format!("{height}p"),
            format_selector: format!(
                "bestvideo[height<={height}]+bestaudio/best[height<={height}]"
            ),
        })
        .collect()
}

/// The first non-empty line of `stderr`, trimmed. `None` if `stderr` is
/// empty or whitespace-only.
fn first_nonempty_stderr_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

/// Parses a yt-dlp progress line of the form
/// `[download]  NN.N% of ~SIZEUNIT ...` into `(downloaded_bytes,
/// total_bytes)`. `None` if `line` isn't a progress line (e.g. it's the
/// `--print "after_move:filepath"` line, or anything else).
fn parse_progress_line(line: &str) -> Option<(u64, u64)> {
    let rest = line.trim().strip_prefix("[download]")?.trim_start();

    let percent_end = rest.find('%')?;
    let percent: f64 = rest[..percent_end].trim().parse().ok()?;

    let after = &rest[percent_end + 1..];
    let of_idx = after.find(" of ")?;
    let size_str = after[of_idx + 4..].split_whitespace().next()?;
    let total_bytes = parse_size(size_str.trim_start_matches('~'))?;

    let downloaded_bytes = ((percent / 100.0) * total_bytes as f64).round() as u64;
    Some((downloaded_bytes, total_bytes))
}

/// Parses a human-readable size like `10.00MiB` / `512KiB` / `3GB` into
/// bytes.
fn parse_size(s: &str) -> Option<u64> {
    let split_at = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = s.split_at(split_at);
    let number: f64 = number.trim().parse().ok()?;

    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "B" => 1.0,
        "KB" | "KIB" => 1024.0,
        "MB" | "MIB" => 1024.0 * 1024.0,
        "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "TB" | "TIB" => 1024.0_f64.powi(4),
        _ => return None,
    };

    Some((number * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::Duration;

    fn tempfile_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "download-manager-extract-jobs-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Source for a tiny native stub "yt-dlp" used by every test below,
    /// compiled once (see [`stub_ytdlp_path`]) and driven purely by the
    /// real argv it receives -- no env vars, so it stays correct under
    /// `cargo test`'s default parallel test execution:
    ///
    /// - Probe mode (`--dump-single-json` present): the `source_url`
    ///   (always the last argument, matching how `jobs.rs` builds its
    ///   probe command) selects the behavior:
    ///   - `stub-fail-once://<marker path>`: fails once (and creates the
    ///     marker file), succeeds on every call after that -- used to
    ///     prove `retry()` actually re-runs the subprocess. Every
    ///     invocation of this form (whether it fails or succeeds) also
    ///     appends its full argv to `<marker's dir>/probe_args.log`, so a
    ///     test can confirm the exact same header flags reached both the
    ///     original probe and the retry.
    ///   - ends with `/fail`: exits 1 with `ERROR: Unsupported URL` on
    ///     stderr.
    ///   - contains `dedup`: emits formats with duplicate/zero heights and
    ///     a `vcodec: "none"` entry, to exercise dedup/sort/filtering.
    ///   - otherwise: a single 480p format.
    ///   The emitted title is always `probed:<source_url>`, so a test can
    ///   confirm exactly which URL was probed.
    /// - Download mode (no `--dump-single-json`): per the fix for the
    ///   anti-goal this was flagged for, there's no bare source URL here
    ///   at all any more -- only `--load-info-json <path>`, matching
    ///   `jobs.rs`'s real download command. The stub reads that file's
    ///   content (the cached probe JSON) and:
    ///   - writes it verbatim to `last_download_call.txt` next to the
    ///     `-o` template's directory, so a test can confirm the download
    ///     step used exactly the JSON an earlier probe produced, never a
    ///     fresh re-probe or another job's cached result;
    ///   - appends its full argv to `download_args.log` in that same
    ///     directory, so a test can confirm `--load-info-json` (not a
    ///     bare URL) was used, and that header flags reached this step;
    ///   - recovers the original `probed:<source_url>` URL embedded in
    ///     that JSON's `title` to decide behavior, exactly like probe
    ///     mode did:
    ///     - ends with `/fail-download`: exits 1 after one progress line.
    ///     - ends with `/spawn-grandchild`: spawns this same stub binary
    ///       again (via `--sleep-then-write <marker path>`, handled at
    ///       the very top of `main()` as a distinct sub-program) as a
    ///       genuine OS child of this process, mirroring `yt-dlp` spawning
    ///       its own `ffmpeg` child, then sleeps 10s itself -- long enough
    ///       for a test to `cancel()` and confirm the grandchild's own
    ///       marker (written 3s into ITS sleep) never appears, proving
    ///       `kill_process_tree()` kills the whole tree, not just this
    ///       stub.
    ///     - ends with `/slow`: prints a progress line, sleeps 10s, then
    ///       prints a completion line and (only now, at the very end)
    ///       writes `slow_completed_marker.txt` -- long enough for a test
    ///       to observe `Downloading`, call `cancel()`, and then confirm
    ///       that marker never appears even once past the 10s mark,
    ///       proving the process was actually killed rather than just
    ///       having its status overwritten in memory.
    ///     - otherwise: prints one progress line then a final line with
    ///       the "downloaded" file's path (the
    ///       `--print "after_move:filepath"` line).
    const STUB_SOURCE: &str = r##"
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

fn append_log(path: &Path, line: &str) {
    let mut existing = fs::read_to_string(path).unwrap_or_default();
    existing.push_str(line);
    existing.push('\n');
    let _ = fs::write(path, existing);
}

fn extract_probed_url(info_json: &str) -> String {
    if let Some(start) = info_json.find("probed:") {
        let after = &info_json[start + "probed:".len()..];
        if let Some(end) = after.find('"') {
            return after[..end].to_string();
        }
    }
    String::new()
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // Simulates the real `ffmpeg` process `yt-dlp` spawns (as ITS OWN OS
    // child, not the test harness's) to merge formats -- used to prove
    // `kill_process_tree()` kills that whole tree, not just this stub
    // itself. Checked before anything else since this is a distinct
    // sub-program this binary plays, not a `yt-dlp` arg shape.
    if args.first().map(String::as_str) == Some("--sleep-then-write") {
        if let Some(marker) = args.get(1) {
            std::thread::sleep(Duration::from_secs(3));
            let _ = fs::write(marker, b"grandchild ran to completion");
        }
        return;
    }

    let is_probe = args.iter().any(|a| a == "--dump-single-json");
    let joined_args = args.join(" ");

    if is_probe {
        let url = args.last().cloned().unwrap_or_default();

        if let Some(marker_path) = url.strip_prefix("stub-fail-once://") {
            let marker = Path::new(marker_path);
            if let Some(marker_dir) = marker.parent() {
                append_log(&marker_dir.join("probe_args.log"), &joined_args);
            }

            if marker.exists() {
                println!(
                    r#"{{"title":"probed:{url}","formats":[{{"height":480,"vcodec":"h264"}},{{"height":720,"vcodec":"h264"}}]}}"#,
                    url = url
                );
            } else {
                let _ = fs::write(marker, b"seen");
                eprintln!("ERROR: Unsupported URL");
                std::process::exit(1);
            }
            return;
        }

        if url.ends_with("/fail") {
            eprintln!("ERROR: Unsupported URL");
            std::process::exit(1);
        }

        let formats = if url.contains("dedup") {
            r#"[{"height":144,"vcodec":"h264"},{"height":480,"vcodec":"h264"},{"height":480,"vcodec":"vp9"},{"height":720,"vcodec":"h264"},{"height":null,"vcodec":"none"}]"#
        } else if url.contains("no-height") {
            // Shaped like yt-dlp's generic extractor resolving a
            // non-video file-hosting page (pixeldrain, gofile, ...): one
            // format, no `height` at all.
            r#"[{"format_id":"matroska","vcodec":null,"resolution":null}]"#
        } else if url.contains("generic-title-test") {
            // Shaped like yt-dlp's generic extractor: a real `extractor_key`
            // of "Generic", with a deliberately bad (id-like) `title` --
            // used to prove the real page's own `<title>` tag wins instead.
            // Two formats (not one) so this stays in `AwaitingFormatChoice`
            // -- this test is about title resolution, not the separate
            // single-format auto-download fast path.
            println!(
                r#"{{"title":"id-like-bad-title","extractor_key":"Generic","formats":[{{"height":480,"vcodec":"h264"}},{{"height":720,"vcodec":"h264"}}]}}"#
            );
            return;
        } else if url.contains("flat-playlist") {
            // Shaped like a real `--flat-playlist` result: a list of
            // entries, no top-level `formats` array at all.
            println!(
                r#"{{"title":"probed:{url}","_type":"playlist","entries":[{{"id":"a"}},{{"id":"b"}}]}}"#,
                url = url
            );
            return;
        } else {
            // Two formats, not one: most tests in this file use a plain
            // `stub://...` URL and then deliberately call `choose_format`
            // themselves to simulate a user picking a quality -- a single
            // format here would hit `run_probe`'s single-format
            // auto-download fast path instead and skip `AwaitingFormatChoice`
            // entirely, same as the real `probe_success_with_no_resolution_
            // info_offers_a_single_download_option` test intentionally
            // exercises. Tests that want exactly one format use `no-height`.
            r#"[{"height":480,"vcodec":"h264"},{"height":720,"vcodec":"h264"}]"#
        };

        if url.contains("strip-url-test") {
            // Shaped like a real `--dump-single-json` result, which
            // always includes `webpage_url`/`original_url` -- used to
            // prove the download step's info-json file never carries
            // them through.
            println!(
                r#"{{"title":"probed:{url}","webpage_url":"{url}","original_url":"{url}","formats":{formats}}}"#,
                url = url,
                formats = formats
            );
            return;
        }

        println!(r#"{{"title":"probed:{url}","formats":{formats}}}"#, url = url, formats = formats);
        return;
    }

    // Download mode: no bare source URL any more -- only cached
    // extraction info, loaded from the file `--load-info-json` points at.
    let output_template = args
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default();
    let dir = Path::new(&output_template).parent().map(|p| p.to_path_buf());

    let info_json_path = args
        .iter()
        .position(|a| a == "--load-info-json")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let info_json = info_json_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();

    if let Some(dir) = &dir {
        let _ = fs::write(dir.join("last_download_call.txt"), &info_json);
        append_log(&dir.join("download_args.log"), &joined_args);
    }

    let embedded_url = extract_probed_url(&info_json);

    if embedded_url.ends_with("/fail-download") {
        println!("[download]  10.0% of ~1.00MiB");
        eprintln!("ERROR: download failed");
        std::process::exit(1);
    }

    if embedded_url.ends_with("/spawn-grandchild") {
        // Mirrors real `yt-dlp`: spawn an `ffmpeg`-like OS child of OUR
        // OWN (not the test harness's), then stay alive ourselves for
        // longer than that child's own sleep -- exactly like `yt-dlp`
        // staying up while it waits on `ffmpeg` to finish merging.
        if let Some(dir) = &dir {
            let marker = dir.join("grandchild_marker.txt");
            if let Ok(exe) = env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .arg("--sleep-then-write")
                    .arg(&marker)
                    .spawn();
            }
        }
        println!("[download]   0.0% of ~1.00MiB");
        std::thread::sleep(Duration::from_secs(10));
        println!("[download] 100.0% of ~1.00MiB");
        return;
    }

    if embedded_url.ends_with("/slow") {
        println!("[download]   0.0% of ~1.00MiB");
        std::thread::sleep(Duration::from_secs(10));
        println!("[download] 100.0% of ~1.00MiB");
        if let Some(dir) = &dir {
            let _ = fs::write(dir.join("slow_completed_marker.txt"), b"done");
        }
    } else {
        println!("[download]  50.0% of ~1.00MiB");
    }

    let final_path = dir.map(|d| d.join("video.mp4")).unwrap_or_default();

    // Mirrors the real bug this mode exists to reproduce: a YouTube
    // "SABR-only" format with no real stream behind it can make yt-dlp
    // exit 0 and still print a final path via `--print after_move:filepath`
    // without ever actually writing bytes to it. Every other branch below
    // writes real, non-empty content, like a real successful download
    // would -- this is the one deliberate exception.
    if embedded_url.ends_with("/empty-file") {
        println!("{}", final_path.display());
        return;
    }

    let _ = fs::write(&final_path, b"fake mp4 bytes");
    println!("{}", final_path.display());
}
"##;

    fn stub_ytdlp_path() -> PathBuf {
        static STUB_PATH: OnceLock<PathBuf> = OnceLock::new();
        STUB_PATH.get_or_init(compile_stub).clone()
    }

    fn compile_stub() -> PathBuf {
        let dir = tempfile_dir();
        let src_path = dir.join("stub_ytdlp.rs");
        std::fs::write(&src_path, STUB_SOURCE).unwrap();
        let exe_path = dir.join("stub_ytdlp.exe");

        let output = std::process::Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg(&src_path)
            .arg("-o")
            .arg(&exe_path)
            .output()
            .expect("failed to invoke rustc to build the test stub yt-dlp binary");

        assert!(
            output.status.success(),
            "failed to compile stub yt-dlp binary:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        exe_path
    }

    async fn wait_for(
        jobs: &ExtractJobManager,
        id: &str,
        pred: impl Fn(&ExtractJobInfo) -> bool,
    ) -> ExtractJobInfo {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut last_seen: Option<ExtractJobInfo> = None;
        loop {
            if let Some(info) = jobs.list().into_iter().find(|j| j.id == id) {
                if pred(&info) {
                    return info;
                }
                last_seen = Some(info);
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for extract job {id} to reach the expected state; last seen: {last_seen:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn new_manager(download_dir: &Path) -> ExtractJobManager {
        let stub = stub_ytdlp_path();
        ExtractJobManager::new_with_binaries(
            download_dir.to_path_buf(),
            stub.clone(),
            stub,
            Arc::new(|| {}),
        )
    }

    #[tokio::test]
    async fn probe_success_dedupes_and_sorts_formats() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://dedup-test".to_string(), HashMap::new());
        let info = wait_for(&manager, &id, |j| {
            matches!(j.status, ExtractStatus::AwaitingFormatChoice | ExtractStatus::Failed)
        })
        .await;

        assert_eq!(info.status, ExtractStatus::AwaitingFormatChoice);
        assert_eq!(info.title, "probed:stub://dedup-test");
        let labels: Vec<&str> = info.formats.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, vec!["720p", "480p", "144p"]);
        assert_eq!(
            info.formats[0].format_selector,
            "bestvideo[height<=720]+bestaudio/best[height<=720]"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `smart_add`-triggered extractions aren't always a video with a
    /// resolution ladder -- a generic file-hosting page (pixeldrain,
    /// gofile, ...) resolves to one plain format with no `height` at all.
    /// That must offer a single "download it" option, not fail as an
    /// unrecognized URL just because it carries no resolution to choose,
    /// AND (there being only the one option) start downloading it
    /// immediately rather than parking for a click that isn't a real
    /// choice -- see the single-format fast path in `run_probe`.
    #[tokio::test]
    async fn probe_success_with_no_resolution_info_offers_a_single_download_option() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://no-height-test".to_string(), HashMap::new());
        let info = wait_for(&manager, &id, |j| {
            matches!(j.status, ExtractStatus::Completed | ExtractStatus::Failed)
        })
        .await;

        assert_eq!(info.status, ExtractStatus::Completed);
        let labels: Vec<&str> = info.formats.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, vec!["Download"]);
        assert_eq!(info.formats[0].format_selector, "best");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real bug this is pinned to (found via `smart_add`'s manual
    /// testing against pixeldrain): yt-dlp's `generic` extractor reported
    /// `title: "3ez5YZmr"` (the bare file id) for a page whose own
    /// `<title>` tag held the real name -- and `rename_to_friendly_name`
    /// uses that bad title for the saved file's name, not just the display.
    #[tokio::test]
    async fn probe_with_generic_extractor_prefers_the_pages_own_title_tag() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/generic-title-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<html><head><title>Real Name From Page</title></head><body></body></html>",
            ))
            .mount(&server)
            .await;

        let url = format!("{}/generic-title-test", server.uri());
        let id = manager.probe(url, HashMap::new());
        let info = wait_for(&manager, &id, |j| {
            matches!(j.status, ExtractStatus::AwaitingFormatChoice | ExtractStatus::Failed)
        })
        .await;

        assert_eq!(info.status, ExtractStatus::AwaitingFormatChoice);
        assert_eq!(info.title, "Real Name From Page");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn probe_failure_reports_stderr_message() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://any/fail".to_string(), HashMap::new());
        let info = wait_for(&manager, &id, |j| j.status == ExtractStatus::Failed).await;

        assert_eq!(info.error, Some("ERROR: Unsupported URL".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn two_probes_produce_distinct_ids_with_independent_results() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id_a = manager.probe("stub://two-a".to_string(), HashMap::new());
        let id_b = manager.probe("stub://two-b".to_string(), HashMap::new());
        assert_ne!(id_a, id_b);

        let info_a = wait_for(&manager, &id_a, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        let info_b = wait_for(&manager, &id_b, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        assert_eq!(info_a.title, "probed:stub://two-a");
        assert_eq!(info_b.title, "probed:stub://two-b");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn choose_format_uses_the_cached_probe_result_not_a_later_probe() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id_a = manager.probe("stub://job-a".to_string(), HashMap::new());
        wait_for(&manager, &id_a, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        let id_b = manager.probe("stub://job-b".to_string(), HashMap::new());
        wait_for(&manager, &id_b, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        manager.choose_format(&id_a, "480p").unwrap();
        wait_for(&manager, &id_a, |j| j.status == ExtractStatus::Completed).await;

        // The download step must be driven entirely from job A's cached
        // probe result -- never a fresh re-probe of the live page, and
        // never job B's (more recently probed) data. `last_download_call`
        // is the exact content of the `--load-info-json` file the
        // download subprocess read, so this also proves the anti-goal
        // fix itself: no bare URL reached the download step at all.
        //
        // Compared as parsed JSON, not a byte-exact string, because
        // `strip_live_reprobe_keys` re-serializes the cached JSON (via
        // `serde_json::Value`, whose object keys come out in sorted
        // order rather than the original's) before it's written to the
        // temp file the stub reads here.
        let logged = std::fs::read_to_string(dir.join("last_download_call.txt")).unwrap();
        let logged_json: Value = serde_json::from_str(&logged).unwrap();
        assert_eq!(
            logged_json,
            serde_json::json!({
                "title": "probed:stub://job-a",
                "formats": [{"height": 480, "vcodec": "h264"}, {"height": 720, "vcodec": "h264"}],
            })
        );

        let download_args = std::fs::read_to_string(dir.join("download_args.log")).unwrap();
        assert!(download_args.contains("--load-info-json"));
        assert!(!download_args.contains("stub://job-a"));
        assert!(!download_args.contains("stub://job-b"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn choose_format_with_unknown_label_errors_without_spawning() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://format-test".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        let result = manager.choose_format(&id, "9999p");
        assert!(result.is_err());

        let info = manager.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(info.status, ExtractStatus::AwaitingFormatChoice);
        assert!(!dir.join("last_download_call.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancel_on_downloading_job_kills_process_and_ends_canceled() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://cancel-test/slow".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        manager.choose_format(&id, "480p").unwrap();

        // The status flip to `Downloading` happens synchronously inside
        // `choose_format`, before the subprocess is even spawned.
        let info = manager.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(info.status, ExtractStatus::Downloading);

        let cancel_started = std::time::Instant::now();
        manager.cancel(&id);

        let info = wait_for(&manager, &id, |j| j.status == ExtractStatus::Canceled).await;
        assert_eq!(info.status, ExtractStatus::Canceled);
        // `cancel()`'s own status flip is synchronous/instant; the status
        // reaching `Canceled` on its own doesn't prove the subprocess was
        // actually killed (both runners skip overwriting an already-
        // `Canceled` status regardless of whether their process is still
        // running). Require it to settle quickly as a sanity check.
        assert!(
            cancel_started.elapsed() < Duration::from_secs(5),
            "took too long to observe Canceled"
        );

        // The real proof: the stub only writes this marker file *after*
        // its full 10s sleep, right before it would otherwise report
        // success. Waiting well past that point and confirming it still
        // never appears proves the subprocess itself was terminated, not
        // just that our own bookkeeping ignored a late natural completion.
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert!(
            !dir.join("slow_completed_marker.txt").exists(),
            "the stub subprocess ran to completion despite cancel() -- it was not actually killed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Windows-only: the fix this proves (`kill_process_tree()`) uses
    // `taskkill /T`, a Windows-specific mechanism this app ships no
    // equivalent for elsewhere -- see that function's doc comment.
    #[cfg(windows)]
    #[tokio::test]
    async fn cancel_kills_the_whole_process_tree_not_just_the_direct_child() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://cancel-test/spawn-grandchild".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        manager.choose_format(&id, "480p").unwrap();
        let info = manager.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(info.status, ExtractStatus::Downloading);

        // Give the stub a moment to actually spawn its grandchild before
        // canceling -- canceling instantly, before the grandchild process
        // even exists, would prove nothing about tree-kill.
        tokio::time::sleep(Duration::from_millis(500)).await;
        manager.cancel(&id);

        wait_for(&manager, &id, |j| j.status == ExtractStatus::Canceled).await;

        // The grandchild writes its marker 3s into its own sleep, started
        // ~0.5s after probe's AwaitingFormatChoice. Waiting past that
        // point and confirming it never appears proves `taskkill /T`
        // reached the grandchild too, not just the stub we spawned
        // directly -- a plain `start_kill()` would leave it running to
        // write this marker right on schedule.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !dir.join("grandchild_marker.txt").exists(),
            "the grandchild process ran to completion despite cancel() -- only the direct child was killed, not the whole tree"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn retry_reprobes_a_failed_job_using_the_retained_url() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let marker_path = dir.join("marker.txt");
        // Forward slashes only: the marker path is embedded verbatim into
        // the stub's JSON `title` field on success, and a raw Windows
        // backslash there would make that JSON invalid. Windows file APIs
        // (and Rust's `Path`) accept `/` just as well as `\`.
        let marker_url = marker_path.to_string_lossy().replace('\\', "/");
        let url = format!("stub-fail-once://{marker_url}");

        let id = manager.probe(url.clone(), HashMap::new());
        let info = wait_for(&manager, &id, |j| j.status == ExtractStatus::Failed).await;
        assert_eq!(info.error, Some("ERROR: Unsupported URL".to_string()));
        assert!(marker_path.exists());

        manager.retry(&id).unwrap();

        // The flip back to `Probing` happens synchronously inside `retry`.
        let info = manager.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(info.status, ExtractStatus::Probing);

        let info = wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        assert_eq!(info.title, format!("probed:{url}"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn retry_retains_original_headers_through_probe_and_download() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let marker_path = dir.join("marker.txt");
        let marker_url = marker_path.to_string_lossy().replace('\\', "/");
        let url = format!("stub-fail-once://{marker_url}");

        let mut headers = HashMap::new();
        headers.insert("Cookie".to_string(), "session=abc123".to_string());
        headers.insert("Referer".to_string(), "https://example.com/page".to_string());
        headers.insert("User-Agent".to_string(), "TestAgent/9.9".to_string());

        let id = manager.probe(url.clone(), headers.clone());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Failed).await;

        manager.retry(&id).unwrap();
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        // Both the original (failed) probe and the retry's probe must
        // have carried the exact same retained headers -- not dropped,
        // not empty, not some other job's.
        let probe_args_log = std::fs::read_to_string(dir.join("probe_args.log")).unwrap();
        let probe_lines: Vec<&str> = probe_args_log.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            probe_lines.len(),
            2,
            "expected exactly one failed probe + one retry probe invocation, got: {probe_lines:?}"
        );
        for line in &probe_lines {
            // Cookie is deliberately never forwarded to yt-dlp (see
            // `add_header_args`'s doc comment) -- Referer/User-Agent still
            // are, and must survive a retry unchanged.
            assert!(!line.contains("session=abc123"), "cookie leaked into args: {line}");
            assert!(line.contains("https://example.com/page"), "line missing referer: {line}");
            assert!(line.contains("TestAgent/9.9"), "line missing user-agent: {line}");
        }

        manager.choose_format(&id, "480p").unwrap();
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Completed).await;

        let download_args_log = std::fs::read_to_string(dir.join("download_args.log")).unwrap();
        assert!(!download_args_log.contains("session=abc123"));
        assert!(download_args_log.contains("https://example.com/page"));
        assert!(download_args_log.contains("TestAgent/9.9"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn retry_on_a_non_failed_job_returns_err() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://not-failed".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        let result = manager.retry(&id);
        assert!(result.is_err());

        let info = manager.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(info.status, ExtractStatus::AwaitingFormatChoice);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_step_strips_webpage_url_and_original_url_from_the_info_json() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        // The stub's `strip-url-test` branch emits a cached probe JSON
        // that (like real `--dump-single-json` output) includes
        // `webpage_url`/`original_url`. `last_download_call.txt` is the
        // exact content of the `--load-info-json` file the download
        // subprocess actually read -- it must have had those two keys
        // removed, since they're yt-dlp's only way to construct a URL
        // for its internal "info failed to process, falling back to a
        // live re-extraction of webpage_url" behavior (the fix for I1).
        let id = manager.probe("stub://strip-url-test".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        manager.choose_format(&id, "480p").unwrap();
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Completed).await;

        let logged = std::fs::read_to_string(dir.join("last_download_call.txt")).unwrap();
        let logged_json: Value = serde_json::from_str(&logged).unwrap();
        let obj = logged_json.as_object().unwrap();

        assert!(
            !obj.contains_key("webpage_url"),
            "info-json handed to yt-dlp still contains webpage_url: {logged}"
        );
        assert!(
            !obj.contains_key("original_url"),
            "info-json handed to yt-dlp still contains original_url: {logged}"
        );
        // Still has what the download step actually needs.
        assert_eq!(
            obj.get("title").and_then(|v| v.as_str()),
            Some("probed:stub://strip-url-test")
        );
        assert!(obj.contains_key("formats"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_completing_with_no_bytes_written_fails_instead_of_shipping() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        // The stub's `/empty-file` branch mirrors a real, observed bug:
        // yt-dlp can exit 0 and print a final path via
        // `--print after_move:filepath` for a YouTube "SABR-only" format
        // that has no real stream behind it, without ever writing bytes to
        // that path. This must surface as a Failed job with a clear
        // message, never a silently "Completed" job for a file that was
        // never actually written.
        let id = manager.probe("stub://any/empty-file".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        manager.choose_format(&id, "480p").unwrap();
        let info = wait_for(&manager, &id, |j| j.status == ExtractStatus::Failed).await;

        assert!(
            info.error.as_deref().unwrap_or("").contains("no file"),
            "expected a clear empty-file error, got: {:?}",
            info.error
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn probe_of_a_flat_playlist_shaped_url_fails_cleanly() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        // The stub's `flat-playlist` branch returns a JSON document
        // shaped like a real `--flat-playlist` result for a
        // playlist/channel/feed-style URL: a list of entries, no
        // top-level `formats` array. This must end the job `Failed`
        // (via the existing empty-formats check), never stuck
        // `Probing` and never panicking.
        let id = manager.probe("stub://flat-playlist-test".to_string(), HashMap::new());
        let info = wait_for(&manager, &id, |j| {
            matches!(j.status, ExtractStatus::Failed | ExtractStatus::AwaitingFormatChoice)
        })
        .await;

        assert_eq!(info.status, ExtractStatus::Failed);
        assert!(info.error.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remove_succeeds_on_each_terminal_status_and_drops_the_job() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        // Completed.
        let id = manager.probe("stub://remove-completed".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        manager.choose_format(&id, "480p").unwrap();
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Completed).await;
        manager.remove(&id).unwrap();
        assert!(manager.list().into_iter().all(|j| j.id != id));

        // Failed.
        let id = manager.probe("stub://remove-failed/fail".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Failed).await;
        manager.remove(&id).unwrap();
        assert!(manager.list().into_iter().all(|j| j.id != id));

        // Canceled.
        let id = manager.probe("stub://remove-canceled".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        manager.cancel(&id);
        let info = wait_for(&manager, &id, |j| j.status == ExtractStatus::Canceled).await;
        assert_eq!(info.status, ExtractStatus::Canceled);
        manager.remove(&id).unwrap();
        assert!(manager.list().into_iter().all(|j| j.id != id));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remove_on_non_terminal_statuses_errs_without_removing_anything() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        // Probing: remove immediately, before the stub subprocess has
        // had a chance to finish and flip the status away from Probing.
        let id = manager.probe("stub://remove-probing".to_string(), HashMap::new());
        let result = manager.remove(&id);
        assert!(result.is_err());
        assert!(manager.list().into_iter().any(|j| j.id == id));
        // Let the background probe finish so it doesn't outlive the test.
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        // AwaitingFormatChoice.
        let result = manager.remove(&id);
        assert!(result.is_err());
        assert!(manager.list().into_iter().any(|j| j.id == id));

        // Downloading.
        manager.choose_format(&id, "480p").unwrap();
        let info = manager.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(info.status, ExtractStatus::Downloading);
        let result = manager.remove(&id);
        assert!(result.is_err());
        assert!(manager.list().into_iter().any(|j| j.id == id));
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Completed).await;

        // Unknown id.
        let result = manager.remove("not-a-real-id");
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remove_keeps_a_completed_file_but_remove_and_delete_file_deletes_it() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let keep_id = manager.probe("stub://remove-keep-file".to_string(), HashMap::new());
        wait_for(&manager, &keep_id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        manager.choose_format(&keep_id, "480p").unwrap();
        let keep_info = wait_for(&manager, &keep_id, |j| j.status == ExtractStatus::Completed).await;
        let keep_path = PathBuf::from(keep_info.final_path.unwrap());
        assert!(keep_path.exists());

        let delete_id = manager.probe("stub://remove-delete-file".to_string(), HashMap::new());
        wait_for(&manager, &delete_id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        manager.choose_format(&delete_id, "480p").unwrap();
        let delete_info = wait_for(&manager, &delete_id, |j| j.status == ExtractStatus::Completed).await;
        let delete_path = PathBuf::from(delete_info.final_path.unwrap());
        assert!(delete_path.exists());

        manager.remove(&keep_id).unwrap();
        assert!(
            keep_path.exists(),
            "plain remove() must never delete a completed job's file"
        );

        manager.remove_and_delete_file(&delete_id).unwrap();
        assert!(
            !delete_path.exists(),
            "remove_and_delete_file() must delete the completed job's file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remove_drops_the_extraction_cache_entry() {
        let dir = tempfile_dir();
        let manager = new_manager(&dir);

        let id = manager.probe("stub://remove-cache-test".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;

        // While still AwaitingFormatChoice, the cache entry backing it
        // must still be usable (choose_format needs it).
        assert!(manager.inner.cache.lock().unwrap().contains_key(&id));

        manager.cancel(&id);
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Canceled).await;

        // `cancel()` itself already drops the cache for a canceled job
        // (see its own doc comment), so re-probe a fresh job and drive
        // it to Completed instead, to prove `remove()` is the one
        // dropping the cache for *that* terminal status.
        let id = manager.probe("stub://remove-cache-test-2".to_string(), HashMap::new());
        wait_for(&manager, &id, |j| j.status == ExtractStatus::AwaitingFormatChoice).await;
        manager.choose_format(&id, "480p").unwrap();
        wait_for(&manager, &id, |j| j.status == ExtractStatus::Completed).await;

        // `complete()` already drops the cache too (see its own doc
        // comment) -- confirm it's gone, then confirm `remove()` is a
        // clean no-op against an already-absent cache entry (it must
        // not panic or error just because the cache was already empty).
        assert!(!manager.inner.cache.lock().unwrap().contains_key(&id));
        manager.remove(&id).unwrap();
        assert!(!manager.inner.cache.lock().unwrap().contains_key(&id));

        std::fs::remove_dir_all(&dir).ok();
    }
}
