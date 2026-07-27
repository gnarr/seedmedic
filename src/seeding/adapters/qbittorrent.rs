//! qBittorrent WebUI adapter.
//!
//! Session handling: `/api/v2/auth/login` sets an `SID` cookie, which we hold
//! behind a `tokio::sync::Mutex<Option<String>>` rather than a `reqwest`
//! cookie jar, so a request that gets a `403` (session expired) can log in
//! once and retry exactly once — see [`QBittorrentClient::send_authenticated`].
//!
//! qBittorrent 5.0 renamed the `torrents/add` field `paused` to `stopped`,
//! and the `torrents/resume` endpoint to `torrents/start`. Rather than parse
//! `/api/v2/app/version` and track which era we are talking to, `add_paused`
//! sends both field names — an unrecognised form field is ignored, so this
//! is free — and `resume` tries `torrents/resume` first, falling back to
//! `torrents/start` only on a `404`. Guessing the version and getting it
//! wrong would risk the "never add a torrent started" invariant; sending
//! both is unconditionally safe.
//!
//! See `docs/todos/0007-qbittorrent-adapter.md` for the endpoint reference
//! and the qBittorrent-state-to-port-state mapping table.

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder, StatusCode, multipart};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;
use url::Url;

use crate::{
    config::Secret,
    seeding::{
        domain::{AddTorrent, ClientTorrentState, DataCompleteness, FileProgress, TorrentStatus},
        ports::{ClientError, TorrentClient},
    },
    torrent::{InfoHash, SafeRelativePath},
};

pub struct QBittorrentClient {
    base_url: Url,
    username: String,
    password: Secret,
    http: Client,
    /// The `SID=...` cookie pair, once logged in. `None` before the first
    /// request and again immediately after a `403`.
    cookie: Mutex<Option<String>>,
}

impl QBittorrentClient {
    pub fn new(base_url: Url, username: String, password: Secret, http: Client) -> Self {
        Self {
            base_url,
            username,
            password,
            http,
            cookie: Mutex::new(None),
        }
    }

    fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base_url
            .join(path)
            .map_err(|error| ClientError::Protocol(format!("cannot build request url: {error}")))
    }

    /// `reqwest::Error` embeds the request URL; it never carries the
    /// password (sent as a form field, not a query parameter), but stripping
    /// it costs nothing and keeps this adapter honest about the invariant.
    fn transport_error(error: reqwest::Error) -> ClientError {
        ClientError::Transport(error.without_url().to_string())
    }

    fn require_success(response: &reqwest::Response, action: &str) -> Result<(), ClientError> {
        if response.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::Protocol(format!(
                "qbittorrent returned status {} for {action}",
                response.status()
            )))
        }
    }

    fn extract_sid_cookie(response: &reqwest::Response) -> Option<String> {
        response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .find_map(|value| {
                let value = value.to_str().ok()?;
                let pair = value.split(';').next()?.trim();
                pair.starts_with("SID=").then(|| pair.to_owned())
            })
    }

    /// `POST /api/v2/auth/login`. Success is a `200` with the literal body
    /// `Ok.` and a fresh `SID` cookie; wrong credentials are still a `200`,
    /// with the body `Fails.` instead — qBittorrent does not use HTTP status
    /// codes for this endpoint.
    async fn perform_login(&self) -> Result<String, ClientError> {
        let url = self.url("api/v2/auth/login")?;
        let response = self
            .http
            .post(url)
            .form(&[
                ("username", self.username.as_str()),
                ("password", self.password.expose()),
            ])
            .send()
            .await
            .map_err(Self::transport_error)?;

        let cookie = Self::extract_sid_cookie(&response);
        let body = response.text().await.map_err(Self::transport_error)?;
        if body.trim() != "Ok." {
            return Err(ClientError::Unauthorized);
        }

        cookie.ok_or_else(|| {
            ClientError::Protocol("qbittorrent login succeeded but set no session cookie".into())
        })
    }

    async fn cookie(&self) -> Result<String, ClientError> {
        let mut guard = self.cookie.lock().await;
        if let Some(cookie) = guard.as_ref() {
            return Ok(cookie.clone());
        }
        let cookie = self.perform_login().await?;
        *guard = Some(cookie.clone());
        Ok(cookie)
    }

    async fn relogin(&self) -> Result<String, ClientError> {
        let mut guard = self.cookie.lock().await;
        let cookie = self.perform_login().await?;
        *guard = Some(cookie.clone());
        Ok(cookie)
    }

    /// Send a request, attaching the current session cookie. On a `403`
    /// (expired session) log in exactly once and retry exactly once; a
    /// second `403` is `Unauthorized`, not another retry.
    async fn send_authenticated<F>(&self, build: F) -> Result<reqwest::Response, ClientError>
    where
        F: Fn(&Client, &str) -> RequestBuilder,
    {
        let cookie = self.cookie().await?;
        let response = build(&self.http, &cookie)
            .send()
            .await
            .map_err(Self::transport_error)?;
        if response.status() != StatusCode::FORBIDDEN {
            return Ok(response);
        }

        let cookie = self.relogin().await?;
        let response = build(&self.http, &cookie)
            .send()
            .await
            .map_err(Self::transport_error)?;
        if response.status() == StatusCode::FORBIDDEN {
            return Err(ClientError::Unauthorized);
        }
        Ok(response)
    }

    /// `GET /api/v2/torrents/files?hash=`. A file name qBittorrent cannot be
    /// made to echo back as anything but our own staged layout is still run
    /// through [`SafeRelativePath`] rather than trusted outright; an entry
    /// that fails to parse is dropped rather than failing the whole call,
    /// since this data is corroborating detail, never a safety input.
    async fn file_progress(&self, info_hash: InfoHash) -> Result<Vec<FileProgress>, ClientError> {
        let hex = info_hash.to_hex();
        let url = self.url("api/v2/torrents/files")?;
        let response = self
            .send_authenticated(|http, cookie| {
                http.get(url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .query(&[("hash", hex.as_str())])
            })
            .await?;

        if !response.status().is_success() {
            return Err(ClientError::Protocol(format!(
                "qbittorrent returned status {} for torrent files",
                response.status()
            )));
        }

        let entries: Vec<TorrentFileEntry> = response.json().await.map_err(|error| {
            ClientError::Protocol(format!("cannot parse torrent files: {error}"))
        })?;

        Ok(entries
            .into_iter()
            .filter_map(|entry| match SafeRelativePath::parse(&entry.name) {
                Ok(torrent_path) => Some(FileProgress {
                    torrent_path,
                    completeness: DataCompleteness::from_ratio(entry.progress),
                }),
                Err(error) => {
                    warn!(name = entry.name, %error, "qbittorrent file entry is not a safe relative path");
                    None
                }
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct TorrentInfoEntry {
    hash: String,
    state: String,
    progress: f64,
    save_path: String,
}

#[derive(Deserialize)]
struct TorrentFileEntry {
    name: String,
    progress: f64,
}

/// Map a qBittorrent `state` string onto the five port states. Exhaustive by
/// construction: anything not explicitly recognised is `Errored` rather than
/// a guess at something more optimistic, per the safety constraints in
/// `docs/todos/0007-qbittorrent-adapter.md`.
fn map_state(value: &str) -> ClientTorrentState {
    match value {
        "pausedUP" | "pausedDL" | "stoppedUP" | "stoppedDL" => ClientTorrentState::Paused,
        "checkingUP" | "checkingDL" | "checkingResumeData" | "queuedForChecking" | "moving" => {
            ClientTorrentState::Checking
        }
        "downloading" | "stalledDL" | "metaDL" | "queuedDL" | "forcedDL" | "allocating" => {
            ClientTorrentState::Downloading
        }
        "uploading" | "stalledUP" | "queuedUP" | "forcedUP" => ClientTorrentState::Seeding,
        _ => ClientTorrentState::Errored,
    }
}

/// `queuedForChecking` means the check has not started — qBittorrent is not
/// making progress on it, so it deserves a longer poll interval than a check
/// that is actually running.
fn is_queued_check(value: &str) -> bool {
    value == "queuedForChecking"
}

#[async_trait]
impl TorrentClient for QBittorrentClient {
    async fn add_paused(&self, request: AddTorrent<'_>) -> Result<(), ClientError> {
        // The port requires re-adding an already-present torrent to be a
        // no-op; checking first makes that true regardless of whether
        // qBittorrent's own `torrents/add` is idempotent.
        if self.status(request.info_hash).await?.is_some() {
            return Ok(());
        }

        let url = self.url("api/v2/torrents/add")?;
        let save_path = request.save_path.to_string_lossy();

        let response = self
            .send_authenticated(|http, cookie| {
                let part = multipart::Part::bytes(request.torrent_file.to_vec())
                    .file_name("upload.torrent")
                    .mime_str("application/x-bittorrent")
                    .expect("static mime type is valid");
                let mut form = multipart::Form::new()
                    .part("torrents", part)
                    .text("savepath", save_path.to_string())
                    .text("skip_checking", "false")
                    // See the module doc: send both spellings rather than
                    // detecting the server version.
                    .text("paused", "true")
                    .text("stopped", "true");
                if let Some(category) = request.category {
                    form = form.text("category", category.to_owned());
                }
                http.post(url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .multipart(form)
            })
            .await?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::Rejected(format!(
                "qbittorrent returned status {} for add",
                response.status()
            )))
        }
    }

    async fn status(&self, info_hash: InfoHash) -> Result<Option<TorrentStatus>, ClientError> {
        let hex = info_hash.to_hex();
        let url = self.url("api/v2/torrents/info")?;
        let response = self
            .send_authenticated(|http, cookie| {
                http.get(url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .query(&[("hashes", hex.as_str())])
            })
            .await?;

        if !response.status().is_success() {
            return Err(ClientError::Protocol(format!(
                "qbittorrent returned status {} for torrent info",
                response.status()
            )));
        }

        let entries: Vec<TorrentInfoEntry> = response.json().await.map_err(|error| {
            ClientError::Protocol(format!("cannot parse torrent info: {error}"))
        })?;

        let Some(entry) = entries
            .into_iter()
            .find(|entry| entry.hash.eq_ignore_ascii_case(&hex))
        else {
            return Ok(None);
        };

        let state = map_state(&entry.state);
        // Per-file detail is what turns a partial recheck into something an
        // operator can act on, but it is only worth the extra request once
        // there is a settled answer to report — not on every poll of a check
        // that is still running.
        let files = if state == ClientTorrentState::Checking {
            None
        } else {
            Some(self.file_progress(info_hash).await?)
        };

        Ok(Some(TorrentStatus {
            state,
            completeness: DataCompleteness::from_ratio(entry.progress),
            save_path: entry.save_path.into(),
            files,
            queued: state == ClientTorrentState::Checking && is_queued_check(&entry.state),
            message: (state == ClientTorrentState::Errored).then(|| entry.state.clone()),
        }))
    }

    async fn recheck(&self, info_hash: InfoHash) -> Result<(), ClientError> {
        let hex = info_hash.to_hex();
        let url = self.url("api/v2/torrents/recheck")?;
        // Re-issuing a recheck against an already-checking torrent is a
        // no-op server-side; no client-side idempotency check is needed.
        let response = self
            .send_authenticated(|http, cookie| {
                http.post(url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .form(&[("hashes", hex.as_str())])
            })
            .await?;
        Self::require_success(&response, "recheck")
    }

    async fn resume(&self, info_hash: InfoHash) -> Result<(), ClientError> {
        let hex = info_hash.to_hex();

        // Resuming an already-started torrent is a no-op server-side; no
        // client-side idempotency check is needed.
        let legacy_url = self.url("api/v2/torrents/resume")?;
        let response = self
            .send_authenticated(|http, cookie| {
                http.post(legacy_url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .form(&[("hashes", hex.as_str())])
            })
            .await?;
        if response.status() != StatusCode::NOT_FOUND {
            return Self::require_success(&response, "resume");
        }

        // qBittorrent 5.0 renamed `torrents/resume` to `torrents/start`.
        let modern_url = self.url("api/v2/torrents/start")?;
        let response = self
            .send_authenticated(|http, cookie| {
                http.post(modern_url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .form(&[("hashes", hex.as_str())])
            })
            .await?;
        Self::require_success(&response, "resume")
    }

    async fn remove(&self, info_hash: InfoHash, delete_files: bool) -> Result<(), ClientError> {
        // Staged data may be hardlinked into the library; this adapter never
        // deletes files, and refuses outright rather than honour the flag.
        if delete_files {
            return Err(ClientError::Rejected(
                "qbittorrent adapter never deletes files; staged data may be hardlinked into \
                 the library"
                    .into(),
            ));
        }

        let hex = info_hash.to_hex();
        let url = self.url("api/v2/torrents/delete")?;
        let response = self
            .send_authenticated(|http, cookie| {
                http.post(url.clone())
                    .header(reqwest::header::COOKIE, cookie.to_owned())
                    .form(&[("hashes", hex.as_str()), ("deleteFiles", "false")])
            })
            .await?;
        Self::require_success(&response, "remove")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

    use super::*;

    const USERNAME: &str = "admin";
    const PASSWORD: &str = "s3cr3t-p4ssw0rd";
    const SID: &str = "abc123session";

    fn client(server: &MockServer) -> QBittorrentClient {
        QBittorrentClient::new(
            Url::parse(&server.uri()).expect("mock server URI parses"),
            USERNAME.to_owned(),
            Secret::new(PASSWORD),
            Client::new(),
        )
    }

    fn hash() -> InfoHash {
        InfoHash::parse_hex("0123456789abcdef0123456789abcdef01234567").expect("valid hex")
    }

    async fn mount_login(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("Ok.")
                    .insert_header("set-cookie", format!("SID={SID}; path=/; HttpOnly")),
            )
            .mount(server)
            .await;
    }

    /// `status` fetches per-file detail for any torrent it does not report as
    /// `Checking`; most `status` tests do not care about it and just need it
    /// answered.
    async fn mount_empty_files(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_403_triggers_one_relogin_and_retry_then_succeeds() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(403))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("retry after relogin succeeds");
        assert_eq!(status, None);
    }

    #[tokio::test]
    async fn a_second_403_after_relogin_is_an_error_not_another_retry() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let error = client(&server)
            .status(hash())
            .await
            .expect_err("a session that stays invalid is an error");
        assert!(matches!(error, ClientError::Unauthorized));
    }

    #[tokio::test]
    async fn add_paused_sends_paused_and_stopped_true_and_the_right_save_path() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/add"))
            .and(body_string_contains("name=\"paused\"\r\n\r\ntrue"))
            .and(body_string_contains("name=\"stopped\"\r\n\r\ntrue"))
            .and(body_string_contains(
                "name=\"savepath\"\r\n\r\n/staging/demo",
            ))
            .and(body_string_contains("name=\"category\"\r\n\r\nseedmedic"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
            .mount(&server)
            .await;

        let request = AddTorrent {
            info_hash: hash(),
            torrent_file: b"d8:announce20:http://example.com/e",
            save_path: std::path::Path::new("/staging/demo"),
            category: Some("seedmedic"),
        };

        client(&server)
            .add_paused(request)
            .await
            .expect("add_paused matches the mocked request");
    }

    #[tokio::test]
    async fn adding_an_existing_torrent_is_a_no_op_that_issues_no_add_request() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "pausedDL",
                    "progress": 0.0,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        mount_empty_files(&server).await;
        // No mock for POST /api/v2/torrents/add: any request to it fails the
        // test with a 404 from wiremock's default "no matching mock" response.

        let request = AddTorrent {
            info_hash: hash(),
            torrent_file: b"d8:announce20:http://example.com/e",
            save_path: std::path::Path::new("/staging/demo"),
            category: None,
        };

        client(&server)
            .add_paused(request)
            .await
            .expect("re-adding an existing torrent is a no-op");
    }

    #[tokio::test]
    async fn status_for_an_unknown_hash_returns_none() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds");
        assert_eq!(status, None);
    }

    #[tokio::test]
    async fn every_documented_state_string_maps_as_tabled() {
        let cases = [
            ("pausedUP", ClientTorrentState::Paused),
            ("pausedDL", ClientTorrentState::Paused),
            ("stoppedUP", ClientTorrentState::Paused),
            ("stoppedDL", ClientTorrentState::Paused),
            ("checkingUP", ClientTorrentState::Checking),
            ("checkingDL", ClientTorrentState::Checking),
            ("checkingResumeData", ClientTorrentState::Checking),
            ("queuedForChecking", ClientTorrentState::Checking),
            ("moving", ClientTorrentState::Checking),
            ("downloading", ClientTorrentState::Downloading),
            ("stalledDL", ClientTorrentState::Downloading),
            ("metaDL", ClientTorrentState::Downloading),
            ("queuedDL", ClientTorrentState::Downloading),
            ("forcedDL", ClientTorrentState::Downloading),
            ("allocating", ClientTorrentState::Downloading),
            ("uploading", ClientTorrentState::Seeding),
            ("stalledUP", ClientTorrentState::Seeding),
            ("queuedUP", ClientTorrentState::Seeding),
            ("forcedUP", ClientTorrentState::Seeding),
            ("error", ClientTorrentState::Errored),
            ("missingFiles", ClientTorrentState::Errored),
            ("unknown", ClientTorrentState::Errored),
            (
                "something-qbittorrent-has-not-invented-yet",
                ClientTorrentState::Errored,
            ),
        ];

        for (state, expected) in cases {
            assert_eq!(map_state(state), expected, "state {state} mapped wrong");
        }
    }

    #[tokio::test]
    async fn progress_below_one_is_partial_and_one_is_complete() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "pausedDL",
                    "progress": 0.999999,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        mount_empty_files(&server).await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");
        assert_eq!(
            status.completeness,
            DataCompleteness::Partial { ratio: 0.999999 }
        );

        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "uploading",
                    "progress": 1.0,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        mount_empty_files(&server).await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");
        assert_eq!(status.completeness, DataCompleteness::Complete);
    }

    #[tokio::test]
    async fn status_fetches_per_file_progress_once_the_check_is_not_running() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "pausedDL",
                    "progress": 0.75,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "name": "Show/e01.mkv", "progress": 1.0 },
                { "name": "Show/e02.mkv", "progress": 0.5 },
            ])))
            .mount(&server)
            .await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");

        let files = status.files.expect("per-file detail was fetched");
        assert_eq!(
            files,
            vec![
                FileProgress {
                    torrent_path: SafeRelativePath::parse("Show/e01.mkv").expect("valid"),
                    completeness: DataCompleteness::Complete,
                },
                FileProgress {
                    torrent_path: SafeRelativePath::parse("Show/e02.mkv").expect("valid"),
                    completeness: DataCompleteness::Partial { ratio: 0.5 },
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_file_entry_that_is_not_a_safe_relative_path_is_dropped_not_fatal() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "pausedDL",
                    "progress": 0.5,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "name": "../escape.mkv", "progress": 1.0 },
                { "name": "Show/e01.mkv", "progress": 1.0 },
            ])))
            .mount(&server)
            .await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");

        assert_eq!(
            status.files.expect("per-file detail was fetched"),
            vec![FileProgress {
                torrent_path: SafeRelativePath::parse("Show/e01.mkv").expect("valid"),
                completeness: DataCompleteness::Complete,
            }]
        );
    }

    #[tokio::test]
    async fn a_queued_check_is_reported_as_queued_without_fetching_file_progress() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "queuedForChecking",
                    "progress": 0.0,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        // No mock for GET /api/v2/torrents/files: a still-checking torrent
        // must not trigger that request at all.

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");

        assert_eq!(status.state, ClientTorrentState::Checking);
        assert!(status.queued);
        assert_eq!(status.files, None);
    }

    #[tokio::test]
    async fn a_running_check_is_not_reported_as_queued() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "checkingDL",
                    "progress": 0.0,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");

        assert!(!status.queued);
    }

    #[tokio::test]
    async fn an_errored_torrent_carries_the_raw_state_as_its_message() {
        let server = MockServer::start().await;
        mount_login(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "hash": hash().to_hex(),
                    "state": "missingFiles",
                    "progress": 0.5,
                    "save_path": "/staging/demo",
                }
            ])))
            .mount(&server)
            .await;
        mount_empty_files(&server).await;

        let status = client(&server)
            .status(hash())
            .await
            .expect("request succeeds")
            .expect("torrent is known");

        assert_eq!(status.state, ClientTorrentState::Errored);
        assert_eq!(status.message, Some("missingFiles".to_owned()));
    }

    #[tokio::test]
    async fn remove_with_delete_files_true_returns_an_error_and_issues_no_request() {
        // No mock server at all: any HTTP call would fail to connect, so a
        // successful error return proves no request was issued.
        let server = MockServer::start().await;

        let error = client(&server)
            .remove(hash(), true)
            .await
            .expect_err("delete_files = true must be refused");
        assert!(matches!(error, ClientError::Rejected(_)));
    }

    #[tokio::test]
    async fn no_test_fixture_or_error_message_contains_the_password() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
            .mount(&server)
            .await;

        let error = client(&server)
            .status(hash())
            .await
            .expect_err("bad credentials are an error");
        assert!(matches!(error, ClientError::Unauthorized));
        assert!(!error.to_string().contains(PASSWORD));
    }
}
