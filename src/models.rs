use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_type!(ProjectId);
id_type!(TaskId);
id_type!(SessionId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRuntimeStatus {
    Working,
    Blocked,
    Idle,
    Done,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Queued,
    Preparing,
    RunningCodex,
    Verifying,
    NeedsReview,
    Failed,
    Cancelled,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub title: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(project_id: ProjectId, title: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            project_id,
            title: title.into(),
            status: TaskStatus::Queued,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub agent_name: String,
    pub runtime_status: AgentRuntimeStatus,
    pub worktree_path: Option<String>,
    pub recent_activity: Option<String>,
    pub changed_files: usize,
    pub attention_required: bool,
    pub updated_at: DateTime<Utc>,
}
