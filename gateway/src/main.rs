//! Tessera gateway: a drop-in reverse proxy that masks personal data before it
//! reaches a model provider and restores it in the response.
//!
//! Every failure refuses the request. A detector that errors or times out, a
//! body whose shape we do not recognize, or a placeholder the mapping does not
//! know all end the request rather than forwarding unmasked text or handing a
//! placeholder to the client.

mod audit;
mod auth;
mod config;
mod detection_cache;
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
    // **`callers` is on the startup line because the two deployments look
    // identical from outside.** An operator who meant to configure the list and
    // mistyped the section would otherwise learn which one they are running
    // from a stranger's request rather than from their own logs. The count is
    // there for the same reason and is not sensitive: it is how many digests
    // were loaded, and the digests themselves are never logged.
    let callers = match &state.callers {
        auth::Callers::Anyone => "anyone".to_owned(),
        auth::Callers::Accepted(accepted) => format!("{} accepted", accepted.len()),
    };
    tracing::info!(
        %bind,
        detector = %config.detector_url,
        audit = %config.audit_path,
        %callers,
        "gateway listening"
    );
    if !state.callers.is_closed() {
        tracing::warn!(
            "accepted_credentials is not set: this gateway serves anyone who can reach it, \
             and a caller who reaches it can read values back out of a session table — set \
             the key before binding anywhere but loopback"
        );
    }
    axum::serve(listener, router(state)).await?;
    Ok(())
}
