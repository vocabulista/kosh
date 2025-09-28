use std::future::{pending, Future};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{routing::get, Router};
use kosh_core::{ApiConfig, KoshConfig};
use thiserror::Error;
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::info;

/// Lightweight wrapper around the Axum HTTP server.
pub struct ApiServer {
    config: Arc<KoshConfig>,
}

impl ApiServer {
    pub fn new(config: KoshConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    pub fn with_config(config: Arc<KoshConfig>) -> Self {
        Self { config }
    }

    pub async fn serve(self) -> Result<(), ApiError> {
        self.serve_with_shutdown(pending::<()>()).await
    }

    pub fn spawn(self) -> JoinHandle<Result<(), ApiError>> {
        tokio::spawn(async move { self.serve().await })
    }

    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), ApiError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let router = self.router();
        let addr = self.socket_addr();

        info!(%addr, "starting HTTP service");

        let listener = TcpListener::bind(addr).await.map_err(ApiError::Bind)?;

        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(ApiError::Server)?;

        Ok(())
    }

    fn router(&self) -> Router {
        Router::new().route("/health", get(Self::healthcheck))
    }

    fn socket_addr(&self) -> SocketAddr {
        let api = &self.config.api;
        let host = api
            .host
            .parse()
            .unwrap_or_else(|_| "0.0.0.0".parse().expect("valid host"));
        SocketAddr::new(host, api.port)
    }

    async fn healthcheck() -> &'static str {
        "ok"
    }

    pub fn api_config(&self) -> &ApiConfig {
        &self.config.api
    }

    pub fn shared_config(&self) -> Arc<KoshConfig> {
        Arc::clone(&self.config)
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("failed to bind listener")]
    Bind(#[source] std::io::Error),
    #[error("http server error")]
    Server(#[source] std::io::Error),
}
