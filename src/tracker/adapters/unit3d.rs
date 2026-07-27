//! Unit3D-family tracker adapter (Blutopia, Aither, and relatives).
//!
//! Unit3D forks agree on the general shape of a Laravel API resource response
//! (`data`, `links.next` for pagination) but disagree on exact endpoint paths
//! and where the API token goes — see the open questions in
//! `docs/todos/0002-unit3d-tracker.md`. `token_placement` in `config::TrackerConfig`
//! covers the latter; the former is not yet configurable per instance because
//! no second instance has needed a different path.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::{
    config::{Secret, TokenPlacement},
    torrent::InfoHash,
    tracker::{
        domain::{HitAndRun, HitAndRunStatus, TrackerId, TrackerTorrentId},
        ports::{TrackerClient, TrackerError},
    },
};

/// Below this, a private tracker is liable to interpret us as hammering it. No
/// evidence yet that anything more sophisticated than a flat floor is needed.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

/// A `Retry-After` we cannot parse still needs a number; this is a
/// conservative guess, not a measurement.
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 60;

/// Runaway pagination is a bug, not a real account size; bail loudly rather
/// than loop forever against a tracker that never stops returning `next`.
const MAX_PAGES: usize = 1000;

pub struct Unit3dTracker {
    id: TrackerId,
    base_url: Url,
    api_key: Secret,
    token_placement: TokenPlacement,
    http: Client,
    last_request: Mutex<Option<Instant>>,
}

impl Unit3dTracker {
    pub fn new(
        id: TrackerId,
        base_url: Url,
        api_key: Secret,
        token_placement: TokenPlacement,
        http: Client,
    ) -> Self {
        Self {
            id,
            base_url,
            api_key,
            token_placement,
            http,
            last_request: Mutex::new(None),
        }
    }

    fn url(&self, path: &str) -> Result<Url, TrackerError> {
        self.base_url
            .join(path)
            .map_err(|error| TrackerError::Protocol(format!("cannot build request URL: {error}")))
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match self.token_placement {
            TokenPlacement::Header => request.bearer_auth(self.api_key.expose()),
            TokenPlacement::Query => request.query(&[("api_token", self.api_key.expose())]),
        }
    }

    /// Enforce the minimum gap between requests. Never retries or backs off by
    /// itself; that decision belongs to the workflow via `is_transient()`.
    async fn throttle(&self) {
        let wait = {
            let mut last = self.last_request.lock().expect("unit3d tracker poisoned");
            let now = Instant::now();
            let wait = last
                .map(|previous| MIN_REQUEST_INTERVAL.saturating_sub(now.duration_since(previous)))
                .unwrap_or(Duration::ZERO);
            *last = Some(now + wait);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Translate a non-2xx status into the right typed error. Never returns
    /// `Ok`; callers only reach it once the status is known bad.
    fn status_error(status: StatusCode, retry_after: Option<u64>) -> TrackerError {
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => TrackerError::Unauthorized,
            StatusCode::TOO_MANY_REQUESTS => TrackerError::RateLimited {
                retry_after_seconds: retry_after.unwrap_or(DEFAULT_RETRY_AFTER_SECONDS),
            },
            StatusCode::NOT_FOUND => TrackerError::Protocol("tracker returned 404".to_owned()),
            other => TrackerError::Protocol(format!("tracker returned status {other}")),
        }
    }

    fn retry_after_seconds(response: &reqwest::Response) -> Option<u64> {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }

    /// A `reqwest::Error` embeds the request URL, which may carry the API
    /// token as a query parameter. Strip it before it can end up in a log line.
    fn transport_error(error: reqwest::Error) -> TrackerError {
        TrackerError::Transport(error.without_url().to_string())
    }

    async fn get(&self, url: Url) -> Result<reqwest::Response, TrackerError> {
        self.throttle().await;
        let request = self.authorize(self.http.get(url));
        let response = request.send().await.map_err(Self::transport_error)?;

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = Self::retry_after_seconds(&response);
        Err(Self::status_error(status, retry_after))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: Url) -> Result<T, TrackerError> {
        let response = self.get(url).await?;
        response
            .json::<T>()
            .await
            .map_err(|error| TrackerError::Protocol(format!("cannot parse response: {error}")))
    }
}

/// One entry in a hit-and-run listing page.
#[derive(Deserialize)]
struct HitAndRunEntry {
    id: u64,
    name: String,
    size: u64,
    #[serde(default)]
    info_hash: Option<String>,
    #[serde(default)]
    hit_and_run_deadline: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct PageLinks {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct HitAndRunPage {
    data: Vec<HitAndRunEntry>,
    #[serde(default)]
    links: Option<PageLinks>,
}

/// The per-torrent status endpoint. Only the field we can interpret is
/// modelled; anything else about the torrent is not this adapter's concern.
#[derive(Deserialize)]
struct TorrentStatusResponse {
    data: TorrentStatusData,
}

#[derive(Deserialize)]
struct TorrentStatusData {
    attributes: TorrentStatusAttributes,
}

#[derive(Deserialize)]
struct TorrentStatusAttributes {
    #[serde(default)]
    hit_and_run_status: Option<String>,
}

#[async_trait]
impl TrackerClient for Unit3dTracker {
    fn id(&self) -> &TrackerId {
        &self.id
    }

    async fn list_hit_and_runs(&self) -> Result<Vec<HitAndRun>, TrackerError> {
        let mut warnings = Vec::new();
        let mut next = Some(self.url("api/hit-and-runs")?);
        let mut pages = 0usize;

        while let Some(url) = next {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(TrackerError::Protocol(format!(
                    "hit-and-run listing did not terminate after {MAX_PAGES} pages"
                )));
            }

            let observed_at = Utc::now();
            let page: HitAndRunPage = self.get_json(url).await?;

            for entry in page.data {
                let info_hash = match entry.info_hash {
                    Some(hex) => Some(InfoHash::parse_hex(&hex).map_err(|error| {
                        TrackerError::Protocol(format!(
                            "hit-and-run {} has an unparseable info_hash: {error}",
                            entry.id
                        ))
                    })?),
                    None => None,
                };

                warnings.push(HitAndRun {
                    tracker: self.id.clone(),
                    torrent_id: TrackerTorrentId::new(entry.id.to_string()),
                    torrent_name: entry.name,
                    info_hash,
                    size_bytes: entry.size,
                    deadline: entry.hit_and_run_deadline,
                    observed_at,
                });
            }

            next = page
                .links
                .and_then(|links| links.next)
                .map(|next| Url::parse(&next))
                .transpose()
                .map_err(|error| {
                    TrackerError::Protocol(format!("unparseable pagination link: {error}"))
                })?;
        }

        Ok(warnings)
    }

    async fn fetch_torrent_file(&self, id: &TrackerTorrentId) -> Result<Vec<u8>, TrackerError> {
        let url = self.url(&format!("api/torrents/{}/download", id.as_str()))?;
        let response = self.get(url).await?;

        let looks_like_torrent = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| content_type.contains("bittorrent"));

        let bytes = response.bytes().await.map_err(Self::transport_error)?;

        // A tracker returning an HTML error page with a 200 status is a real
        // failure mode; a `.torrent` is always a bencoded dict, so it starts
        // with `d`. Trust either signal, since instances are inconsistent
        // about setting content-type correctly.
        if !looks_like_torrent && bytes.first() != Some(&b'd') {
            return Err(TrackerError::Protocol(
                "response for a .torrent download does not look like a torrent".to_owned(),
            ));
        }

        Ok(bytes.to_vec())
    }

    async fn hit_and_run_status(
        &self,
        id: &TrackerTorrentId,
    ) -> Result<HitAndRunStatus, TrackerError> {
        let url = self.url(&format!("api/torrents/{}", id.as_str()))?;
        let response: TorrentStatusResponse = self.get_json(url).await?;

        Ok(
            match response.data.attributes.hit_and_run_status.as_deref() {
                Some("active") => HitAndRunStatus::Active,
                Some("cleared") => HitAndRunStatus::Cleared,
                _ => HitAndRunStatus::Unknown,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param, query_param_is_missing},
    };

    use super::*;

    const TOKEN: &str = "s3cr3t-token";

    fn tracker(server: &MockServer, token_placement: TokenPlacement) -> Unit3dTracker {
        Unit3dTracker::new(
            TrackerId::new("test"),
            Url::parse(&server.uri()).expect("mock server URI parses"),
            Secret::new(TOKEN),
            token_placement,
            Client::new(),
        )
    }

    enum AuthMatcher {
        Header,
        Query,
    }

    impl wiremock::Match for AuthMatcher {
        fn matches(&self, request: &wiremock::Request) -> bool {
            match self {
                Self::Header => request
                    .headers
                    .get("authorization")
                    .is_some_and(|value| value == format!("Bearer {TOKEN}").as_str()),
                Self::Query => request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "api_token" && value == TOKEN),
            }
        }
    }

    fn auth_matcher(token_placement: TokenPlacement) -> AuthMatcher {
        match token_placement {
            TokenPlacement::Header => AuthMatcher::Header,
            TokenPlacement::Query => AuthMatcher::Query,
        }
    }

    #[tokio::test]
    async fn a_listing_with_two_warnings_maps_to_two_hit_and_runs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .and(auth_matcher(TokenPlacement::Header))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "id": 1,
                        "name": "Demo.Movie.2024.1080p",
                        "size": 1_073_741_824u64,
                        "info_hash": "0123456789abcdef0123456789abcdef01234567",
                        "hit_and_run_deadline": "2026-08-01T00:00:00Z",
                    },
                    {
                        "id": 2,
                        "name": "Demo.Show.S01.1080p",
                        "size": 2_147_483_648u64,
                    },
                ],
                "links": { "next": null },
            })))
            .mount(&server)
            .await;

        let warnings = tracker(&server, TokenPlacement::Header)
            .list_hit_and_runs()
            .await
            .expect("listing succeeds");

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].torrent_id, TrackerTorrentId::new("1"));
        assert_eq!(warnings[0].torrent_name, "Demo.Movie.2024.1080p");
        assert_eq!(warnings[0].size_bytes, 1_073_741_824);
        assert_eq!(
            warnings[0].info_hash,
            Some(InfoHash::parse_hex("0123456789abcdef0123456789abcdef01234567").unwrap())
        );
        assert!(warnings[0].deadline.is_some());
        assert_eq!(warnings[1].torrent_id, TrackerTorrentId::new("2"));
        assert_eq!(warnings[1].info_hash, None);
        assert_eq!(warnings[1].deadline, None);
    }

    #[tokio::test]
    async fn paging_follows_links_next_across_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .and(query_param_is_missing("page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": 1, "name": "Page.One", "size": 1}],
                "links": { "next": format!("{}/api/hit-and-runs?page=2", server.uri()) },
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": 2, "name": "Page.Two", "size": 2}],
                "links": { "next": null },
            })))
            .mount(&server)
            .await;

        let warnings = tracker(&server, TokenPlacement::Header)
            .list_hit_and_runs()
            .await
            .expect("listing succeeds");

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].torrent_id, TrackerTorrentId::new("1"));
        assert_eq!(warnings[1].torrent_id, TrackerTorrentId::new("2"));
    }

    #[tokio::test]
    async fn an_empty_listing_returns_an_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [], "links": {}})),
            )
            .mount(&server)
            .await;

        let warnings = tracker(&server, TokenPlacement::Header)
            .list_hit_and_runs()
            .await
            .expect("listing succeeds");

        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn a_malformed_listing_returns_protocol_not_an_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})))
            .mount(&server)
            .await;

        let error = tracker(&server, TokenPlacement::Header)
            .list_hit_and_runs()
            .await
            .expect_err("malformed body must not be treated as zero warnings");

        assert!(matches!(error, TrackerError::Protocol(_)));
    }

    #[tokio::test]
    async fn unauthorized_is_not_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let error = tracker(&server, TokenPlacement::Header)
            .list_hit_and_runs()
            .await
            .expect_err("401 is an error");

        assert!(matches!(error, TrackerError::Unauthorized));
        assert!(!error.is_transient());
    }

    #[tokio::test]
    async fn rate_limiting_reports_retry_after_and_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
            .mount(&server)
            .await;

        let error = tracker(&server, TokenPlacement::Header)
            .list_hit_and_runs()
            .await
            .expect_err("429 is an error");

        assert!(matches!(
            error,
            TrackerError::RateLimited {
                retry_after_seconds: 30
            }
        ));
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn fetching_an_html_error_page_with_a_200_status_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/torrents/1/download"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>please log in</body></html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&server)
            .await;

        let error = tracker(&server, TokenPlacement::Header)
            .fetch_torrent_file(&TrackerTorrentId::new("1"))
            .await
            .expect_err("an HTML page is not a torrent");

        assert!(matches!(error, TrackerError::Protocol(_)));
    }

    #[tokio::test]
    async fn a_real_looking_torrent_file_is_returned_as_bytes() {
        let server = MockServer::start().await;
        let bencoded = b"d8:announce20:http://example.com/e";
        Mock::given(method("GET"))
            .and(path("/api/torrents/1/download"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bencoded.to_vec())
                    .insert_header("content-type", "application/x-bittorrent"),
            )
            .mount(&server)
            .await;

        let bytes = tracker(&server, TokenPlacement::Header)
            .fetch_torrent_file(&TrackerTorrentId::new("1"))
            .await
            .expect("bencoded body is accepted");

        assert_eq!(bytes, bencoded);
    }

    #[tokio::test]
    async fn a_status_response_that_means_nothing_returns_unknown_not_cleared() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/torrents/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "attributes": { "hit_and_run_status": "on_probation" } }
            })))
            .mount(&server)
            .await;

        let status = tracker(&server, TokenPlacement::Header)
            .hit_and_run_status(&TrackerTorrentId::new("1"))
            .await
            .expect("response parses");

        assert_eq!(status, HitAndRunStatus::Unknown);
    }

    #[tokio::test]
    async fn a_cleared_status_response_reports_cleared() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/torrents/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "attributes": { "hit_and_run_status": "cleared" } }
            })))
            .mount(&server)
            .await;

        let status = tracker(&server, TokenPlacement::Header)
            .hit_and_run_status(&TrackerTorrentId::new("1"))
            .await
            .expect("response parses");

        assert_eq!(status, HitAndRunStatus::Cleared);
    }

    #[tokio::test]
    async fn the_token_can_be_sent_as_a_query_parameter_instead_of_a_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .and(auth_matcher(TokenPlacement::Query))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [], "links": {}})),
            )
            .mount(&server)
            .await;

        tracker(&server, TokenPlacement::Query)
            .list_hit_and_runs()
            .await
            .expect("query-parameter auth reaches the mock");
    }

    #[tokio::test]
    async fn no_error_message_contains_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let error = tracker(&server, TokenPlacement::Query)
            .list_hit_and_runs()
            .await
            .expect_err("500 is an error");

        assert!(!error.to_string().contains(TOKEN));
    }
}
