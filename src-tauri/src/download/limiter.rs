use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// Token-bucket state guarded by the limiter's mutex.
struct BucketState {
    /// Configured rate, in bytes/sec. `None` means unlimited.
    bytes_per_sec: Option<u64>,
    /// Bytes currently available to spend without waiting.
    available: u64,
    /// When `available` was last topped up.
    last_refill: Instant,
}

/// Maps a configured limit to its effective meaning: `Some(0)` is treated
/// as "no limit" (`None`) rather than "allow zero bytes per second", which
/// would permanently stall a download -- an easy value to enter by accident
/// in a numeric settings field, and never a useful one.
pub fn normalize_limit(bytes_per_sec: Option<u64>) -> Option<u64> {
    bytes_per_sec.filter(|&rate| rate > 0)
}

/// A shared, per-download token-bucket rate limiter.
///
/// Call [`SpeedLimiter::acquire`] with the number of bytes you're about to
/// write *before* writing them; it awaits until the configured rate allows
/// that many bytes to be "spent". When the limit is `None`, `acquire`
/// returns immediately with no delay.
///
/// A single `Arc<SpeedLimiter>` is meant to be shared across every
/// concurrent segment of one download, so the configured rate bounds the
/// download's *total* combined throughput rather than each segment
/// individually.
pub struct SpeedLimiter {
    state: Mutex<BucketState>,
}

impl SpeedLimiter {
    /// Creates a new limiter. `bytes_per_sec` of `None` -- or `Some(0)`,
    /// see [`normalize_limit`] -- means unlimited.
    ///
    /// The bucket starts full (one second's worth of budget available), so
    /// an initial burst up to the configured rate is allowed before
    /// throttling kicks in -- matching typical token-bucket limiters and
    /// avoiding an up-front stall on the very first `acquire`.
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        let bytes_per_sec = normalize_limit(bytes_per_sec);
        Self {
            state: Mutex::new(BucketState {
                bytes_per_sec,
                available: bytes_per_sec.unwrap_or(0),
                last_refill: Instant::now(),
            }),
        }
    }

    /// Updates the configured rate. `None` (or `Some(0)`, see
    /// [`normalize_limit`]) removes the limit; any previously accumulated
    /// token balance is left untouched.
    pub fn set_limit(&self, bytes_per_sec: Option<u64>) {
        let bytes_per_sec = normalize_limit(bytes_per_sec);
        // `set_limit` is not `async`, so we can't `.await` the mutex here;
        // `blocking_lock` would panic inside an async context, but a plain
        // spin via `try_lock` is fine since this lock is only ever held
        // briefly (never across an `.await`) in `acquire`.
        loop {
            if let Ok(mut state) = self.state.try_lock() {
                state.bytes_per_sec = bytes_per_sec;
                return;
            }
        }
    }

    /// Awaits until `bytes` worth of budget is available under the
    /// configured rate, then spends it. Returns immediately when unlimited.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }

        loop {
            let wait = {
                let mut state = self.state.lock().await;

                let Some(rate) = state.bytes_per_sec else {
                    return;
                };

                if rate == 0 {
                    // Unreachable in practice: `new`/`set_limit` normalize
                    // `Some(0)` to `None` (unlimited). Kept as a defensive
                    // "treat as unlimited" rather than a permanent stall.
                    return;
                } else {
                    // Refill based on elapsed time since the last refill.
                    // Normally capped at one second's worth so a long idle
                    // period doesn't let a burst exceed the configured
                    // rate, but widened to `bytes` when a single request
                    // asks for more than that: otherwise a chunk larger
                    // than the configured rate (e.g. an 8KB stream chunk
                    // under a very low limit) could never be admitted and
                    // `acquire` would never return.
                    let capacity = rate.max(bytes);
                    let now = Instant::now();
                    let elapsed = now.duration_since(state.last_refill);
                    let refill = ((elapsed.as_secs_f64()) * rate as f64) as u64;
                    if refill > 0 {
                        state.available = (state.available + refill).min(capacity);
                        state.last_refill = now;
                    }

                    if state.available >= bytes {
                        state.available -= bytes;
                        None
                    } else {
                        // Not enough budget yet: figure out how long until
                        // enough accrues, release the lock, and sleep.
                        let shortfall = bytes - state.available;
                        let secs_needed = shortfall as f64 / rate as f64;
                        Some(Duration::from_secs_f64(secs_needed))
                    }
                }
            };

            match wait {
                None => return,
                Some(duration) => {
                    tokio::time::sleep(duration).await;
                }
            }
        }
    }
}
