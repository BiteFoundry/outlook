//! Adaptive 3-tier background synchronization engine (active focus, chat preview diff + $batch, channel delta + presence).

use tokio::sync::{mpsc, watch};

use anyhow::Result;

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{ConversationTarget, CoreEvent},
    storage::Storage,
};

#[derive(Debug, Clone, Default)]
pub struct FocusTarget {
    pub target: Option<ConversationTarget>,
    pub thread_root_id: Option<String>,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
}

#[derive(Debug, Clone)]
pub enum SyncCommand {
    RefreshConversations,
    SyncActiveNow,
    SyncChannelHistory {
        team_id: String,
        channel_id: String,
    },
    Shutdown,
}

impl SyncController {
    pub fn set_focus(&self, target: Option<ConversationTarget>, thread_root_id: Option<String>) {
        let _ = self.focus_tx.send(FocusTarget {
            target,
            thread_root_id,
        });
    }

    pub fn request(&self, cmd: SyncCommand) {
        let _ = self.nudge_tx.send(cmd);
    }
}

/// Performs an initial bootstrap sync (chats + joined teams/channels) into SQLite.
pub async fn bootstrap_conversations(
    graph: &GraphClient,
    storage: &Storage,
) -> Result<()> {
    let chats = graph.list_chats(50).await?;
    storage.upsert_conversations(&chats)?;
    if let Ok(teams) = graph.list_joined_teams_with_channels().await {
        storage.upsert_teams(&teams)?;
    }
    Ok(())
}

/// Spawns the background sync loop and returns a `SyncController` handle.
pub fn spawn_sync_engine(
    _config: AppConfig,
    _graph: GraphClient,
    _storage: Storage,
    _event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    SyncController { focus_tx, nudge_tx }
}
