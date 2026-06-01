//! Binary entry point for the `bx402` service.

use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("{}", bx402::banner());

    let config = bx402::Config::from_env()?;
    println!("brave search api: {}", config.brave_search_api_base_url);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    println!("listening on http://{}", listener.local_addr()?);

    axum::serve(listener, bx402::app()).await?;
    Ok(())
}
