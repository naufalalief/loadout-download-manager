use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use loadout::download::queue::QueueManager;
use loadout::download::types::{DownloadInfo, DownloadStatus};
use loadout::extract::jobs::ExtractJobManager;
use loadout::server::{PairingState, router_for_port};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{any, header};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Source for a trivial native stub "yt-dlp"/"ffmpeg" binary used by this
/// file's `/extract` tests, compiled once (see [`stub_binary_path`]) and
/// shared as both the `ytdlp_path` and `ffmpeg_path` arguments to
/// `ExtractJobManager::new_with_binaries`. It does nothing but exit `0`
/// immediately -- a compiled native stub rather than a `.cmd` batch file,
/// because cmd.exe's own `%`-expansion mangles yt-dlp's
/// `-o "%(title)s.%(ext)s"` template argument (see Task 2's own stub for
/// the same reasoning).
///
/// These tests exercise the `/extract` HTTP route's auth/validation/wiring
/// (token check, URL-scheme check, `probe()` being called and its id
/// showing up in `list()`), never yt-dlp's actual probe/download behavior
/// -- that's already covered by `extract::jobs`'s own tests -- so the stub
/// doesn't need to emit anything meaningful.
const STUB_SOURCE: &str = "fn main() {}";

fn stub_binary_path() -> std::path::PathBuf {
    static STUB_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    STUB_PATH.get_or_init(compile_stub).clone()
}

fn compile_stub() -> std::path::PathBuf {
    let dir = tempfile_dir();
    let src_path = dir.join("stub_ytdlp.rs");
    std::fs::write(&src_path, STUB_SOURCE).unwrap();
    let exe_path = dir.join("stub_ytdlp.exe");

    let output = std::process::Command::new("rustc")
        .arg("--edition")
        .arg("2021")
        .arg(&src_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("failed to invoke rustc to build the test stub yt-dlp binary");

    assert!(
        output.status.success(),
        "failed to compile stub yt-dlp binary:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    exe_path
}

fn new_extract_jobs(download_dir: &std::path::Path) -> Arc<ExtractJobManager> {
    let stub = stub_binary_path();
    Arc::new(ExtractJobManager::new_with_binaries(
        download_dir.to_path_buf(),
        stub.clone(),
        stub,
        Arc::new(|| {}),
    ))
}

/// Spins up `server::router(...)` on an OS-assigned ephemeral port (rather
/// than the fixed `HANDOFF_PORT`, so tests can run concurrently / repeatedly
/// without port collisions) and returns its base URL plus the `QueueManager`/
/// `PairingState` handles the router was built with, so tests can both make
/// HTTP requests against it and inspect/compare against its backing state
/// directly.
///
/// The router is built with `router_for_port` for the ephemeral port, so
/// its `Host`-header check accepts `127.0.0.1:<that port>` -- exactly what
/// reqwest sends for the returned base URL.
async fn setup() -> (String, Arc<QueueManager>, Arc<PairingState>) {
    let (base_url, queue, pairing, _extract_jobs, _dir) = setup_with_extract().await;
    (base_url, queue, pairing)
}

async fn setup_with_dir() -> (String, Arc<QueueManager>, Arc<PairingState>, std::path::PathBuf) {
    let (base_url, queue, pairing, _extract_jobs, dir) = setup_with_extract().await;
    (base_url, queue, pairing, dir)
}

async fn setup_with_extract() -> (
    String,
    Arc<QueueManager>,
    Arc<PairingState>,
    Arc<ExtractJobManager>,
    std::path::PathBuf,
) {
    let dir = tempfile_dir();
    let state_file = dir.join("queue_state.json");
    let storage_file = dir.join("paired_profiles.json");

    let queue = Arc::new(QueueManager::new(dir.clone(), state_file, 2));
    let pairing = Arc::new(PairingState::new(storage_file));
    let extract_jobs = new_extract_jobs(&dir);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    let app = router_for_port(
        Arc::clone(&queue),
        Arc::clone(&pairing),
        Arc::clone(&extract_jobs),
        addr.port(),
    );

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), queue, pairing, extract_jobs, dir)
}

/// Pairs a fresh client against `base_url` (the very first, zero-click
/// pairing on a server whose `PairingState` has no profiles yet) and
/// returns its token. Most tests below only need *a* valid token to
/// exercise `/downloads`/`/extract` with, not the pairing flow itself.
async fn pair_and_get_token(base_url: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "expected pairing to succeed");
    let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    body["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn ping_responds_ok_without_auth() {
    let (base_url, _queue, _pairing) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/ping"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn pair_succeeds_once_then_rejects_further_attempts() {
    let (base_url, _queue, _pairing) = setup().await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let body: serde_json::Value =
        serde_json::from_str(&first.text().await.unwrap()).unwrap();
    assert!(!body["token"].as_str().unwrap().is_empty());

    let second = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 403);
    let body: serde_json::Value = serde_json::from_str(&second.text().await.unwrap()).unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "pairing_closed");
    assert!(
        body["message"].as_str().unwrap().contains("Allow new pairing"),
        "{body:?}"
    );
}

/// A `POST /pair` request carrying a non-extension `Origin` header (as an
/// ordinary web page's own cross-origin `fetch()` would, even with
/// `mode: 'no-cors'`) must be rejected with `403` -- and, critically, must
/// NOT consume the first, zero-click pairing slot: a legitimate follow-up
/// request (no `Origin`, or a `chrome-extension://` origin) must still be
/// able to pair afterwards.
#[tokio::test]
async fn pair_rejects_non_extension_origin_without_consuming_pairing() {
    let (base_url, _queue, pairing) = setup().await;
    let client = reqwest::Client::new();

    let malicious = client
        .post(format!("{base_url}/pair"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(malicious.status(), 403);
    assert!(
        pairing.list().is_empty(),
        "a rejected /pair with a foreign Origin must not consume pairing"
    );

    let legitimate = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        legitimate.status(),
        200,
        "a legitimate pair attempt must still succeed after a malicious one was rejected"
    );
    let body: serde_json::Value =
        serde_json::from_str(&legitimate.text().await.unwrap()).unwrap();
    assert!(!body["token"].as_str().unwrap().is_empty());
}

/// A `POST /pair` request carrying a `chrome-extension://` origin -- what a
/// real extension's own service-worker `fetch()` sends when it does include
/// `Origin` -- must succeed normally.
#[tokio::test]
async fn pair_succeeds_with_chrome_extension_origin() {
    let (base_url, _queue, _pairing) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base_url}/pair"))
        .header("Origin", "chrome-extension://abcdefghijklmnopabcdefghijklmnop")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    assert!(!body["token"].as_str().unwrap().is_empty());
}

/// After a successful `/pair`, `/unpair` with that same token removes that
/// one profile -- leaving the paired-profile list empty again, which is
/// equivalent to a fresh install: a following `/pair` call succeeds again
/// (zero-click, no pairing mode needed), minting a brand-new token rather
/// than handing back the one just removed.
#[tokio::test]
async fn unpair_with_valid_token_reopens_pairing() {
    let (base_url, _queue, pairing) = setup().await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let body: serde_json::Value = serde_json::from_str(&first.text().await.unwrap()).unwrap();
    let token = body["token"].as_str().unwrap().to_string();

    let unpair_resp = client
        .post(format!("{base_url}/unpair"))
        .header("X-Dm-Token", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(unpair_resp.status(), 204);
    assert!(pairing.list().is_empty());
    assert!(!pairing.is_token_valid(&token));

    let second = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let body: serde_json::Value = serde_json::from_str(&second.text().await.unwrap()).unwrap();
    let new_token = body["token"].as_str().unwrap().to_string();
    assert!(!new_token.is_empty());
}

/// `/unpair` rejects a missing or wrong token with `401` and leaves pairing
/// untouched -- an arbitrary local caller that doesn't hold a real token
/// can't force a real, paired profile to be kicked off.
#[tokio::test]
async fn unpair_rejects_missing_or_wrong_token() {
    let (base_url, _queue, pairing) = setup().await;
    let client = reqwest::Client::new();

    let paired = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(paired.status(), 200);

    let missing = client
        .post(format!("{base_url}/unpair"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);
    assert_eq!(pairing.list().len(), 1);

    let wrong = client
        .post(format!("{base_url}/unpair"))
        .header("X-Dm-Token", "definitely-not-the-right-token")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert_eq!(pairing.list().len(), 1);
}

/// Once a first profile is already paired, `/pair` stays closed (`403`,
/// with the explanatory body) until `PairingState::allow_new_pairing` opens
/// it -- at which point it succeeds and mints a *second*, independent
/// token; both tokens stay valid afterwards (pairing a second profile never
/// invalidates the first).
#[tokio::test]
async fn allow_new_pairing_opens_a_second_pairing_window() {
    let (base_url, _queue, pairing) = setup().await;
    let client = reqwest::Client::new();

    let first_token = pair_and_get_token(&base_url).await;

    let closed = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 403);

    pairing.allow_new_pairing(Duration::from_secs(60));
    assert!(pairing.pairing_mode_remaining().is_some());

    let opened = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(opened.status(), 200);
    let body: serde_json::Value = serde_json::from_str(&opened.text().await.unwrap()).unwrap();
    let second_token = body["token"].as_str().unwrap().to_string();

    assert_ne!(first_token, second_token);
    assert!(pairing.is_token_valid(&first_token));
    assert!(pairing.is_token_valid(&second_token));
    assert_eq!(pairing.list().len(), 2);
}

/// Pairing mode closes again once its duration elapses -- a `/pair` call
/// just after expiry is rejected the same way it would be if pairing mode
/// had never been opened.
#[tokio::test]
async fn pairing_mode_expires_after_its_duration() {
    let (base_url, _queue, pairing) = setup().await;
    let client = reqwest::Client::new();
    let _first = pair_and_get_token(&base_url).await;

    pairing.allow_new_pairing(Duration::from_millis(30));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(pairing.pairing_mode_remaining().is_none());

    let resp = client
        .post(format!("{base_url}/pair"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

/// Forgetting one paired profile by id (the Settings panel's per-row FORGET
/// button) removes only that profile -- every other paired profile's token
/// stays valid.
#[tokio::test]
async fn forget_by_id_removes_only_that_profile() {
    let (_base_url, _queue, pairing) = setup().await;

    let first = pairing.try_pair().expect("first pairing should succeed");
    pairing.allow_new_pairing(Duration::from_secs(60));
    let second = pairing.try_pair().expect("second pairing should succeed");

    let profiles = pairing.list();
    assert_eq!(profiles.len(), 2);
    // `list()` deliberately never exposes raw tokens (see `PairedProfileInfo`),
    // so the first profile is identified by `try_pair`'s own default-label
    // convention ("Browser" for the very first, "Browser N" after) instead.
    let first_id = profiles
        .iter()
        .find(|p| p.label == "Browser")
        .map(|p| p.id.clone())
        .unwrap();

    assert!(pairing.forget_by_id(&first_id));
    assert!(!pairing.is_token_valid(&first));
    assert!(pairing.is_token_valid(&second));
    assert_eq!(pairing.list().len(), 1);

    assert!(!pairing.forget_by_id(&first_id), "forgetting twice removes nothing the second time");
}

/// Renaming a paired profile only changes its label -- never its token's
/// validity.
#[tokio::test]
async fn rename_updates_label_without_touching_token_validity() {
    let (_base_url, _queue, pairing) = setup().await;
    let token = pairing.try_pair().expect("pairing should succeed");
    let id = pairing.list()[0].id.clone();

    assert!(pairing.rename(&id, "Work laptop, Chrome profile 2"));
    assert_eq!(pairing.list()[0].label, "Work laptop, Chrome profile 2");
    assert!(pairing.is_token_valid(&token));

    assert!(!pairing.rename("not-a-real-id", "x"));
}

#[tokio::test]
async fn downloads_rejects_missing_or_wrong_token() {
    let (base_url, _queue, _pairing) = setup().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "url": "https://example.invalid/file.bin",
        "filename": null,
        "headers": {}
    });

    let missing = client
        .post(format!("{base_url}/downloads"))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);

    let wrong = client
        .post(format!("{base_url}/downloads"))
        .header("X-Dm-Token", "definitely-not-the-right-token")
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn downloads_rejects_malformed_body_without_touching_queue() {
    let (base_url, queue, _pairing) = setup().await;
    let token = pair_and_get_token(&base_url).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base_url}/downloads"))
        .header("X-Dm-Token", token)
        .header("Content-Type", "application/json")
        .body("this is not valid json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    assert!(queue.list().is_empty());
}

/// C1: a `/downloads` body whose `url` isn't `http://`/`https://` (e.g. a
/// `blob:` URL, which `reqwest` has no way to fetch and which the browser's
/// own native download has typically already been canceled for by the time
/// this request is made) must be rejected with `400` and never reach the
/// queue -- defense in depth alongside the extension's own scheme check.
#[tokio::test]
async fn downloads_rejects_non_http_url_without_touching_queue() {
    let (base_url, queue, _pairing) = setup().await;
    let token = pair_and_get_token(&base_url).await;
    let client = reqwest::Client::new();

    for url in [
        "blob:https://example.com/abc-123",
        "data:text/plain;base64,aGVsbG8=",
        "not a url at all",
    ] {
        let body = serde_json::json!({
            "url": url,
            "filename": "file.bin",
            "headers": {}
        });

        let resp = client
            .post(format!("{base_url}/downloads"))
            .header("X-Dm-Token", &token)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400, "expected 400 for url {url:?}");
    }

    assert!(queue.list().is_empty());
}

#[tokio::test]
async fn downloads_with_valid_token_and_body_adds_to_queue() {
    let (base_url, queue, _pairing) = setup().await;
    let token = pair_and_get_token(&base_url).await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "url": "https://example.invalid/file.bin",
        "filename": "file.bin",
        "headers": {}
    });

    let resp = client
        .post(format!("{base_url}/downloads"))
        .header("X-Dm-Token", &token)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let parsed: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    let id = parsed["id"].as_str().unwrap().to_string();

    let listed = queue.list();
    assert!(listed.iter().any(|d| d.id == id));
}

#[tokio::test]
async fn extract_rejects_missing_or_wrong_token_without_probing() {
    let (base_url, _queue, _pairing, extract_jobs, _dir) = setup_with_extract().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "url": "https://example.invalid/video",
        "headers": {}
    });

    let missing = client
        .post(format!("{base_url}/extract"))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);

    let wrong = client
        .post(format!("{base_url}/extract"))
        .header("X-Dm-Token", "definitely-not-the-right-token")
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    assert!(extract_jobs.list().is_empty());
}

/// Same C1 defense in depth as `/downloads`: a `/extract` body whose `url`
/// isn't `http://`/`https://` must be rejected with `400` and never start a
/// probe.
#[tokio::test]
async fn extract_rejects_non_http_url_without_probing() {
    let (base_url, _queue, _pairing, extract_jobs, _dir) = setup_with_extract().await;
    let token = pair_and_get_token(&base_url).await;
    let client = reqwest::Client::new();

    for url in [
        "blob:https://example.com/abc-123",
        "data:text/plain;base64,aGVsbG8=",
        "not a url at all",
    ] {
        let body = serde_json::json!({ "url": url, "headers": {} });

        let resp = client
            .post(format!("{base_url}/extract"))
            .header("X-Dm-Token", &token)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400, "expected 400 for url {url:?}");
    }

    assert!(extract_jobs.list().is_empty());
}

#[tokio::test]
async fn extract_with_valid_token_and_http_url_returns_201_and_is_listed() {
    let (base_url, _queue, _pairing, extract_jobs, _dir) = setup_with_extract().await;
    let token = pair_and_get_token(&base_url).await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "url": "https://example.invalid/video",
        "headers": {}
    });

    let resp = client
        .post(format!("{base_url}/extract"))
        .header("X-Dm-Token", &token)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let parsed: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    let id = parsed["id"].as_str().unwrap().to_string();

    let listed = extract_jobs.list();
    assert!(listed.iter().any(|j| j.id == id), "{listed:?}");
}

/// Fires several concurrent `POST /pair` requests at the *same* running
/// server/`PairingState` instance (via `tokio::join!`, so they're all
/// in-flight together rather than sequential) and asserts exactly one gets
/// `200` and the rest get `403`.
///
/// This still exercises the same race the old single-token design's atomic
/// `compare_exchange` was built to close: with the paired-profile list
/// starting empty, `try_pair`'s emptiness check and its insert happen under
/// one `Mutex` lock, so exactly one concurrent caller can ever win that
/// first, zero-click slot, regardless of scheduling. It runs on a
/// multi-threaded runtime so the handlers genuinely execute in parallel on
/// different OS threads (on the default single-threaded test runtime they
/// could only interleave at `.await` points).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pair_requests_yield_exactly_one_success() {
    let (base_url, _queue, _pairing) = setup().await;
    let client = reqwest::Client::new();

    let fire = |n: u32| {
        let client = client.clone();
        let base_url = base_url.clone();
        async move {
            let _ = n;
            client
                .post(format!("{base_url}/pair"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };

    let (s1, s2, s3, s4, s5, s6, s7, s8) = tokio::join!(
        fire(1),
        fire(2),
        fire(3),
        fire(4),
        fire(5),
        fire(6),
        fire(7),
        fire(8),
    );
    let statuses = [s1, s2, s3, s4, s5, s6, s7, s8];

    let ok_count = statuses.iter().filter(|&&s| s == 200).count();
    let forbidden_count = statuses.iter().filter(|&&s| s == 403).count();

    assert_eq!(
        ok_count, 1,
        "expected exactly one 200 among concurrent /pair calls, got statuses: {statuses:?}"
    );
    assert_eq!(
        forbidden_count,
        statuses.len() - 1,
        "expected all non-winning calls to get 403, got statuses: {statuses:?}"
    );
}

/// An unparseable (or otherwise invalid) `paired_profiles.json` is treated
/// the same as "no profiles yet" -- the storage format being corrupt (e.g.
/// truncated by a crash mid-write) must never crash the app on startup,
/// and reopens the zero-click first-pairing slot rather than permanently
/// locking everyone out.
#[test]
fn pairing_state_treats_an_unparseable_storage_file_as_no_profiles() {
    let dir = tempfile_dir();
    let storage_file = dir.join("paired_profiles.json");
    std::fs::write(&storage_file, "not valid json").unwrap();

    let pairing = PairingState::new(storage_file);
    assert!(pairing.list().is_empty());
    assert!(pairing.try_pair().is_some());
}

/// A pre-multi-profile install's single `pairing_token.txt` (+ its
/// `.paired` marker, proving it was actually paired, not just that a token
/// had been generated) is migrated into a one-entry paired-profile list the
/// first time `PairingState::new` runs over a `storage_file` that doesn't
/// exist yet -- so upgrading the app never silently drops an existing,
/// real pairing.
#[test]
fn pairing_state_migrates_a_legacy_single_token_install() {
    let dir = tempfile_dir();
    let legacy_token_file = dir.join("pairing_token.txt");
    std::fs::write(&legacy_token_file, "legacy-token-abc123").unwrap();
    std::fs::write(dir.join("pairing_token.txt.paired"), b"paired").unwrap();

    let storage_file = dir.join("paired_profiles.json");
    assert!(!storage_file.exists());

    let pairing = PairingState::new(storage_file.clone());

    assert!(pairing.is_token_valid("legacy-token-abc123"));
    let profiles = pairing.list();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].label, "Browser (migrated)");
    assert!(storage_file.exists(), "migration must persist the new format");

    // A fresh pairing attempt stays closed -- the migrated install already
    // counts as "a profile is paired", so it's not treated as a new
    // install's zero-click slot.
    assert!(pairing.try_pair().is_none());
}

/// A legacy token file that exists but was never actually marked paired
/// (no `.paired` marker -- e.g. the app generated a token on a prior run
/// but `/pair` was never called) must NOT be migrated: nothing was ever
/// really paired, so the new storage starts empty, same as any other fresh
/// install.
#[test]
fn pairing_state_does_not_migrate_an_unpaired_legacy_token_file() {
    let dir = tempfile_dir();
    std::fs::write(dir.join("pairing_token.txt"), "generated-but-never-paired").unwrap();

    let storage_file = dir.join("paired_profiles.json");
    let pairing = PairingState::new(storage_file);

    assert!(pairing.list().is_empty());
    assert!(!pairing.is_token_valid("generated-but-never-paired"));
}

/// I6(c): once a profile is paired, a restart (a fresh `PairingState::new`
/// over the same `storage_file` -- which happens at every login) must not
/// reopen the zero-click first-pairing slot, and the same token keeps
/// working.
#[test]
fn pairing_state_stays_paired_across_restarts() {
    let dir = tempfile_dir();
    let storage_file = dir.join("paired_profiles.json");

    let first = PairingState::new(storage_file.clone());
    assert!(first.list().is_empty());
    let token = first.try_pair().expect("first pairing should succeed");
    drop(first);

    let restarted = PairingState::new(storage_file.clone());
    assert!(
        !restarted.list().is_empty(),
        "pairing must persist across restarts"
    );
    assert!(
        restarted.try_pair().is_none(),
        "the zero-click slot must stay closed after a restart"
    );
    assert!(
        restarted.is_token_valid(&token),
        "the same token keeps working"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// Same as above, end to end over HTTP: `/pair` succeeds on the first
/// server, and a second server (simulated restart) over the same storage
/// file rejects it.
#[tokio::test]
async fn pair_endpoint_stays_closed_after_a_restart() {
    let dir = tempfile_dir();
    let storage_file = dir.join("paired_profiles.json");
    let client = reqwest::Client::new();

    async fn serve_once(dir: &std::path::Path, storage_file: &std::path::Path) -> String {
        let queue = Arc::new(QueueManager::new(dir.to_path_buf(), dir.join("q.json"), 2));
        let pairing = Arc::new(PairingState::new(storage_file.to_path_buf()));
        let extract_jobs = new_extract_jobs(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router_for_port(queue, pairing, extract_jobs, addr.port());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    let first = serve_once(&dir, &storage_file).await;
    let resp = client.post(format!("{first}/pair")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let second = serve_once(&dir, &storage_file).await;
    let resp = client.post(format!("{second}/pair")).send().await.unwrap();
    assert_eq!(resp.status(), 403);
}

async fn wait_for_status(queue: &QueueManager, id: &str, status: DownloadStatus) -> DownloadInfo {
    for _ in 0..400 {
        let entry = queue.list().into_iter().find(|d| d.id == id).unwrap();
        if entry.status == status {
            return entry;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    queue.list().into_iter().find(|d| d.id == id).unwrap()
}

async fn post_download(
    base_url: &str,
    token: &str,
    url: &str,
    filename: &str,
    headers: HashMap<&str, &str>,
) -> String {
    let body = serde_json::json!({ "url": url, "filename": filename, "headers": headers });
    let resp = reqwest::Client::new()
        .post(format!("{base_url}/downloads"))
        .header("X-Dm-Token", token)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let parsed: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    parsed["id"].as_str().unwrap().to_string()
}

/// I11: a header sent in a `POST /downloads` body's `headers` field (here
/// the session cookie a site like Pixeldrain requires, else 403) makes it
/// all the way through handoff server -> queue -> engine -> the wire.
#[tokio::test]
async fn cookie_from_handoff_request_reaches_the_remote_server() {
    let remote = MockServer::start().await;
    let file_body: Vec<u8> = (0..20_000u32).map(|b| (b % 251) as u8).collect();
    Mock::given(header("Cookie", "session=secret"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(file_body.clone()))
        .with_priority(1)
        .mount(&remote)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(403))
        .with_priority(10)
        .mount(&remote)
        .await;

    let (base_url, queue, _pairing) = setup().await;
    let token = pair_and_get_token(&base_url).await;
    let url = format!("{}/file.bin", remote.uri());

    let ok_id = post_download(
        &base_url,
        &token,
        &url,
        "with.bin",
        HashMap::from([("Cookie", "session=secret")]),
    )
    .await;
    let denied_id =
        post_download(&base_url, &token, &url, "without.bin", HashMap::new()).await;

    let ok = wait_for_status(&queue, &ok_id, DownloadStatus::Completed).await;
    assert_eq!(ok.status, DownloadStatus::Completed, "{ok:?}");
    assert_eq!(std::fs::read(&ok.dest_path).unwrap(), file_body);

    let denied = wait_for_status(&queue, &denied_id, DownloadStatus::Failed).await;
    assert_eq!(denied.status, DownloadStatus::Failed, "{denied:?}");
    assert!(denied.error.as_deref().unwrap_or("").contains("403"), "{denied:?}");
}

/// I8: the default speed limit (set from Settings) applies to downloads
/// handed off through the server too, not just UI-added ones.
#[tokio::test]
async fn default_speed_limit_applies_to_handoff_downloads() {
    let remote = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 300_000]))
        .mount(&remote)
        .await;

    let (base_url, queue, _pairing) = setup().await;
    let token = pair_and_get_token(&base_url).await;
    // 100 KB/s: one second of burst, then 200 KB more at 100 KB/s => ~2s.
    queue.set_default_speed_limit(Some(100_000));

    let start = std::time::Instant::now();
    let id = post_download(
        &base_url,
        &token,
        &format!("{}/file.bin", remote.uri()),
        "limited.bin",
        HashMap::new(),
    )
    .await;
    let done = wait_for_status(&queue, &id, DownloadStatus::Completed).await;
    assert_eq!(done.status, DownloadStatus::Completed, "{done:?}");
    assert!(
        start.elapsed() >= Duration::from_millis(1500),
        "a 300KB download under a 100KB/s default limit finished in {:?}",
        start.elapsed()
    );
}

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

/// I6(a): a filename in a `/downloads` body that tries to escape the
/// download folder (relative `..` traversal or an absolute path, e.g. into
/// the Startup folder) is reduced to its last component inside it.
#[tokio::test]
async fn downloads_cannot_escape_the_download_folder_via_filename() {
    let (base_url, queue, _pairing, dir) = setup_with_dir().await;
    let token = pair_and_get_token(&base_url).await;
    let client = reqwest::Client::new();

    let startup_path = [
        "C:", "Users", "victim", "AppData", "Roaming", "Microsoft", "Windows", "Start Menu",
        "Programs", "Startup", "evil.exe",
    ]
    .join("\\");

    for (filename, expected) in [
        ("../../escape.bin".to_string(), "escape.bin"),
        (startup_path, "evil.exe"),
        ("..".to_string(), "fromurl.bin"),
    ] {
        let body = serde_json::json!({
            "url": "https://example.invalid/fromurl.bin",
            "filename": filename,
            "headers": {}
        });
        let resp = client
            .post(format!("{base_url}/downloads"))
            .header("X-Dm-Token", &token)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let parsed: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
        let id = parsed["id"].as_str().unwrap();

        let entry = queue.list().into_iter().find(|d| d.id == id).unwrap();
        assert_eq!(entry.filename, expected, "for {filename:?}");
        assert_eq!(
            entry.dest_path.parent().unwrap(),
            dir.as_path(),
            "{filename:?} escaped the download folder: {:?}",
            entry.dest_path
        );
    }
}

/// Sends a raw HTTP/1.1 request with exactly the given `Host` header line
/// (or none), bypassing reqwest (which always derives `Host` from the
/// URL), and returns the response's status code.
async fn raw_status(base_url: &str, method: &str, path: &str, host: Option<&str>) -> u16 {
    let addr = base_url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let host_line = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\n{host_line}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    text.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// I6(b) DNS rebinding: a page on an attacker's domain that re-resolves to
/// 127.0.0.1 still sends its own domain in `Host`. Every route -- and in
/// particular `/pair` -- must reject anything but the loopback host:port,
/// before doing anything (the rejected `/pair` must not consume pairing).
#[tokio::test]
async fn requests_with_a_foreign_or_missing_host_header_are_rejected() {
    let (base_url, _queue, pairing) = setup().await;
    let port = base_url.rsplit(':').next().unwrap().to_string();

    for path in ["/ping", "/pair", "/downloads", "/extract"] {
        let method = if path == "/ping" { "GET" } else { "POST" };
        assert_eq!(
            raw_status(&base_url, method, path, Some(&format!("evil.example:{port}"))).await,
            403,
            "{path} with a foreign Host"
        );
        assert_eq!(
            raw_status(&base_url, method, path, Some("evil.example")).await,
            403,
            "{path} with a foreign Host (no port)"
        );
        assert_eq!(
            raw_status(&base_url, method, path, Some("127.0.0.1:1")).await,
            403,
            "{path} with the wrong port"
        );
    }
    assert!(pairing.list().is_empty(), "a rejected /pair must not consume pairing");

    // HTTP/1.1 requires Host; a request without one is rejected too
    // (by hyper itself or by the Host check -- either way, never served).
    let missing = raw_status(&base_url, "GET", "/ping", None).await;
    assert!(missing == 400 || missing == 403, "missing Host got {missing}");

    // The legitimate forms still work.
    assert_eq!(
        raw_status(&base_url, "GET", "/ping", Some(&format!("127.0.0.1:{port}"))).await,
        200
    );
    assert_eq!(
        raw_status(&base_url, "GET", "/ping", Some(&format!("localhost:{port}"))).await,
        200
    );
}
