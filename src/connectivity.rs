//! Connection tests: prove a configured tracker, *arr instance, or download
//! client is reachable and its credentials are accepted, before a repair
//! needs it — for the settings UI's "Test connection" buttons and
//! `--check-connections`. See `docs/todos/0019-connection-tests.md`.
//!
//! No port gained a new method for this: each probe calls exactly the method
//! `docs/todos/0019` verified is already sufficient — `TorrentClient::summary`,
//! `TrackerClient::list_hit_and_runs`, `CandidateSource::find_candidates` with
//! a probe release title. Building the throwaway adapter each probe needs is
//! delegated to `bootstrap::build_tracker`/`build_client`/`build_arr_source`,
//! so this module — like `bootstrap.rs` — never names a concrete adapter
//! itself.

use std::time::Duration;

use crate::{
    bootstrap,
    config::{ArrConfig, DownloadClientConfig, TrackerConfig},
    library::CandidateQuery,
};

/// Bounds both a probe's HTTP client (per request) and the whole probe call
/// (across every request an adapter makes, including pagination) — see the
/// module doc's note on `Unit3dTracker::list_hit_and_runs`'s unbounded
/// pagination: a probe must fail loudly rather than hang.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Genuinely parses as a release (`api/v3/parse` does real work) but matches
/// nothing in a real library, so an *arr probe proves reachability and
/// credentials without depending on anything the operator has actually
/// imported.
const PROBE_RELEASE_TITLE: &str = "Test.Show.S01E01.1080p.WEB-DL.x264-GROUP";

/// A message truncated to about this many characters — long enough to be
/// useful, short enough that a remote response fragment
/// (`ClientError::Rejected`/`TrackerError`/`CandidateError` can all carry
/// one) cannot turn a probe result into an unbounded render.
const MAX_DETAIL_CHARS: usize = 200;

#[derive(Debug)]
pub struct ProbeResult {
    pub ok: bool,
    pub detail: String,
}

impl ProbeResult {
    fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn failed(detail: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            detail: truncate(&detail.to_string()),
        }
    }

    fn timed_out() -> Self {
        Self::failed(format!(
            "timed out after {}s waiting for a response",
            PROBE_TIMEOUT.as_secs()
        ))
    }
}

fn truncate(message: &str) -> String {
    if message.chars().count() <= MAX_DETAIL_CHARS {
        return message.to_owned();
    }
    let mut truncated: String = message.chars().take(MAX_DETAIL_CHARS).collect();
    truncated.push('…');
    truncated
}

/// A fresh `reqwest::Client` per probe, timed out per request — separate
/// from `bootstrap`'s shared, untimed client (see `docs/todos/0019`'s note
/// that the production adapters' lack of a timeout is a real but separate
/// problem).
fn probe_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(concat!("seedmedic/", env!("CARGO_PKG_VERSION")))
        .timeout(PROBE_TIMEOUT)
        .build()
}

pub async fn test_tracker(config: &TrackerConfig) -> ProbeResult {
    let http = match probe_http_client() {
        Ok(http) => http,
        Err(error) => return ProbeResult::failed(error),
    };
    let tracker = bootstrap::build_tracker(config, http);

    match tokio::time::timeout(PROBE_TIMEOUT, tracker.list_hit_and_runs()).await {
        Ok(Ok(warnings)) => ProbeResult::ok(format!(
            "reachable — {} outstanding hit-and-run(s)",
            warnings.len()
        )),
        Ok(Err(error)) => ProbeResult::failed(error),
        Err(_) => ProbeResult::timed_out(),
    }
}

pub async fn test_download_client(config: &DownloadClientConfig) -> ProbeResult {
    let http = match probe_http_client() {
        Ok(http) => http,
        Err(error) => return ProbeResult::failed(error),
    };
    let client = bootstrap::build_client(config, http);

    match tokio::time::timeout(PROBE_TIMEOUT, client.summary()).await {
        Ok(Ok(summary)) => ProbeResult::ok(format!(
            "reachable — {} torrent(s) known",
            summary.torrent_count
        )),
        Ok(Err(error)) => ProbeResult::failed(error),
        Err(_) => ProbeResult::timed_out(),
    }
}

pub async fn test_arr(config: &ArrConfig) -> ProbeResult {
    let http = match probe_http_client() {
        Ok(http) => http,
        Err(error) => return ProbeResult::failed(error),
    };
    let source = bootstrap::build_arr_source(config, http);
    let query = CandidateQuery {
        torrent_name: PROBE_RELEASE_TITLE,
        files: &[],
    };

    match tokio::time::timeout(PROBE_TIMEOUT, source.find_candidates(&query)).await {
        Ok(Ok(_)) => ProbeResult::ok("reachable — the configured API key was accepted"),
        Ok(Err(error)) => ProbeResult::failed(error),
        Err(_) => ProbeResult::timed_out(),
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::config::{ArrKind, Secret, TokenPlacement, TrackerKind};

    fn tracker_config(base_url: &str) -> TrackerConfig {
        TrackerConfig {
            id: "test".to_owned(),
            kind: TrackerKind::Unit3d,
            base_url: url::Url::parse(base_url).expect("valid url"),
            api_key: Secret::new("s3cr3t-token"),
            api_key_file: None,
            token_placement: TokenPlacement::Header,
        }
    }

    fn download_client_config(base_url: &str) -> DownloadClientConfig {
        DownloadClientConfig {
            kind: crate::config::DownloadClientKind::QBittorrent,
            base_url: url::Url::parse(base_url).expect("valid url"),
            username: "admin".to_owned(),
            password: Secret::new("s3cr3t-p4ssw0rd"),
            password_file: None,
            category: None,
        }
    }

    fn arr_config(base_url: &str) -> ArrConfig {
        ArrConfig {
            kind: ArrKind::Sonarr,
            name: "main".to_owned(),
            base_url: url::Url::parse(base_url).expect("valid url"),
            api_key: Secret::new("s3cr3t-arr-key"),
            api_key_file: None,
            path_mappings: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_healthy_tracker_reports_success_and_the_count() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": 1, "name": "One", "size": 1},
                    {"id": 2, "name": "Two", "size": 2},
                ],
                "links": { "next": null },
            })))
            .mount(&server)
            .await;

        let result = test_tracker(&tracker_config(&server.uri())).await;
        assert!(result.ok);
        assert!(result.detail.contains('2'));
    }

    #[tokio::test]
    async fn a_401_reports_rejected_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let result = test_tracker(&tracker_config(&server.uri())).await;
        assert!(!result.ok);
        assert!(result.detail.contains("credentials"));
    }

    #[tokio::test]
    async fn a_refused_connection_reports_a_transport_failure() {
        // No mock server bound at all: the connection itself is refused.
        let result = test_tracker(&tracker_config("http://127.0.0.1:1")).await;
        assert!(!result.ok);
    }

    /// A slow server hangs the request itself, not just the adapter's own
    /// call — so it is the probe HTTP client's own `.timeout()` that fires
    /// here, surfacing as the adapter's ordinary transport error rather than
    /// this module's own "timed out" message (that message is for a slow
    /// adapter whose *aggregate* work — e.g. pagination — outlasts the
    /// call as a whole; see the module doc). Either way, the requirement
    /// this test actually protects is that a server which never responds is
    /// reported as a failure well within `PROBE_TIMEOUT`, never left to hang.
    #[tokio::test]
    async fn a_tracker_that_never_responds_reports_a_failure_not_a_hang() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;

        let result = tokio::time::timeout(
            PROBE_TIMEOUT + Duration::from_secs(5),
            test_tracker(&tracker_config(&server.uri())),
        )
        .await
        .expect("the probe itself must return well within the test's own timeout");
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn an_arr_probe_with_a_valid_key_and_an_unmatched_release_reports_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "series": null })),
            )
            .mount(&server)
            .await;

        let result = test_arr(&arr_config(&server.uri())).await;
        assert!(result.ok, "{}", result.detail);
    }

    #[tokio::test]
    async fn an_arr_probe_with_403_reports_the_rejected_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let result = test_arr(&arr_config(&server.uri())).await;
        assert!(!result.ok);
        assert!(result.detail.contains("API key"));
    }

    #[tokio::test]
    async fn a_download_client_probe_reports_the_torrent_count() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("Ok.")
                    .insert_header("set-cookie", "SID=abc123; path=/; HttpOnly"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"hash": "a", "state": "uploading", "progress": 1.0, "save_path": "/x"},
            ])))
            .mount(&server)
            .await;

        let result = test_download_client(&download_client_config(&server.uri())).await;
        assert!(result.ok, "{}", result.detail);
        assert!(result.detail.contains('1'));
    }

    #[tokio::test]
    async fn a_download_client_probe_with_a_wrong_password_reports_rejected_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
            .mount(&server)
            .await;

        let result = test_download_client(&download_client_config(&server.uri())).await;
        assert!(!result.ok);
        assert!(result.detail.contains("credentials"));
    }

    /// A remote fragment (`ClientError::Rejected`, `TrackerError::Protocol`,
    /// `CandidateError::Protocol` can all carry one) must never reach a
    /// probe result unbounded.
    #[test]
    fn a_long_message_is_truncated() {
        let long = "x".repeat(10_000);
        let truncated = truncate(&long);
        assert_eq!(truncated.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn a_short_message_is_untouched() {
        assert_eq!(truncate("short"), "short");
    }
}
