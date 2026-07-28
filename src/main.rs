use anyhow::{Context, Result};
use seedmedic::{bootstrap, config::Config, repair::reconcile::reconcile_on_startup, web};
use tokio::{net::TcpListener, signal, sync::watch};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    if std::env::args().nth(1).as_deref() == Some("--check-config") {
        return check_config();
    }

    let config = Config::load()?;
    let app = bootstrap::build(config).await?;

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
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker = tokio::spawn(app.worker().run(wait_for_shutdown(shutdown_rx)));

    info!(address = %app.bind_address, "seedmedic listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server failed")?;

    let _ = shutdown_tx.send(true);
    worker.await.context("repair worker panicked")?;

    Ok(())
}

/// Validate the configuration and print a redacted summary, without opening
/// the database or touching the network. Exits non-zero (via the `?` above)
/// on any error, with a message naming what is wrong.
fn check_config() -> Result<()> {
    let config = Config::load()?;
    println!("{}", config.redacted_summary());
    println!("configuration OK");
    Ok(())
}

fn init_logging() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
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
