use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A single download request: where to fetch it from, what to call the
/// resulting file, and any extra headers to send with every request
/// (auth tokens, cookies, etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    pub url: String,
    pub filename: Option<String>,
    pub headers: HashMap<String, String>,
}

/// Lifecycle status of a download, as tracked by the queue manager.
///
/// `Serialize`/`Deserialize` are needed because this is embedded in
/// [`DownloadInfo`], which is persisted to the queue's state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
    Canceled,
}

/// What we learned about a remote resource before downloading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeInfo {
    pub total_bytes: Option<u64>,
    pub supports_range: bool,
}

/// A clone-able, thread-safe flag used to request cancellation of an
/// in-progress download. Cloning shares the same underlying flag, so
/// canceling any clone cancels all of them.
///
/// A token can also have *child* tokens (see [`CancelToken::child`]): a
/// child reports canceled whenever its parent (or any ancestor) is, but
/// canceling the child does not cancel the parent. The engine uses this to
/// stop its own sibling segment tasks without signaling the caller's token.
#[derive(Debug, Clone)]
pub struct CancelToken {
    /// `flags[0]` is this token's own flag; the rest are its ancestors'.
    flags: Vec<Arc<AtomicBool>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flags: vec![Arc::new(AtomicBool::new(false))],
        }
    }

    /// A new token that is canceled whenever `self` is, but that can also
    /// be canceled on its own without affecting `self`.
    pub fn child(&self) -> Self {
        let mut flags = Vec::with_capacity(self.flags.len() + 1);
        flags.push(Arc::new(AtomicBool::new(false)));
        flags.extend(self.flags.iter().cloned());
        Self { flags }
    }

    pub fn cancel(&self) {
        self.flags[0].store(true, Ordering::SeqCst);
    }

    pub fn is_canceled(&self) -> bool {
        self.flags.iter().any(|flag| flag.load(Ordering::SeqCst))
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors surfaced by the download engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    Network(String),
    Io(String),
    Canceled,
    ServerError(u16),
    /// A segmented (range) request came back as something other than a
    /// `206 Partial Content` for the exact range asked for -- e.g. a server
    /// that advertises `Accept-Ranges: bytes` but then ignores the `Range`
    /// header and answers `200` with the whole body. Internal to the
    /// engine: `download_segmented` reacts to it by falling back to a
    /// single-stream download rather than surfacing it.
    RangeNotHonored,
    /// A finished segment's part file doesn't have exactly the number of
    /// bytes its planned range covers, so merging it would produce a
    /// corrupt file. The offending part file is deleted (so a retry
    /// re-fetches that segment from scratch) and no merged output is
    /// written.
    SegmentSizeMismatch {
        index: usize,
        expected: u64,
        actual: u64,
    },
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Network(msg) => write!(f, "network error: {msg}"),
            DownloadError::Io(msg) => write!(f, "io error: {msg}"),
            DownloadError::Canceled => write!(f, "download canceled"),
            DownloadError::ServerError(code) => write!(f, "server returned status {code}"),
            DownloadError::RangeNotHonored => {
                write!(f, "server did not honor a range request")
            }
            DownloadError::SegmentSizeMismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "segment {index} has {actual} bytes on disk, expected {expected}; refusing to merge a corrupt file"
            ),
        }
    }
}

impl std::error::Error for DownloadError {}

/// The persisted record of one download tracked by the queue manager.
///
/// Deliberately excludes the originating [`DownloadRequest`]'s `headers`
/// (auth tokens, cookies, etc): those must only ever live in memory for a
/// download's lifetime and must never be written to the state file (or any
/// log). The queue manager carries them through a separate, non-persisted
/// side channel instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadInfo {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub dest_path: PathBuf,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub status: DownloadStatus,
    pub error: Option<String>,
    /// Unix seconds when this download was first added to the queue; absent
    /// (defaults to `0`) in state files persisted before this field existed.
    #[serde(default)]
    pub created_at: i64,
}
