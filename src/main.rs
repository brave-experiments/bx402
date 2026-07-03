//! Binary entry point for the `bx402` service.

use std::error::Error;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        // Print the error's `Display`; returning it from `main` would surface the
        // `Debug` form instead.
        eprintln!("{err}");
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

    println!("{}", bx402::banner());

    let config = bx402::Config::from_env()?;
    println!("brave search api: {}", config.brave_search_api_base_url);
    println!("x402 facilitator: {}", config.x402_facilitator_url);

    // A configured but unreachable bucket aborts startup, so the service never serves
    // traffic with a broken screener.
    let (screener, screening) = bx402::init_screener(&config).await?;
    println!("restricted address screening: {screening}");

    let app = bx402::app(config, screener)?;

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app).await?;
    Ok(())
}
