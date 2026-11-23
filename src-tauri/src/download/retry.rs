use std::future::Future;
use std::time::Duration;

use super::types::{CancelToken, DownloadError};

/// How often a backoff sleep re-checks its `CancelToken`, so a pause/cancel
/// issued while a download is waiting to retry is noticed promptly instead
/// of only after the (possibly multi-second) backoff delay has elapsed.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Whether a failed attempt is worth retrying at all.
///
/// Retried: network-level failures (connection refused/reset, timeouts,
/// a stream that drops mid-body) and server responses that signal a
/// *temporary* condition -- `408 Request Timeout`, `425 Too Early`,
/// `429 Too Many Requests`, and every `5xx`.
///
/// Not retried: `Canceled` (someone wants the operation to stop now),
/// local I/O errors (a full disk or an unwritable path won't fix itself
/// in a few seconds), any other `4xx` (`403`/`404`/... are a property of
/// the request, not a transient blip -- retrying them only delays an
/// inevitable failure), and the engine's own internal range/merge
/// validation signals, which callers handle explicitly.
pub fn is_retryable(error: &DownloadError) -> bool {
    match error {
        DownloadError::Network(_) => true,
        DownloadError::ServerError(code) => matches!(code, 408 | 425 | 429 | 500..=599),
        DownloadError::Io(_)
        | DownloadError::Canceled
        | DownloadError::RangeNotHonored
        | DownloadError::SegmentSizeMismatch { .. } => false,
    }
}

/// Calls `f` up to `max_attempts` times, waiting `base_delay` after the
/// first failed attempt and doubling that delay after each subsequent
/// failed attempt, until `f` succeeds, the attempts are exhausted, or an
/// attempt fails with an error [`is_retryable`] says is not worth retrying
/// (returned immediately, as-is).
///
/// `DownloadError::Canceled` is never retried: it means someone (the
/// caller, or a sibling segment's failure) wants the whole operation to
/// stop right now, not that this particular attempt hit a transient
/// failure worth trying again.
pub async fn retry_with_backoff<T, F, Fut>(
    max_attempts: u32,
    base_delay: Duration,
    f: F,
) -> Result<T, DownloadError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DownloadError>>,
{
    retry_with_backoff_cancelable(max_attempts, base_delay, &CancelToken::new(), f).await
}

/// Same as [`retry_with_backoff`], but every backoff sleep also watches
/// `cancel`: if it is signaled while waiting to retry, this returns
/// `Err(DownloadError::Canceled)` promptly instead of sleeping out the rest
/// of the delay and making another attempt.
pub async fn retry_with_backoff_cancelable<T, F, Fut>(
    max_attempts: u32,
    base_delay: Duration,
    cancel: &CancelToken,
    mut f: F,
) -> Result<T, DownloadError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DownloadError>>,
{
    let mut delay = base_delay;
    let mut attempt = 0;

    loop {
        attempt += 1;

        match f().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if !is_retryable(&e) || attempt >= max_attempts {
                    return Err(e);
                }

                sleep_unless_canceled(delay, cancel).await?;
                delay *= 2;
            }
        }
    }
}

/// Sleeps for `duration`, returning `Err(Canceled)` early if `cancel` is
/// signaled in the meantime.
async fn sleep_unless_canceled(duration: Duration, cancel: &CancelToken) -> Result<(), DownloadError> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if cancel.is_canceled() {
            return Err(DownloadError::Canceled);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        tokio::time::sleep((deadline - now).min(CANCEL_POLL_INTERVAL)).await;
    }
}

/// Resolves once `cancel` has been signaled (polling every
/// `CANCEL_POLL_INTERVAL`). Used with `tokio::select!` to make an otherwise
/// uninterruptible wait -- e.g. a speed limiter's `acquire` sleeping off a
/// large shortfall -- abandon promptly on pause/cancel.
pub async fn canceled(cancel: &CancelToken) {
    while !cancel.is_canceled() {
        tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
    }
}
