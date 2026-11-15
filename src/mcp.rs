//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0).

use anyhow::{bail, Result};
use serde_json::Value;

use crate::{config::AppConfig, graph::GraphClient, storage::Storage};

pub async fn run(_config: AppConfig, _graph: GraphClient, _storage: Storage) -> Result<()> {
    bail!("mcp::run not yet implemented")
}

/// Single-line JSON-RPC handler exposed for deterministic unit & integration testing.
pub async fn handle_rpc_line(
    _config: &AppConfig,
    _graph: &GraphClient,
    _storage: &Storage,
    _line: &str,
) -> Option<Value> {
    None
}
