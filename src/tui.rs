//! TUI entry shell: terminal raw mode, mouse capture, keyboard enhancement, event loop, external editor.

use anyhow::{bail, Result};

use crate::{auth::AuthManager, config::AppConfig, graph::GraphClient, storage::Storage};

pub async fn run(
    _config: AppConfig,
    _auth: AuthManager,
    _graph: GraphClient,
    _storage: Storage,
) -> Result<()> {
    bail!("tui::run not yet implemented")
}
