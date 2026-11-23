use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};
use tokio::io::AsyncWriteExt;

use super::limiter::SpeedLimiter;
use super::retry::{canceled, retry_with_backoff_cancelable};
use super::segments::{part_path, plan_segments, resume_offset};
use super::types::{CancelToken, DownloadError, DownloadRequest, ProbeInfo};

/// Attempts for a single segment's fetch (or for the whole single-stream
/// fallback download), including the first try.
///
/// 5 attempts with a 1s base delay doubling each time means retries wait
/// 1s, 2s, 4s, 8s: ~15s of total backoff, enough to ride out a brief Wi-Fi
/// drop / router hiccup / server restart, while a server that is
/// genuinely gone still fails the download in well under half a minute.
/// Only transient failures are retried at all (see
/// [`super::retry::is_retryable`]): a `403`/`404` or a local I/O error
/// fails immediately. Each retry of a segment resumes from whatever its
/// part file already holds, so retries never re-download finished bytes.
pub const DOWNLOAD_RETRY_MAX_ATTEMPTS: u32 = 5;

/// Delay before the first retry of a failed segment/single-stream fetch;
/// doubles after each subsequent failed attempt.
pub const DOWNLOAD_RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Attempts for the up-front probe. Shorter than the download budget: a
/// failed probe is not fatal (the queue falls back to a ranged-GET probe
/// and then to an unknown-size single-stream download, which has its own
/// retries), so there's no point waiting long here.
pub const PROBE_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Delay before the first probe retry; doubles after each failed attempt.
pub const PROBE_RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Learns the size and range-support of a remote resource.
///
/// Issues a `HEAD` request first. Some servers reject `HEAD` (405/501) or
/// simply don't answer it usefully, so if that happens we fall back to a
/// ranged `GET` (`Range: bytes=0-0`) which most servers honor even when
/// they don't implement `HEAD`.
///
/// Makes a single attempt; callers that want retries wrap it (see
/// `QueueManager`'s use of [`PROBE_RETRY_MAX_ATTEMPTS`]).
pub async fn probe(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<ProbeInfo, DownloadError> {
    let mut builder = client.head(url);
    builder = apply_headers(builder, headers);

    let response = builder
        .send()
        .await
        .map_err(|e| DownloadError::Network(e.to_string()))?;

    if response.status().is_success() {
        return Ok(probe_info_from_response(&response));
    }

    if is_head_rejection(response.status()) {
        return probe_via_ranged_get(client, url, headers).await;
    }

    Err(DownloadError::ServerError(response.status().as_u16()))
}

/// Whether a HEAD response indicates the *method* is unsupported (so we
/// should retry the probe via a ranged GET), as opposed to the resource
/// itself being unavailable or restricted. `403 Forbidden` is deliberately
/// excluded: it signals the resource is access-restricted, not that HEAD
/// specifically is unsupported, so a HEAD-403 should surface directly as
/// `DownloadError::ServerError(403)` rather than trigger a second
/// (likely also-403) GET round trip.
fn is_head_rejection(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    )
}

async fn probe_via_ranged_get(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<ProbeInfo, DownloadError> {
    let mut builder = client.get(url).header("Range", "bytes=0-0");
    builder = apply_headers(builder, headers);

    let response = builder
        .send()
        .await
        .map_err(|e| DownloadError::Network(e.to_string()))?;

    if response.status().is_success() || response.status() == StatusCode::PARTIAL_CONTENT {
        Ok(probe_info_from_response(&response))
    } else {
        Err(DownloadError::ServerError(response.status().as_u16()))
    }
}

fn probe_info_from_response(response: &reqwest::Response) -> ProbeInfo {
    let supports_range = response
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false)
        || response.status() == StatusCode::PARTIAL_CONTENT;

    let total_bytes = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(total_from_content_range)
        .or_else(|| {
            response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        });

    ProbeInfo {
        total_bytes,
        supports_range,
    }
}

/// Parses the start offset out of a `Content-Range: bytes 100-199/12345`
/// header.
fn start_from_content_range(value: &str) -> Option<u64> {
    value
        .trim()
        .strip_prefix("bytes")?
        .trim()
        .split('-')
        .next()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Parses the total size out of a `Content-Range: bytes 0-0/12345` header.
fn total_from_content_range(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse::<u64>().ok()
}

fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    for (key, value) in headers {
        builder = builder.header(key, value);
    }
    builder
}

/// Waits for `limiter` to admit `bytes`, but gives up with
/// `Err(Canceled)` as soon as `cancel` is signaled -- both while waiting
/// (a low limit can make `acquire` sleep for seconds) and right after it
/// returns, so the caller never writes a chunk after a pause/cancel.
async fn acquire_unless_canceled(
    limiter: &SpeedLimiter,
    bytes: u64,
    cancel: &CancelToken,
) -> Result<(), DownloadError> {
    tokio::select! {
        _ = limiter.acquire(bytes) => {}
        _ = canceled(cancel) => return Err(DownloadError::Canceled),
    }
    if cancel.is_canceled() {
        return Err(DownloadError::Canceled);
    }
    Ok(())
}

/// Downloads the full body of `req.url` to `dest`, calling `on_progress`
/// with the cumulative number of bytes written after each chunk.
///
/// Cancellation is checked before each chunk is read from the stream; if
/// `cancel` has been signaled, the function stops and returns
/// `DownloadError::Canceled` without completing the file.
///
/// `limiter.acquire(chunk_len)` is awaited before each chunk is written,
/// so the download's throughput is bounded by `limiter`'s configured rate
/// (no delay when it's unlimited). That wait is itself abandoned as soon
/// as `cancel` is signaled, and `cancel` is re-checked after it, so a
/// chunk is never written after a pause/cancel even if the limiter made
/// this call wait for seconds first.
///
/// Makes exactly one attempt; [`download_segmented`]'s fallback path wraps
/// this in a retry loop.
pub async fn download_single_stream(
    client: &reqwest::Client,
    req: &DownloadRequest,
    dest: &Path,
    cancel: CancelToken,
    limiter: Arc<SpeedLimiter>,
    on_progress: impl Fn(u64) + Send + 'static,
) -> Result<(), DownloadError> {
    let mut builder = client.get(&req.url);
    builder = apply_headers(builder, &req.headers);

    let response = builder
        .send()
        .await
        .map_err(|e| DownloadError::Network(e.to_string()))?;

    if !response.status().is_success() {
        return Err(DownloadError::ServerError(response.status().as_u16()));
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;

    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;

    loop {
        if cancel.is_canceled() {
            return Err(DownloadError::Canceled);
        }

        match stream.next().await {
            Some(chunk_result) => {
                let chunk = chunk_result.map_err(|e| DownloadError::Network(e.to_string()))?;

                acquire_unless_canceled(&limiter, chunk.len() as u64, &cancel).await?;

                file.write_all(&chunk)
                    .await
                    .map_err(|e| DownloadError::Io(e.to_string()))?;

                written += chunk.len() as u64;
                on_progress(written);
            }
            None => break,
        }
    }

    file.flush()
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;

    Ok(())
}

/// Downloads `req.url` to `dest` as `segment_count` concurrent, resumable
/// range requests, then merges the parts in order.
///
/// If the server doesn't support range requests, or we don't know the
/// total size up front, there is nothing to split, so this falls back to
/// [`download_single_stream`] instead (with retries). The same fallback is
/// taken if the server *claimed* range support but then answers a segment
/// request with anything other than a `206` for the exact range asked for
/// (e.g. `200` with the whole body): any part files written so far are
/// discarded and the whole file is fetched as a single stream instead,
/// rather than writing a full-body response into every part file.
///
/// Each segment streams into its own `<dest>.partN` file (see
/// [`super::segments`]). If a part file already has bytes on disk from a
/// previous, interrupted run, the segment's request resumes from that
/// offset via a `Range` header rather than re-downloading them. Once every
/// segment has finished, each part file's size is checked against its
/// planned range (see `merge_segments`), then the part files are
/// concatenated into `dest` in order and removed.
///
/// `limiter` is shared (via its `Arc`) across every concurrent segment, so
/// its configured rate bounds the download's combined throughput across
/// all segments rather than limiting each one individually.
///
/// `cancel` is only ever *read* here, never signaled: stopping sibling
/// segments after one fails uses a private child token instead.
pub async fn download_segmented(
    client: &reqwest::Client,
    req: &DownloadRequest,
    probe: &ProbeInfo,
    dest: &Path,
    segment_count: usize,
    cancel: CancelToken,
    limiter: Arc<SpeedLimiter>,
    on_progress: impl Fn(u64) + Send + Sync + 'static,
) -> Result<(), DownloadError> {
    let on_progress: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(on_progress);

    let total_bytes = match (probe.supports_range, probe.total_bytes) {
        (true, Some(total)) if total > 0 => total,
        _ => {
            return download_single_stream_with_retry(
                client,
                req,
                dest,
                cancel,
                limiter,
                on_progress,
            )
            .await;
        }
    };

    let ranges = plan_segments(total_bytes, segment_count);

    match download_ranges(client, req, &ranges, dest, &cancel, &limiter, &on_progress).await {
        Err(DownloadError::RangeNotHonored) => {
            // The server said it supports ranges but didn't actually honor
            // one. Whatever any segment wrote is suspect: throw all of it
            // away and fetch the whole thing as one stream instead.
            remove_part_files(dest, ranges.len());
            download_single_stream_with_retry(client, req, dest, cancel, limiter, on_progress)
                .await
        }
        other => other,
    }
}

/// Runs [`download_single_stream`] under the download retry budget (see
/// [`DOWNLOAD_RETRY_MAX_ATTEMPTS`]). Each attempt starts the file over from
/// scratch: without range support there is no way to resume mid-body.
async fn download_single_stream_with_retry(
    client: &reqwest::Client,
    req: &DownloadRequest,
    dest: &Path,
    cancel: CancelToken,
    limiter: Arc<SpeedLimiter>,
    on_progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<(), DownloadError> {
    retry_with_backoff_cancelable(
        DOWNLOAD_RETRY_MAX_ATTEMPTS,
        DOWNLOAD_RETRY_BASE_DELAY,
        &cancel,
        || {
            let on_progress = Arc::clone(&on_progress);
            download_single_stream(
                client,
                req,
                dest,
                cancel.clone(),
                Arc::clone(&limiter),
                move |written| on_progress(written),
            )
        },
    )
    .await
}

/// Best-effort removal of `dest.part0..dest.part(count-1)`.
fn remove_part_files(dest: &Path, count: usize) {
    for index in 0..count {
        let _ = std::fs::remove_file(part_path(dest, index));
    }
}

/// The segmented half of [`download_segmented`]: fetches every range in
/// `ranges` concurrently into its part file, then validates and merges
/// them into `dest`. Returns `Err(RangeNotHonored)` (after stopping every
/// segment) if any segment's response wasn't a proper `206`, so the caller
/// can fall back to a single stream.
async fn download_ranges(
    client: &reqwest::Client,
    req: &DownloadRequest,
    ranges: &[(u64, u64)],
    dest: &Path,
    cancel: &CancelToken,
    limiter: &Arc<SpeedLimiter>,
    on_progress: &Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<(), DownloadError> {
    // Progress already on disk from a previous run counts too, so the
    // first progress callback reflects true cumulative progress rather
    // than resetting to zero.
    let initial_written: u64 = (0..ranges.len())
        .map(|index| resume_offset(&part_path(dest, index)))
        .sum();

    let progress = SegmentProgress {
        written: Arc::new(AtomicU64::new(initial_written)),
        on_progress: Arc::clone(on_progress),
    };

    // Canceled when either the caller's `cancel` is, or when we decide to
    // stop the remaining siblings after one segment fails -- without
    // signaling the caller's own token (which, e.g., the single-stream
    // fallback still needs un-canceled afterwards).
    let stop = cancel.child();

    let mut join_set = tokio::task::JoinSet::new();

    for (index, &range) in ranges.iter().enumerate() {
        let client = client.clone();
        let url = req.url.clone();
        let headers = req.headers.clone();
        let part = part_path(dest, index);
        let stop = stop.clone();
        let progress = progress.clone();
        let limiter = limiter.clone();

        join_set.spawn(async move {
            retry_with_backoff_cancelable(
                DOWNLOAD_RETRY_MAX_ATTEMPTS,
                DOWNLOAD_RETRY_BASE_DELAY,
                &stop,
                || {
                    download_segment(
                        &client,
                        &url,
                        &headers,
                        &part,
                        range,
                        stop.clone(),
                        limiter.clone(),
                        progress.clone(),
                    )
                },
            )
            .await
        });
    }

    // Drain every spawned segment task to completion, whether it finishes
    // normally or gets aborted below. This is what guarantees that once
    // this function returns, no segment task is still running (and so
    // still writing into its `.partN` file): a dropped `JoinHandle` does
    // *not* stop its task, so simply returning on the first error (and
    // leaving the rest of the set unawaited) would leave siblings
    // detached and still downloading/writing in the background.
    let mut first_error: Option<DownloadError> = None;

    while let Some(result) = join_set.join_next().await {
        let outcome = match result {
            Ok(inner) => inner,
            Err(join_err) if join_err.is_cancelled() => {
                // Expected for siblings we just aborted below.
                continue;
            }
            Err(join_err) => Err(DownloadError::Io(join_err.to_string())),
        };

        if let Err(e) = outcome
            && first_error.is_none()
        {
            first_error = Some(e);
            // Stop the rest of the segments as soon as possible: the
            // `stop` token makes any segment still mid-stream (or waiting
            // on the limiter / a retry backoff) notice and exit;
            // `abort_all` additionally force-cancels any segment that's
            // still waiting on its HTTP response (and so hasn't reached a
            // cancellation check yet). Both are needed for a prompt,
            // complete stop.
            stop.cancel();
            join_set.abort_all();
        }
    }

    if let Some(error) = first_error {
        // If the caller canceled, report that rather than whichever
        // sibling happened to notice first with some other error.
        if cancel.is_canceled() {
            return Err(DownloadError::Canceled);
        }
        return Err(error);
    }

    merge_segments(dest, ranges).await
}

/// Shared, cloneable progress-reporting state for concurrently running
/// segment downloads: a running total of bytes written across all
/// segments, and the callback to report it through.
#[derive(Clone)]
struct SegmentProgress {
    written: Arc<AtomicU64>,
    on_progress: Arc<dyn Fn(u64) + Send + Sync>,
}

impl SegmentProgress {
    fn report(&self, newly_written: u64) {
        let total_written = self.written.fetch_add(newly_written, Ordering::SeqCst) + newly_written;
        (self.on_progress)(total_written);
    }
}

/// Downloads a single inclusive `range` (`(start, end)`) segment of `url`
/// into `part`, resuming from `part`'s existing size if any.
///
/// Returns `Ok(())` without making a request if the part file already
/// covers the whole segment. Returns `Err(RangeNotHonored)` -- without
/// writing anything -- unless the server answers with a `206` for the
/// exact start offset requested.
async fn download_segment(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    part: &Path,
    range: (u64, u64),
    cancel: CancelToken,
    limiter: Arc<SpeedLimiter>,
    progress: SegmentProgress,
) -> Result<(), DownloadError> {
    let (range_start, range_end) = range;
    let resume = resume_offset(part);
    let segment_len = range_end - range_start + 1;

    if resume >= segment_len {
        // Already fully downloaded in a previous run. (If it somehow holds
        // *more* than `segment_len` bytes, `merge_segments`'s size check
        // catches that rather than silently merging it.)
        return Ok(());
    }

    let start = range_start + resume;

    let mut builder = client
        .get(url)
        .header("Range", format!("bytes={start}-{range_end}"));
    builder = apply_headers(builder, headers);

    let response = builder
        .send()
        .await
        .map_err(|e| DownloadError::Network(e.to_string()))?;

    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        // A `206` for some *other* range than the one asked for would be
        // just as corrupting as a `200`; check it when the server says.
        let served_start = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(start_from_content_range);
        if served_start.is_some_and(|served| served != start) {
            return Err(DownloadError::RangeNotHonored);
        }
    } else if status.is_success() {
        // Typically `200` with the *entire* body: the server ignored our
        // `Range` header. Writing that into this segment's part file would
        // corrupt the merged result.
        return Err(DownloadError::RangeNotHonored);
    } else {
        return Err(DownloadError::ServerError(status.as_u16()));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;

    let mut stream = response.bytes_stream();

    loop {
        if cancel.is_canceled() {
            return Err(DownloadError::Canceled);
        }

        match stream.next().await {
            Some(chunk_result) => {
                let chunk = chunk_result.map_err(|e| DownloadError::Network(e.to_string()))?;

                acquire_unless_canceled(&limiter, chunk.len() as u64, &cancel).await?;

                file.write_all(&chunk)
                    .await
                    .map_err(|e| DownloadError::Io(e.to_string()))?;

                progress.report(chunk.len() as u64);
            }
            None => break,
        }
    }

    file.flush()
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;

    Ok(())
}

/// Concatenates `dest.part0..dest.part(N-1)` (one per entry in `ranges`)
/// into `dest`, in order, then removes the part files.
///
/// Before writing anything, checks that every part file holds exactly as
/// many bytes as its planned range covers. A part that is short or long
/// (a misbehaving server, an overlapping write, a truncated file) would
/// produce a corrupt, wrong-sized output that still *looks* finished, so
/// instead every mismatched part file is deleted (letting a retry
/// re-fetch just those segments) and `SegmentSizeMismatch` is returned
/// without ever creating `dest`.
async fn merge_segments(dest: &Path, ranges: &[(u64, u64)]) -> Result<(), DownloadError> {
    let mut mismatch: Option<DownloadError> = None;
    for (index, &(start, end)) in ranges.iter().enumerate() {
        let part = part_path(dest, index);
        let expected = end - start + 1;
        let actual = resume_offset(&part);
        if actual != expected {
            let _ = tokio::fs::remove_file(&part).await;
            if mismatch.is_none() {
                mismatch = Some(DownloadError::SegmentSizeMismatch {
                    index,
                    expected,
                    actual,
                });
            }
        }
    }
    if let Some(error) = mismatch {
        return Err(error);
    }

    let mut out = tokio::fs::File::create(dest)
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;

    for index in 0..ranges.len() {
        let part = part_path(dest, index);
        let mut part_file = tokio::fs::File::open(&part)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;

        tokio::io::copy(&mut part_file, &mut out)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
    }

    out.flush()
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;

    for index in 0..ranges.len() {
        let part = part_path(dest, index);
        tokio::fs::remove_file(&part)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
    }

    Ok(())
}
