use anyhow::{Context, Result};
use seedmedic::{
    bootstrap,
    config::{Config, Severity},
    repair::reconcile::reconcile_on_startup,
    web,
};
use tokio::{net::TcpListener, signal, sync::watch};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    if std::env::args().nth(1).as_deref() == Some("--check-config") {
        return check_config();
    }

    let config_path = Config::default_path();
    let config = Config::load_from(&config_path)?;
    let app = bootstrap::build(config, &config_path).await?;

    // Before any new work: make the persisted state agree with reality.
    reconcile_on_startup(&app.deps, &app.worker_config.owner).await;

    let listener = TcpListener::bind(app.bind_address)
        .await
        .with_context(|| format!("failed to bind {}", app.bind_address))?;
    let router = web::router(
        app.deps.clone(),
        app.auth_token.clone(),
        app.health_threshold,
        app.config_summary.clone(),
        app.metrics_enabled,
        app.chrome.clone(),
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker = tokio::spawn(app.worker().run(shutdown_rx));

    info!(address = %app.bind_address, "seedmedic listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server failed")?;

    let _ = shutdown_tx.send(true);
    worker.await.context("repair worker panicked")?;

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
