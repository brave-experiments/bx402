//! Binary entry point for the `bx402` service.

use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load a local `.env` for development (no-op if absent); real environment
    // variables still take precedence.
    dotenvy::dotenv().ok();

    println!("{}", bx402::banner());

    let config = bx402::Config::from_env().unwrap_or_else(|err| {
        // Print the error's `Display` because `?` would surface the `Debug` form.
        eprintln!("{err}");
        std::process::exit(1);
    });
    println!("brave search api: {}", config.brave_search_api_base_url);
    println!("x402 facilitator: {}", config.x402_facilitator_url);

    // A configured but unreachable bucket aborts startup, so the service never serves
    // traffic with a broken screener.
    let (_screener, screening) = bx402::init_screener(&config).await.unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });
    println!("restricted address screening: {screening}");

    let app = bx402::app(config).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app).await?;
    Ok(())
}
