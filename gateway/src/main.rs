//! Tessera gateway: a drop-in reverse proxy that masks personal data before it
//! reaches a model provider and restores it in the response.
//!
//! Every failure refuses the request. A detector that errors or times out, a
//! body whose shape we do not recognize, or a placeholder the mapping does not
//! know all end the request rather than forwarding unmasked text or handing a
//! placeholder to the client.

mod audit;
mod config;
mod detector;
mod mapping;
mod provider;
mod proxy;
mod session;
mod stream;

use std::sync::Arc;

use config::Config;
use proxy::{router, AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let path = std::env::args().nth(1);
    let config = match path {
        Some(path) => Config::from_toml(&std::fs::read_to_string(path)?)?,
        None => Config::from_toml("")?,
    };

    let bind = config.bind.clone();
    let audit = Arc::new(audit::Audit::open(std::path::Path::new(
        &config.audit_path,
    ))?);
    let state = Arc::new(AppState::from_config(&config, audit));
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        %bind,
        detector = %config.detector_url,
        audit = %config.audit_path,
        "gateway listening"
    );
    axum::serve(listener, router(state)).await?;
    Ok(())
}
