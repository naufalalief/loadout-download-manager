use std::collections::HashMap;
use std::sync::Arc;

use loadout::download::engine::download_segmented;
use loadout::download::limiter::SpeedLimiter;
use loadout::download::segments::{part_path, plan_segments, resume_offset};
use loadout::download::types::{CancelToken, DownloadError, DownloadRequest, ProbeInfo};

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[test]
fn plan_segments_splits_evenly_with_remainder_on_last() {
    let ranges = plan_segments(1000, 3);
    assert_eq!(ranges, vec![(0, 332), (333, 665), (666, 999)]);
}

#[test]
fn plan_segments_clamps_segment_count_to_available_bytes_without_underflow() {
    // Fewer bytes than requested segments must not panic (a zero-byte
    // segment has no valid inclusive (start, end) u64 representation --
    // computing `end` would underflow) and must still produce valid,
    // contiguous, non-overlapping ranges covering every byte.
    let ranges = plan_segments(2, 5);

    assert!(!ranges.is_empty());
    assert!(ranges.len() <= 2);

    for &(start, end) in &ranges {
        assert!(end >= start, "invalid range ({start}, {end})");
    }

    assert_eq!(ranges.first().unwrap().0, 0);
    assert_eq!(ranges.last().unwrap().1, 1);

    for pair in ranges.windows(2) {
        let (_, prev_end) = pair[0];
        let (next_start, _) = pair[1];
        assert_eq!(next_start, prev_end + 1, "ranges must be contiguous");
    }
}

#[test]
fn resume_offset_is_zero_when_part_file_absent() {
    let dir = tempfile_dir();
    let dest = dir.join("file.bin");
    let part = part_path(&dest, 0);

    assert_eq!(resume_offset(&part), 0);

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resume_offset_matches_existing_part_file_size() {
    let dir = tempfile_dir();
    let dest = dir.join("file.bin");
    let part = part_path(&dest, 0);

    std::fs::write(&part, vec![0u8; 50]).unwrap();

    assert_eq!(resume_offset(&part), 50);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn download_segmented_falls_back_to_single_stream_without_range_support() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..1000u32).map(|b| (b % 256) as u8).collect();

    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
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
        supports_range: false,
    };

    let dir = tempfile_dir();
    let dest = dir.join("downloaded.bin");

    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        4,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await
    .unwrap();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);

    for index in 0..4 {
        assert!(!part_path(&dest, index).exists());
    }

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn download_segmented_merges_all_segments_into_dest() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..999u32).map(|b| (b % 256) as u8).collect();
    let body_for_responder = body.clone();

    Mock::given(method("GET"))
        .and(path("/file"))
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

    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        3,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await
    .unwrap();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);

    for index in 0..3 {
        assert!(!part_path(&dest, index).exists());
    }

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn download_segmented_resumes_partial_segment_without_redownloading_existing_bytes() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..300u32).map(|b| (b % 256) as u8).collect();

    // Segment 0 covers bytes 0-99 but 40 bytes are already on disk, so the
    // request for it must start at 40, not 0.
    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=40-99"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 40-99/300")
                .set_body_bytes(body[40..=99].to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=100-199"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 100-199/300")
                .set_body_bytes(body[100..=199].to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=200-299"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 200-299/300")
                .set_body_bytes(body[200..=299].to_vec()),
        )
        .expect(1)
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

    // Pre-write the first 40 bytes of segment 0's part file, simulating a
    // crash/resume scenario.
    std::fs::write(part_path(&dest, 0), &body[0..40]).unwrap();

    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        3,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await
    .unwrap();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);

    for index in 0..3 {
        assert!(!part_path(&dest, index).exists());
    }

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn download_segmented_aborts_sibling_segments_on_failure() {
    let server = MockServer::start().await;

    // Segment 0 fails immediately with a non-retryable server error (a
    // 404 is permanent, so `retry_with_backoff` gives up on it at once
    // rather than spending its ~15s transient-failure backoff budget).
    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=0-99"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // Segments 1 and 2 would succeed, but only after a delay. If they
    // aren't aborted as soon as segment 0 fails, they'll still be
    // in-flight (and will go on to create+write their part files) well
    // after `download_segmented` has already returned its error.
    let delay = std::time::Duration::from_millis(1500);

    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=100-199"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 100-199/300")
                .set_body_bytes(vec![1u8; 100])
                .set_delay(delay),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=200-299"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 200-299/300")
                .set_body_bytes(vec![2u8; 100])
                .set_delay(delay),
        )
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
        total_bytes: Some(300),
        supports_range: true,
    };

    let dir = tempfile_dir();
    let dest = dir.join("downloaded.bin");

    let result = download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        3,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await;

    assert_eq!(result, Err(DownloadError::ServerError(404)));

    // Give the delayed mocks' responses plenty of time to have arrived,
    // in case the sibling segments weren't actually stopped.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    // `download_segment` only creates a part file after it receives a
    // response, so if these are still absent, the delayed segments were
    // truly aborted while awaiting their response -- not merely slow.
    assert!(
        !part_path(&dest, 1).exists(),
        "segment 1 kept running (and wrote its part file) after download_segmented returned"
    );
    assert!(
        !part_path(&dest, 2).exists(),
        "segment 2 kept running (and wrote its part file) after download_segmented returned"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// I2 regression: a server that advertises range support (so the probe
/// says `supports_range: true`) but then ignores the `Range` header on the
/// actual GETs, answering `200` with the *full* body every time. Before the
/// fix, every segment accepted that `200` and wrote the whole body into its
/// part file, producing a merged file `segment_count` times too large that
/// was still reported as a success. Now any non-`206` segment response
/// makes `download_segmented` discard the part files and fall back to a
/// single-stream download, yielding exactly the right bytes.
#[tokio::test]
async fn download_segmented_falls_back_to_single_stream_when_server_ignores_range() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..10_000u32).map(|b| (b % 251) as u8).collect();
    let ranged_requests = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let plain_requests = Arc::new(std::sync::atomic::AtomicU32::new(0));

    {
        let body = body.clone();
        let ranged_requests = ranged_requests.clone();
        let plain_requests = plain_requests.clone();
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(move |request: &Request| {
                if request.headers.get("Range").is_some() {
                    ranged_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                } else {
                    plain_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                // Ignores Range entirely.
                ResponseTemplate::new(200)
                    .insert_header("Accept-Ranges", "bytes")
                    .set_body_bytes(body.clone())
            })
            .mount(&server)
            .await;
    }

    let client = reqwest::Client::new();
    let req = DownloadRequest {
        url: format!("{}/file", server.uri()),
        filename: None,
        headers: HashMap::new(),
    };
    let probe = ProbeInfo {
        total_bytes: Some(body.len() as u64),
        supports_range: true,
    };

    let dir = tempfile_dir();
    let dest = dir.join("downloaded.bin");

    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        4,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), body, "final file must be exactly the body");
    for index in 0..4 {
        assert!(!part_path(&dest, index).exists(), "part {index} left behind");
    }
    assert!(
        ranged_requests.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "segmented path should have been attempted first"
    );
    assert_eq!(
        plain_requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one single-stream (non-ranged) fallback GET expected"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// I2 regression: a part file whose size doesn't match its planned
/// segment length (here: longer than the segment, as a server that sent
/// too much or an overlapping writer would leave it) must make the merge
/// fail cleanly -- no corrupt output file -- and the bad part file must be
/// deleted so a retry re-fetches that segment instead of failing forever.
#[tokio::test]
async fn download_segmented_refuses_to_merge_a_wrong_sized_part_file() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..300u32).map(|b| (b % 256) as u8).collect();
    {
        let body = body.clone();
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(move |request: &Request| {
                let (start, end) =
                    parse_range(request.headers.get("Range").unwrap().to_str().unwrap());
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", format!("bytes {start}-{end}/{}", body.len()))
                    .set_body_bytes(body[start as usize..=end as usize].to_vec())
            })
            .mount(&server)
            .await;
    }

    let client = reqwest::Client::new();
    let req = DownloadRequest {
        url: format!("{}/file", server.uri()),
        filename: None,
        headers: HashMap::new(),
    };
    let probe = ProbeInfo {
        total_bytes: Some(body.len() as u64),
        supports_range: true,
    };

    let dir = tempfile_dir();
    let dest = dir.join("downloaded.bin");

    // Segment 0 covers 100 bytes; pre-seed its part file with 150. Since
    // that's >= the segment length, the segment is treated as "already
    // done" and never re-requested -- only the merge-time size check can
    // catch it.
    std::fs::write(part_path(&dest, 0), vec![0xEEu8; 150]).unwrap();

    let result = download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        3,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await;

    assert_eq!(
        result,
        Err(DownloadError::SegmentSizeMismatch {
            index: 0,
            expected: 100,
            actual: 150
        })
    );
    assert!(!dest.exists(), "no (corrupt) merged output may be written");
    assert!(
        !part_path(&dest, 0).exists(),
        "the wrong-sized part must be deleted so a retry re-fetches it"
    );

    // And a retry now succeeds with the right content.
    download_segmented(
        &client,
        &req,
        &probe,
        &dest,
        3,
        CancelToken::new(),
        Arc::new(SpeedLimiter::new(None)),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);

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
