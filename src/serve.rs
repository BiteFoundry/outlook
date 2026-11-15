//! Local HTTP JSON API (`teams serve` / `teams api`) powered by `axum`.

use anyhow::{bail, Result};

use crate::{config::AppConfig, graph::GraphClient, storage::Storage};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8788,
            token: None,
        }
    }
}

pub fn build_router(
    _config: AppConfig,
    _graph: GraphClient,
    _storage: Storage,
    _token: Option<String>,
) -> axum::Router {
    axum::Router::new()
}

pub async fn run(
    _config: AppConfig,
    _graph: GraphClient,
    _storage: Storage,
    _opts: ServeOptions,
) -> Result<()> {
    bail!("serve::run not yet implemented")
}
