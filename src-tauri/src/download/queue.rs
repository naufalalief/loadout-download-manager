use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use super::engine::{PROBE_RETRY_BASE_DELAY, PROBE_RETRY_MAX_ATTEMPTS, download_segmented, probe};
use super::limiter::{SpeedLimiter, normalize_limit};
use super::persistence::{load_state, save_state};
use super::retry::retry_with_backoff_cancelable;
use super::segments::part_path;
use super::types::{
    CancelToken, DownloadError, DownloadInfo, DownloadRequest, DownloadStatus, ProbeInfo,
};

/// Number of concurrent range-request segments used for every download.
const DEFAULT_SEGMENT_COUNT: usize = 4;

/// Upper bound on the `name (N).ext` suffix tried when uniquifying a
/// filename, so a pathological directory can't spin forever.
const MAX_UNIQUE_SUFFIX: u32 = 10_000;

/// Windows reserved device names: a file literally named e.g. `CON.txt`
/// can't be created normally, so such names get a `_` prefix.
const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Current time as unix seconds, used to stamp `created_at` on newly
/// created `DownloadInfo`/`ExtractJobInfo` entries. Falls back to `0` on a
/// clock error rather than panicking.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What `run()` does with whatever its download produced, decided from the
/// entry's authoritative current state under the `downloads` lock (see
/// `Inner::resolve`).
enum RunDisposition {
    /// The download finished and the entry was still wanted: it is now
    /// `Completed` and the committed file at its `dest_path` stays.
    Keep,
    /// Leave everything on disk as it is: either partial files that a
    /// later resume/retry will continue from (the entry is `Paused`,
    /// `Queued`, or now `Failed`), or nothing to do at all.
    LeaveAsIs,
    /// The entry was `Canceled` or removed while this run was alive:
    /// whatever this run produced -- its staging/part files, or the file
    /// it just committed -- must not survive it.
    Discard,
}

/// Orchestrates a persisted list of downloads, running up to
/// `max_concurrent` of them at a time.
///
/// `QueueManager` is a thin handle: all of its state lives in `Inner`,
/// shared across every spawned download task.
///
/// # Concurrency model
///
/// **At most one `run()` task exists per download id at any time.** An id
/// is inserted into `Inner::running` by `schedule()` (atomically with its
/// `Queued` -> `Downloading` flip) and removed again only by that run
/// itself, in the same `downloads`-lock critical section in which it
/// records its final outcome (`Inner::resolve`). `schedule()` never picks
/// a `Queued` entry whose id is still in `running`, so a `pause()`
/// immediately followed by `resume()` simply leaves the entry `Queued`
/// until the previous run has fully stopped writing; the stopping run's
/// exit re-triggers `schedule()`, which then starts the new run. Two runs
/// can therefore never write the same staging/part files concurrently.
///
/// # Persistence
///
/// Only status (and other structural) changes are written to disk, never
/// per-chunk progress, and every write goes through `Inner::mutate`, which
/// serializes writers and writes atomically (see `save_state`).
pub struct QueueManager {
    inner: Arc<Inner>,
}

struct Inner {
    /// Where a newly `add()`-ed download's file lands. A `Mutex`, not a
    /// plain `PathBuf`, so `set_download_dir` can change it live (see that
    /// method's doc comment) -- an already-running download keeps writing
    /// to the `dest_path` it was given when `add()` ran, since that path
    /// was already captured into its own `DownloadInfo` at that point.
    download_dir: Mutex<PathBuf>,
    state_file: PathBuf,
    client: reqwest::Client,

    /// The persisted list. Never holds a `.lock()` across an `.await`.
    downloads: Mutex<Vec<DownloadInfo>>,

    /// Serializes every "mutate the list, then write it to disk" sequence
    /// (see `mutate`), so two state-file writes can never interleave and
    /// the order of writes always matches the order of the snapshots they
    /// write -- the file can never end up holding an older state than the
    /// most recent write. Lock order: `persist_lock` -> `downloads` ->
    /// `running` -> `cancel_tokens`.
    persist_lock: Mutex<()>,

    /// Number of state-file writes so far. Test-support only (see
    /// `QueueManager::persist_count`).
    persist_count: AtomicU64,

    /// In-memory-only side channel for each download's request headers
    /// (auth tokens, cookies, etc), keyed by download id. Deliberately
    /// kept separate from `downloads` so it can never be swept up into
    /// `save_state`/the persisted JSON -- see `DownloadInfo`'s doc comment
    /// for why that matters.
    headers: Mutex<HashMap<String, HashMap<String, String>>>,

    /// Cancellation signal for whichever run is currently alive for a
    /// given id, if any. Inserted with `running`, removed in `resolve`.
    cancel_tokens: Mutex<HashMap<String, CancelToken>>,

    /// Ids that currently have a live `run()` task. See the type-level
    /// "Concurrency model" docs.
    running: Mutex<HashSet<String>>,

    /// Per-download speed limiter, created on `add` (if a default limit is
    /// configured), on `set_speed_limit`, or when a download starts,
    /// whichever comes first; kept across pause/resume and failure/retry.
    limiters: Mutex<HashMap<String, Arc<SpeedLimiter>>>,

    /// Speed limit applied to every newly added download (and to any
    /// download that starts without a limiter of its own). `None` means
    /// unlimited.
    default_speed_limit: Mutex<Option<u64>>,

    /// Gates how many downloads run at once.
    semaphore: Arc<Semaphore>,

    /// Captured at construction time so background download tasks can be
    /// spawned from synchronous methods (`add`, `resume`, ...). `None` when
    /// no Tokio runtime is active (e.g. plain `#[test]` functions that only
    /// exercise persistence) -- in that case scheduling is simply skipped.
    runtime: Option<tokio::runtime::Handle>,
}

impl QueueManager {
    /// Loads any existing [`DownloadInfo`] list from `state_file`, resets
    /// any entry left `Downloading` (crash recovery: the process that was
    /// running it is gone) to `Queued`, and starts driving the queue up to
    /// `max_concurrent` at a time.
    pub fn new(download_dir: PathBuf, state_file: PathBuf, max_concurrent: usize) -> Self {
        let mut downloads = load_state(&state_file);
        for download in &mut downloads {
            if download.status == DownloadStatus::Downloading {
                download.status = DownloadStatus::Queued;
            }
        }

        let inner = Arc::new(Inner {
            download_dir: Mutex::new(download_dir),
            state_file,
            client: reqwest::Client::new(),
            downloads: Mutex::new(downloads),
            persist_lock: Mutex::new(()),
            persist_count: AtomicU64::new(0),
            headers: Mutex::new(HashMap::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
            running: Mutex::new(HashSet::new()),
            limiters: Mutex::new(HashMap::new()),
            default_speed_limit: Mutex::new(None),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            runtime: tokio::runtime::Handle::try_current().ok(),
        });

        // Persist immediately so the on-disk state reflects the crash
        // recovery reset even if nothing else ever mutates it.
        inner.mutate(|_| ((), true));
        inner.schedule();

        Self { inner }
    }

    /// Queues a new download and returns its generated id.
    ///
    /// The filename (requested, or derived from the URL) is first reduced
    /// to a single safe path component (see [`sanitize_filename`]) -- so
    /// nothing handed in through the UI or the handoff server can make a
    /// download land outside `download_dir` -- and then made unique: if a
    /// file with that name already exists in `download_dir`, or another
    /// (non-canceled) download in the queue is already headed for it, a
    /// browser-style ` (1)`, ` (2)`, ... suffix is added.
    ///
    /// `req.headers` is handed to the in-memory-only side channel, never
    /// to the persisted list. The current default speed limit (see
    /// [`Self::set_default_speed_limit`]) is applied to the new download.
    pub fn add(&self, req: DownloadRequest) -> String {
        let id = Uuid::new_v4().to_string();
        let requested = req
            .filename
            .as_deref()
            .and_then(sanitize_filename)
            .or_else(|| sanitize_filename(&derive_filename(&req.url, &id)))
            .unwrap_or_else(|| id.clone());

        // Side channels first, so they're in place before `schedule()`
        // can possibly start this download.
        if !req.headers.is_empty() {
            self.inner
                .headers
                .lock()
                .unwrap()
                .insert(id.clone(), req.headers);
        }
        let default_limit = *self.inner.default_speed_limit.lock().unwrap();
        if default_limit.is_some() {
            self.inner
                .limiters
                .lock()
                .unwrap()
                .insert(id.clone(), Arc::new(SpeedLimiter::new(default_limit)));
        }

        let download_dir = self.inner.download_dir.lock().unwrap().clone();
        let url = req.url;
        let new_id = id.clone();
        self.inner.mutate(move |downloads| {
            // Picked under the `downloads` lock so two concurrent `add`s
            // can't both claim the same free name.
            let filename = unique_filename(&download_dir, &requested, downloads);
            let dest_path = download_dir.join(&filename);
            downloads.push(DownloadInfo {
                id: new_id,
                url,
                filename,
                dest_path,
                total_bytes: None,
                downloaded_bytes: 0,
                status: DownloadStatus::Queued,
                error: None,
                created_at: now_unix(),
            });
            ((), true)
        });

        self.inner.schedule();

        id
    }

    /// Pauses a currently-`Downloading` entry. A no-op if it isn't
    /// `Downloading` (e.g. already `Queued`, `Paused`, or finished).
    pub fn pause(&self, id: &str) {
        let paused = self.inner.mutate(|downloads| {
            match downloads.iter_mut().find(|d| d.id == id) {
                Some(d) if d.status == DownloadStatus::Downloading => {
                    d.status = DownloadStatus::Paused;
                    (true, true)
                }
                _ => (false, false),
            }
        });

        if paused {
            if let Some(token) = self.inner.cancel_tokens.lock().unwrap().get(id) {
                token.cancel();
            }
        }
    }

    /// Resumes a `Paused` entry by requeuing it. A no-op if it isn't
    /// `Paused`.
    ///
    /// If the paused run is still winding down (e.g. it was sleeping in
    /// the speed limiter when paused), the entry goes back to `Queued`
    /// immediately but its new run only starts once the old one has fully
    /// exited -- see the type-level "Concurrency model" docs.
    pub fn resume(&self, id: &str) {
        self.requeue_from(id, DownloadStatus::Paused);
    }

    /// Retries a `Failed` entry by requeuing it (and clearing its error).
    /// A no-op for any other status.
    ///
    /// A failed download keeps its partial files, its in-memory request
    /// headers (e.g. cookies handed over by the extension), and its speed
    /// limiter, so the retry resumes from the bytes already on disk with
    /// the same auth -- the normal segment-resume logic picks them up.
    /// (Headers are memory-only by design, so after an app restart a
    /// retried download goes out without them.)
    pub fn retry(&self, id: &str) {
        self.requeue_from(id, DownloadStatus::Failed);
    }

    fn requeue_from(&self, id: &str, from: DownloadStatus) {
        let requeued = self.inner.mutate(|downloads| {
            match downloads.iter_mut().find(|d| d.id == id) {
                Some(d) if d.status == from => {
                    d.status = DownloadStatus::Queued;
                    d.error = None;
                    (true, true)
                }
                _ => (false, false),
            }
        });

        if requeued {
            self.inner.schedule();
        }
    }

    /// Cancels an entry, but only if it's actually cancelable: `Queued`,
    /// `Downloading`, or `Paused`. A no-op for an entry that's already
    /// `Completed`, `Failed`, or `Canceled`.
    ///
    /// Only ever deletes files this download itself wrote and still owns
    /// -- its staging file and `.partN` segment files -- never anything
    /// at `dest_path` (which, for a not-yet-finished download, belongs to
    /// something else if it exists at all).
    ///
    /// If a run is still alive for this entry, it is signaled to stop and
    /// performs that cleanup itself once it has actually stopped writing
    /// (it observes `Canceled` when it records its outcome); otherwise the
    /// cleanup happens here, immediately.
    pub fn cancel(&self, id: &str) {
        let outcome = self.inner.mutate(|downloads| {
            let found = downloads.iter_mut().find(|d| d.id == id).and_then(|d| match d.status {
                DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::Canceled => {
                    None
                }
                _ => {
                    d.status = DownloadStatus::Canceled;
                    Some(d.dest_path.clone())
                }
            });
            match found {
                Some(dest_path) => {
                    let run_alive = self.inner.running.lock().unwrap().contains(id);
                    (Some((dest_path, run_alive)), true)
                }
                None => (None, false),
            }
        });

        let Some((dest_path, run_alive)) = outcome else {
            return;
        };

        self.inner.forget_side_channels(id);

        if let Some(token) = self.inner.cancel_tokens.lock().unwrap().get(id) {
            token.cancel();
        }

        if !run_alive {
            Inner::cleanup_staging_files(&dest_path, id);
        }
    }

    /// Removes an entry from the queue and cleans up any staging/partial
    /// files it owns. **Never deletes a `Completed` entry's finished
    /// file** -- see [`Self::remove_and_delete_file`] for that.
    pub fn remove(&self, id: &str) {
        self.remove_impl(id, false);
    }

    /// Same as [`Self::remove`], but if the entry is `Completed`, also
    /// deletes its finished file from disk. Meant to be called only after
    /// the user has explicitly confirmed that deletion. For any other
    /// status this is identical to `remove` (there is no finished file to
    /// delete, and nothing at `dest_path` belongs to this download).
    pub fn remove_and_delete_file(&self, id: &str) {
        self.remove_impl(id, true);
    }

    fn remove_impl(&self, id: &str, delete_completed_file: bool) {
        let removed = self.inner.mutate(|downloads| {
            match downloads.iter().position(|d| d.id == id) {
                Some(index) => {
                    let info = downloads.remove(index);
                    let run_alive = self.inner.running.lock().unwrap().contains(id);
                    (Some((info, run_alive)), true)
                }
                None => (None, false),
            }
        });

        self.inner.forget_side_channels(id);
        if let Some(token) = self.inner.cancel_tokens.lock().unwrap().get(id) {
            token.cancel();
        }

        let Some((info, run_alive)) = removed else {
            return;
        };

        if !run_alive {
            // Nothing is writing to these any more (a live run would have
            // kept `id` in `running` until it recorded its outcome, which
            // it can no longer do for a removed entry without cleaning up
            // after itself).
            Inner::cleanup_staging_files(&info.dest_path, id);
        }

        // `Completed` is only ever set by a run in the same critical
        // section that removes it from `running`, so a `Completed` entry
        // never has a live run racing this deletion.
        if delete_completed_file && info.status == DownloadStatus::Completed {
            let _ = std::fs::remove_file(&info.dest_path);
        }
    }

    /// Returns a snapshot of every tracked download.
    pub fn list(&self) -> Vec<DownloadInfo> {
        self.inner.downloads.lock().unwrap().clone()
    }

    /// Sets (or updates) a per-download speed limit. Works whether or not
    /// the download has started yet: a limiter is created up front if
    /// needed, and picked up once the download actually runs. `None` or
    /// `Some(0)` means unlimited.
    pub fn set_speed_limit(&self, id: &str, bytes_per_sec: Option<u64>) {
        let mut limiters = self.inner.limiters.lock().unwrap();
        match limiters.get(id) {
            Some(limiter) => limiter.set_limit(bytes_per_sec),
            None => {
                limiters.insert(id.to_string(), Arc::new(SpeedLimiter::new(bytes_per_sec)));
            }
        }
    }

    /// Sets the speed limit applied to every download added from now on
    /// (whether via the UI or the handoff server), and to any already
    /// queued download that starts without a limiter of its own. Does not
    /// change downloads that already have a limiter. `None` or `Some(0)`
    /// means unlimited.
    pub fn set_default_speed_limit(&self, bytes_per_sec: Option<u64>) {
        *self.inner.default_speed_limit.lock().unwrap() = normalize_limit(bytes_per_sec);
    }

    /// Changes where a newly `add()`-ed download's file lands, effective
    /// immediately -- the Settings panel's download-folder field used to
    /// need an app restart to take effect (`add()` captured `download_dir`
    /// once, at construction time); now every `add()` call reads whatever
    /// was last set here. A download already `Queued`/`Downloading` when
    /// this is called keeps writing to the folder it was added under --
    /// its `dest_path` was fixed at `add()` time and is never recomputed.
    pub fn set_download_dir(&self, dir: PathBuf) {
        *self.inner.download_dir.lock().unwrap() = dir;
    }

    /// Test-support hook: whether any in-memory-only side channel (request
    /// headers or speed limiter) is still held for `id`.
    #[doc(hidden)]
    pub fn has_side_channel_state(&self, id: &str) -> bool {
        self.inner.headers.lock().unwrap().contains_key(id)
            || self.inner.limiters.lock().unwrap().contains_key(id)
    }

    /// Test-support hook: how many times the state file has been written
    /// since this manager was constructed. Used to prove progress updates
    /// are not persisted per chunk.
    #[doc(hidden)]
    pub fn persist_count(&self) -> u64 {
        self.inner.persist_count.load(Ordering::SeqCst)
    }
}

/// Everything a spawned `run()` needs about its entry, captured by
/// `schedule()` at spawn time -- so a run can still clean up after itself
/// even if its entry has since been removed from the list.
struct RunSpec {
    id: String,
    url: String,
    dest_path: PathBuf,
}

/// Owned by a live `run()` task. On a normal exit, dropping it releases
/// the run's semaphore permit and re-triggers `schedule()` (so the queue
/// keeps moving, and a requeued entry waiting on this run to exit gets
/// started).
///
/// If the run never reached `Inner::resolve` -- only possible if the task
/// panicked, or was dropped mid-flight because the runtime is shutting
/// down -- it instead releases the id from `running`/`cancel_tokens`
/// itself (so it can never leak), marks a still-`Downloading` entry
/// `Failed` if (and only if) this is a panic (so the user can retry it),
/// and does *not* re-trigger `schedule()`: during a shutdown that would
/// just spawn more tasks for the dying runtime to drop.
struct RunGuard {
    inner: Arc<Inner>,
    id: String,
    permit: Option<OwnedSemaphorePermit>,
    resolved: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.resolved {
            drop(self.permit.take());
            self.inner.schedule();
            return;
        }

        // Poison-tolerant: we may be unwinding from a panic that happened
        // while one of these locks was held.
        if std::thread::panicking()
            && let Some(d) = self
                .inner
                .downloads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter_mut()
                .find(|d| d.id == self.id && d.status == DownloadStatus::Downloading)
        {
            d.status = DownloadStatus::Failed;
            d.error = Some("download task stopped unexpectedly".to_string());
        }
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        self.inner
            .cancel_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        drop(self.permit.take());
    }
}

impl Inner {
    /// Runs `f` on the download list and, if it reports a change, writes
    /// the resulting list to the state file -- all while holding
    /// `persist_lock`, so writes are serialized and always land in the
    /// same order as the snapshots they write.
    ///
    /// `f` returns `(result, changed)`. The `downloads` lock is held only
    /// while `f` runs and the snapshot is cloned (once), never during the
    /// file I/O, so `list()` and progress updates are never blocked on
    /// disk. `f` may take the `running`/`cancel_tokens` locks (they come
    /// after `downloads` in the lock order) but must not call back into
    /// anything that takes `persist_lock` or `downloads`.
    ///
    /// Never call this while holding `downloads`.
    fn mutate<R>(&self, f: impl FnOnce(&mut Vec<DownloadInfo>) -> (R, bool)) -> R {
        let _persist = self.persist_lock.lock().unwrap();
        let (result, snapshot) = {
            let mut downloads = self.downloads.lock().unwrap();
            let (result, changed) = f(&mut downloads);
            (result, changed.then(|| downloads.clone()))
        };
        if let Some(snapshot) = snapshot {
            self.persist_count.fetch_add(1, Ordering::SeqCst);
            if let Err(e) = save_state(&self.state_file, &snapshot) {
                // Persistence failing is unfortunate but must never take
                // the queue down with it -- log and carry on.
                eprintln!("queue manager: failed to save state: {e}");
            }
        }
        result
    }

    /// Updates an entry in memory only -- for high-frequency, non-status
    /// fields (progress, total size). These reach disk with the next
    /// status change; on a crash the persisted progress may lag, but
    /// resuming always recomputes real progress from the part files.
    fn update_in_memory(&self, id: &str, f: impl FnOnce(&mut DownloadInfo)) {
        let mut downloads = self.downloads.lock().unwrap();
        if let Some(d) = downloads.iter_mut().find(|d| d.id == id) {
            f(d);
        }
    }

    /// Removes `id`'s in-memory-only side channels (request headers and
    /// speed limiter). Called when a download's lifetime definitively ends
    /// (`Completed`, `Canceled`, removed) so cookies/auth headers don't
    /// linger in process memory. Deliberately *not* called on `Failed`:
    /// a failed download can be retried, and needs the same headers.
    fn forget_side_channels(&self, id: &str) {
        self.headers.lock().unwrap().remove(id);
        self.limiters.lock().unwrap().remove(id);
    }

    /// Returns the path a download identified by `id` actually writes to
    /// while it's in progress -- deliberately distinct from (but
    /// deterministically derived from) `dest_path` and `id`, so two
    /// downloads can never collide on the same `.partN` files or
    /// single-stream file. Only once a download fully completes is this
    /// staging file committed to the real `dest_path` (see
    /// `commit_staging`).
    ///
    /// Deterministic in `id` (not random) so that reloading persisted
    /// state and resuming an interrupted download recomputes the exact
    /// same staging path its earlier run was using, letting
    /// `resume_offset` find the bytes it already wrote. Keying by id alone
    /// (not by run) is safe because at most one run per id is ever alive.
    fn staging_path(dest_path: &Path, id: &str) -> PathBuf {
        let mut os_string = dest_path.as_os_str().to_os_string();
        os_string.push(format!(".{id}.part"));
        PathBuf::from(os_string)
    }

    /// Best-effort removal of the files a download writes *before* it
    /// owns `dest_path`: its staging file (single-stream downloads) and
    /// its `staging.part0..N` segment files. Never touches `dest_path`
    /// itself. Errors (including "doesn't exist") are ignored.
    fn cleanup_staging_files(dest_path: &Path, id: &str) {
        let staging = Self::staging_path(dest_path, id);
        let _ = std::fs::remove_file(&staging);
        for index in 0..DEFAULT_SEGMENT_COUNT {
            let _ = std::fs::remove_file(part_path(&staging, index));
        }
    }

    /// Looks for permits + queued work and spawns a run for each match it
    /// can make right now. Safe to call from anywhere, any number of times:
    /// it only ever acts on entries it atomically flips from `Queued` to
    /// `Downloading` itself, and skips any `Queued` entry whose previous
    /// run is still alive (still in `running`) -- that run's exit calls
    /// `schedule()` again.
    ///
    /// The `running` insertion and `CancelToken` registration happen in the
    /// same `downloads`-lock critical section as the status flip, so any
    /// `pause`/`cancel`/`remove` that observes `Downloading` is guaranteed
    /// to also find the token and the `running` entry in place.
    fn schedule(self: &Arc<Self>) {
        let Some(handle) = self.runtime.clone() else {
            return;
        };

        loop {
            let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => return,
            };

            let scheduled = self.mutate(|downloads| {
                let mut running = self.running.lock().unwrap();
                let entry = downloads
                    .iter_mut()
                    .find(|d| d.status == DownloadStatus::Queued && !running.contains(&d.id));
                match entry {
                    Some(d) => {
                        d.status = DownloadStatus::Downloading;
                        running.insert(d.id.clone());
                        let cancel = CancelToken::new();
                        self.cancel_tokens
                            .lock()
                            .unwrap()
                            .insert(d.id.clone(), cancel.clone());
                        let spec = RunSpec {
                            id: d.id.clone(),
                            url: d.url.clone(),
                            dest_path: d.dest_path.clone(),
                        };
                        (Some((spec, cancel)), true)
                    }
                    None => (None, false),
                }
            });

            match scheduled {
                Some((spec, cancel)) => {
                    let guard = RunGuard {
                        inner: Arc::clone(self),
                        id: spec.id.clone(),
                        permit: Some(permit),
                        resolved: false,
                    };
                    let inner = Arc::clone(self);
                    handle.spawn(async move {
                        inner.run(spec, cancel, guard).await;
                    });
                }
                None => {
                    // Nothing to run right now; give the permit back.
                    drop(permit);
                    return;
                }
            }
        }
    }

    /// Runs one download end to end -- probe and `download_segmented` into
    /// the staging path -- then records the outcome via `resolve` (which
    /// also commits a finished file, if still wanted) and acts on it.
    /// Dropping `guard` at the end releases the permit and re-triggers
    /// `schedule()`.
    async fn run(self: Arc<Self>, spec: RunSpec, cancel: CancelToken, mut guard: RunGuard) {
        let RunSpec { id, url, dest_path } = spec;

        // Paused/canceled/removed between `schedule()` and this task
        // actually starting: don't download anything, just resolve as a
        // stopped run (which cleans up if it was canceled/removed).
        let still_wanted = self
            .downloads
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.id == id && d.status == DownloadStatus::Downloading);

        let outcome = if still_wanted && !cancel.is_canceled() {
            let headers = self
                .headers
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .unwrap_or_default();

            let limiter = {
                let default_limit = *self.default_speed_limit.lock().unwrap();
                let mut limiters = self.limiters.lock().unwrap();
                Arc::clone(
                    limiters
                        .entry(id.clone())
                        .or_insert_with(|| Arc::new(SpeedLimiter::new(default_limit))),
                )
            };

            let req = DownloadRequest {
                url,
                filename: None,
                headers,
            };

            self.run_download(&req, &dest_path, cancel, limiter, &id)
                .await
        } else {
            Err(DownloadError::Canceled)
        };

        let disposition = self.resolve(&id, &outcome);
        guard.resolved = true;

        match disposition {
            RunDisposition::Keep => {
                self.forget_side_channels(&id);
                // `commit_staging` already unlinked the staging name after
                // hard-linking it into place; this only matters if that
                // unlink failed (e.g. an antivirus scanner briefly holding
                // the file on Windows). Never touches the committed file.
                Self::cleanup_staging_files(&dest_path, &id);
            }
            RunDisposition::LeaveAsIs => {}
            // Only this download's own staging/part files: nothing was
            // ever committed to `dest_path` for a discarded run.
            RunDisposition::Discard => Self::cleanup_staging_files(&dest_path, &id),
        }

        drop(guard);
    }

    /// Records a finished run's outcome against the entry's authoritative
    /// current status -- committing a finished staging file into place if
    /// (and only if) the entry still wants it -- and releases the id from
    /// `running`/`cancel_tokens`, all in one `downloads`-lock critical
    /// section. Because `cancel`/`remove` check `running` under that same
    /// lock, they always agree with this run about who does the file
    /// cleanup, and a canceled/removed download can never end up with a
    /// committed file.
    ///
    /// `outcome` is `Ok(staging)` when the whole file sits finished in its
    /// staging path.
    ///
    /// | outcome          | status now          | result                                         |
    /// |------------------|---------------------|------------------------------------------------|
    /// | `Ok(staging)`    | Downloading/Queued  | commit -> `Completed` (`Keep`); commit error -> `Failed` |
    /// | `Ok(staging)`    | Paused              | `LeaveAsIs`: finished staging file kept; resume commits it without re-downloading |
    /// | `Err(other)`     | Downloading         | `Failed` -> `LeaveAsIs` (partials kept, retryable) |
    /// | `Err(_)`         | Paused/Queued       | `LeaveAsIs` (partials kept for the resume)      |
    /// | any              | Canceled / removed  | `Discard` (staging/parts only, never `dest_path`) |
    ///
    /// `Queued` here means the entry was paused and then resumed while this
    /// run was still wrapping up: the user wants it downloaded, and it is.
    fn resolve(&self, id: &str, outcome: &Result<PathBuf, DownloadError>) -> RunDisposition {
        self.mutate(|downloads| {
            self.running.lock().unwrap().remove(id);
            self.cancel_tokens.lock().unwrap().remove(id);

            let Some(d) = downloads.iter_mut().find(|d| d.id == id) else {
                return (RunDisposition::Discard, false);
            };

            use DownloadStatus::*;
            match (outcome, d.status) {
                (_, Canceled) => (RunDisposition::Discard, false),
                (Ok(staging), Downloading | Queued) => match commit_staging(staging, &d.dest_path)
                {
                    Ok(committed) => {
                        let len = std::fs::metadata(&committed).ok().map(|m| m.len());
                        d.status = Completed;
                        d.error = None;
                        if let Some(name) = committed.file_name() {
                            d.filename = name.to_string_lossy().into_owned();
                        }
                        d.dest_path = committed;
                        if let Some(len) = len {
                            d.downloaded_bytes = len;
                            d.total_bytes = Some(len);
                        }
                        (RunDisposition::Keep, true)
                    }
                    Err(e) => {
                        d.status = Failed;
                        d.error = Some(e.to_string());
                        (RunDisposition::LeaveAsIs, true)
                    }
                },
                (Err(DownloadError::Canceled), Downloading) => {
                    // Nothing should cancel a run without first moving its
                    // entry out of `Downloading`; don't leave it stuck.
                    d.status = Failed;
                    d.error = Some(DownloadError::Canceled.to_string());
                    (RunDisposition::LeaveAsIs, true)
                }
                (Err(e), Downloading) => {
                    d.status = Failed;
                    d.error = Some(e.to_string());
                    (RunDisposition::LeaveAsIs, true)
                }
                // Paused (finished or not) / Queued after a stopped or
                // failed run: keep everything for the resume. Completed/
                // Failed can't be observed while this run was alive.
                _ => (RunDisposition::LeaveAsIs, false),
            }
        })
    }

    /// Probes `req.url` and runs `download_segmented` into a per-download
    /// staging path (see `staging_path`), wiring its progress callback
    /// back into this entry's in-memory `DownloadInfo`. Returns the
    /// staging path once it holds the whole file; committing it to
    /// `dest_path` is `resolve`'s job.
    async fn run_download(
        self: &Arc<Self>,
        req: &DownloadRequest,
        dest_path: &Path,
        cancel: CancelToken,
        limiter: Arc<SpeedLimiter>,
        id: &str,
    ) -> Result<PathBuf, DownloadError> {
        if let Some(parent) = dest_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| DownloadError::Io(e.to_string()))?;
        }

        let staging = Self::staging_path(dest_path, id);

        // A failed probe (after retrying transient failures) must not be
        // treated as fatal on its own: e.g. a server that doesn't support
        // HEAD and isn't caught by `probe`'s own narrow 405/501 fallback,
        // or one that 404s on HEAD only. Try a ranged GET ourselves as a
        // second-chance probe before giving up to "unknown size, no range
        // support"; a genuinely unreachable/broken server still ends up
        // `Failed` from the real download attempt (and its retries).
        let probed = retry_with_backoff_cancelable(
            PROBE_RETRY_MAX_ATTEMPTS,
            PROBE_RETRY_BASE_DELAY,
            &cancel,
            || probe(&self.client, &req.url, &req.headers),
        )
        .await;
        let probe_info = match probed {
            Ok(info) => info,
            Err(DownloadError::Canceled) => return Err(DownloadError::Canceled),
            Err(_) => probe_via_ranged_get_fallback(&self.client, &req.url, &req.headers).await,
        };
        self.update_in_memory(id, |d| d.total_bytes = probe_info.total_bytes);

        // A previous run already finished this download into `staging`
        // but was paused before committing it (see `resolve`). With no
        // part files left (segmented downloads only delete them after a
        // complete merge) and the exact expected size, it's whole.
        if let Some(total) = probe_info.total_bytes {
            let staging_len = std::fs::metadata(&staging).ok().map(|m| m.len());
            let parts_left =
                (0..DEFAULT_SEGMENT_COUNT).any(|index| part_path(&staging, index).exists());
            if staging_len == Some(total) && !parts_left {
                self.update_in_memory(id, |d| d.downloaded_bytes = total);
                return Ok(staging);
            }
        }

        let id_for_progress = id.to_string();
        let inner_for_progress = Arc::clone(self);
        let on_progress = move |written: u64| {
            inner_for_progress.update_in_memory(&id_for_progress, |d| {
                d.downloaded_bytes = written;
            });
        };

        download_segmented(
            &self.client,
            req,
            &probe_info,
            &staging,
            DEFAULT_SEGMENT_COUNT,
            cancel,
            limiter,
            on_progress,
        )
        .await?;

        Ok(staging)
    }
}

/// Moves a finished `staging` file into place at `dest`, **never replacing
/// an existing file**. If something already exists at `dest` (another
/// program created it after `add()` picked the name), the next free
/// ` (N)` sibling is used instead. Returns the path actually written.
///
/// Uses a hard link + unlink of the staging name, which atomically fails
/// with `AlreadyExists` if the target exists -- unlike `rename`, which on
/// Windows silently replaces it. On filesystems without hard-link support
/// (FAT32/exFAT drives, some network shares) it falls back to "check
/// existence, then rename", which leaves a narrow, unavoidable TOCTOU
/// window between the check and the rename.
fn commit_staging(staging: &Path, dest: &Path) -> Result<PathBuf, DownloadError> {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = dest.parent().unwrap_or_else(|| Path::new(""));

    for n in 0..=MAX_UNIQUE_SUFFIX {
        let candidate = if n == 0 {
            dest.to_path_buf()
        } else {
            parent.join(numbered_name(&file_name, n))
        };

        match std::fs::hard_link(staging, &candidate) {
            Ok(()) => {
                let _ = std::fs::remove_file(staging);
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                if path_exists(&candidate) {
                    continue;
                }
                std::fs::rename(staging, &candidate)
                    .map_err(|e| DownloadError::Io(e.to_string()))?;
                return Ok(candidate);
            }
        }
    }

    Err(DownloadError::Io(format!(
        "no free filename found for {}",
        dest.display()
    )))
}

/// Whether anything (file, directory, even a dangling symlink) exists at
/// `path`.
fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `name (n).ext` for `n >= 1` -- the browser-style duplicate name.
fn numbered_name(file_name: &str, n: u32) -> String {
    let path = Path::new(file_name);
    match (path.file_stem(), path.extension()) {
        (Some(stem), Some(ext)) => format!(
            "{} ({n}).{}",
            stem.to_string_lossy(),
            ext.to_string_lossy()
        ),
        _ => format!("{file_name} ({n})"),
    }
}

/// Picks `requested`, or the first free `requested (N)` variant: free
/// means nothing exists at that path in `dir`, and no non-canceled entry in
/// `downloads` is already headed for it (compared case-insensitively, as
/// Windows filenames are).
fn unique_filename(dir: &Path, requested: &str, downloads: &[DownloadInfo]) -> String {
    let taken_by_queue = |path: &Path| {
        let wanted = path.to_string_lossy().to_lowercase();
        downloads.iter().any(|d| {
            d.status != DownloadStatus::Canceled
                && d.dest_path.to_string_lossy().to_lowercase() == wanted
        })
    };

    for n in 0..=MAX_UNIQUE_SUFFIX {
        let candidate = if n == 0 {
            requested.to_string()
        } else {
            numbered_name(requested, n)
        };
        let path = dir.join(&candidate);
        if !path_exists(&path) && !taken_by_queue(&path) {
            return candidate;
        }
    }

    // Practically unreachable; a uuid-prefixed name can't collide.
    format!("{}-{requested}", Uuid::new_v4())
}

/// Reduces an untrusted filename (from the UI, the handoff server, or a
/// URL) to a single, safe path component, or `None` if nothing usable is
/// left:
///
/// - only the last component survives (anything before a `/` or `\` is
///   dropped), so `../../x`, `C:\Users\...\Startup\x.exe` or `/etc/x` can
///   never escape the download folder;
/// - characters Windows forbids in filenames (`< > : " | ? *`, which also
///   covers drive-relative forms like `C:x`) and control characters become
///   `_`;
/// - leading/trailing whitespace and trailing dots are trimmed (Windows
///   silently strips trailing dots/spaces, and `.`/`..` become empty);
/// - reserved device names (`CON`, `NUL`, `COM1`, ...) get a `_` prefix.
///
/// Sanitizing rather than rejecting outright keeps a request with a
/// merely odd name working; an empty result makes the caller fall back to
/// the URL-derived name.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let replaced: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced.trim().trim_end_matches(['.', ' ']).to_string();
    if trimmed.is_empty() {
        return None;
    }

    let base = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    if RESERVED_WINDOWS_NAMES.contains(&base.as_str()) {
        return Some(format!("_{trimmed}"));
    }

    Some(trimmed)
}

/// A second-chance probe used only after `engine::probe` has already
/// failed: issues a `Range: bytes=0-0` GET directly and reads whatever it
/// can from the response, rather than giving up immediately on a server
/// whose `HEAD` handling doesn't fit `probe`'s own (deliberately narrow)
/// HEAD-rejection fallback. Any failure here (network error, non-success,
/// non-range status) just yields "unknown size, no range support", which
/// is exactly what the unprobed path already falls back to.
async fn probe_via_ranged_get_fallback(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> ProbeInfo {
    let unknown = ProbeInfo {
        total_bytes: None,
        supports_range: false,
    };

    let mut builder = client.get(url).header("Range", "bytes=0-0");
    for (key, value) in headers {
        builder = builder.header(key, value);
    }

    let Ok(response) = builder.send().await else {
        return unknown;
    };

    let status = response.status();
    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return unknown;
    }

    let supports_range = status == reqwest::StatusCode::PARTIAL_CONTENT
        || response
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);

    let total_bytes = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit('/').next()?.trim().parse::<u64>().ok())
        .or_else(|| {
            response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        });

    ProbeInfo {
        total_bytes,
        supports_range,
    }
}

/// Derives a filename from a URL's last path segment, falling back to
/// `fallback_id` if the URL can't be parsed or has no usable last segment
/// (e.g. it ends in `/`).
fn derive_filename(url: &str, fallback_id: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.next_back().map(|s| s.to_string()))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_id.to_string())
}
