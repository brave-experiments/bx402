//! Binary entry point for the `bx402` service.

use std::error::Error;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        // Log the `Display` form; returning the error from `main` would surface the
        // `Debug` form instead.
        tracing::error!("{err}");
        std::process::exit(1);
    }
}

/// Boot the service: load configuration, wire dependencies, and serve until
/// shutdown. Every startup failure flows out through `?` to the single exit
/// site in `main`.
async fn run() -> Result<(), Box<dyn Error>> {
    // Load a local `.env` for development (no-op if absent); real environment
    // variables still take precedence.
    dotenvy::dotenv().ok();
    init_tracing();

    tracing::info!("{}", bx402::banner());

    let config = bx402::Config::from_env()?;
    tracing::info!("brave search api: {}", config.brave_search_api_base_url);
    tracing::info!("x402 facilitator: {}", config.x402_facilitator_url);

    // A configured but unreachable bucket aborts startup, so the service never serves
    // traffic with a broken screener.
    let (screener, screening) = bx402::init_screener(&config).await?;
    tracing::info!("restricted address screening: {screening}");

    let app = bx402::app(config, screener)?;

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Install the log subscriber on stderr. `RUST_LOG` sets the level (default `info`).
/// Color is on only for a terminal, so piped and container logs stay plain.
fn init_tracing() {
    use std::io::IsTerminal;
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
