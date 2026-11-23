use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use loadout::download::engine::download_segmented;
use loadout::download::limiter::SpeedLimiter;
use loadout::download::retry::retry_with_backoff;
use loadout::download::segments::part_path;
use loadout::download::types::{CancelToken, DownloadError, DownloadRequest, ProbeInfo};

use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn retry_with_backoff_retries_until_success() {
    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_closure = calls.clone();

    let result = retry_with_backoff(3, Duration::from_millis(1), move || {
        let calls = calls_for_closure.clone();
        async move {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                Err(DownloadError::Network("transient".to_string()))
            } else {
                Ok(42)
            }
        }
    })
    .await;

    assert_eq!(result, Ok(42));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retry_with_backoff_gives_up_after_max_attempts() {
    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_closure = calls.clone();
    let max_attempts = 3;

    let result: Result<(), DownloadError> =
        retry_with_backoff(max_attempts, Duration::from_millis(1), move || {
            let calls = calls_for_closure.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(DownloadError::Network("always fails".to_string()))
            }
        })
        .await;

    assert_eq!(
        result,
        Err(DownloadError::Network("always fails".to_string()))
    );
    assert_eq!(calls.load(Ordering::SeqCst), max_attempts);
}

#[tokio::test]
async fn retry_with_backoff_does_not_retry_cancellation() {
    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_closure = calls.clone();

    let result: Result<(), DownloadError> =
        retry_with_backoff(3, Duration::from_millis(1), move || {
            let calls = calls_for_closure.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(DownloadError::Canceled)
            }
        })
        .await;

    assert_eq!(result, Err(DownloadError::Canceled));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Confirms the delay actually doubles between attempts, not just that
/// retries happen: measures the gap between the 1st->2nd and 2nd->3rd
/// attempts and asserts the second gap is meaningfully larger than the
/// first (allowing slack for scheduler/timer jitter).
#[tokio::test]
async fn retry_with_backoff_delay_doubles_between_attempts() {
    let timestamps: Arc<std::sync::Mutex<Vec<Instant>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let timestamps_for_closure = timestamps.clone();
    let base_delay = Duration::from_millis(100);

    let result: Result<(), DownloadError> = retry_with_backoff(3, base_delay, move || {
        let timestamps = timestamps_for_closure.clone();
        async move {
            timestamps.lock().unwrap().push(Instant::now());
            Err(DownloadError::Network("always fails".to_string()))
        }
    })
    .await;

    assert!(result.is_err());

    let timestamps = timestamps.lock().unwrap();
    assert_eq!(timestamps.len(), 3);

    let first_gap = timestamps[1].duration_since(timestamps[0]);
    let second_gap = timestamps[2].duration_since(timestamps[1]);

    // First gap should be roughly base_delay, second roughly 2x base_delay.
    assert!(
        first_gap >= base_delay,
        "first gap {first_gap:?} should be at least base_delay {base_delay:?}"
    );
    assert!(
        second_gap > first_gap + first_gap / 2,
        "second gap {second_gap:?} should be meaningfully larger than first gap {first_gap:?} (expected ~2x)"
    );
}

#[tokio::test]
async fn download_segmented_recovers_from_one_transient_segment_failure() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..300u32).map(|b| (b % 256) as u8).collect();
    let body_for_responder = body.clone();
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_for_responder = attempts.clone();

    Mock::given(method("GET"))
        .respond_with(move |_request: &wiremock::Request| {
            let attempt = attempts_for_responder.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(206)
                    .insert_header(
                        "Content-Range",
                        format!(
                            "bytes 0-{}/{}",
                            body_for_responder.len() - 1,
                            body_for_responder.len()
                        ),
                    )
                    .set_body_bytes(body_for_responder.clone())
            }
        })
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/file", server.uri());

    let req = DownloadRequest {
        url,
        filename: None,
        headers: HashMap::new(),
    };

    let probe = ProbeInfo {
        total_bytes: Some(body.len() as u64),
        supports_range: true,
    };

    let dir = tempfile_dir();
    let dest = dir.join("downloaded.bin");

    // A single segment, so the one transient failure is deterministically
    // on the only in-flight request rather than a race between segments.
    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        1,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await
    .unwrap();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);
    assert!(!part_path(&dest, 0).exists());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    std::fs::remove_dir_all(dir).ok();
}

/// Creates a fresh temp directory for a single test to write into, avoiding
/// a dependency on a `tempfile` crate.
fn tempfile_dir() -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let unique = format!(
        "download-manager-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    dir.push(unique);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// I5: only transient failures are worth retrying. A permanent `404`
/// must fail on the first attempt (not burn the whole backoff budget),
/// while a `503` is retried.
#[tokio::test]
async fn retry_with_backoff_skips_permanent_errors_but_retries_transient_ones() {
    use loadout::download::retry::is_retryable;

    assert!(!is_retryable(&DownloadError::ServerError(404)));
    assert!(!is_retryable(&DownloadError::ServerError(403)));
    assert!(!is_retryable(&DownloadError::Io("disk full".to_string())));
    assert!(is_retryable(&DownloadError::ServerError(503)));
    assert!(is_retryable(&DownloadError::ServerError(429)));
    assert!(is_retryable(&DownloadError::Network("reset".to_string())));

    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_closure = calls.clone();
    let result: Result<(), DownloadError> =
        retry_with_backoff(5, Duration::from_millis(1), move || {
            let calls = calls_for_closure.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(DownloadError::ServerError(404))
            }
        })
        .await;
    assert_eq!(result, Err(DownloadError::ServerError(404)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_closure = calls.clone();
    let result: Result<(), DownloadError> =
        retry_with_backoff(5, Duration::from_millis(1), move || {
            let calls = calls_for_closure.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(DownloadError::ServerError(503))
            }
        })
        .await;
    assert_eq!(result, Err(DownloadError::ServerError(503)));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

/// A pause/cancel issued while a download is sitting in a (now multi-
/// second) retry backoff must be noticed promptly, not after the delay.
#[tokio::test]
async fn retry_backoff_sleep_is_interrupted_by_cancellation() {
    use loadout::download::retry::retry_with_backoff_cancelable;

    let cancel = CancelToken::new();
    let cancel_later = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_later.cancel();
    });

    let start = Instant::now();
    let result: Result<(), DownloadError> =
        retry_with_backoff_cancelable(5, Duration::from_secs(10), &cancel, || async {
            Err(DownloadError::Network("down".to_string()))
        })
        .await;

    assert_eq!(result, Err(DownloadError::Canceled));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "cancellation during backoff took {:?}",
        start.elapsed()
    );
}
