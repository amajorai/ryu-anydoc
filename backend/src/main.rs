//! Process shell for `ryu-anydoc`.
//!
//! With Core, the binary binds loopback and receives the per-plugin
//! `RYU_EXT_TOKEN`. As a standalone service, set `RYU_ANYDOC_HOST=0.0.0.0`
//! and one of the API-key variables, then put TLS and any edge rate limiting in
//! front of it.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use ryu_anydoc::{api::MOUNT, router, AnyDocState};
use tracing_subscriber::EnvFilter;

const DEFAULT_PORT: u16 = 8097;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let state = AnyDocState::from_env().context("initializing AnyDoc configuration")?;
    if !state.auth.has_any_token() {
        tracing::warn!(
			"no RYU_EXT_TOKEN or standalone API key configured; protected AnyDoc routes are fail-closed"
		);
    }

    let port = std::env::var("RYU_ANYDOC_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let host = std::env::var("RYU_ANYDOC_HOST")
        .or_else(|_| std::env::var("RYU_ANYDOC_HOSTNAME"))
        .unwrap_or_else(|_| "127.0.0.1".to_owned());
    let host: IpAddr = host
        .parse()
        .with_context(|| format!("RYU_ANYDOC_HOST must be an IP address, got `{host}`"))?;
    let address = SocketAddr::new(host, port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding AnyDoc at {address}"))?;
    tracing::info!(
        %address,
        mount = MOUNT,
        "ryu-anydoc listening; local conversion is offline and OCR is not provided"
    );
    axum::serve(listener, router(state))
        .await
        .context("AnyDoc HTTP server stopped")?;
    Ok(())
}
