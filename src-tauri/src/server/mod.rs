//! Local "handoff" HTTP server.
//!
//! Lets an external, local-only client (e.g. a browser extension) pair with
//! this app -- via a random token persisted to disk over `/pair` -- and then
//! hand off download requests to the same `QueueManager` the Tauri UI itself
//! uses, via `/downloads`. More than one client (e.g. one per Chrome
//! profile) can be paired at once -- see [`PairingState`].
//!
//! Bound to `127.0.0.1` only (see [`serve`]): this is a local handoff
//! mechanism, never meant to be reachable from the network. Every request
//! must also carry a `Host` header naming that loopback address (see
//! [`router_for_port`]), which defeats DNS-rebinding attacks: a web page on
//! `evil.example` whose DNS is flipped to `127.0.0.1` still sends
//! `Host: evil.example`, and is rejected before any route runs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header::HOST, header::ORIGIN};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::download::queue::QueueManager;
use crate::download::types::DownloadRequest;

/// Fixed local port the handoff server listens on.
pub const HANDOFF_PORT: u16 = 57431;

/// Name of the HTTP header a caller must send on `/downloads`, carrying the
/// token obtained from a prior, successful `/pair`.
const TOKEN_HEADER: &str = "X-Dm-Token";

/// How long [`PairingState::allow_new_pairing`] opens `/pair` back up for.
pub const PAIRING_MODE_DURATION: Duration = Duration::from_secs(120);

/// One paired client: its own token, a user-facing label, and when it
/// paired (Unix seconds -- formatted into a real date/time by the frontend,
/// which can do that in the viewer's own locale with nothing more than
/// `new Date(paired_at * 1000)`, never by Rust code here).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedProfile {
    pub id: String,
    pub token: String,
    pub label: String,
    pub paired_at: u64,
}

/// What the Settings panel's "PAIRED BROWSERS" list actually gets -- never
/// the raw `token`, which that UI has no legitimate use for and shouldn't
/// be rendering into the DOM regardless.
#[derive(Debug, Clone, Serialize)]
pub struct PairedProfileInfo {
    pub id: String,
    pub label: String,
    pub paired_at: u64,
}

impl From<&PairedProfile> for PairedProfileInfo {
    fn from(p: &PairedProfile) -> Self {
        Self {
            id: p.id.clone(),
            label: p.label.clone(),
            paired_at: p.paired_at,
        }
    }
}

/// Where the legacy (pre-multi-profile) single-token file and its "already
/// paired" marker lived, next to whatever `storage_file` [`PairingState`]
/// is given -- used only by [`PairingState::new`] to migrate a pre-existing
/// install's one pairing into the new multi-profile list, once.
fn legacy_token_file(storage_file: &Path) -> PathBuf {
    storage_file.with_file_name("pairing_token.txt")
}
fn legacy_marker_file(legacy_token_file: &Path) -> PathBuf {
    let mut os = legacy_token_file.as_os_str().to_os_string();
    os.push(".paired");
    PathBuf::from(os)
}

/// Tracks every currently-paired client (e.g. one browser extension install
/// per Chrome profile) and, separately, whether `/pair` is currently willing
/// to mint a new one.
///
/// `/pair` hands out a brand-new token on every call that succeeds --
/// unlike the single-token design this replaced, pairing a client was never
/// a rotation of one shared secret, it's adding one more trusted entry to a
/// list. A call succeeds in exactly two cases: the list is still empty (so
/// the very first browser a fresh install sees pairs with zero clicks, same
/// feel as before) or pairing mode is currently open (see
/// [`allow_new_pairing`](Self::allow_new_pairing)) -- otherwise it's
/// rejected, so an arbitrary local process can't mint itself a trusted
/// token at any later moment just by asking. Forgetting every paired
/// profile naturally reopens that first, zero-click slot: with nothing
/// trusted left to protect, the state is equivalent to a fresh install.
pub struct PairingState {
    profiles: Mutex<Vec<PairedProfile>>,
    pairing_mode_until: Mutex<Option<Instant>>,
    storage_file: PathBuf,
}

impl PairingState {
    /// Loads the paired-profile list from `storage_file` if it exists, or
    /// migrates a pre-multi-profile install's single token (if its own
    /// `pairing_token.txt`/`.paired` marker, next to `storage_file`, show it
    /// was ever paired) into a one-entry list, or starts with an empty list
    /// for a genuinely fresh install.
    pub fn new(storage_file: PathBuf) -> Self {
        let (profiles, migrated) = Self::load_or_migrate(&storage_file);
        let state = Self {
            profiles: Mutex::new(profiles),
            pairing_mode_until: Mutex::new(None),
            storage_file,
        };
        if migrated {
            let snapshot = state.profiles.lock().unwrap().clone();
            state.persist(&snapshot);
        }
        state
    }

    /// Returns the loaded/migrated profile list, plus whether it came from
    /// a legacy migration -- which the new storage file has never been
    /// written for yet, unlike the "loaded `storage_file` as-is" case,
    /// where it's already on disk in the current format. [`Self::new`]
    /// uses that flag to persist the migrated list once, immediately.
    fn load_or_migrate(storage_file: &Path) -> (Vec<PairedProfile>, bool) {
        if let Some(loaded) = fs::read_to_string(storage_file)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<PairedProfile>>(&s).ok())
        {
            return (loaded, false);
        }

        let legacy_token = legacy_token_file(storage_file);
        let legacy_marker = legacy_marker_file(&legacy_token);
        if !legacy_marker.exists() {
            return (Vec::new(), false);
        }
        let Some(token) = fs::read_to_string(&legacy_token)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            return (Vec::new(), false);
        };
        (
            vec![PairedProfile {
                id: Uuid::new_v4().to_string(),
                token,
                label: "Browser (migrated)".to_string(),
                paired_at: now_unix(),
            }],
            true,
        )
    }

    fn persist(&self, profiles: &[PairedProfile]) {
        if let Some(parent) = self.storage_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(profiles) {
            Ok(json) => {
                if let Err(e) = fs::write(&self.storage_file, json) {
                    eprintln!("pairing state: failed to persist paired profiles: {e}");
                }
            }
            Err(e) => eprintln!("pairing state: failed to serialize paired profiles: {e}"),
        }
    }

    /// Whether `token` matches any currently-paired profile -- the auth
    /// check `/downloads` and `/unpair` both use in place of the old
    /// design's single string comparison.
    pub fn is_token_valid(&self, token: &str) -> bool {
        self.profiles.lock().unwrap().iter().any(|p| p.token == token)
    }

    /// Snapshot of every paired profile, for the Settings panel's "PAIRED
    /// BROWSERS" list.
    pub fn list(&self) -> Vec<PairedProfileInfo> {
        self.profiles.lock().unwrap().iter().map(PairedProfileInfo::from).collect()
    }

    /// Opens `/pair` back up for `duration` (the Settings panel's "ALLOW NEW
    /// PAIRING" button calls this with [`PAIRING_MODE_DURATION`]).
    pub fn allow_new_pairing(&self, duration: Duration) {
        *self.pairing_mode_until.lock().unwrap() = Instant::now().checked_add(duration);
    }

    /// How much longer pairing mode stays open, or `None` if it's closed --
    /// backs the Settings panel's live countdown, including resuming it
    /// correctly if the panel is closed and reopened while still counting
    /// down.
    pub fn pairing_mode_remaining(&self) -> Option<Duration> {
        let until = *self.pairing_mode_until.lock().unwrap();
        until.and_then(|t| t.checked_duration_since(Instant::now()))
            .filter(|d| !d.is_zero())
    }

    fn pairing_mode_active(&self) -> bool {
        self.pairing_mode_remaining().is_some()
    }

    /// Attempts to pair a new client, returning its freshly minted token on
    /// success. Succeeds only if no profile has ever been paired yet (the
    /// list is still empty) or pairing mode is currently open; the
    /// emptiness check and the insert happen under the same lock, so
    /// concurrent callers racing for that first, zero-click slot still see
    /// exactly one winner, same guarantee the old design's atomic
    /// `compare_exchange` gave.
    pub fn try_pair(&self) -> Option<String> {
        let mut profiles = self.profiles.lock().unwrap();
        if !profiles.is_empty() && !self.pairing_mode_active() {
            return None;
        }

        let token = generate_token();
        let label = if profiles.is_empty() {
            "Browser".to_string()
        } else {
            format!("Browser {}", profiles.len() + 1)
        };
        profiles.push(PairedProfile {
            id: Uuid::new_v4().to_string(),
            token: token.clone(),
            label,
            paired_at: now_unix(),
        });
        self.persist(&profiles);
        Some(token)
    }

    /// Removes the profile whose token is `token`, if any -- what `/unpair`
    /// calls, after authenticating the caller by that same token (so only
    /// whoever currently holds a live token can remove *that* profile,
    /// never an arbitrary local caller, and never someone else's). Returns
    /// whether anything was removed.
    pub fn forget_by_token(&self, token: &str) -> bool {
        let mut profiles = self.profiles.lock().unwrap();
        let before = profiles.len();
        profiles.retain(|p| p.token != token);
        let changed = profiles.len() != before;
        if changed {
            self.persist(&profiles);
        }
        changed
    }

    /// Removes the profile with id `id` -- what the Settings panel's
    /// per-row FORGET button calls (addressing by id, since the frontend
    /// never sees raw tokens). Returns whether anything was removed.
    pub fn forget_by_id(&self, id: &str) -> bool {
        let mut profiles = self.profiles.lock().unwrap();
        let before = profiles.len();
        profiles.retain(|p| p.id != id);
        let changed = profiles.len() != before;
        if changed {
            self.persist(&profiles);
        }
        changed
    }

    /// Renames the profile with id `id`. Returns whether a profile with
    /// that id exists.
    pub fn rename(&self, id: &str, new_label: &str) -> bool {
        let mut profiles = self.profiles.lock().unwrap();
        match profiles.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.label = new_label.to_string();
                self.persist(&profiles);
                true
            }
            None => false,
        }
    }
}

/// Generates a random 32-byte token, hex-encoded (64 characters).
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Shared handler state: cheap to clone (just two `Arc` clones), as axum's
/// `State` extractor requires.
#[derive(Clone)]
struct AppState {
    queue: Arc<QueueManager>,
    pairing: Arc<PairingState>,
}

/// Body of a `POST /downloads` request. Mirrors [`DownloadRequest`] exactly
/// (same field names/types), kept as its own type so this module's wire
/// format doesn't implicitly change if `DownloadRequest` ever gains
/// `Deserialize`/fields for unrelated reasons.
///
/// `filename` is untrusted: `QueueManager::add` reduces it to a single safe
/// path component (see `download::queue::sanitize_filename`) -- stripping
/// any directory part such as `../` or an absolute path, and replacing
/// characters Windows forbids -- and falls back to a name derived from the
/// URL if nothing usable is left, so it can never place a file outside the
/// download folder.
#[derive(Deserialize)]
struct DownloadRequestBody {
    url: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

impl From<DownloadRequestBody> for DownloadRequest {
    fn from(body: DownloadRequestBody) -> Self {
        DownloadRequest {
            url: body.url,
            filename: body.filename,
            headers: body.headers,
        }
    }
}

#[derive(Serialize)]
struct PairResponse {
    token: String,
}

/// Body of a rejected `/pair` -- a human-readable `message` the extension's
/// options page shows verbatim, so "pairing is closed right now" reads
/// distinctly from a generic network/server error.
#[derive(Serialize)]
struct PairErrorResponse {
    error: &'static str,
    message: String,
}

fn pairing_closed_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(PairErrorResponse {
            error: "pairing_closed",
            message: "Pairing is closed. Open Loadout, go to Settings, and click \
                      \"Allow new pairing\" first."
                .to_string(),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct DownloadResponse {
    id: String,
}

async fn ping() -> &'static str {
    "ok"
}

/// Whether an `Origin` header value is acceptable for `/pair`: either the
/// header is absent (a legitimate extension service-worker `fetch()` sends
/// no `Origin` at all for this kind of request) or it names a
/// `chrome-extension://` origin (a legitimate extension sending its own
/// origin). Anything else -- in particular an ordinary web page's own
/// `https://`/`http://` origin -- is rejected: see [`pair`].
fn origin_is_acceptable_for_pair(headers: &HeaderMap) -> bool {
    match headers.get(ORIGIN).and_then(|value| value.to_str().ok()) {
        None => true,
        Some(origin) => origin.starts_with("chrome-extension://"),
    }
}

/// `POST /pair`: mints and hands out a brand-new token for the very first
/// profile ever (zero clicks, same feel as before) or, after that, only
/// while pairing mode is open (see
/// [`PairingState::allow_new_pairing`]) -- any other call is rejected with
/// `403` and [`pairing_closed_response`]'s explanatory body.
///
/// Also rejects (`403`, without touching pairing) any request carrying an
/// `Origin` header that isn't a `chrome-extension://` origin. The `Host`
/// check alone isn't enough here: an ordinary web page's own cross-origin
/// `fetch('http://127.0.0.1:57431/pair', {mode: 'no-cors', method: 'POST'})`
/// sends a correct loopback `Host` (browsers always set that from the URL)
/// but carries its own page's `Origin` (e.g. `https://evil.example`), which
/// a legitimate extension service-worker request never does -- it either
/// omits `Origin` entirely or sends `chrome-extension://<its-own-id>`.
async fn pair(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !origin_is_acceptable_for_pair(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    match state.pairing.try_pair() {
        Some(token) => (StatusCode::OK, Json(PairResponse { token })).into_response(),
        None => pairing_closed_response(),
    }
}

/// `POST /unpair`: requires a valid `X-Dm-Token` header (else `401`, same
/// check as `/downloads`) -- so only whoever currently holds a live token
/// can remove *that* profile, never an arbitrary local caller, and never
/// someone else's. On success, removes that one profile (leaving every
/// other paired profile untouched) and returns `204`.
///
/// This is what the extension's "Forget Pairing" button calls (with its
/// stored token, before clearing that token from its own storage) so that
/// forgetting on the extension side and the app actually agreeing that
/// profile is gone happen together, instead of the app staying permanently
/// locked to a token nothing holds anymore.
async fn unpair(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let token = headers.get(TOKEN_HEADER).and_then(|value| value.to_str().ok());
    let removed = match token {
        Some(token) => state.pairing.forget_by_token(token),
        None => false,
    };

    if !removed {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// Whether `url` parses as an absolute URL with an `http` or `https`
/// scheme.
///
/// C1 defense in depth: the extension's own `onDeterminingFilename`
/// listener is supposed to never attempt a handoff for a non-http(s) URL
/// (`blob:`, `data:`, or anything else) in the first place, since
/// `reqwest::Client` has no way to fetch those schemes and the browser's
/// native download has typically already been canceled by the time a
/// request reaches here -- letting one through would silently lose the
/// download for good. This check gives the server the same guarantee
/// regardless of what sent the request (a future extension bug, or any
/// other local caller of `/downloads`), rejecting before the queue is
/// ever touched.
fn url_scheme_is_http_or_https(url: &str) -> bool {
    reqwest::Url::parse(url)
        .map(|parsed| parsed.scheme() == "http" || parsed.scheme() == "https")
        .unwrap_or(false)
}

/// `POST /downloads`: requires a valid `X-Dm-Token` header (else `401`), a
/// well-formed JSON body matching [`DownloadRequestBody`] (axum's `Json`
/// extractor rejects a malformed/unparseable body with `400` before this
/// handler body ever runs, so the queue is never touched in that case), and
/// a `url` that parses as `http://` or `https://` (else `400`, see
/// [`url_scheme_is_http_or_https`] -- also never touches the queue).
///
/// On success, queues the request via `QueueManager::add` and returns `201`
/// with the generated id.
async fn add_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DownloadRequestBody>,
) -> impl IntoResponse {
    let token_matches = headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| state.pairing.is_token_valid(value))
        .unwrap_or(false);

    if !token_matches {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if !url_scheme_is_http_or_https(&body.url) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let id = state.queue.add(body.into());

    (StatusCode::CREATED, Json(DownloadResponse { id })).into_response()
}

/// Rejects (`403`) any request whose `Host` header isn't exactly one of
/// `allowed` -- before any route handler runs.
async fn require_loopback_host(
    State(allowed): State<Arc<Vec<String>>>,
    request: Request,
    next: Next,
) -> Response {
    let host_ok = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(|host| allowed.iter().any(|a| a.eq_ignore_ascii_case(host)))
        .unwrap_or(false);

    if !host_ok {
        return StatusCode::FORBIDDEN.into_response();
    }

    next.run(request).await
}

/// Builds the handoff server's `axum::Router` for the real
/// [`HANDOFF_PORT`]. See [`router_for_port`].
pub fn router(queue: Arc<QueueManager>, pairing: Arc<PairingState>) -> Router {
    router_for_port(queue, pairing, HANDOFF_PORT)
}

/// Builds the handoff server's `axum::Router`, wired to `queue` and
/// `pairing`, accepting only requests whose `Host` header is
/// `127.0.0.1:<port>` or `localhost:<port>` (on every route, `/ping`
/// included -- the extension always addresses the server that way, and
/// nothing else has a legitimate reason to talk to it). Exposed separately
/// from [`serve`] (and parameterized by port) so tests can run it on an
/// ephemeral port.
pub fn router_for_port(queue: Arc<QueueManager>, pairing: Arc<PairingState>, port: u16) -> Router {
    let state = AppState { queue, pairing };
    let allowed_hosts = Arc::new(vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
    ]);

    Router::new()
        .route("/ping", get(ping))
        .route("/pair", post(pair))
        .route("/unpair", post(unpair))
        .route("/downloads", post(add_download))
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            allowed_hosts,
            require_loopback_host,
        ))
}

/// Binds [`router`] to `127.0.0.1:HANDOFF_PORT` and serves it forever (or
/// until the bind itself fails, which is logged and returned from rather
/// than panicking -- a background task failing to bind, e.g. because the
/// port is already in use, must not take the rest of the app down with it).
pub async fn serve(queue: Arc<QueueManager>, pairing: Arc<PairingState>) {
    let app = router(queue, pairing);

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", HANDOFF_PORT)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("handoff server: failed to bind 127.0.0.1:{HANDOFF_PORT}: {e}");
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("handoff server: error while serving: {e}");
    }
}
