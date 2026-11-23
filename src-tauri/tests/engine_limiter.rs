use std::collections::HashMap;
use std::sync::Arc;

use loadout::download::engine::download_segmented;
use loadout::download::limiter::SpeedLimiter;
use loadout::download::types::{CancelToken, DownloadRequest, ProbeInfo};

use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

#[tokio::test]
async fn acquire_is_immediate_when_unlimited() {
    let limiter = SpeedLimiter::new(None);
    let start = std::time::Instant::now();
    limiter.acquire(10_000_000).await;
    assert!(start.elapsed() < std::time::Duration::from_millis(50));
}

/// A limit of 0 means "no limit", never "stall forever" -- both at
/// construction and when set later (e.g. from the per-row UI control).
#[tokio::test]
async fn zero_limit_means_unlimited_not_a_permanent_stall() {
    let limiter = SpeedLimiter::new(Some(0));
    let start = std::time::Instant::now();
    limiter.acquire(10_000_000).await;
    assert!(start.elapsed() < std::time::Duration::from_millis(50));

    let limiter = SpeedLimiter::new(Some(1000));
    limiter.set_limit(Some(0));
    let start = std::time::Instant::now();
    limiter.acquire(10_000_000).await;
    assert!(start.elapsed() < std::time::Duration::from_millis(50));
}

#[tokio::test]
async fn acquire_throttles_to_configured_rate() {
    let limiter = SpeedLimiter::new(Some(1000)); // 1000 bytes/sec
    let start = std::time::Instant::now();
    limiter.acquire(1000).await;
    limiter.acquire(1000).await;
    assert!(start.elapsed() >= std::time::Duration::from_millis(900));
}

/// Two segments share one `Arc<SpeedLimiter>` capped at 1000 bytes/sec.
/// Each segment's response delivers its whole 1000-byte share in a single
/// chunk, so with a correctly *shared* limiter the bucket's initial
/// one-second burst (1000 bytes) can only satisfy one of the two segments'
/// `acquire(1000)` calls; the other must wait out essentially a full
/// second for the bucket to refill.
///
/// If the limiter were (incorrectly) constructed once per segment instead
/// of shared, each segment would get its own full 1000-byte burst and
/// both would finish near-instantly, well under the assertion below -- so
/// this test fails under a per-segment implementation and passes only
/// when the limit is enforced on the combined, per-download throughput.
#[tokio::test]
async fn download_segmented_combined_throughput_respects_shared_limit() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..2000u32).map(|b| (b % 256) as u8).collect();
    let body_for_responder = body.clone();

    Mock::given(method("GET"))
        .respond_with(move |request: &Request| {
            let range_header = request
                .headers
                .get("Range")
                .expect("expected a Range header on every segmented request")
                .to_str()
                .unwrap();

            let (start, end) = parse_range(range_header);
            let slice = body_for_responder[start as usize..=end as usize].to_vec();

            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!("bytes {start}-{end}/{}", body_for_responder.len()),
                )
                .set_body_bytes(slice)
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

    let limiter = Arc::new(SpeedLimiter::new(Some(1000)));

    let start = std::time::Instant::now();

    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        2,
        CancelToken::new(),
        limiter,
        |_| {},
    )
    .await
    .unwrap();

    let elapsed = start.elapsed();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);

    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "expected combined throughput to be throttled to the shared limit (~1s for the second \
         segment to wait out the bucket refill), but completed in {elapsed:?}"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// Parses a `Range: bytes=START-END` header value into `(start, end)`.
fn parse_range(value: &str) -> (u64, u64) {
    let range = value.trim_start_matches("bytes=");
    let mut parts = range.split('-');
    let start = parts.next().unwrap().parse::<u64>().unwrap();
    let end = parts.next().unwrap().parse::<u64>().unwrap();
    (start, end)
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
