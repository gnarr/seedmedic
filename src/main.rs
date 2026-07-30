use anyhow::{Context, Result};
use seedmedic::{
    bootstrap,
    config::{Config, Severity},
    connectivity::{self, ProbeResult},
    runtime::RuntimeHandle,
    web,
};
use tokio::{net::TcpListener, signal};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    match std::env::args().nth(1).as_deref() {
        Some("--check-config") => return check_config(),
        Some("--check-connections") => return check_connections().await,
        _ => {}
    }

    let config_path = Config::default_path();
    let config = Config::load_from(&config_path)?;
    let bind_address = config
        .server
        .bind_address
        .parse()
        .with_context(|| format!("invalid bind address {}", config.server.bind_address))?;

    // Opens the database — the only thing that outlives every future
    // reload — then wires the first generation, reconciles against reality,
    // and spawns its worker. See `seedmedic::runtime`.
    let persistent = bootstrap::open(&config).await?;
    let handle = RuntimeHandle::start(&config, persistent, config_path).await?;

    let listener = TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("failed to bind {bind_address}"))?;
    let router = web::router(handle.clone(), bind_address);

    info!(address = %bind_address, "seedmedic listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server failed")?;

    handle.stop_worker().await;

    Ok(())
}

/// Print a redacted summary and every configuration problem in one pass,
/// without opening the database or touching the network. Exits non-zero if
/// at least one is an error. A missing or unparseable file still fails hard,
/// via the `?` above, before anything is printed.
fn check_config() -> Result<()> {
    let config = Config::load_unvalidated()?;
    println!("{}", config.redacted_summary());

    let mut problems = config.problems();
    problems.extend(config.problems_on_disk());
    problems.sort_by_key(|problem| problem.severity != Severity::Error);

    for problem in &problems {
        let label = match problem.severity {
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
        };
        match &problem.key {
            Some(key) => println!("{label} {key}: {}", problem.message),
            None => println!("{label}: {}", problem.message),
        }
    }

    if problems
        .iter()
        .any(|problem| problem.severity == Severity::Error)
    {
        anyhow::bail!("configuration has errors");
    }
    println!("configuration OK");
    Ok(())
}

/// Probe every configured tracker, the download client, and every *arr
/// instance, for operators who never open a browser — the same probes
/// `/settings`'s "Test connection" buttons run, against the saved
/// configuration rather than an unsaved draft. Deliberately a separate flag
/// from `--check-config`, which must stay network-free to keep its "safe to
/// run against a production config from anywhere" property — see
/// docs/todos/0011-configuration-and-secrets.md.
async fn check_connections() -> Result<()> {
    let config = Config::load_unvalidated()?;
    let (results, any_failed) = probe_all(&config).await;

    for (label, result) in &results {
        let status = if result.ok { "OK" } else { "FAILED" };
        println!("{status} {label}: {}", result.detail);
    }

    if any_failed {
        anyhow::bail!("one or more connections failed");
    }
    Ok(())
}

/// The testable core of [`check_connections`]: probe every configured
/// integration and report whether any failed, without touching `stdout` or
/// `std::env`.
async fn probe_all(config: &Config) -> (Vec<(String, ProbeResult)>, bool) {
    let mut results = Vec::new();
    let mut any_failed = false;

    for tracker in &config.trackers {
        let result = connectivity::test_tracker(tracker).await;
        any_failed |= !result.ok;
        results.push((format!("tracker {}", tracker.id), result));
    }
    if let Some(download_client) = &config.download_client {
        let result = connectivity::test_download_client(download_client).await;
        any_failed |= !result.ok;
        results.push(("download client".to_owned(), result));
    }
    for arr in &config.arr {
        let result = connectivity::test_arr(arr).await;
        any_failed |= !result.ok;
        results.push((format!("arr {}", arr.name), result));
    }

    (results, any_failed)
}

fn init_logging() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install terminate handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutting down");
}

#[cfg(test)]
mod tests {
    use seedmedic::config::{Secret, TokenPlacement, TrackerConfig, TrackerKind};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    #[tokio::test]
    async fn a_failing_tracker_is_named_and_flags_the_whole_run() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hit-and-runs"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let mut config = Config::default();
        config.trackers.push(TrackerConfig {
            id: "broken".to_owned(),
            kind: TrackerKind::Unit3d,
            base_url: url::Url::parse(&server.uri()).expect("valid url"),
            api_key: Secret::new("s3cr3t-token"),
            api_key_file: None,
            token_placement: TokenPlacement::Header,
        });

        let (results, any_failed) = probe_all(&config).await;

        assert!(any_failed);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "tracker broken");
        assert!(!results[0].1.ok);
    }

    #[tokio::test]
    async fn every_integration_healthy_reports_no_failure() {
        let (results, any_failed) = probe_all(&Config::default()).await;
        assert!(!any_failed);
        assert!(results.is_empty());
    }
}
