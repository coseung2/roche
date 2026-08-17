use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::models::AgentRuntimeStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HerdrSessionSnapshot {
    pub workspace_id: String,
    pub pane_id: String,
    pub agent_name: String,
    pub status: AgentRuntimeStatus,
    pub worktree_path: Option<String>,
    pub recent_activity: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HerdrEvent {
    SessionUpserted(HerdrSessionSnapshot),
    SessionRemoved {
        pane_id: String,
    },
    TerminalActivity {
        pane_id: String,
        summary: String,
        at: DateTime<Utc>,
    },
}

#[derive(Debug, Error)]
pub enum HerdrError {
    #[error("Herdr transport is unavailable: {0}")]
    Unavailable(String),
    #[error("Herdr protocol error: {0}")]
    Protocol(String),
}

#[async_trait]
pub trait HerdrClient: Send + Sync {
    async fn session_snapshot(&self) -> Result<Vec<HerdrSessionSnapshot>, HerdrError>;
    async fn agent_prompt(&self, pane_id: &str, prompt: &str) -> Result<(), HerdrError>;
    async fn agent_stop(&self, pane_id: &str) -> Result<(), HerdrError>;
    async fn pane_recent_output(
        &self,
        pane_id: &str,
        max_lines: usize,
    ) -> Result<Vec<String>, HerdrError>;
}
