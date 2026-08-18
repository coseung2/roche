//! Public Codex connection, activity, history, event, and catalog models.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexConnection {
    Starting,
    Ready { version: String },
    Offline { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexActivityKind {
    Terminal,
    FileChange,
    ToolCall,
    WebSearch,
    Worker,
}

impl CodexActivityKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "터미널 작업",
            Self::FileChange => "파일 변경",
            Self::ToolCall => "도구 요청",
            Self::WebSearch => "웹 검색",
            Self::Worker => "워커 작업",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexActivityPhase {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexActivity {
    pub item_id: String,
    pub kind: CodexActivityKind,
    pub phase: CodexActivityPhase,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub worker_thread_id: Option<String>,
    #[serde(default)]
    pub worker_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexStoredThread {
    pub thread_id: String,
    pub name: Option<String>,
    pub preview: String,
    pub cwd: PathBuf,
    pub parent_thread_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHistoryRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHistoryMessage {
    pub role: CodexHistoryRole,
    pub text: String,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexEvent {
    Connection(CodexConnection),
    StoredThreads {
        threads: Vec<CodexStoredThread>,
    },
    ThreadHistoryLoaded {
        thread_id: String,
        messages: Vec<CodexHistoryMessage>,
    },
    ThreadResumeFailed {
        thread_id: String,
        message: String,
    },
    ThreadStarted {
        thread_id: String,
        model: Option<String>,
    },
    TurnStarted {
        thread_id: String,
        turn_id: String,
    },
    AssistantDelta {
        thread_id: String,
        turn_id: String,
        delta: String,
    },
    AssistantCompleted {
        thread_id: String,
        turn_id: String,
        text: String,
    },
    Activity {
        thread_id: String,
        turn_id: String,
        activity: CodexActivity,
    },
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        status: String,
    },
    CatalogLoaded {
        source: String,
        models: Vec<CodexCatalogModel>,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCatalogModel {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    pub default_reasoning_level: Option<String>,
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexReasoningLevel {
    pub effort: String,
    pub description: Option<String>,
}
