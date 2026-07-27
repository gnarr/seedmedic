//! Sonarr and Radarr candidate discovery.
//!
//! Both APIs support parsing a release name (`GET /api/v3/parse?title=`) into
//! the series or movie it belongs to, then listing that title's imported files
//! (`GET /api/v3/episodefile?seriesId=` / `GET /api/v3/moviefile?movieId=`).
//! The two differ in endpoint names and response shapes but not in structure,
//! so one adapter serves both behind an `ArrKind` discriminant.
//!
//! Targets Sonarr/Radarr v3 (the currently supported API generation for both
//! projects). Read-only: nothing here ever calls anything but `GET`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tracing::warn;
use url::Url;

use crate::{
    config::Secret,
    library::{
        domain::{Candidate, CandidateOrigin, CandidateQuery},
        ports::{CandidateError, CandidateSource},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArrKind {
    Sonarr,
    Radarr,
}

impl ArrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr",
            Self::Radarr => "radarr",
        }
    }
}

/// Rewrites a path as the *arr's container sees it (`/tv/Show/...`) to the
/// path SeedMedic sees (`/srv/media/tv/Show/...`). The most common source of
/// "it worked for me" bug reports in this genre.
#[derive(Clone, Debug)]
pub struct PathMapping {
    pub from: PathBuf,
    pub to: PathBuf,
}

pub struct ArrCandidateSource {
    label: String,
    kind: ArrKind,
    instance: String,
    base_url: Url,
    api_key: Secret,
    http: Client,
    path_mappings: Vec<PathMapping>,
}

impl ArrCandidateSource {
    pub fn new(
        kind: ArrKind,
        instance: &str,
        base_url: Url,
        api_key: Secret,
        http: Client,
        path_mappings: Vec<PathMapping>,
    ) -> Self {
        Self {
            label: format!("{}:{instance}", kind.as_str()),
            kind,
            instance: instance.to_owned(),
            base_url,
            api_key,
            http,
            path_mappings,
        }
    }

    fn origin(&self) -> CandidateOrigin {
        match self.kind {
            ArrKind::Sonarr => CandidateOrigin::Sonarr {
                instance: self.instance.clone(),
            },
            ArrKind::Radarr => CandidateOrigin::Radarr {
                instance: self.instance.clone(),
            },
        }
    }

    fn url(&self, path: &str) -> Result<Url, CandidateError> {
        self.base_url
            .join(path)
            .map_err(|error| CandidateError::Protocol(format!("cannot build request URL: {error}")))
    }

    fn map_path(&self, reported: &str) -> PathBuf {
        let path = Path::new(reported);
        self.path_mappings
            .iter()
            .find_map(|mapping| {
                path.strip_prefix(&mapping.from)
                    .ok()
                    .map(|rest| mapping.to.join(rest))
            })
            .unwrap_or_else(|| path.to_path_buf())
    }

    /// A `reqwest::Error` embeds the request URL, which never carries the API
    /// key (it always goes in a header here) but is stripped anyway for
    /// consistency with the tracker adapters.
    fn transport_error(error: reqwest::Error) -> CandidateError {
        CandidateError::Transport(error.without_url().to_string())
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: Url) -> Result<T, CandidateError> {
        let response = self
            .http
            .get(url)
            .header("X-Api-Key", self.api_key.expose())
            .send()
            .await
            .map_err(Self::transport_error)?;

        let status = response.status();
        if status.is_success() {
            return response.json::<T>().await.map_err(|error| {
                CandidateError::Protocol(format!("cannot parse response: {error}"))
            });
        }

        Err(match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                CandidateError::Protocol(format!("{} rejected the configured API key", self.label))
            }
            other if other.is_server_error() => {
                CandidateError::Transport(format!("{} returned status {other}", self.label))
            }
            other => CandidateError::Protocol(format!("{} returned status {other}", self.label)),
        })
    }

    /// A candidate we cannot open is worse than no candidate: verify the
    /// mapped path exists and matches the reported size before returning it.
    async fn verify(&self, reported_path: &str, size: u64) -> Option<Candidate> {
        let path = self.map_path(reported_path);
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.len() == size => Some(Candidate {
                path,
                size_bytes: size,
                origin: self.origin(),
            }),
            Ok(metadata) => {
                warn!(
                    source = self.label,
                    path = %path.display(),
                    reported_size = size,
                    actual_size = metadata.len(),
                    "arr reported a size that does not match the file on disk; dropping candidate"
                );
                None
            }
            Err(error) => {
                warn!(
                    source = self.label,
                    path = %path.display(),
                    %error,
                    "arr-reported file is not accessible; dropping candidate"
                );
                None
            }
        }
    }

    async fn find_sonarr_candidates(
        &self,
        parse_url: Url,
    ) -> Result<Vec<Candidate>, CandidateError> {
        let parsed: SonarrParseResponse = self.get_json(parse_url).await?;
        let Some(series) = parsed.series else {
            return Ok(Vec::new());
        };

        let mut files_url = self.url("api/v3/episodefile")?;
        files_url
            .query_pairs_mut()
            .append_pair("seriesId", &series.id.to_string());
        let files: Vec<EpisodeFile> = self.get_json(files_url).await?;

        let wanted_season = parsed
            .parsed_episode_info
            .and_then(|info| info.season_number);

        let mut candidates = Vec::new();
        for file in files {
            if wanted_season.is_some_and(|season| season != file.season_number) {
                continue;
            }
            if let Some(candidate) = self.verify(&file.path, file.size).await {
                candidates.push(candidate);
            }
        }
        Ok(candidates)
    }

    async fn find_radarr_candidates(
        &self,
        parse_url: Url,
    ) -> Result<Vec<Candidate>, CandidateError> {
        let parsed: RadarrParseResponse = self.get_json(parse_url).await?;
        let Some(movie) = parsed.movie else {
            return Ok(Vec::new());
        };

        let mut files_url = self.url("api/v3/moviefile")?;
        files_url
            .query_pairs_mut()
            .append_pair("movieId", &movie.id.to_string());
        let files: Vec<MovieFile> = self.get_json(files_url).await?;

        let mut candidates = Vec::new();
        for file in files {
            if let Some(candidate) = self.verify(&file.path, file.size).await {
                candidates.push(candidate);
            }
        }
        Ok(candidates)
    }
}

#[derive(Deserialize)]
struct SonarrParseResponse {
    series: Option<SeriesRef>,
    #[serde(rename = "parsedEpisodeInfo", default)]
    parsed_episode_info: Option<ParsedEpisodeInfo>,
}

#[derive(Deserialize)]
struct SeriesRef {
    id: u64,
}

#[derive(Deserialize)]
struct ParsedEpisodeInfo {
    #[serde(rename = "seasonNumber", default)]
    season_number: Option<i32>,
}

#[derive(Deserialize)]
struct EpisodeFile {
    #[serde(rename = "seasonNumber")]
    season_number: i32,
    path: String,
    size: u64,
}

#[derive(Deserialize)]
struct RadarrParseResponse {
    movie: Option<MovieRef>,
}

#[derive(Deserialize)]
struct MovieRef {
    id: u64,
}

#[derive(Deserialize)]
struct MovieFile {
    path: String,
    size: u64,
}

#[async_trait]
impl CandidateSource for ArrCandidateSource {
    fn label(&self) -> &str {
        &self.label
    }

    async fn find_candidates(
        &self,
        query: &CandidateQuery<'_>,
    ) -> Result<Vec<Candidate>, CandidateError> {
        let mut parse_url = self.url("api/v3/parse")?;
        parse_url
            .query_pairs_mut()
            .append_pair("title", query.torrent_name);

        match self.kind {
            ArrKind::Sonarr => self.find_sonarr_candidates(parse_url).await,
            ArrKind::Radarr => self.find_radarr_candidates(parse_url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    use super::*;
    use crate::torrent::{SafeRelativePath, TorrentFile};

    const API_KEY: &str = "s3cr3t-arr-key";

    fn source(
        server: &MockServer,
        kind: ArrKind,
        path_mappings: Vec<PathMapping>,
    ) -> ArrCandidateSource {
        ArrCandidateSource::new(
            kind,
            "main",
            Url::parse(&server.uri()).expect("mock server URI parses"),
            Secret::new(API_KEY),
            Client::new(),
            path_mappings,
        )
    }

    fn query<'a>(files: &'a [TorrentFile]) -> CandidateQuery<'a> {
        CandidateQuery {
            torrent_name: "Show.S01.1080p.WEB-DL",
            files,
        }
    }

    #[tokio::test]
    async fn a_sonarr_season_pack_yields_one_candidate_per_episode_file() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().expect("tempdir");
        let episode_one = root.path().join("e01.mkv");
        let episode_two = root.path().join("e02.mkv");
        std::fs::write(&episode_one, vec![0u8; 10]).expect("write");
        std::fs::write(&episode_two, vec![0u8; 20]).expect("write");

        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .and(query_param("title", "Show.S01.1080p.WEB-DL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "series": { "id": 7 },
                "parsedEpisodeInfo": { "seasonNumber": 1 },
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/episodefile"))
            .and(query_param("seriesId", "7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "seasonNumber": 1, "path": episode_one.to_str().unwrap(), "size": 10 },
                { "seasonNumber": 1, "path": episode_two.to_str().unwrap(), "size": 20 },
                { "seasonNumber": 2, "path": "/should/be/filtered.mkv", "size": 99 },
            ])))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("Show.S01.1080p.WEB-DL/e01.mkv").expect("valid"),
            length: 10,
        }];

        let candidates = source(&server, ArrKind::Sonarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect("lookup succeeds");

        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.path == episode_one)
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.path == episode_two)
        );
        assert!(candidates.iter().all(|candidate| candidate.origin
            == CandidateOrigin::Sonarr {
                instance: "main".to_owned()
            }));
    }

    #[tokio::test]
    async fn a_radarr_parse_response_yields_the_movie_file() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().expect("tempdir");
        let movie = root.path().join("movie.mkv");
        std::fs::write(&movie, vec![0u8; 1_000]).expect("write");

        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "movie": { "id": 3 },
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/moviefile"))
            .and(query_param("movieId", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "path": movie.to_str().unwrap(), "size": 1_000 },
            ])))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("movie.mkv").expect("valid"),
            length: 1_000,
        }];

        let candidates = source(&server, ArrKind::Radarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect("lookup succeeds");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, movie);
        assert_eq!(candidates[0].size_bytes, 1_000);
        assert_eq!(
            candidates[0].origin,
            CandidateOrigin::Radarr {
                instance: "main".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn an_unmatched_release_yields_an_empty_vec_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "series": null })))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("e01.mkv").expect("valid"),
            length: 10,
        }];

        let candidates = source(&server, ArrKind::Sonarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect("an honest empty result is not an error");

        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn a_500_yields_a_transient_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("e01.mkv").expect("valid"),
            length: 10,
        }];

        let error = source(&server, ArrKind::Sonarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect_err("500 is an error");

        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn a_401_yields_a_non_transient_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("e01.mkv").expect("valid"),
            length: 10,
        }];

        let error = source(&server, ArrKind::Sonarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect_err("401 is an error");

        assert!(!error.is_transient());
    }

    #[tokio::test]
    async fn path_mapping_rewrites_a_container_path_to_a_host_path() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().expect("tempdir");
        let host_dir = root.path().join("srv/media");
        std::fs::create_dir_all(&host_dir).expect("dirs");
        std::fs::write(host_dir.join("movie.mkv"), vec![0u8; 5]).expect("write");

        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "movie": { "id": 1 } })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/moviefile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "path": "/movies/movie.mkv", "size": 5 },
            ])))
            .mount(&server)
            .await;

        let mappings = vec![PathMapping {
            from: PathBuf::from("/movies"),
            to: host_dir.clone(),
        }];

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("movie.mkv").expect("valid"),
            length: 5,
        }];

        let candidates = source(&server, ArrKind::Radarr, mappings)
            .find_candidates(&query(&files))
            .await
            .expect("lookup succeeds");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, host_dir.join("movie.mkv"));
    }

    #[tokio::test]
    async fn a_mapped_path_that_does_not_exist_is_dropped_not_returned() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "movie": { "id": 1 } })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/moviefile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "path": "/movies/missing.mkv", "size": 5 },
            ])))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("movie.mkv").expect("valid"),
            length: 5,
        }];

        let candidates = source(&server, ArrKind::Radarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect("a missing file is dropped, not an error");

        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn no_api_key_appears_in_any_error_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/parse"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("e01.mkv").expect("valid"),
            length: 10,
        }];

        let error = source(&server, ArrKind::Sonarr, Vec::new())
            .find_candidates(&query(&files))
            .await
            .expect_err("500 is an error");

        assert!(!error.to_string().contains(API_KEY));
    }
}
