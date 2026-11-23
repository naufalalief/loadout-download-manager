use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use loadout::download::engine::{download_single_stream, probe};
use loadout::download::limiter::SpeedLimiter;
use loadout::download::types::{CancelToken, DownloadError, DownloadRequest, ProbeInfo};

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn probe_reports_size_and_range_support() {
    let server = MockServer::start().await;

    Mock::given(method("HEAD"))
        .and(path("/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "1000")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/file", server.uri());

    let info = probe(&client, &url, &HashMap::new()).await.unwrap();

    assert_eq!(
        info,
        ProbeInfo {
            total_bytes: Some(1000),
            supports_range: true,
        }
    );
}

#[tokio::test]
async fn probe_reports_no_range_support_when_header_absent() {
    let server = MockServer::start().await;

    Mock::given(method("HEAD"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "1000"))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/file", server.uri());

    let info = probe(&client, &url, &HashMap::new()).await.unwrap();

    assert_eq!(info.total_bytes, Some(1000));
    assert!(!info.supports_range);
}

#[tokio::test]
async fn probe_falls_back_to_ranged_get_when_head_rejected() {
    let server = MockServer::start().await;

    // Server rejects HEAD with 405, so `probe` must fall back to a
    // ranged GET to learn the size and range support.
    Mock::given(method("HEAD"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("Range", "bytes=0-0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 0-0/5000")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/file", server.uri());

    let info = probe(&client, &url, &HashMap::new()).await.unwrap();

    assert_eq!(
        info,
        ProbeInfo {
            total_bytes: Some(5000),
            supports_range: true,
        }
    );
}

#[tokio::test]
async fn probe_surfaces_server_error_on_head_forbidden_without_fallback() {
    let server = MockServer::start().await;

    // 403 means the resource is access-restricted, not that HEAD itself
    // is unsupported, so `probe` must surface the error directly instead
    // of attempting a ranged GET fallback (which is not mocked here --
    // if `probe` incorrectly fell back, wiremock would return its
    // default 404 "no matching mock" response instead of 403, failing
    // this assertion).
    Mock::given(method("HEAD"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/file", server.uri());

    let result = probe(&client, &url, &HashMap::new()).await;

    assert_eq!(result, Err(DownloadError::ServerError(403)));
}

#[tokio::test]
async fn download_single_stream_writes_full_body_and_reports_progress() {
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..10_000u32).map(|b| (b % 256) as u8).collect();

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

    let dir = tempfile_dir();
    let dest = dir.join("downloaded.bin");

    let progress: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let progress_clone = Arc::clone(&progress);

    let cancel = CancelToken::new();

    download_single_stream(
        &client,
        &req,
        &dest,
        cancel,
        Arc::new(SpeedLimiter::new(None)),
        move |bytes| {
            progress_clone.lock().unwrap().push(bytes);
        },
    )
    .await
    .unwrap();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);

    let progress = progress.lock().unwrap();
    assert_eq!(progress.last().copied(), Some(10_000));

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn download_single_stream_stops_on_cancel() {
    let server = MockServer::start().await;

    let body: Vec<u8> = vec![0u8; 10_000];

    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/file", server.uri());

    let req = DownloadRequest {
        url,
        filename: None,
        headers: HashMap::new(),
    };

    let dir = tempfile_dir();
    let dest = dir.join("canceled.bin");

    let cancel = CancelToken::new();
    cancel.cancel();

    let result = download_single_stream(
        &client,
        &req,
        &dest,
        cancel,
        Arc::new(SpeedLimiter::new(None)),
        |_bytes| {},
    )
    .await;

    assert_eq!(result, Err(DownloadError::Canceled));

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
