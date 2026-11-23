use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use loadout::download::persistence::load_state;
use loadout::download::queue::QueueManager;
use loadout::download::types::{DownloadInfo, DownloadRequest, DownloadStatus};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn add_creates_queued_entry_and_persists_it() {
    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let manager = QueueManager::new(dir.clone(), state_file.clone(), 2);

    let id = manager.add(DownloadRequest {
        url: "https://example.invalid/file.bin".to_string(),
        filename: Some("file.bin".to_string()),
        headers: HashMap::new(),
    });

    let listed = manager.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].status, DownloadStatus::Queued);
    assert_eq!(listed[0].filename, "file.bin");

    let persisted = load_state(&state_file);
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].id, id);
    assert_eq!(persisted[0].status, DownloadStatus::Queued);

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn loading_existing_state_resets_downloading_to_queued() {
    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let pre_existing = vec![DownloadInfo {
        id: "abc123".to_string(),
        url: "https://example.invalid/file.bin".to_string(),
        filename: "file.bin".to_string(),
        dest_path: dir.join("file.bin"),
        total_bytes: Some(1000),
        downloaded_bytes: 500,
        status: DownloadStatus::Downloading,
        error: None,
        created_at: 1_700_000_000,
    }];

    std::fs::write(&state_file, serde_json::to_string(&pre_existing).unwrap()).unwrap();

    let manager = QueueManager::new(dir.clone(), state_file.clone(), 2);

    let listed = manager.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "abc123");
    assert_eq!(listed[0].status, DownloadStatus::Queued);

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn loading_legacy_state_without_created_at_defaults_to_zero() {
    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    // Hand-written legacy JSON: exactly what a pre-created_at build persisted.
    std::fs::write(
        &state_file,
        r#"[{"id":"abc123","url":"https://example.invalid/file.bin","filename":"file.bin","dest_path":"file.bin","total_bytes":1000,"downloaded_bytes":500,"status":"Paused","error":null}]"#,
    )
    .unwrap();

    let manager = QueueManager::new(dir.clone(), state_file.clone(), 2);
    let loaded = manager.list();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].created_at, 0);
}

#[tokio::test]
async fn queue_runs_at_most_max_concurrent_downloads_at_once() {
    let server = MockServer::start().await;

    let body = vec![0u8; 2_000];

    Mock::given(method("GET"))
        .respond_with(move |_req: &wiremock::Request| {
            std::thread::sleep(Duration::from_millis(300));
            ResponseTemplate::new(200).set_body_bytes(body.clone())
        })
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let manager = QueueManager::new(dir.clone(), state_file, 1);

    let url = format!("{}/file", server.uri());

    manager.add(DownloadRequest {
        url: url.clone(),
        filename: Some("first.bin".to_string()),
        headers: HashMap::new(),
    });
    manager.add(DownloadRequest {
        url,
        filename: Some("second.bin".to_string()),
        headers: HashMap::new(),
    });

    // Give both a moment to be picked up by the scheduler.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let downloading_count = manager
        .list()
        .iter()
        .filter(|d| d.status == DownloadStatus::Downloading)
        .count();
    assert_eq!(
        downloading_count, 1,
        "max_concurrent=1 must cap simultaneous downloads to 1"
    );

    // Wait for both to finish.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listed = manager.list();
        if listed
            .iter()
            .all(|d| d.status == DownloadStatus::Completed)
        {
            break;
        }
    }

    let listed = manager.list();
    assert_eq!(listed.len(), 2);
    for d in &listed {
        assert_eq!(d.status, DownloadStatus::Completed, "entry {:?}", d);
    }

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn pause_then_resume_continues_a_download() {
    let server = MockServer::start().await;

    let body = vec![7u8; 5_000_000];

    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            // Simulate real network latency on every response (including
            // the probing round trip): over actual loopback, a 5MB
            // transfer with no delay at all can complete in well under the
            // 100ms window this test gives itself to call `pause` while
            // the download is still in flight, making the test flaky on a
            // fast machine. A deliberate per-request delay (mirroring the
            // same pattern in `cancel_removes_partial_files_...` below)
            // keeps "pause lands mid-download" deterministic.
            std::thread::sleep(Duration::from_millis(150));
            if let Some(range) = req.headers.get("Range") {
                let range = range.to_str().unwrap();
                let (start, end) = parse_range(range);
                ResponseTemplate::new(206)
                    .insert_header(
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", body.len()),
                    )
                    .set_body_bytes(body[start as usize..=end as usize].to_vec())
            } else {
                ResponseTemplate::new(200)
                    .insert_header("Accept-Ranges", "bytes")
                    .insert_header("Content-Length", body.len().to_string())
                    .set_body_bytes(body.clone())
            }
        })
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let url = format!("{}/file", server.uri());
    let id = manager.add(DownloadRequest {
        url,
        filename: Some("paused.bin".to_string()),
        headers: HashMap::new(),
    });

    // Let it get going.
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.pause(&id);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_pause = manager.list();
    let entry = after_pause.iter().find(|d| d.id == id).unwrap();
    assert_eq!(entry.status, DownloadStatus::Paused);
    let progress_at_pause = entry.downloaded_bytes;

    // Confirm no further progress while paused.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let still_paused = manager.list();
    let entry = still_paused.iter().find(|d| d.id == id).unwrap();
    assert_eq!(entry.status, DownloadStatus::Paused);
    assert_eq!(entry.downloaded_bytes, progress_at_pause);

    manager.resume(&id);

    let mut completed = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listed = manager.list();
        let entry = listed.iter().find(|d| d.id == id).unwrap();
        if entry.status == DownloadStatus::Completed {
            completed = true;
            break;
        }
        assert_ne!(entry.status, DownloadStatus::Failed, "entry: {:?}", entry);
    }

    assert!(completed, "resumed download should reach Completed");

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn cancel_removes_partial_files_and_marks_canceled() {
    let server = MockServer::start().await;

    let body = vec![3u8; 5_000_000];

    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(range) = req.headers.get("Range") {
                let range = range.to_str().unwrap();
                let (start, end) = parse_range(range);
                ResponseTemplate::new(206)
                    .insert_header(
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", body.len()),
                    )
                    .set_body_bytes(body[start as usize..=end as usize].to_vec())
            } else {
                ResponseTemplate::new(200)
                    .insert_header("Accept-Ranges", "bytes")
                    .insert_header("Content-Length", body.len().to_string())
                    .set_body_bytes(body.clone())
            }
        })
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let url = format!("{}/file", server.uri());
    let id = manager.add(DownloadRequest {
        url,
        filename: Some("canceled.bin".to_string()),
        headers: HashMap::new(),
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    manager.cancel(&id);

    let mut final_status = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listed = manager.list();
        let entry = listed.iter().find(|d| d.id == id).unwrap();
        if entry.status == DownloadStatus::Canceled {
            final_status = Some(entry.status);
            break;
        }
    }
    assert_eq!(final_status, Some(DownloadStatus::Canceled));

    // Give the background task a moment to finish its post-cancel cleanup.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let dest = dir.join("canceled.bin");
    assert!(!dest.exists(), "merged destination file should be gone");

    // Scan the whole directory rather than only the merged destination
    // file's own legacy-named `.partN` siblings: downloads now stage into
    // a per-id-unique path (see the collision-prevention fix), so this
    // also catches a leftover staging file or its own `.partN` segments
    // under whatever internal naming scheme is actually in use.
    let leftover: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "state.json")
        .collect();
    assert!(
        leftover.is_empty(),
        "leftover files after cancel: {:?}",
        leftover
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn failed_download_does_not_block_the_rest_of_the_queue() {
    let server = MockServer::start().await;

    let body = vec![9u8; 2_000];

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let failing_id = manager.add(DownloadRequest {
        url: "http://127.0.0.1:1/unreachable".to_string(),
        filename: Some("fails.bin".to_string()),
        headers: HashMap::new(),
    });

    let working_url = format!("{}/file", server.uri());
    let working_id = manager.add(DownloadRequest {
        url: working_url,
        filename: Some("works.bin".to_string()),
        headers: HashMap::new(),
    });

    // The unreachable download now goes through the probe's retries
    // (~1.5s) and the single-stream download's retries (~15s of backoff,
    // see `DOWNLOAD_RETRY_MAX_ATTEMPTS`) before it is marked Failed, so
    // allow plenty of time.
    let mut failing_done = false;
    let mut working_done = false;
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listed = manager.list();
        let failing = listed.iter().find(|d| d.id == failing_id).unwrap();
        let working = listed.iter().find(|d| d.id == working_id).unwrap();
        failing_done = failing.status == DownloadStatus::Failed;
        working_done = working.status == DownloadStatus::Completed;
        if failing_done && working_done {
            break;
        }
    }

    let listed = manager.list();
    let failing = listed.iter().find(|d| d.id == failing_id).unwrap();
    assert!(failing_done, "expected failing entry Failed, got {:?}", failing);
    assert!(failing.error.is_some());
    assert!(working_done, "working download should still complete");

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn unwritable_destination_marks_failed_without_crashing_the_queue() {
    let server = MockServer::start().await;

    let body = vec![1u8; 1_000];

    // A short delay so the test can block the download's write target
    // after `add()` but before the download actually writes anything.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");

    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let url = format!("{}/file", server.uri());
    let blocked_id = manager.add(DownloadRequest {
        url: url.clone(),
        filename: Some("blocked.bin".to_string()),
        headers: HashMap::new(),
    });

    // Create a directory at the exact path the download writes its data
    // to (its per-id staging file -- `add()` would simply pick a
    // different, free *destination* name if `blocked.bin` itself were
    // taken), so creating the file there fails with a local I/O error.
    let blocked_dest = manager
        .list()
        .into_iter()
        .find(|d| d.id == blocked_id)
        .unwrap()
        .dest_path;
    let staging = format!("{}.{}.part", blocked_dest.display(), blocked_id);
    std::fs::create_dir_all(&staging).unwrap();

    let normal_id = manager.add(DownloadRequest {
        url,
        filename: Some("normal.bin".to_string()),
        headers: HashMap::new(),
    });

    let mut blocked_done = false;
    let mut normal_done = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listed = manager.list();
        let blocked = listed.iter().find(|d| d.id == blocked_id).unwrap();
        let normal = listed.iter().find(|d| d.id == normal_id).unwrap();
        blocked_done = blocked.status == DownloadStatus::Failed;
        normal_done = normal.status == DownloadStatus::Completed;
        if blocked_done && normal_done {
            break;
        }
    }

    let listed = manager.list();
    let blocked = listed.iter().find(|d| d.id == blocked_id).unwrap();
    assert!(
        blocked_done,
        "expected the colliding entry to end Failed, got {:?}",
        blocked
    );
    assert!(blocked.error.is_some());
    assert!(
        normal_done,
        "a second, normal download must still reach Completed"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn cancel_on_completed_entry_is_a_no_op_and_leaves_the_file() {
    let server = MockServer::start().await;
    let body = vec![4u8; 1_000];

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let url = format!("{}/file", server.uri());
    let id = manager.add(DownloadRequest {
        url,
        filename: Some("done.bin".to_string()),
        headers: HashMap::new(),
    });

    let mut completed = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listed = manager.list();
        let entry = listed.iter().find(|d| d.id == id).unwrap();
        if entry.status == DownloadStatus::Completed {
            completed = true;
            break;
        }
    }
    assert!(
        completed,
        "expected the download to reach Completed before we try to cancel it"
    );

    let dest = dir.join("done.bin");
    assert!(dest.exists(), "completed file should exist before cancel()");
    let original_contents = std::fs::read(&dest).unwrap();
    assert_eq!(original_contents, body);

    // Canceling an already-Completed entry must be a no-op: it must not
    // flip the status away from Completed, and -- critically -- must
    // never delete the finished destination file. (The bug: cancel()
    // used to set status = Canceled and run cleanup unconditionally,
    // deriving its cleanup target purely from `dest_path` the same way
    // regardless of whether the download was actually still in flight.)
    manager.cancel(&id);

    let listed = manager.list();
    let entry = listed.iter().find(|d| d.id == id).unwrap();
    assert_eq!(
        entry.status,
        DownloadStatus::Completed,
        "cancel() on an already-Completed entry must be a no-op"
    );

    assert!(
        dest.exists(),
        "cancel() on a Completed entry must never delete its finished file"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), original_contents);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_race_immediately_after_scheduling_is_never_silently_lost() {
    // Regression test for a race between `schedule()` marking an entry
    // `Downloading` and `run()` registering its `CancelToken`: if a
    // `pause`/`cancel` request lands in that window, it could flip the
    // stored status with no live signal behind it to deliver, letting the
    // un-interrupted download finish anyway and the completion path
    // unconditionally stomp the status back to `Completed`.
    //
    // The fix closes this structurally (the token is now registered
    // under the same `downloads` lock as the `Queued` -> `Downloading`
    // transition, so no caller can ever observe `Downloading` without
    // the token already in place) and defensively (`resolve_completion`/
    // `mark_failed` now refuse to overwrite a status that isn't still
    // `Downloading`). This test can't force the original race's exact
    // (very narrow, sub-millisecond) timing window on demand, so instead
    // it fires `pause()` from a separate spawned task immediately after
    // every one of many rapid `add()` calls, racing real OS-thread
    // concurrency (multi-threaded runtime) against each download's
    // background task, and checks the invariant the fix guarantees holds
    // for every one of them that actually caught the entry mid-flight.
    let server = MockServer::start().await;
    let body = vec![6u8; 4_000];

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = Arc::new(QueueManager::new(dir.clone(), state_file, 8));
    let url = format!("{}/file", server.uri());

    let mut pause_tasks = Vec::new();
    for i in 0..200 {
        let id = manager.add(DownloadRequest {
            url: url.clone(),
            filename: Some(format!("race{i}.bin")),
            headers: HashMap::new(),
        });

        let manager2 = Arc::clone(&manager);
        let id2 = id.clone();
        pause_tasks.push(tokio::spawn(async move {
            manager2.pause(&id2);
            let landed = manager2
                .list()
                .iter()
                .find(|d| d.id == id2)
                .map(|d| d.status == DownloadStatus::Paused)
                .unwrap_or(false);
            (id2, landed)
        }));
    }

    let mut landed_ids = Vec::new();
    for task in pause_tasks {
        let (id, landed) = task.await.unwrap();
        if landed {
            landed_ids.push(id);
        }
    }

    // Let anything still in flight settle.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listed = manager.list();
        if listed
            .iter()
            .all(|d| !matches!(d.status, DownloadStatus::Downloading | DownloadStatus::Queued))
        {
            break;
        }
    }

    assert!(
        !landed_ids.is_empty(),
        "test never managed to catch a download mid-flight with pause() -- \
         can't exercise the race this test targets"
    );

    let listed = manager.list();
    for id in &landed_ids {
        let entry = listed.iter().find(|d| &d.id == id).unwrap();
        assert_ne!(
            entry.status,
            DownloadStatus::Completed,
            "pause() that successfully flipped status to Paused must never be \
             silently discarded by a download that kept running anyway: entry {:?}",
            entry
        );
    }

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn completing_a_download_forgets_its_headers_and_limiter() {
    let server = MockServer::start().await;
    let body = vec![8u8; 1_000];

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let url = format!("{}/file", server.uri());
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer secret-token".to_string(),
    );

    let id = manager.add(DownloadRequest {
        url,
        filename: Some("with-headers.bin".to_string()),
        headers,
    });
    manager.set_speed_limit(&id, Some(1_000_000));

    assert!(
        manager.has_side_channel_state(&id),
        "headers/limiter should be tracked while the download is pending/running"
    );

    let mut completed = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listed = manager.list();
        let entry = listed.iter().find(|d| d.id == id).unwrap();
        if entry.status == DownloadStatus::Completed {
            completed = true;
            break;
        }
    }
    assert!(completed, "expected the download to complete");

    assert!(
        !manager.has_side_channel_state(&id),
        "a Completed download's in-memory headers/limiter must be forgotten, \
         not kept resident in memory indefinitely until the user explicitly \
         calls remove()"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn remove_while_downloading_does_not_race_the_writer() {
    let server = MockServer::start().await;
    let body = vec![3u8; 5_000_000];

    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(range) = req.headers.get("Range") {
                let range = range.to_str().unwrap();
                let (start, end) = parse_range(range);
                ResponseTemplate::new(206)
                    .insert_header(
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", body.len()),
                    )
                    .set_body_bytes(body[start as usize..=end as usize].to_vec())
            } else {
                ResponseTemplate::new(200)
                    .insert_header("Accept-Ranges", "bytes")
                    .insert_header("Content-Length", body.len().to_string())
                    .set_body_bytes(body.clone())
            }
        })
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = QueueManager::new(dir.clone(), state_file, 2);

    let url = format!("{}/file", server.uri());
    let id = manager.add(DownloadRequest {
        url,
        filename: Some("removed.bin".to_string()),
        headers: HashMap::new(),
    });

    // Confirm it's actually `Downloading` before removing it, so this
    // test exercises the in-flight path the fix targets.
    let mut downloading = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if manager
            .list()
            .iter()
            .any(|d| d.id == id && d.status == DownloadStatus::Downloading)
        {
            downloading = true;
            break;
        }
    }
    assert!(
        downloading,
        "expected the download to be Downloading before remove()"
    );

    manager.remove(&id);

    // Removed from the list immediately.
    assert!(manager.list().iter().all(|d| d.id != id));

    // Give the background task plenty of time to actually notice the
    // cancellation and run its deferred cleanup.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Nothing related to this download should remain on disk: not the
    // final destination file, not its staging file, not any part files.
    // A cleanup that races the still-running writer (deleting
    // immediately rather than waiting for it to actually stop) could let
    // that writer recreate one of these just after cleanup ran -- this
    // is exactly what a full directory scan after a generous wait would
    // catch.
    let leftover: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "state.json")
        .collect();
    assert!(
        leftover.is_empty(),
        "remove() on an in-flight download left files behind: {:?}",
        leftover
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn two_downloads_with_the_same_filename_do_not_corrupt_each_others_parts() {
    let server = MockServer::start().await;

    let body_a = vec![0xAAu8; 400_000];
    let body_b = vec![0xBBu8; 400_000];

    {
        let body_a = body_a.clone();
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(move |req: &wiremock::Request| respond_rangeable(req, &body_a))
            .mount(&server)
            .await;
    }
    {
        let body_b = body_b.clone();
        Mock::given(method("GET"))
            .and(path("/b"))
            .respond_with(move |req: &wiremock::Request| respond_rangeable(req, &body_b))
            .mount(&server)
            .await;
    }

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = QueueManager::new(dir.clone(), state_file, 2);

    // Both downloads deliberately request the exact same destination
    // filename -- the collision the fix targets is between their
    // internal part-file paths (previously derived purely from
    // `dest_path`), not between their ids.
    let id_a = manager.add(DownloadRequest {
        url: format!("{}/a", server.uri()),
        filename: Some("shared.bin".to_string()),
        headers: HashMap::new(),
    });
    let id_b = manager.add(DownloadRequest {
        url: format!("{}/b", server.uri()),
        filename: Some("shared.bin".to_string()),
        headers: HashMap::new(),
    });

    let mut both_done = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listed = manager.list();
        let a = listed.iter().find(|d| d.id == id_a).unwrap();
        let b = listed.iter().find(|d| d.id == id_b).unwrap();
        let settled = |d: &DownloadInfo| {
            !matches!(d.status, DownloadStatus::Downloading | DownloadStatus::Queued)
        };
        if settled(a) && settled(b) {
            both_done = true;
            break;
        }
    }
    assert!(both_done, "expected both downloads to settle");

    let listed = manager.list();
    let a = listed.iter().find(|d| d.id == id_a).unwrap();
    let b = listed.iter().find(|d| d.id == id_b).unwrap();
    assert_eq!(a.status, DownloadStatus::Completed, "entry a: {:?}", a);
    assert_eq!(b.status, DownloadStatus::Completed, "entry b: {:?}", b);

    // Both downloads asked for the same name, so `add()` gave the second
    // one a browser-style ` (1)` name instead of letting it overwrite the
    // first (I7). Each file must hold EXACTLY its own complete,
    // uncorrupted source body -- not an interleaved/corrupted mix, which
    // is what colliding, id-agnostic part-file paths would produce.
    assert_eq!(a.filename, "shared.bin");
    assert_eq!(b.filename, "shared (1).bin");
    assert_eq!(std::fs::read(&a.dest_path).unwrap(), body_a, "a's file corrupted");
    assert_eq!(std::fs::read(&b.dest_path).unwrap(), body_b, "b's file corrupted");

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_racing_a_successful_finish_never_leaves_a_committed_file() {
    // Regression test for the fix-round-2 finding: `run()`'s success
    // (`Ok(())`) match arm used to unconditionally call `mark_completed`,
    // with no check at all for whether `cancel()`/`remove()` had been
    // requested for this id while it was downloading. If the cancellation
    // signal lost the race against the download finishing successfully
    // anyway -- which small, delay-free downloads over loopback do
    // routinely, since nothing forces a mid-flight cancellation check
    // before the last byte is already written -- the fully-downloaded
    // file still got renamed into place at `dest_path` and left there: a
    // `Canceled` entry with a complete file on disk as if it had
    // succeeded.
    //
    // Forcing that exact race on demand isn't practical: the window the
    // fix closes spans a background task actually finishing its download
    // while a `cancel()`/`remove()` call happens concurrently on another
    // thread, and there's no supported hook to pause execution there from
    // a test. This instead follows the same strategy as
    // `pause_race_immediately_after_scheduling_is_never_silently_lost`:
    // many rapid, tiny, delay-free downloads (so each is likely to finish
    // before its segments ever notice a cancellation signal), each
    // immediately raced by a `cancel()` call from a separately spawned
    // task on a multi-threaded runtime. Not every iteration lands in the
    // race window -- some are still `Queued` when canceled, some get
    // genuinely interrupted mid-flight, and some may already be
    // `Completed` by the time `cancel()` runs (a legitimate no-op, not
    // the bug this targets) -- but across enough attempts, some reliably
    // do land in it. This asserts the invariant the fix guarantees for
    // every entry that ends up `Canceled`, regardless of which of those
    // actually happened: no committed file at `dest_path`.
    let server = MockServer::start().await;
    let body = vec![5u8; 500];

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = Arc::new(QueueManager::new(dir.clone(), state_file, 8));
    let url = format!("{}/file", server.uri());

    let mut ids = Vec::new();
    let mut race_tasks = Vec::new();
    for i in 0..200 {
        let id = manager.add(DownloadRequest {
            url: url.clone(),
            filename: Some(format!("race-cancel-{i}.bin")),
            headers: HashMap::new(),
        });
        ids.push(id.clone());

        let manager2 = Arc::clone(&manager);
        race_tasks.push(tokio::spawn(async move {
            manager2.cancel(&id);
        }));
    }

    for task in race_tasks {
        task.await.unwrap();
    }

    // Let anything still settling finish.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let listed = manager.list();
        if listed
            .iter()
            .all(|d| !matches!(d.status, DownloadStatus::Downloading | DownloadStatus::Queued))
        {
            break;
        }
    }

    let listed = manager.list();
    let mut saw_canceled = false;
    for id in &ids {
        let entry = listed.iter().find(|d| &d.id == id).unwrap();
        if entry.status == DownloadStatus::Canceled {
            saw_canceled = true;
            assert!(
                !entry.dest_path.exists(),
                "entry ended Canceled but a completed file was left behind at \
                 dest_path (the bug: `run()`'s `Ok(())` arm didn't check \
                 whether a cancel had been requested, so a cancel that lost \
                 the race against a successful finish still got its file \
                 committed): {:?}",
                entry
            );
        }
    }

    assert!(
        saw_canceled,
        "test never managed to get any entry to end Canceled -- can't \
         exercise the invariant it targets"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_racing_a_successful_finish_never_leaves_a_committed_file() {
    // Same underlying race as `cancel_racing_a_successful_finish_never_
    // leaves_a_committed_file`, but for `remove()`: by the time its
    // background task notices the cancellation (if it ever does -- see
    // that test's doc comment on why this can't be forced deterministically),
    // the entry is already gone from `downloads` entirely, so there's no
    // status to check -- only whether a completed file was left behind on
    // disk for an id nothing refers to any more.
    //
    // Uses `remove_and_delete_file()` rather than plain `remove()`: some of
    // these tiny downloads legitimately reach `Completed` *before* the
    // racing removal lands, and plain `remove()` now deliberately keeps a
    // completed download's file (C1 product ruling -- the file is only
    // deleted after the user confirms). For every non-`Completed` entry
    // the two are the exact same code path, so this still exercises the
    // race in full, and it lets the "nothing at all left on disk"
    // invariant stay exact.
    let server = MockServer::start().await;
    let body = vec![5u8; 500];

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = Arc::new(QueueManager::new(dir.clone(), state_file, 8));
    let url = format!("{}/file", server.uri());

    let mut dest_paths = Vec::new();
    let mut race_tasks = Vec::new();
    for i in 0..200 {
        let filename = format!("race-remove-{i}.bin");
        dest_paths.push(dir.join(&filename));

        let id = manager.add(DownloadRequest {
            url: url.clone(),
            filename: Some(filename),
            headers: HashMap::new(),
        });

        let manager2 = Arc::clone(&manager);
        race_tasks.push(tokio::spawn(async move {
            manager2.remove_and_delete_file(&id);
        }));
    }

    for task in race_tasks {
        task.await.unwrap();
    }

    assert!(
        manager.list().is_empty(),
        "every entry in this test was removed"
    );

    // Give any background task that was still writing plenty of time to
    // actually notice the cancellation, stop, and run its deferred cleanup
    // (or, pre-fix, to finish anyway and commit its file).
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let leftover: Vec<_> = dest_paths.iter().filter(|p| p.exists()).collect();
    assert!(
        leftover.is_empty(),
        "remove() left a completed file behind for a removed download \
         (the bug: `run()`'s `Ok(())` arm didn't check whether a remove had \
         been requested, so a remove that lost the race against a \
         successful finish still got its file committed): {:?}",
        leftover
    );
    // Nor any staging/part file.
    assert!(files_in(&dir).is_empty(), "leftover files: {:?}", files_in(&dir));

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_quick_resume_never_lets_a_stale_run_discard_a_newer_completed_file() {
    // Regression test for the fix-round-3 finding: `resolve_cancellation`/
    // `resolve_completion` used to decide a finished run's fate purely by
    // re-reading the entry's *current* status, special-casing only
    // `Paused` and treating everything else as `Discard`. That's
    // ambiguous: it can't tell "this status reflects MY run's own
    // outcome" from "this status reflects a DIFFERENT, newer run that has
    // since taken over the same id." Concretely: `pause()` sets `Paused`
    // and signals a `CancelToken`; if `resume()` is called quickly
    // afterward, it flips `Paused` -> `Queued`, and `schedule()` can spawn
    // a brand-new `run()` task (task B) for the same id *before* the
    // original task (task A, still unwinding from the earlier
    // cancellation signal) has actually returned. When task A's stale
    // `download_segmented` call eventually returns `Err(Canceled)`, the
    // old code's `resolve_cancellation` would see task B's *current*
    // status (e.g. `Completed`, if task B already finished) and return
    // `Discard`, deleting task B's legitimate, just-completed file.
    //
    // Forcing that exact interleaving deterministically isn't practical
    // (same caveat as this file's other race tests, e.g.
    // `pause_race_immediately_after_scheduling_is_never_silently_lost` and
    // `cancel_racing_a_successful_finish_never_leaves_a_committed_file`):
    // there's no supported hook to pause a background task at a specific
    // point. Instead, this fires `pause()` immediately followed by
    // `resume()` (no artificial delay between them) against many rapid,
    // small, slightly-delayed downloads on a multi-threaded runtime, so
    // some iterations are likely to land in the window the fix targets
    // (pause() actually reaching a Downloading entry, with its signal not
    // yet noticed by the time resume() respawns a new run). The invariant
    // asserted holds regardless of which run ends up "winning" for a
    // given id: status and file-presence must never disagree -- no
    // `Completed` status with a missing file (the bug this targets), and
    // no file on disk for an entry whose final status says it has none.
    let server = MockServer::start().await;
    let body = vec![2u8; 4_000];

    Mock::given(method("GET"))
        .respond_with(move |_req: &wiremock::Request| {
            // A small per-request delay so a `pause()` signal has a
            // realistic chance of landing before the in-flight request's
            // next cancellation check, giving an immediately-following
            // `resume()` room to spawn a new run before the old one
            // notices and returns -- the exact window this test targets.
            std::thread::sleep(Duration::from_millis(10));
            ResponseTemplate::new(200).set_body_bytes(body.clone())
        })
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = Arc::new(QueueManager::new(dir.clone(), state_file, 8));
    let url = format!("{}/file", server.uri());

    let mut ids = Vec::new();
    let mut race_tasks = Vec::new();
    for i in 0..200 {
        let id = manager.add(DownloadRequest {
            url: url.clone(),
            filename: Some(format!("race-pause-resume-{i}.bin")),
            headers: HashMap::new(),
        });
        ids.push(id.clone());

        let manager2 = Arc::clone(&manager);
        let id2 = id.clone();
        race_tasks.push(tokio::spawn(async move {
            manager2.pause(&id2);
            manager2.resume(&id2);
        }));
    }

    for task in race_tasks {
        task.await.unwrap();
    }

    // Let anything still settling finish.
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let listed = manager.list();
        if listed.iter().all(|d| {
            !matches!(
                d.status,
                DownloadStatus::Downloading | DownloadStatus::Queued | DownloadStatus::Paused
            )
        }) {
            break;
        }
    }

    let listed = manager.list();
    let mut saw_completed = false;
    for id in &ids {
        let entry = listed.iter().find(|d| &d.id == id).unwrap();
        let file_exists = entry.dest_path.exists();
        match entry.status {
            DownloadStatus::Completed => {
                saw_completed = true;
                assert!(
                    file_exists,
                    "entry ended Completed but its file is missing -- a \
                     stale, superseded run's Err(Canceled) must never let \
                     resolve_cancellation discard a newer run's \
                     legitimately-completed file: {:?}",
                    entry
                );
            }
            DownloadStatus::Canceled | DownloadStatus::Failed => {
                assert!(
                    !file_exists,
                    "entry ended {:?} but a file still exists at \
                     dest_path -- status and file-presence disagree: {:?}",
                    entry.status, entry
                );
            }
            _ => {}
        }
    }

    assert!(
        saw_completed,
        "test never got any entry to Completed -- can't exercise the \
         invariant it targets"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// Polls `manager` until `id`'s entry satisfies `done`, or `max_wait`
/// passes; returns the last observed entry either way.
async fn wait_for(
    manager: &QueueManager,
    id: &str,
    max_wait: Duration,
    done: impl Fn(&DownloadInfo) -> bool,
) -> DownloadInfo {
    let deadline = std::time::Instant::now() + max_wait;
    loop {
        let entry = manager.list().into_iter().find(|d| d.id == id);
        if let Some(entry) = &entry {
            if done(entry) {
                return entry.clone();
            }
        }
        if std::time::Instant::now() >= deadline {
            return entry.expect("entry vanished while waiting");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn status_is(status: DownloadStatus) -> impl Fn(&DownloadInfo) -> bool {
    move |d| d.status == status
}

/// Lists every file in `dir` except the queue's own state file (and its
/// atomic-write temp file, which can exist for an instant: a status change
/// is visible via `list()` slightly before its state-file write finishes).
fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "state.json" && name != "state.json.tmp")
        .collect();
    names.sort();
    names
}

/// C1 regression: a download only owns `dest_path` once it has committed
/// its finished file there. Before that -- Queued, Downloading, Paused --
/// anything at `dest_path` belongs to someone else (here: a file another
/// program created after `add()` picked the name), and cancel/remove must
/// never delete it. Only the download's own staging/part files go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_and_remove_never_delete_a_file_the_download_did_not_write() {
    let server = MockServer::start().await;
    let body: Vec<u8> = (0..400_000u32).map(|b| (b % 251) as u8).collect();
    {
        let body = body.clone();
        Mock::given(method("GET"))
            .respond_with(move |req: &wiremock::Request| {
                std::thread::sleep(Duration::from_millis(100));
                respond_rangeable(req, &body)
            })
            .mount(&server)
            .await;
    }

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 1);
    let url = format!("{}/file", server.uri());
    let add = |name: &str| {
        manager.add(DownloadRequest {
            url: url.clone(),
            filename: Some(name.to_string()),
            headers: HashMap::new(),
        })
    };
    let dest_of = |id: &str| {
        manager
            .list()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap()
            .dest_path
    };

    // Downloading, slowed by a speed limit so it can't finish first.
    let downloading = add("downloading.bin");
    manager.set_speed_limit(&downloading, Some(50_000));
    // Queued behind it (max_concurrent = 1).
    let queued = add("queued.bin");
    let removed_queued = add("removed.bin");

    wait_for(&manager, &downloading, Duration::from_secs(5), status_is(DownloadStatus::Downloading)).await;

    for id in [&downloading, &queued, &removed_queued] {
        std::fs::write(dest_of(id), b"someone else's precious file").unwrap();
    }

    manager.cancel(&queued);
    manager.remove(&removed_queued);
    // Let the downloading one write some part data first.
    tokio::time::sleep(Duration::from_millis(400)).await;
    manager.cancel(&downloading);

    wait_for(&manager, &downloading, Duration::from_secs(5), status_is(DownloadStatus::Canceled)).await;
    // Give the stopping run time to finish its own cleanup.
    tokio::time::sleep(Duration::from_millis(500)).await;

    for name in ["downloading.bin", "queued.bin", "removed.bin"] {
        assert_eq!(
            std::fs::read(dir.join(name)).unwrap(),
            b"someone else's precious file",
            "{name} was deleted/modified by a cancel/remove of a download that never wrote it"
        );
    }
    // ...and the downloads' own staging/part files are all gone.
    assert_eq!(
        files_in(&dir),
        vec!["downloading.bin", "queued.bin", "removed.bin"],
        "only the foreign files may remain"
    );

    // Paused (run fully stopped) then removed: same rule.
    let paused = add("paused.bin");
    manager.set_speed_limit(&paused, Some(50_000));
    wait_for(&manager, &paused, Duration::from_secs(5), status_is(DownloadStatus::Downloading)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    manager.pause(&paused);
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(dest_of(&paused), b"foreign").unwrap();
    manager.remove(&paused);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read(dir.join("paused.bin")).unwrap(), b"foreign");
    assert!(
        !files_in(&dir).iter().any(|n| n.contains(".part")),
        "paused download's partial files should be cleaned up: {:?}",
        files_in(&dir)
    );

    std::fs::remove_dir_all(dir).ok();
}

/// C1 product ruling: plain `remove()` of a Completed entry never deletes
/// its file; `remove_and_delete_file()` (only called after the UI's
/// confirmation) does.
#[tokio::test]
async fn remove_keeps_a_completed_file_but_remove_and_delete_file_deletes_it() {
    let server = MockServer::start().await;
    let body = vec![4u8; 1_000];
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let url = format!("{}/file", server.uri());

    let keep = manager.add(DownloadRequest {
        url: url.clone(),
        filename: Some("keep.bin".to_string()),
        headers: HashMap::new(),
    });
    let delete = manager.add(DownloadRequest {
        url,
        filename: Some("delete.bin".to_string()),
        headers: HashMap::new(),
    });

    let keep_entry =
        wait_for(&manager, &keep, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    let delete_entry =
        wait_for(&manager, &delete, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    assert_eq!(keep_entry.status, DownloadStatus::Completed);
    assert_eq!(delete_entry.status, DownloadStatus::Completed);

    manager.remove(&keep);
    assert!(manager.list().iter().all(|d| d.id != keep));
    assert_eq!(
        std::fs::read(&keep_entry.dest_path).unwrap(),
        body,
        "plain remove() must never delete a completed download's file"
    );

    manager.remove_and_delete_file(&delete);
    assert!(manager.list().iter().all(|d| d.id != delete));
    assert!(
        !delete_entry.dest_path.exists(),
        "remove_and_delete_file() must delete the completed file"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// I7: `add()` never picks a name that already exists in the download
/// folder; it uses the browser-style ` (1)` suffix instead.
#[test]
fn add_picks_a_free_name_instead_of_overwriting_an_existing_file() {
    let dir = tempfile_dir();
    std::fs::write(dir.join("report.pdf"), b"original").unwrap();
    std::fs::write(dir.join("report (1).pdf"), b"original 1").unwrap();

    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let id = manager.add(DownloadRequest {
        url: "https://example.invalid/report.pdf".to_string(),
        filename: Some("report.pdf".to_string()),
        headers: HashMap::new(),
    });

    let entry = manager.list().into_iter().find(|d| d.id == id).unwrap();
    assert_eq!(entry.filename, "report (2).pdf");
    assert_eq!(entry.dest_path, dir.join("report (2).pdf"));

    std::fs::remove_dir_all(dir).ok();
}

/// I7 defense in depth: if a file appears at the chosen name *after*
/// `add()` (another program, between add and finish), the finished
/// download is committed under the next free name rather than silently
/// replacing it.
#[tokio::test]
async fn finished_download_never_overwrites_a_file_that_appeared_meanwhile() {
    let server = MockServer::start().await;
    let body = vec![7u8; 2_000];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let id = manager.add(DownloadRequest {
        url: format!("{}/report.pdf", server.uri()),
        filename: Some("report.pdf".to_string()),
        headers: HashMap::new(),
    });

    std::fs::write(dir.join("report.pdf"), b"appeared meanwhile").unwrap();

    let entry =
        wait_for(&manager, &id, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    assert_eq!(entry.status, DownloadStatus::Completed, "{entry:?}");
    assert_eq!(entry.filename, "report (1).pdf");
    assert_eq!(entry.dest_path, dir.join("report (1).pdf"));
    assert_eq!(std::fs::read(&entry.dest_path).unwrap(), body);
    assert_eq!(
        std::fs::read(dir.join("report.pdf")).unwrap(),
        b"appeared meanwhile",
        "the other file must be untouched"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// I6: an untrusted filename is reduced to a single safe component in the
/// download folder -- it can never escape it.
#[test]
fn add_sanitizes_path_escaping_and_invalid_filenames() {
    use loadout::download::queue::sanitize_filename;

    assert_eq!(sanitize_filename("../../escape.bin").as_deref(), Some("escape.bin"));
    assert_eq!(
        sanitize_filename(r"C:\Users\x\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\evil.exe")
            .as_deref(),
        Some("evil.exe")
    );
    assert_eq!(sanitize_filename("/etc/passwd").as_deref(), Some("passwd"));
    assert_eq!(sanitize_filename("C:evil.exe").as_deref(), Some("C_evil.exe"));
    assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*.txt").as_deref(), Some("a_b_c_d_e_f_g_.txt"));
    assert_eq!(sanitize_filename("trailing. . ").as_deref(), Some("trailing"));
    assert_eq!(sanitize_filename("CON.txt").as_deref(), Some("_CON.txt"));
    assert_eq!(sanitize_filename(".."), None);
    assert_eq!(sanitize_filename("dir/"), None);
    assert_eq!(sanitize_filename("   "), None);

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    for (requested, expected) in [
        ("../../escape.bin", "escape.bin"),
        (r"..\..\Startup\evil.exe", "evil.exe"),
        ("..", "fromurl.bin"),
    ] {
        let id = manager.add(DownloadRequest {
            url: "https://example.invalid/fromurl.bin".to_string(),
            filename: Some(requested.to_string()),
            headers: HashMap::new(),
        });
        let entry = manager.list().into_iter().find(|d| d.id == id).unwrap();
        assert_eq!(entry.filename, expected, "for requested {requested:?}");
        assert_eq!(entry.dest_path.parent().unwrap(), dir.as_path());
    }

    std::fs::remove_dir_all(dir).ok();
}

/// I5: a Failed download can be retried, resumes through the normal
/// scheduling path, and keeps its in-memory request headers (here a
/// cookie the server insists on) so the retry is still authorized.
#[tokio::test]
async fn retry_requeues_a_failed_download_and_keeps_its_headers() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let server = MockServer::start().await;
    let body = vec![3u8; 5_000];
    let outage = Arc::new(AtomicBool::new(true));

    {
        let body = body.clone();
        let outage = outage.clone();
        Mock::given(method("GET"))
            .respond_with(move |req: &wiremock::Request| {
                if outage.load(Ordering::SeqCst) {
                    // Permanent-looking failure: not retried automatically.
                    return ResponseTemplate::new(404);
                }
                let cookie_ok = req
                    .headers
                    .get("Cookie")
                    .map(|v| v.to_str().unwrap() == "session=secret")
                    .unwrap_or(false);
                if !cookie_ok {
                    return ResponseTemplate::new(403);
                }
                ResponseTemplate::new(200).set_body_bytes(body.clone())
            })
            .mount(&server)
            .await;
    }

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let mut headers = HashMap::new();
    headers.insert("Cookie".to_string(), "session=secret".to_string());
    let id = manager.add(DownloadRequest {
        url: format!("{}/file.bin", server.uri()),
        filename: Some("file.bin".to_string()),
        headers,
    });

    let failed =
        wait_for(&manager, &id, Duration::from_secs(10), status_is(DownloadStatus::Failed)).await;
    assert_eq!(failed.status, DownloadStatus::Failed, "{failed:?}");
    assert!(failed.error.as_deref().unwrap_or("").contains("404"), "{failed:?}");

    // Not retryable from other states.
    manager.resume(&id);
    assert_eq!(
        manager.list().into_iter().find(|d| d.id == id).unwrap().status,
        DownloadStatus::Failed,
        "resume() must stay Paused-only"
    );

    outage.store(false, Ordering::SeqCst);
    manager.retry(&id);

    let done =
        wait_for(&manager, &id, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    assert_eq!(done.status, DownloadStatus::Completed, "{done:?}");
    assert_eq!(done.error, None);
    assert_eq!(std::fs::read(&done.dest_path).unwrap(), body);

    std::fs::remove_dir_all(dir).ok();
}

/// I1 (+ the parked T3 "progress unasserted" Minor): progress is updated
/// in memory continuously during a segmented download, but the state file
/// is only written on status changes -- not once per network chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_is_tracked_in_memory_but_only_status_changes_hit_disk() {
    let server = MockServer::start().await;
    let body: Vec<u8> = (0..1_200_000u32).map(|b| (b % 251) as u8).collect();
    {
        let body = body.clone();
        Mock::given(method("GET"))
            .respond_with(move |req: &wiremock::Request| respond_rangeable(req, &body))
            .mount(&server)
            .await;
    }

    let dir = tempfile_dir();
    let state_file = dir.join("state.json");
    let manager = QueueManager::new(dir.clone(), state_file.clone(), 2);
    // ~2s download, so there are many chunks and many progress samples.
    manager.set_default_speed_limit(Some(400_000));

    let id = manager.add(DownloadRequest {
        url: format!("{}/file.bin", server.uri()),
        filename: Some("file.bin".to_string()),
        headers: HashMap::new(),
    });

    wait_for(&manager, &id, Duration::from_secs(5), |d| {
        d.status == DownloadStatus::Downloading && d.downloaded_bytes > 0
    })
    .await;

    let writes_while_downloading = manager.persist_count();
    let mut samples = Vec::new();
    loop {
        let entry = manager.list().into_iter().find(|d| d.id == id).unwrap();
        if entry.status != DownloadStatus::Downloading {
            break;
        }
        samples.push(entry.downloaded_bytes);
        assert_eq!(
            manager.persist_count(),
            writes_while_downloading,
            "the state file was written while only progress changed"
        );
        let on_disk = load_state(&state_file);
        let on_disk = on_disk.iter().find(|d| d.id == id).unwrap();
        assert_eq!(on_disk.status, DownloadStatus::Downloading);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    samples.dedup();
    assert!(
        samples.len() >= 3,
        "expected progress to be reported several times during the segmented download, got {samples:?}"
    );
    assert!(samples.windows(2).all(|w| w[0] < w[1]), "progress must only grow: {samples:?}");

    let done =
        wait_for(&manager, &id, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    assert_eq!(done.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(&done.dest_path).unwrap(), body);

    let on_disk = load_state(&state_file);
    let on_disk = on_disk.iter().find(|d| d.id == id).unwrap();
    assert_eq!(on_disk.status, DownloadStatus::Completed);
    assert_eq!(on_disk.downloaded_bytes, body.len() as u64);
    assert!(
        manager.persist_count() <= writes_while_downloading + 2,
        "completion should cost one write, got {} -> {}",
        writes_while_downloading,
        manager.persist_count()
    );
    assert!(
        !dir.join("state.json.tmp").exists(),
        "atomic-write temp file must not be left behind"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// I3: pausing and immediately resuming a speed-limited segmented
/// download -- repeatedly -- must never let an old, still-winding-down run
/// and a new run write the same part files at once. The final file must be
/// byte-for-byte correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quick_pause_resume_under_a_speed_limit_never_corrupts_the_file() {
    let server = MockServer::start().await;
    let body: Vec<u8> = (0..1_000_000u32).map(|b| (b.wrapping_mul(7) % 251) as u8).collect();
    {
        let body = body.clone();
        Mock::given(method("GET"))
            .respond_with(move |req: &wiremock::Request| respond_rangeable(req, &body))
            .mount(&server)
            .await;
    }

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let id = manager.add(DownloadRequest {
        url: format!("{}/file.bin", server.uri()),
        filename: Some("file.bin".to_string()),
        headers: HashMap::new(),
    });
    manager.set_speed_limit(&id, Some(250_000));

    for _ in 0..4 {
        wait_for(&manager, &id, Duration::from_secs(5), |d| {
            d.status == DownloadStatus::Downloading
        })
        .await;
        tokio::time::sleep(Duration::from_millis(350)).await;
        manager.pause(&id);
        manager.resume(&id);
        let status = manager.list().into_iter().find(|d| d.id == id).unwrap().status;
        assert!(
            matches!(
                status,
                DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Completed
            ),
            "unexpected status right after pause+resume: {status:?}"
        );
    }

    let done =
        wait_for(&manager, &id, Duration::from_secs(30), status_is(DownloadStatus::Completed)).await;
    assert_eq!(done.status, DownloadStatus::Completed, "{done:?}");
    let written = std::fs::read(&done.dest_path).unwrap();
    assert_eq!(written.len(), body.len(), "wrong size: part files were corrupted");
    assert!(written == body, "content mismatch: part files were corrupted");
    assert_eq!(files_in(&dir), vec!["file.bin"], "leftover staging/part files");

    std::fs::remove_dir_all(dir).ok();
}

/// Mounts a mock that serves `body` only when the request carries
/// `Cookie: session=secret` -- 403 otherwise, like Pixeldrain and friends
/// do for a download link opened without the browser's session. With
/// `ranged`, it advertises and honors range requests (segmented path);
/// without, it never does (single-stream path). Returns counters of
/// (ranged GETs, plain GETs) that carried the cookie.
async fn mount_cookie_gated(
    server: &MockServer,
    body: Vec<u8>,
    ranged: bool,
) -> (Arc<std::sync::atomic::AtomicU32>, Arc<std::sync::atomic::AtomicU32>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    use wiremock::matchers::{any, header};

    let ranged_hits = Arc::new(AtomicU32::new(0));
    let plain_hits = Arc::new(AtomicU32::new(0));
    {
        let ranged_hits = ranged_hits.clone();
        let plain_hits = plain_hits.clone();
        Mock::given(header("Cookie", "session=secret"))
            .respond_with(move |req: &wiremock::Request| {
                if req.method.as_str() == "GET" {
                    if req.headers.get("Range").is_some() {
                        ranged_hits.fetch_add(1, Ordering::SeqCst);
                    } else {
                        plain_hits.fetch_add(1, Ordering::SeqCst);
                    }
                }
                if ranged {
                    respond_rangeable(req, &body)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(body.clone())
                }
            })
            .with_priority(1)
            .mount(server)
            .await;
    }
    Mock::given(any())
        .respond_with(ResponseTemplate::new(403))
        .with_priority(10)
        .mount(server)
        .await;

    (ranged_hits, plain_hits)
}

/// I11: request headers handed to `QueueManager::add` (here a session
/// cookie) actually go out over the wire on every request of a
/// *segmented* download, and the download fails with the server's 403
/// when they're missing.
#[tokio::test]
async fn cookie_header_reaches_the_server_on_the_segmented_path() {
    let server = MockServer::start().await;
    let body: Vec<u8> = (0..200_000u32).map(|b| (b % 251) as u8).collect();
    let (ranged_hits, _plain) = mount_cookie_gated(&server, body.clone(), true).await;

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let url = format!("{}/file.bin", server.uri());

    let mut headers = HashMap::new();
    headers.insert("Cookie".to_string(), "session=secret".to_string());
    let with_cookie = manager.add(DownloadRequest {
        url: url.clone(),
        filename: Some("with.bin".to_string()),
        headers,
    });
    let without_cookie = manager.add(DownloadRequest {
        url,
        filename: Some("without.bin".to_string()),
        headers: HashMap::new(),
    });

    let ok = wait_for(&manager, &with_cookie, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    assert_eq!(ok.status, DownloadStatus::Completed, "{ok:?}");
    assert_eq!(std::fs::read(&ok.dest_path).unwrap(), body);
    assert!(
        ranged_hits.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "expected several ranged (segmented) GETs carrying the cookie"
    );

    let denied = wait_for(&manager, &without_cookie, Duration::from_secs(10), status_is(DownloadStatus::Failed)).await;
    assert_eq!(denied.status, DownloadStatus::Failed, "{denied:?}");
    assert!(denied.error.as_deref().unwrap_or("").contains("403"), "{denied:?}");

    std::fs::remove_dir_all(dir).ok();
}

/// I11: same as above for a server without range support (single-stream
/// fallback path).
#[tokio::test]
async fn cookie_header_reaches_the_server_on_the_single_stream_path() {
    let server = MockServer::start().await;
    let body: Vec<u8> = (0..50_000u32).map(|b| (b % 251) as u8).collect();
    let (ranged_hits, plain_hits) = mount_cookie_gated(&server, body.clone(), false).await;

    let dir = tempfile_dir();
    let manager = QueueManager::new(dir.clone(), dir.join("state.json"), 2);
    let url = format!("{}/file.bin", server.uri());

    let mut headers = HashMap::new();
    headers.insert("Cookie".to_string(), "session=secret".to_string());
    let with_cookie = manager.add(DownloadRequest {
        url: url.clone(),
        filename: Some("with.bin".to_string()),
        headers,
    });
    let without_cookie = manager.add(DownloadRequest {
        url,
        filename: Some("without.bin".to_string()),
        headers: HashMap::new(),
    });

    let ok = wait_for(&manager, &with_cookie, Duration::from_secs(10), status_is(DownloadStatus::Completed)).await;
    assert_eq!(ok.status, DownloadStatus::Completed, "{ok:?}");
    assert_eq!(std::fs::read(&ok.dest_path).unwrap(), body);
    assert!(
        plain_hits.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "expected a plain (single-stream) GET carrying the cookie"
    );
    // The only ranged request allowed is the 1-byte fallback probe.
    assert!(ranged_hits.load(std::sync::atomic::Ordering::SeqCst) <= 1);

    let denied = wait_for(&manager, &without_cookie, Duration::from_secs(10), status_is(DownloadStatus::Failed)).await;
    assert_eq!(denied.status, DownloadStatus::Failed, "{denied:?}");
    assert!(denied.error.as_deref().unwrap_or("").contains("403"), "{denied:?}");

    std::fs::remove_dir_all(dir).ok();
}

/// Shared by `two_downloads_with_the_same_filename_do_not_corrupt_each_others_parts`'s
/// two mocks: answers a ranged request with the matching slice of `body`
/// (so segmented, resumable downloading is exercised), or the full `body`
/// with range-support headers for the initial probe.
fn respond_rangeable(req: &wiremock::Request, body: &[u8]) -> ResponseTemplate {
    if let Some(range) = req.headers.get("Range") {
        let range = range.to_str().unwrap();
        let (start, end) = parse_range(range);
        ResponseTemplate::new(206)
            .insert_header("Content-Range", format!("bytes {start}-{end}/{}", body.len()))
            .set_body_bytes(body[start as usize..=end as usize].to_vec())
    } else {
        ResponseTemplate::new(200)
            .insert_header("Accept-Ranges", "bytes")
            .insert_header("Content-Length", body.len().to_string())
            .set_body_bytes(body.to_vec())
    }
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
