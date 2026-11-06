use aduana::{AppState, app, config};
use anyhow::{Context, Result, bail};
use tokio::{
    net::{TcpListener, TcpStream},
    signal::unix::{SignalKind, signal},
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => serve().await,
        Some("gen-env") => {
            print!("{}", config::generate_env()?);
            Ok(())
        }
        // The runtime image has no shell or curl for a Docker healthcheck.
        Some("healthcheck") => {
            TcpStream::connect(("127.0.0.1", 8080))
                .await
                .context("service is not accepting connections")?;
            Ok(())
        }
        Some(other) => bail!("unknown command '{other}'; expected serve, gen-env or healthcheck"),
    }
}

async fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::from_env()?;
    let listen_addr = config.listen_addr;
    let state = AppState::new(config).await?;

    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("could not listen on {listen_addr}"))?;
    tracing::info!(%listen_addr, "aduana listening");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

async fn shutdown_signal() {
    let mut terminate = signal(SignalKind::terminate()).expect("could not install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    tracing::info!("shutting down");
}
