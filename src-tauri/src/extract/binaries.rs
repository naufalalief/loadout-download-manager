use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// Default GitHub API base used to look up yt-dlp's latest release.
/// Overridable via [`BinaryManager::new_with_endpoints`] so tests can point
/// at a stubbed server instead of the real GitHub API.
const DEFAULT_YTDLP_API_BASE: &str = "https://api.github.com";

/// Default URL for ffmpeg's "latest" build alias.
///
/// This is a stable alias (not a pinned release tag), which is why it has
/// no checksum verification to match: BtbN's build host does not publish a
/// per-release checksum for this alias, so there is nothing to verify
/// against. That is an accepted tradeoff for v1, not an oversight.
const DEFAULT_FFMPEG_ZIP_URL: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";

/// GitHub's API returns 403 on any request without a `User-Agent` header,
/// so every request `BinaryManager` makes sets one explicitly.
const USER_AGENT: &str = "loadout-farm-log";

/// Errors from fetching/verifying/updating the `yt-dlp`/`ffmpeg` binaries.
///
/// Mirrors [`crate::download::types::DownloadError`]'s shape: a plain
/// `Clone + PartialEq + Eq` enum with a `Display`/`Error` impl, so callers
/// can match on variants and the error still prints a useful message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryError {
    Network(String),
    Io(String),
    ChecksumMismatch,
    Unexpected(String),
}

impl std::fmt::Display for BinaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryError::Network(msg) => write!(f, "network error: {msg}"),
            BinaryError::Io(msg) => write!(f, "io error: {msg}"),
            BinaryError::ChecksumMismatch => {
                write!(f, "downloaded binary failed checksum verification")
            }
            BinaryError::Unexpected(msg) => write!(f, "unexpected error: {msg}"),
        }
    }
}

impl std::error::Error for BinaryError {}

/// Where `BinaryManager` fetches each binary from.
#[derive(Debug, Clone)]
struct Endpoints {
    ytdlp_api_base: String,
    ffmpeg_zip_url: String,
}

/// Fetches, verifies, and updates the `yt-dlp.exe` / `ffmpeg.exe` binaries
/// the extraction feature depends on.
///
/// `ensure_ytdlp`/`ensure_ffmpeg` are no-ops (no network calls at all) once
/// the respective binary already exists on disk. Each binary's fetch is
/// guarded by its own mutex held for the whole ensure/fetch, so two
/// overlapping callers never both download: the second simply waits for
/// the first's completed file.
pub struct BinaryManager {
    dir: PathBuf,
    client: reqwest::Client,
    endpoints: Endpoints,
    ytdlp_lock: Mutex<()>,
    ffmpeg_lock: Mutex<()>,
}

impl BinaryManager {
    pub fn new(dir: PathBuf) -> Self {
        Self::new_with_endpoints(
            dir,
            DEFAULT_YTDLP_API_BASE.to_string(),
            DEFAULT_FFMPEG_ZIP_URL.to_string(),
        )
    }

    #[doc(hidden)]
    pub fn new_with_endpoints(dir: PathBuf, ytdlp_api_base: String, ffmpeg_zip_url: String) -> Self {
        Self {
            dir,
            client: reqwest::Client::new(),
            endpoints: Endpoints {
                ytdlp_api_base,
                ffmpeg_zip_url,
            },
            ytdlp_lock: Mutex::new(()),
            ffmpeg_lock: Mutex::new(()),
        }
    }

    pub fn ytdlp_path(&self) -> PathBuf {
        self.dir.join("yt-dlp.exe")
    }

    pub fn ffmpeg_path(&self) -> PathBuf {
        self.dir.join("ffmpeg.exe")
    }

    /// Returns the existing binary immediately if present, otherwise fetches
    /// it. Guarded by `ytdlp_lock` so concurrent callers only trigger one
    /// download.
    pub async fn ensure_ytdlp(&self) -> Result<PathBuf, BinaryError> {
        let path = self.ytdlp_path();
        if path.exists() {
            return Ok(path);
        }

        let _guard = self.ytdlp_lock.lock().await;
        if path.exists() {
            // Another caller finished the download while we were waiting
            // for the lock.
            return Ok(path);
        }

        self.fetch_ytdlp().await
    }

    /// Returns the existing binary immediately if present, otherwise
    /// downloads the ffmpeg build archive and extracts `ffmpeg.exe` from
    /// it. Guarded by `ffmpeg_lock` so concurrent callers only trigger one
    /// download.
    pub async fn ensure_ffmpeg(&self) -> Result<PathBuf, BinaryError> {
        let path = self.ffmpeg_path();
        if path.exists() {
            return Ok(path);
        }

        let _guard = self.ffmpeg_lock.lock().await;
        if path.exists() {
            return Ok(path);
        }

        self.fetch_ffmpeg().await
    }

    /// Unconditionally re-fetches and overwrites `yt-dlp.exe`, regardless
    /// of whether it already exists.
    pub async fn update_ytdlp(&self) -> Result<PathBuf, BinaryError> {
        let _guard = self.ytdlp_lock.lock().await;
        self.fetch_ytdlp().await
    }

    async fn fetch_ytdlp(&self) -> Result<PathBuf, BinaryError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| BinaryError::Io(e.to_string()))?;

        let release_url = format!(
            "{}/repos/yt-dlp/yt-dlp/releases/latest",
            self.endpoints.ytdlp_api_base
        );
        let release_body = self.get_text(&release_url).await?;
        let release: Value = serde_json::from_str(&release_body)
            .map_err(|e| BinaryError::Unexpected(format!("invalid release JSON: {e}")))?;

        let assets = release["assets"].as_array().ok_or_else(|| {
            BinaryError::Unexpected("release JSON is missing an `assets` array".to_string())
        })?;

        let binary_url = asset_download_url(assets, "yt-dlp.exe")?;
        let checksums_url = asset_download_url(assets, "SHA2-256SUMS")?;

        let checksums_body = self.get_text(&checksums_url).await?;
        let expected_hex = extract_checksum(&checksums_body, "yt-dlp.exe")?;

        let binary_bytes = self.get_bytes(&binary_url).await?;
        let actual_hex = sha256_hex(&binary_bytes);

        if !actual_hex.eq_ignore_ascii_case(&expected_hex) {
            // Deliberately not written anywhere: the mismatched bytes stay
            // in memory and are dropped here, so `ytdlp_path()` never ends
            // up holding an unverified binary.
            return Err(BinaryError::ChecksumMismatch);
        }

        self.write_atomically(&self.ytdlp_path(), &binary_bytes)
            .await?;
        Ok(self.ytdlp_path())
    }

    async fn fetch_ffmpeg(&self) -> Result<PathBuf, BinaryError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| BinaryError::Io(e.to_string()))?;

        let zip_bytes = self.get_bytes(&self.endpoints.ffmpeg_zip_url).await?;
        let ffmpeg_bytes = extract_ffmpeg_exe(&zip_bytes)?;

        self.write_atomically(&self.ffmpeg_path(), &ffmpeg_bytes)
            .await?;
        Ok(self.ffmpeg_path())
    }

    async fn get_text(&self, url: &str) -> Result<String, BinaryError> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| BinaryError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(BinaryError::Network(format!(
                "unexpected status {} from {url}",
                response.status()
            )));
        }

        response
            .text()
            .await
            .map_err(|e| BinaryError::Network(e.to_string()))
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, BinaryError> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| BinaryError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(BinaryError::Network(format!(
                "unexpected status {} from {url}",
                response.status()
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| BinaryError::Network(e.to_string()))?;
        Ok(bytes.to_vec())
    }

    /// Writes `bytes` to a temp path next to `dest` and renames it into
    /// place, so a reader never observes a partially-written file at
    /// `dest`.
    async fn write_atomically(&self, dest: &Path, bytes: &[u8]) -> Result<(), BinaryError> {
        let tmp_path = dest.with_extension("download-tmp");

        tokio::fs::write(&tmp_path, bytes)
            .await
            .map_err(|e| BinaryError::Io(e.to_string()))?;

        if let Err(e) = tokio::fs::rename(&tmp_path, dest).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(BinaryError::Io(e.to_string()));
        }

        Ok(())
    }
}

/// Finds the asset named exactly `name` in a GitHub release's `assets`
/// array and returns its `browser_download_url`.
fn asset_download_url(assets: &[Value], name: &str) -> Result<String, BinaryError> {
    assets
        .iter()
        .find(|asset| asset["name"].as_str() == Some(name))
        .and_then(|asset| asset["browser_download_url"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| BinaryError::Unexpected(format!("release is missing asset `{name}`")))
}

/// Extracts the hex digest for `filename` out of a `SHA2-256SUMS`-style
/// checksums file (lines formatted as `<hex>  <filename>`).
fn extract_checksum(checksums_body: &str, filename: &str) -> Result<String, BinaryError> {
    checksums_body
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hex = parts.next()?;
            let name = parts.next()?;
            if name == filename { Some(hex.to_string()) } else { None }
        })
        .ok_or_else(|| {
            BinaryError::Unexpected(format!(
                "checksums file has no entry for `{filename}`"
            ))
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Opens `zip_bytes` as a zip archive and extracts the entry whose path
/// ends in `/bin/ffmpeg.exe`.
fn extract_ffmpeg_exe(zip_bytes: &[u8]) -> Result<Vec<u8>, BinaryError> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| BinaryError::Unexpected(format!("invalid ffmpeg zip: {e}")))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| BinaryError::Unexpected(format!("failed to read ffmpeg zip entry: {e}")))?;

        if entry.name().ends_with("/bin/ffmpeg.exe") {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|e| BinaryError::Io(e.to_string()))?;
            return Ok(buf);
        }
    }

    Err(BinaryError::Unexpected(
        "ffmpeg zip has no */bin/ffmpeg.exe entry".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tempfile_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "download-manager-binaries-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn release_json(binary_url: &str, checksums_url: &str) -> String {
        format!(
            r#"{{
                "assets": [
                    {{ "name": "yt-dlp.exe", "browser_download_url": "{binary_url}" }},
                    {{ "name": "SHA2-256SUMS", "browser_download_url": "{checksums_url}" }}
                ]
            }}"#
        )
    }

    fn make_ffmpeg_zip(nested_dir: &str, ffmpeg_contents: &[u8]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();

        writer
            .start_file(format!("{nested_dir}/bin/ffmpeg.exe"), options)
            .unwrap();
        writer.write_all(ffmpeg_contents).unwrap();

        // An unrelated entry, so the extractor has to actually pick the
        // right one rather than just grabbing whatever is first/only.
        writer
            .start_file(format!("{nested_dir}/README.txt"), options)
            .unwrap();
        writer.write_all(b"not ffmpeg").unwrap();

        writer.finish().unwrap().into_inner()
    }

    #[tokio::test]
    async fn ensure_ytdlp_downloads_and_verifies_on_success() {
        let server = MockServer::start().await;
        let binary_body = b"pretend this is yt-dlp.exe".to_vec();
        let checksum = sha256_hex(&binary_body);

        Mock::given(method("GET"))
            .and(path("/repos/yt-dlp/yt-dlp/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(release_json(
                &format!("{}/download/yt-dlp.exe", server.uri()),
                &format!("{}/download/SHA2-256SUMS", server.uri()),
            )))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/download/SHA2-256SUMS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{checksum}  yt-dlp.exe\n")),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/download/yt-dlp.exe"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(binary_body.clone()))
            .mount(&server)
            .await;

        let dir = tempfile_dir();
        let manager = BinaryManager::new_with_endpoints(
            dir.clone(),
            server.uri(),
            "unused://ffmpeg".to_string(),
        );

        let path_result = manager.ensure_ytdlp().await.unwrap();

        assert_eq!(path_result, manager.ytdlp_path());
        let written = std::fs::read(&path_result).unwrap();
        assert_eq!(written, binary_body);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ensure_ytdlp_returns_checksum_mismatch_and_leaves_no_file() {
        let server = MockServer::start().await;
        let binary_body = b"pretend this is yt-dlp.exe".to_vec();
        // Deliberately wrong checksum.
        let wrong_checksum = sha256_hex(b"something else entirely");

        Mock::given(method("GET"))
            .and(path("/repos/yt-dlp/yt-dlp/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(release_json(
                &format!("{}/download/yt-dlp.exe", server.uri()),
                &format!("{}/download/SHA2-256SUMS", server.uri()),
            )))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/download/SHA2-256SUMS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{wrong_checksum}  yt-dlp.exe\n")),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/download/yt-dlp.exe"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(binary_body))
            .mount(&server)
            .await;

        let dir = tempfile_dir();
        let manager = BinaryManager::new_with_endpoints(
            dir.clone(),
            server.uri(),
            "unused://ffmpeg".to_string(),
        );

        let result = manager.ensure_ytdlp().await;

        assert_eq!(result, Err(BinaryError::ChecksumMismatch));
        assert!(!manager.ytdlp_path().exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ensure_ytdlp_is_a_no_op_when_file_already_exists() {
        // A real mock server, but with no mocks mounted: any HTTP call
        // `ensure_ytdlp` made would hit wiremock's default 404 handler and
        // still be recorded, so asserting zero received requests below
        // proves no network call happened at all.
        let server = MockServer::start().await;

        let dir = tempfile_dir();
        let manager = BinaryManager::new_with_endpoints(
            dir.clone(),
            server.uri(),
            "unused://ffmpeg".to_string(),
        );

        std::fs::write(manager.ytdlp_path(), b"already here").unwrap();

        let path_result = manager.ensure_ytdlp().await.unwrap();

        assert_eq!(path_result, manager.ytdlp_path());
        assert_eq!(std::fs::read(&path_result).unwrap(), b"already here");
        assert_eq!(server.received_requests().await.unwrap().len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ensure_ffmpeg_downloads_and_extracts_nested_binary() {
        let server = MockServer::start().await;
        let ffmpeg_body = b"pretend this is ffmpeg.exe".to_vec();
        let zip_bytes = make_ffmpeg_zip("ffmpeg-master-latest-win64-gpl", &ffmpeg_body);

        Mock::given(method("GET"))
            .and(path("/ffmpeg.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
            .mount(&server)
            .await;

        let dir = tempfile_dir();
        let manager = BinaryManager::new_with_endpoints(
            dir.clone(),
            "unused://ytdlp".to_string(),
            format!("{}/ffmpeg.zip", server.uri()),
        );

        let path_result = manager.ensure_ffmpeg().await.unwrap();

        assert_eq!(path_result, manager.ffmpeg_path());
        assert_eq!(std::fs::read(&path_result).unwrap(), ffmpeg_body);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn concurrent_ensure_ytdlp_calls_only_trigger_one_download() {
        let server = MockServer::start().await;
        let binary_body = b"pretend this is yt-dlp.exe".to_vec();
        let checksum = sha256_hex(&binary_body);

        Mock::given(method("GET"))
            .and(path("/repos/yt-dlp/yt-dlp/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(release_json(
                &format!("{}/download/yt-dlp.exe", server.uri()),
                &format!("{}/download/SHA2-256SUMS", server.uri()),
            )))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/download/SHA2-256SUMS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{checksum}  yt-dlp.exe\n")),
            )
            .mount(&server)
            .await;

        // Slow stub for the binary itself, so both callers are definitely
        // in flight concurrently before either finishes.
        Mock::given(method("GET"))
            .and(path("/download/yt-dlp.exe"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(binary_body)
                    .set_delay(std::time::Duration::from_millis(200)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile_dir();
        let manager = std::sync::Arc::new(BinaryManager::new_with_endpoints(
            dir.clone(),
            server.uri(),
            "unused://ffmpeg".to_string(),
        ));

        let a = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.ensure_ytdlp().await })
        };
        let b = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.ensure_ytdlp().await })
        };

        let (result_a, result_b) = tokio::join!(a, b);
        result_a.unwrap().unwrap();
        result_b.unwrap().unwrap();

        // wiremock's `.expect(1)` on the binary mock is verified when the
        // `MockServer` is dropped; `server.verify()` makes the assertion
        // explicit here instead of relying on the drop-time panic.
        server.verify().await;

        std::fs::remove_dir_all(&dir).ok();
    }
}
