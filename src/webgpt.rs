use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    codex::{CodexCommand, CodexConnection, CodexEvent},
    sessions::{SessionGraph, SessionRuntime, SessionStatus},
};

pub const DEFAULT_WEBGPT_BRIDGE_ADDR: &str = "127.0.0.1:47831";
const BRIDGE_DESCRIPTOR_RELATIVE_PATH: &str = ".ai-bridge/roche-webgpt-runtime.json";
const MAX_TASK_EVENTS: usize = 2_000;
const UI_POLL_INTERVAL: Duration = Duration::from_millis(150);

static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);
static IN_PROCESS_BRIDGE: OnceLock<BridgeClientConfig> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
struct BridgeClientConfig {
    address: String,
    token: String,
    pid: u32,
    project_root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorTaskStatus {
    Queued,
    Preparing,
    RunningCodex,
    NeedsReview,
    Failed,
    Cancelled,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorTask {
    pub id: String,
    pub title: String,
    pub goal: String,
    pub acceptance: Vec<String>,
    pub effort: String,
    pub status: OrchestratorTaskStatus,
    pub turn_id: Option<String>,
    pub result: Option<String>,
    pub tool_activity: Vec<String>,
    pub revision_count: u32,
    pub cancel_requested: bool,
    pub created_at_ms: u128,
    pub updated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorEvent {
    pub seq: u64,
    pub task_id: Option<String>,
    pub event: String,
    pub summary: String,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub root: String,
    pub status_short: String,
    pub diff_stat: String,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebChatStatus {
    Pending,
    Claimed,
    Answered,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebChatRequest {
    pub id: String,
    pub text: String,
    pub reasoning_level: String,
    pub status: WebChatStatus,
    pub response: Option<String>,
    pub created_at_ms: u128,
    pub updated_at_ms: u128,
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    auth: Option<String>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug)]
struct BridgeState {
    project_root: PathBuf,
    auth_token: String,
    codex_ready: bool,
    codex_busy: bool,
    active_task_id: Option<String>,
    queue: VecDeque<String>,
    tasks: BTreeMap<String, OrchestratorTask>,
    events: VecDeque<OrchestratorEvent>,
    next_event_seq: u64,
    chat_requests: BTreeMap<String, WebChatRequest>,
    pending_chat: VecDeque<String>,
    sessions: SessionGraph,
    root_session_id: String,
}

impl BridgeState {
    fn new(project_root: PathBuf, auth_token: String) -> Self {
        let project_key = project_root.display().to_string();
        let mut sessions = SessionGraph::new();
        let root = sessions.create_root(&project_key, SessionRuntime::Unified, "Main");
        Self {
            project_root,
            auth_token,
            codex_ready: false,
            codex_busy: false,
            active_task_id: None,
            queue: VecDeque::new(),
            tasks: BTreeMap::new(),
            events: VecDeque::new(),
            next_event_seq: 1,
            chat_requests: BTreeMap::new(),
            pending_chat: VecDeque::new(),
            sessions,
            root_session_id: root.id,
        }
    }

    fn push_event(
        &mut self,
        task_id: Option<String>,
        event: impl Into<String>,
        summary: impl Into<String>,
    ) {
        let entry = OrchestratorEvent {
            seq: self.next_event_seq,
            task_id,
            event: event.into(),
            summary: summary.into(),
            timestamp_ms: now_ms(),
        };
        self.next_event_seq = self.next_event_seq.saturating_add(1);
        self.events.push_back(entry);
        while self.events.len() > MAX_TASK_EVENTS {
            self.events.pop_front();
        }
    }

    fn create_task(&mut self, params: &Value) -> Result<Value, String> {
        let goal = required_string(params, "goal")?;
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Web GPT task")
            .to_owned();
        let acceptance = params
            .get("acceptance")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let effort = normalize_effort(
            params
                .get("effort")
                .and_then(Value::as_str)
                .unwrap_or("high"),
        )?;
        let id = next_task_id();
        let timestamp = now_ms();
        let task = OrchestratorTask {
            id: id.clone(),
            title,
            goal,
            acceptance,
            effort,
            status: OrchestratorTaskStatus::Queued,
            turn_id: None,
            result: None,
            tool_activity: Vec::new(),
            revision_count: 0,
            cancel_requested: false,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        self.tasks.insert(id.clone(), task);
        self.queue.push_back(id.clone());
        self.push_event(Some(id.clone()), "task.queued", "Task queued by Web GPT");
        Ok(
            serde_json::to_value(self.tasks.get(&id).expect("inserted task"))
                .expect("task serialization cannot fail"),
        )
    }

    fn revise_task(&mut self, params: &Value) -> Result<Value, String> {
        let task_id = required_string(params, "task_id")?;
        let prompt = required_string(params, "prompt")?;
        let effort = params
            .get("effort")
            .and_then(Value::as_str)
            .map(normalize_effort)
            .transpose()?;
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| format!("Unknown task: {task_id}"))?;
        if !matches!(
            task.status,
            OrchestratorTaskStatus::NeedsReview | OrchestratorTaskStatus::Failed
        ) {
            return Err(format!(
                "Task {task_id} is {:?}; revision requires needs_review or failed",
                task.status
            ));
        }
        task.goal = format!(
            "{}\n\nRevision request #{}:\n{}",
            task.goal,
            task.revision_count.saturating_add(1),
            prompt
        );
        if let Some(effort) = effort {
            task.effort = effort;
        }
        task.revision_count = task.revision_count.saturating_add(1);
        task.status = OrchestratorTaskStatus::Queued;
        task.turn_id = None;
        task.result = None;
        task.cancel_requested = false;
        task.updated_at_ms = now_ms();
        self.queue.push_front(task_id.clone());
        self.push_event(
            Some(task_id.clone()),
            "task.revision_queued",
            "Revision queued by Web GPT",
        );
        Ok(
            serde_json::to_value(self.tasks.get(&task_id).expect("existing task"))
                .expect("task serialization cannot fail"),
        )
    }

    fn cancel_task(
        &mut self,
        params: &Value,
        commands: &Sender<CodexCommand>,
    ) -> Result<Value, String> {
        let task_id = required_string(params, "task_id")?;
        let active = self.active_task_id.as_deref() == Some(task_id.as_str());
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| format!("Unknown task: {task_id}"))?;
        match task.status {
            OrchestratorTaskStatus::Queued | OrchestratorTaskStatus::Preparing => {
                self.queue.retain(|queued| queued != &task_id);
                task.status = OrchestratorTaskStatus::Cancelled;
            }
            OrchestratorTaskStatus::RunningCodex if active => {
                task.cancel_requested = true;
                commands
                    .send(CodexCommand::Interrupt)
                    .map_err(|_| "Codex command channel is closed".to_owned())?;
            }
            OrchestratorTaskStatus::NeedsReview | OrchestratorTaskStatus::Failed => {
                task.status = OrchestratorTaskStatus::Cancelled;
            }
            OrchestratorTaskStatus::Cancelled | OrchestratorTaskStatus::Completed => {}
            OrchestratorTaskStatus::RunningCodex => {
                return Err("Task is running but is not the active orchestrated task".to_owned());
            }
        }
        task.updated_at_ms = now_ms();
        self.push_event(
            Some(task_id.clone()),
            "task.cancel_requested",
            if active {
                "Cancellation sent to Rust Codex runtime"
            } else {
                "Queued task cancelled"
            },
        );
        Ok(
            serde_json::to_value(self.tasks.get(&task_id).expect("existing task"))
                .expect("task serialization cannot fail"),
        )
    }

    fn approve_task(&mut self, params: &Value) -> Result<Value, String> {
        let task_id = required_string(params, "task_id")?;
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| format!("Unknown task: {task_id}"))?;
        if task.status != OrchestratorTaskStatus::NeedsReview {
            return Err(format!(
                "Task {task_id} is {:?}; only needs_review can be approved",
                task.status
            ));
        }
        task.status = OrchestratorTaskStatus::Completed;
        task.updated_at_ms = now_ms();
        self.push_event(
            Some(task_id.clone()),
            "task.completed",
            "Task approved after review",
        );
        Ok(
            serde_json::to_value(self.tasks.get(&task_id).expect("existing task"))
                .expect("task serialization cannot fail"),
        )
    }

    fn submit_chat(&mut self, params: &Value) -> Result<Value, String> {
        let text = required_string(params, "text")?;
        let reasoning_level = params
            .get("reasoning_level")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("very_high")
            .to_owned();
        let id = format!(
            "chat-{}-{}",
            now_ms(),
            TASK_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let timestamp = now_ms();
        let request = WebChatRequest {
            id: id.clone(),
            text,
            reasoning_level,
            status: WebChatStatus::Pending,
            response: None,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        self.chat_requests.insert(id.clone(), request);
        self.pending_chat.push_back(id.clone());
        self.push_event(
            None,
            "chat.pending",
            format!("Web GPT chat request queued: {id}"),
        );
        Ok(
            serde_json::to_value(self.chat_requests.get(&id).expect("inserted chat request"))
                .expect("chat request serialization cannot fail"),
        )
    }

    fn claim_pending_chat(&mut self) -> Result<Value, String> {
        while let Some(id) = self.pending_chat.pop_front() {
            let Some(request) = self.chat_requests.get_mut(&id) else {
                continue;
            };
            if request.status != WebChatStatus::Pending {
                continue;
            }
            request.status = WebChatStatus::Claimed;
            request.updated_at_ms = now_ms();
            let value = serde_json::to_value(&*request)
                .map_err(|error| format!("Could not serialize chat request: {error}"))?;
            self.push_event(
                None,
                "chat.claimed",
                format!("Web GPT claimed chat request: {id}"),
            );
            return Ok(value);
        }
        Ok(Value::Null)
    }

    fn release_chat(&mut self, params: &Value) -> Result<Value, String> {
        let request_id = required_string(params, "request_id")?;
        let request = self
            .chat_requests
            .get_mut(&request_id)
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))?;
        if request.status != WebChatStatus::Claimed {
            return Err(format!(
                "Chat request {request_id} is {:?}; only claimed requests can be released",
                request.status
            ));
        }
        request.status = WebChatStatus::Pending;
        request.updated_at_ms = now_ms();
        self.pending_chat.push_front(request_id.clone());
        self.push_event(
            None,
            "chat.released",
            format!("Web GPT released chat request: {request_id}"),
        );
        Ok(serde_json::to_value(
            self.chat_requests
                .get(&request_id)
                .expect("existing chat request"),
        )
        .expect("chat request serialization cannot fail"))
    }

    fn respond_chat(&mut self, params: &Value) -> Result<Value, String> {
        let request_id = required_string(params, "request_id")?;
        let text = required_string(params, "text")?;
        let request = self
            .chat_requests
            .get_mut(&request_id)
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))?;
        if matches!(
            request.status,
            WebChatStatus::Answered | WebChatStatus::Cancelled
        ) {
            return Err(format!(
                "Chat request {request_id} is already {:?}",
                request.status
            ));
        }
        request.status = WebChatStatus::Answered;
        request.response = Some(text);
        request.updated_at_ms = now_ms();
        self.push_event(
            None,
            "chat.answered",
            format!("Web GPT answered chat request: {request_id}"),
        );
        Ok(serde_json::to_value(
            self.chat_requests
                .get(&request_id)
                .expect("existing chat request"),
        )
        .expect("chat request serialization cannot fail"))
    }

    fn poll_chat(&self, params: &Value) -> Result<Value, String> {
        let request_id = required_string(params, "request_id")?;
        self.chat_requests
            .get(&request_id)
            .map(|request| {
                serde_json::to_value(request).expect("chat request serialization cannot fail")
            })
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))
    }

    fn cancel_chat(&mut self, params: &Value) -> Result<Value, String> {
        let request_id = required_string(params, "request_id")?;
        let request = self
            .chat_requests
            .get_mut(&request_id)
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))?;
        if request.status != WebChatStatus::Answered {
            request.status = WebChatStatus::Cancelled;
            request.updated_at_ms = now_ms();
            self.pending_chat.retain(|id| id != &request_id);
        }
        self.push_event(
            None,
            "chat.cancelled",
            format!("Web GPT chat request cancelled: {request_id}"),
        );
        Ok(serde_json::to_value(
            self.chat_requests
                .get(&request_id)
                .expect("existing chat request"),
        )
        .expect("chat request serialization cannot fail"))
    }

    fn dispatch_next(&mut self, commands: &Sender<CodexCommand>) {
        if !self.codex_ready || self.codex_busy || self.active_task_id.is_some() {
            return;
        }
        while let Some(task_id) = self.queue.pop_front() {
            let Some(task) = self.tasks.get_mut(&task_id) else {
                continue;
            };
            if task.status != OrchestratorTaskStatus::Queued {
                continue;
            }
            task.status = OrchestratorTaskStatus::Preparing;
            task.updated_at_ms = now_ms();
            let prompt = orchestrator_prompt(task);
            let effort = task.effort.clone();
            self.active_task_id = Some(task_id.clone());
            self.codex_busy = true;
            self.push_event(
                Some(task_id.clone()),
                "task.preparing",
                "Rust Orchestrator dispatched task to Codex",
            );
            if commands
                .send(CodexCommand::Send {
                    text: prompt,
                    attachments: Vec::new(),
                    effort,
                    model: None,
                })
                .is_err()
            {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::Failed;
                    task.updated_at_ms = now_ms();
                }
                self.active_task_id = None;
                self.codex_busy = false;
                self.push_event(
                    Some(task_id),
                    "task.failed",
                    "Codex command channel is closed",
                );
            }
            break;
        }
    }

    fn handle_codex_event(&mut self, event: CodexEvent, commands: &Sender<CodexCommand>) {
        match event {
            CodexEvent::Connection(connection) => {
                self.codex_ready = matches!(connection, CodexConnection::Ready { .. });
                let summary = match connection {
                    CodexConnection::Starting => "Codex runtime starting".to_owned(),
                    CodexConnection::Ready { version } => format!("Codex runtime ready: {version}"),
                    CodexConnection::Offline { message } => {
                        if let Some(task_id) = self.active_task_id.take()
                            && let Some(task) = self.tasks.get_mut(&task_id)
                        {
                            task.status = OrchestratorTaskStatus::Failed;
                            task.updated_at_ms = now_ms();
                        }
                        self.codex_busy = false;
                        format!("Codex runtime offline: {message}")
                    }
                };
                self.push_event(None, "runtime.connection", summary);
            }
            CodexEvent::TurnStarted { turn_id, .. } => {
                self.codex_busy = true;
                if let Some(task_id) = self.active_task_id.clone() {
                    if let Some(task) = self.tasks.get_mut(&task_id) {
                        task.status = OrchestratorTaskStatus::RunningCodex;
                        task.turn_id = Some(turn_id.clone());
                        task.updated_at_ms = now_ms();
                    }
                    self.push_event(
                        Some(task_id),
                        "task.running_codex",
                        format!("Codex turn started: {turn_id}"),
                    );
                } else {
                    self.push_event(
                        None,
                        "runtime.unmanaged_turn",
                        "UI-started Codex turn detected",
                    );
                }
            }
            CodexEvent::AssistantCompleted { turn_id, text, .. } => {
                if let Some(task_id) = self.active_task_id.clone()
                    && self
                        .tasks
                        .get(&task_id)
                        .and_then(|task| task.turn_id.as_deref())
                        == Some(turn_id.as_str())
                {
                    if let Some(task) = self.tasks.get_mut(&task_id) {
                        task.result = Some(text);
                        task.updated_at_ms = now_ms();
                    }
                    self.push_event(
                        Some(task_id),
                        "task.codex_result",
                        "Codex produced a final message",
                    );
                }
            }
            CodexEvent::ToolActivity {
                turn_id, summary, ..
            } => {
                if let Some(task_id) = self.active_task_id.clone()
                    && self
                        .tasks
                        .get(&task_id)
                        .and_then(|task| task.turn_id.as_deref())
                        == Some(turn_id.as_str())
                {
                    if let Some(task) = self.tasks.get_mut(&task_id) {
                        if task.tool_activity.len() >= 200 {
                            task.tool_activity.remove(0);
                        }
                        task.tool_activity.push(summary.clone());
                        task.updated_at_ms = now_ms();
                    }
                    self.push_event(Some(task_id), "task.tool", summary);
                }
            }
            CodexEvent::TurnCompleted {
                turn_id, status, ..
            } => {
                self.codex_busy = false;
                if let Some(task_id) = self.active_task_id.take() {
                    let is_matching = self
                        .tasks
                        .get(&task_id)
                        .and_then(|task| task.turn_id.as_deref())
                        .is_none_or(|known| known == turn_id);
                    if is_matching {
                        if let Some(task) = self.tasks.get_mut(&task_id) {
                            task.status = if task.cancel_requested {
                                OrchestratorTaskStatus::Cancelled
                            } else if status.eq_ignore_ascii_case("completed") {
                                OrchestratorTaskStatus::NeedsReview
                            } else {
                                OrchestratorTaskStatus::Failed
                            };
                            task.updated_at_ms = now_ms();
                        }
                        let terminal_event = self
                            .tasks
                            .get(&task_id)
                            .map(|task| match task.status {
                                OrchestratorTaskStatus::NeedsReview => "task.needs_review",
                                OrchestratorTaskStatus::Cancelled => "task.cancelled",
                                _ => "task.failed",
                            })
                            .unwrap_or("task.failed");
                        self.push_event(
                            Some(task_id),
                            terminal_event,
                            format!(
                                "Codex turn ended with status {status}; Rust review gate remains"
                            ),
                        );
                    }
                } else {
                    self.push_event(
                        None,
                        "runtime.turn_completed",
                        format!("UI Codex turn ended: {status}"),
                    );
                }
            }
            CodexEvent::Error(message) => {
                if let Some(task_id) = self.active_task_id.clone() {
                    self.push_event(Some(task_id), "runtime.error", message);
                } else {
                    self.push_event(None, "runtime.error", message);
                }
            }
            CodexEvent::Notice(message) => self.push_event(None, "runtime.notice", message),
            CodexEvent::ThreadStarted { .. }
            | CodexEvent::AssistantDelta { .. }
            | CodexEvent::CatalogLoaded { .. } => {}
        }
        self.dispatch_next(commands);
    }

    fn session_list(&self) -> Result<Value, String> {
        let project_key = self.project_root.display().to_string();
        serde_json::to_value(self.sessions.list_project(&project_key))
            .map_err(|error| format!("Could not serialize session list: {error}"))
    }

    fn session_get(&self, params: &Value) -> Result<Value, String> {
        let session_id = required_string(params, "session_id")?;
        self.sessions
            .get(&session_id)
            .map(|session| {
                serde_json::to_value(session).expect("session serialization cannot fail")
            })
            .ok_or_else(|| format!("Unknown session: {session_id}"))
    }

    fn session_spawn(&mut self, params: &Value) -> Result<Value, String> {
        let parent_session_id = params
            .get("parent_session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(self.root_session_id.as_str())
            .to_owned();
        let runtime = params
            .get("runtime")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing non-empty parameter: runtime".to_owned())
            .and_then(parse_session_runtime)?;
        if runtime == SessionRuntime::Unified {
            return Err("Worker runtime must be web_gpt or codex".to_owned());
        }
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(runtime.label())
            .to_owned();
        let session = self
            .sessions
            .spawn_worker(&parent_session_id, runtime, title)?;
        serde_json::to_value(session)
            .map_err(|error| format!("Could not serialize worker session: {error}"))
    }

    fn session_set_status(&mut self, params: &Value) -> Result<Value, String> {
        let session_id = required_string(params, "session_id")?;
        let status = params
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing non-empty parameter: status".to_owned())
            .and_then(parse_session_status)?;
        let session = self.sessions.set_status(&session_id, status)?;
        serde_json::to_value(session)
            .map_err(|error| format!("Could not serialize session: {error}"))
    }

    fn session_workers(&self, params: &Value) -> Result<Value, String> {
        let session_id = required_string(params, "session_id")?;
        serde_json::to_value(self.sessions.workers_of(&session_id)?)
            .map_err(|error| format!("Could not serialize worker sessions: {error}"))
    }

    fn session_events(&self, params: &Value) -> Result<Value, String> {
        let after = params.get("after").and_then(Value::as_u64).unwrap_or(0);
        serde_json::to_value(self.sessions.events_after(after))
            .map_err(|error| format!("Could not serialize session events: {error}"))
    }

    fn handle_rpc(&mut self, request: RpcRequest, commands: &Sender<CodexCommand>) -> RpcResponse {
        if !capability_matches(&self.auth_token, request.auth.as_deref()) {
            return RpcResponse {
                jsonrpc: "2.0",
                id: request.id,
                result: None,
                error: Some(RpcError {
                    code: -32001,
                    message: "Roche bridge capability authentication failed".to_owned(),
                }),
            };
        }

        let result = match request.method.as_str() {
            "health" => Ok(json!({
                "bridge": "ready",
                "address": bridge_addr(),
                "codex_ready": self.codex_ready,
                "codex_busy": self.codex_busy,
                "active_task_id": self.active_task_id,
                "queued": self.queue.len(),
                "task_count": self.tasks.len(),
                "pending_chat": self.pending_chat.len(),
                "chat_count": self.chat_requests.len(),
                "project_root": self.project_root,
                "root_session_id": self.root_session_id,
                "active_sessions": self.sessions.active_count(&self.project_root.display().to_string()),
            })),
            "session.list" => self.session_list(),
            "session.get" => self.session_get(&request.params),
            "session.spawn" => self.session_spawn(&request.params),
            "session.status" => self.session_set_status(&request.params),
            "session.workers" => self.session_workers(&request.params),
            "session.events" => self.session_events(&request.params),
            "chat.submit" => self.submit_chat(&request.params),
            "chat.pending" => self.claim_pending_chat(),
            "chat.poll" => self.poll_chat(&request.params),
            "chat.release" => self.release_chat(&request.params),
            "chat.respond" => self.respond_chat(&request.params),
            "chat.cancel" => self.cancel_chat(&request.params),
            "task.create" => self.create_task(&request.params),
            "task.get" => {
                let id = required_string(&request.params, "task_id");
                id.and_then(|id| {
                    self.tasks
                        .get(&id)
                        .map(|task| {
                            serde_json::to_value(task).expect("task serialization cannot fail")
                        })
                        .ok_or_else(|| format!("Unknown task: {id}"))
                })
            }
            "task.list" => Ok(
                serde_json::to_value(self.tasks.values().collect::<Vec<_>>())
                    .expect("task list serialization cannot fail"),
            ),
            "task.revise" => self.revise_task(&request.params),
            "task.cancel" => self.cancel_task(&request.params, commands),
            "task.approve" => self.approve_task(&request.params),
            "task.events" => {
                let after = request
                    .params
                    .get("after")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let task_id = request.params.get("task_id").and_then(Value::as_str);
                let items = self
                    .events
                    .iter()
                    .filter(|event| event.seq > after)
                    .filter(|event| task_id.is_none_or(|id| event.task_id.as_deref() == Some(id)))
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(serde_json::to_value(items).expect("event serialization cannot fail"))
            }
            "project.snapshot" => project_snapshot(&self.project_root).and_then(|snapshot| {
                serde_json::to_value(snapshot).map_err(|error| error.to_string())
            }),
            _ => Err(format!("Unknown method: {}", request.method)),
        };
        if matches!(request.method.as_str(), "task.create" | "task.revise") {
            self.dispatch_next(commands);
        }
        match result {
            Ok(result) => RpcResponse {
                jsonrpc: "2.0",
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(message) => RpcResponse {
                jsonrpc: "2.0",
                id: request.id,
                result: None,
                error: Some(RpcError {
                    code: -32000,
                    message,
                }),
            },
        }
    }
}

pub(crate) fn spawn_orchestrator_bridge(
    project_root: PathBuf,
    commands: Sender<CodexCommand>,
    codex_events: Receiver<CodexEvent>,
) -> Result<(), String> {
    let listener = bind_bridge_listener()?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not configure Roche Web GPT bridge: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("Could not read Roche Web GPT bridge address: {error}"))?
        .to_string();
    let token = generate_bridge_token()?;
    let client = BridgeClientConfig {
        address,
        token: token.clone(),
        pid: std::process::id(),
        project_root: project_root.display().to_string(),
    };
    IN_PROCESS_BRIDGE
        .set(client.clone())
        .map_err(|_| "Roche Web GPT bridge is already initialized in this process".to_owned())?;
    write_bridge_descriptor(&project_root, &client)?;
    thread::Builder::new()
        .name("roche-webgpt-orchestrator".to_owned())
        .spawn(move || bridge_worker(listener, project_root, commands, codex_events, token))
        .map_err(|error| format!("Could not start Roche Web GPT bridge worker: {error}"))?;
    Ok(())
}

fn bridge_worker(
    listener: TcpListener,
    project_root: PathBuf,
    commands: Sender<CodexCommand>,
    codex_events: Receiver<CodexEvent>,
    auth_token: String,
) {
    let mut state = BridgeState::new(project_root, auth_token);
    loop {
        while let Ok(event) = codex_events.try_recv() {
            state.handle_codex_event(event, &commands);
        }
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, &mut state, &commands),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn handle_connection(
    mut stream: TcpStream,
    state: &mut BridgeState,
    commands: &Sender<CodexCommand>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let reader_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<RpcRequest>(&line) {
        Ok(request) => state.handle_rpc(request, commands),
        Err(error) => RpcResponse {
            jsonrpc: "2.0",
            id: None,
            result: None,
            error: Some(RpcError {
                code: -32700,
                message: format!("Invalid JSON request: {error}"),
            }),
        },
    };
    if serde_json::to_writer(&mut stream, &response).is_ok() {
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
    let _ = stream.shutdown(Shutdown::Both);
}

pub fn rpc_call(method: &str, params: Value) -> Result<Value, String> {
    let client = discover_bridge_client()?;
    rpc_call_with_client(&client, method, params)
}

fn rpc_call_with_client(
    client: &BridgeClientConfig,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let address = &client.address;
    let socket_address = address
        .parse::<SocketAddr>()
        .map_err(|error| format!("Invalid Roche bridge address {address}: {error}"))?;
    if !socket_address.ip().is_loopback() {
        return Err(format!(
            "Refusing non-loopback Roche bridge address: {socket_address}"
        ));
    }
    let mut stream =
        TcpStream::connect_timeout(&socket_address, Duration::from_secs(2)).map_err(|error| {
            format!("Roche app orchestrator is not reachable at {address}: {error}")
        })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
        "auth": client.token,
    });
    serde_json::to_writer(&mut stream, &request)
        .map_err(|error| format!("Could not encode Roche bridge request: {error}"))?;
    stream
        .write_all(b"\n")
        .map_err(|error| format!("Could not write Roche bridge request: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("Could not flush Roche bridge request: {error}"))?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| format!("Could not finish Roche bridge request: {error}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("Could not read Roche bridge response: {error}"))?;
    let value: Value = serde_json::from_str(&line)
        .map_err(|error| format!("Invalid Roche bridge response: {error}"))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Roche orchestrator returned an error");
        return Err(message.to_owned());
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| "Roche orchestrator response did not include result".to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebGptRuntimeEvent {
    SessionsUpdated {
        sessions: Vec<crate::sessions::AgentSession>,
    },
    Submitted {
        local_id: String,
        request_id: String,
    },
    Answered {
        local_id: String,
        request_id: String,
        text: String,
    },
    Cancelled {
        local_id: String,
        request_id: String,
    },
    Error {
        local_id: Option<String>,
        message: String,
    },
}

#[derive(Debug)]
enum WebGptRuntimeCommand {
    Submit {
        local_id: String,
        text: String,
        reasoning_level: String,
    },
    Cancel {
        request_id: String,
    },
    Shutdown,
}

#[derive(Debug)]
struct PendingUiRequest {
    local_id: String,
    next_poll: Instant,
}

pub struct WebGptRuntimeController {
    commands: Sender<WebGptRuntimeCommand>,
    events: Receiver<WebGptRuntimeEvent>,
}

impl WebGptRuntimeController {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("roche-webgpt-runtime".to_owned())
            .spawn(move || webgpt_runtime_worker(command_rx, event_tx))
            .expect("failed to start Roche Web GPT runtime worker");
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn submit(&self, text: String, reasoning_level: String) -> String {
        let local_id = format!(
            "local-chat-{}-{}",
            now_ms(),
            TASK_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let _ = self.commands.send(WebGptRuntimeCommand::Submit {
            local_id: local_id.clone(),
            text,
            reasoning_level,
        });
        local_id
    }

    pub fn cancel(&self, request_id: String) {
        let _ = self
            .commands
            .send(WebGptRuntimeCommand::Cancel { request_id });
    }

    pub fn drain(&self) -> Vec<WebGptRuntimeEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for WebGptRuntimeController {
    fn drop(&mut self) {
        let _ = self.commands.send(WebGptRuntimeCommand::Shutdown);
    }
}

fn webgpt_runtime_worker(
    commands: Receiver<WebGptRuntimeCommand>,
    events: Sender<WebGptRuntimeEvent>,
) {
    let mut pending: HashMap<String, PendingUiRequest> = HashMap::new();
    let mut next_session_poll = Instant::now();
    let mut running = true;
    while running {
        match commands.recv_timeout(Duration::from_millis(40)) {
            Ok(WebGptRuntimeCommand::Submit {
                local_id,
                text,
                reasoning_level,
            }) => match rpc_call(
                "chat.submit",
                json!({"text": text, "reasoning_level": reasoning_level}),
            ) {
                Ok(value) => {
                    if let Some(request_id) = value.get("id").and_then(Value::as_str) {
                        let request_id = request_id.to_owned();
                        pending.insert(
                            request_id.clone(),
                            PendingUiRequest {
                                local_id: local_id.clone(),
                                next_poll: Instant::now(),
                            },
                        );
                        let _ = events.send(WebGptRuntimeEvent::Submitted {
                            local_id,
                            request_id,
                        });
                    } else {
                        let _ = events.send(WebGptRuntimeEvent::Error {
                            local_id: Some(local_id),
                            message: "chat.submit response did not include request id".to_owned(),
                        });
                    }
                }
                Err(message) => {
                    let _ = events.send(WebGptRuntimeEvent::Error {
                        local_id: Some(local_id),
                        message,
                    });
                }
            },
            Ok(WebGptRuntimeCommand::Cancel { request_id }) => {
                let local_id = pending
                    .get(&request_id)
                    .map(|request| request.local_id.clone());
                if let Err(message) = rpc_call("chat.cancel", json!({"request_id": request_id})) {
                    let _ = events.send(WebGptRuntimeEvent::Error { local_id, message });
                }
            }
            Ok(WebGptRuntimeCommand::Shutdown) => running = false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !running {
            break;
        }

        let now = Instant::now();
        if now >= next_session_poll {
            if let Ok(value) = rpc_call("session.list", json!({}))
                && let Ok(sessions) =
                    serde_json::from_value::<Vec<crate::sessions::AgentSession>>(value)
            {
                let _ = events.send(WebGptRuntimeEvent::SessionsUpdated { sessions });
            }
            next_session_poll = now + Duration::from_millis(750);
        }

        let due = pending
            .iter()
            .filter(|(_, request)| request.next_poll <= now)
            .map(|(request_id, request)| (request_id.clone(), request.local_id.clone()))
            .collect::<Vec<_>>();
        for (request_id, local_id) in due {
            match rpc_call("chat.poll", json!({"request_id": request_id})) {
                Ok(value) => match value.get("status").and_then(Value::as_str) {
                    Some("answered") => {
                        let text = value
                            .get("response")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        pending.remove(&request_id);
                        let _ = events.send(WebGptRuntimeEvent::Answered {
                            local_id,
                            request_id,
                            text,
                        });
                    }
                    Some("cancelled") => {
                        pending.remove(&request_id);
                        let _ = events.send(WebGptRuntimeEvent::Cancelled {
                            local_id,
                            request_id,
                        });
                    }
                    _ => {
                        if let Some(request) = pending.get_mut(&request_id) {
                            request.next_poll = now + UI_POLL_INTERVAL;
                        }
                    }
                },
                Err(message) => {
                    if let Some(request) = pending.get_mut(&request_id) {
                        request.next_poll = now + Duration::from_secs(1);
                    }
                    let _ = events.send(WebGptRuntimeEvent::Error {
                        local_id: Some(local_id),
                        message,
                    });
                }
            }
        }
    }
}

fn orchestrator_prompt(task: &OrchestratorTask) -> String {
    let acceptance = if task.acceptance.is_empty() {
        "- Preserve existing behavior outside the requested change.\n- Run relevant deterministic checks before reporting done."
            .to_owned()
    } else {
        task.acceptance
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Roche Task {}\nTitle: {}\n\nGoal:\n{}\n\nAcceptance criteria:\n{}\n\nRuntime contract:\n- Work only in the current Roche project root.\n- Inspect applicable AGENTS.md before editing.\n- Implement the goal; do not redesign unrelated UI or architecture.\n- Run relevant tests/checks.\n- Finish with a concise summary of changes and verification.\n- Do not mark the product task complete yourself; Rust Orchestrator owns completion state.",
        task.id, task.title, task.goal, acceptance
    )
}

fn project_snapshot(root: &Path) -> Result<ProjectSnapshot, String> {
    let status_short = git_output(root, ["status", "--short"])?;
    let diff_stat = git_output(root, ["diff", "--stat"])?;
    let changed_files = git_output(root, ["diff", "--name-only"])?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(ProjectSnapshot {
        root: root.display().to_string(),
        status_short,
        diff_stat,
        changed_files,
    })
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("Could not run git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn required_string(params: &Value, name: &str) -> Result<String, String> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Missing non-empty parameter: {name}"))
}

fn parse_session_runtime(value: &str) -> Result<SessionRuntime, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "unified" | "mixed" => Ok(SessionRuntime::Unified),
        "web" | "web_gpt" | "webgpt" | "gpt" | "gpt-5.6" => Ok(SessionRuntime::WebGpt),
        "codex" => Ok(SessionRuntime::Codex),
        other => Err(format!("Unsupported session runtime: {other}")),
    }
}

fn parse_session_status(value: &str) -> Result<SessionStatus, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "idle" => Ok(SessionStatus::Idle),
        "running" => Ok(SessionStatus::Running),
        "waiting_on_workers" | "waiting" => Ok(SessionStatus::WaitingOnWorkers),
        "needs_input" => Ok(SessionStatus::NeedsInput),
        "completed" => Ok(SessionStatus::Completed),
        "failed" => Ok(SessionStatus::Failed),
        "cancelled" | "canceled" => Ok(SessionStatus::Cancelled),
        "offline" => Ok(SessionStatus::Offline),
        other => Err(format!("Unsupported session status: {other}")),
    }
}

fn normalize_effort(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" | "fast" | "빠름" => Ok("low".to_owned()),
        "medium" => Ok("medium".to_owned()),
        "high" | "높음" => Ok("high".to_owned()),
        "xhigh" | "very_high" | "very-high" | "매우 높음" => Ok("xhigh".to_owned()),
        other => Err(format!("Unsupported reasoning effort: {other}")),
    }
}

fn next_task_id() -> String {
    let count = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("task-{}-{count}", now_ms())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn bridge_addr() -> String {
    discover_bridge_client()
        .map(|client| client.address)
        .unwrap_or_else(|_| DEFAULT_WEBGPT_BRIDGE_ADDR.to_owned())
}

fn validated_bridge_addr() -> Result<SocketAddr, String> {
    let configured = std::env::var("ROCHE_WEBGPT_BRIDGE_ADDR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_WEBGPT_BRIDGE_ADDR.to_owned());
    let address = configured
        .parse::<SocketAddr>()
        .map_err(|error| format!("Invalid Roche bridge address {configured}: {error}"))?;
    if !address.ip().is_loopback() {
        return Err(format!(
            "Refusing non-loopback Roche bridge bind address: {address}"
        ));
    }
    Ok(address)
}

fn bind_bridge_listener() -> Result<TcpListener, String> {
    let configured = validated_bridge_addr()?;
    match TcpListener::bind(configured) {
        Ok(listener) => Ok(listener),
        Err(error)
            if error.kind() == std::io::ErrorKind::AddrInUse
                && std::env::var_os("ROCHE_WEBGPT_BRIDGE_ADDR").is_none() =>
        {
            TcpListener::bind("127.0.0.1:0").map_err(|fallback_error| {
                format!(
                    "Could not bind Roche Web GPT bridge at {configured} ({error}) or fallback loopback port ({fallback_error})"
                )
            })
        }
        Err(error) => Err(format!(
            "Could not bind Roche Web GPT bridge at {configured}: {error}"
        )),
    }
}

fn generate_bridge_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not generate Roche bridge capability token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn capability_matches(expected: &str, provided: Option<&str>) -> bool {
    let Some(provided) = provided else {
        return false;
    };
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .iter()
        .zip(provided)
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn bridge_descriptor_path(project_root: &Path) -> PathBuf {
    project_root.join(BRIDGE_DESCRIPTOR_RELATIVE_PATH)
}

fn write_bridge_descriptor(project_root: &Path, client: &BridgeClientConfig) -> Result<(), String> {
    let path = bridge_descriptor_path(project_root);
    let parent = path
        .parent()
        .ok_or_else(|| format!("Invalid Roche bridge descriptor path: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Could not create Roche bridge descriptor directory {}: {error}",
            parent.display()
        )
    })?;
    let encoded = serde_json::to_vec_pretty(client)
        .map_err(|error| format!("Could not encode Roche bridge descriptor: {error}"))?;
    fs::write(&path, encoded).map_err(|error| {
        format!(
            "Could not write Roche bridge descriptor {}: {error}",
            path.display()
        )
    })
}

fn discover_bridge_client() -> Result<BridgeClientConfig, String> {
    if let Some(client) = IN_PROCESS_BRIDGE.get() {
        return Ok(client.clone());
    }

    let mut roots = Vec::new();
    if let Some(root) = std::env::var_os("ROCHE_PROJECT_ROOT") {
        roots.push(PathBuf::from(root));
    }
    if let Ok(current) = std::env::current_dir() {
        roots.push(current);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        roots.extend(parent.ancestors().take(4).map(Path::to_path_buf));
    }

    roots.dedup();
    for root in roots {
        let path = bridge_descriptor_path(&root);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let client = serde_json::from_slice::<BridgeClientConfig>(&bytes).map_err(|error| {
            format!(
                "Invalid Roche bridge descriptor {}: {error}",
                path.display()
            )
        })?;
        let address = client.address.parse::<SocketAddr>().map_err(|error| {
            format!(
                "Invalid Roche bridge descriptor address {}: {error}",
                client.address
            )
        })?;
        if !address.ip().is_loopback() {
            return Err(format!(
                "Refusing non-loopback Roche bridge descriptor address: {address}"
            ));
        }
        if client.token.len() < 64 {
            return Err("Roche bridge descriptor contains an invalid capability token".to_owned());
        }
        return Ok(client);
    }

    Err(format!(
        "Roche bridge descriptor not found. Start Roche in this project first (expected {BRIDGE_DESCRIPTOR_RELATIVE_PATH})."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_keeps_rust_as_completion_authority() {
        let task = OrchestratorTask {
            id: "task-test".to_owned(),
            title: "Fix login".to_owned(),
            goal: "Fix retry handling".to_owned(),
            acceptance: vec!["Tests pass".to_owned()],
            effort: "high".to_owned(),
            status: OrchestratorTaskStatus::Queued,
            turn_id: None,
            result: None,
            tool_activity: Vec::new(),
            revision_count: 0,
            cancel_requested: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let prompt = orchestrator_prompt(&task);
        assert!(prompt.contains("Rust Orchestrator owns completion state"));
        assert!(prompt.contains("Fix retry handling"));
        assert!(prompt.contains("Tests pass"));
    }

    #[test]
    fn effort_aliases_are_deterministic() {
        assert_eq!(normalize_effort("빠름").unwrap(), "low");
        assert_eq!(normalize_effort("높음").unwrap(), "high");
        assert_eq!(normalize_effort("매우 높음").unwrap(), "xhigh");
        assert!(normalize_effort("ultra").is_err());
    }

    #[test]
    fn bridge_rejects_missing_or_wrong_capability() {
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let token = "a".repeat(64);
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), token.clone());

        let missing = state.handle_rpc(
            RpcRequest {
                id: Some(json!(1)),
                method: "health".to_owned(),
                params: json!({}),
                auth: None,
            },
            &command_tx,
        );
        assert_eq!(missing.error.as_ref().map(|error| error.code), Some(-32001));

        let wrong = state.handle_rpc(
            RpcRequest {
                id: Some(json!(2)),
                method: "health".to_owned(),
                params: json!({}),
                auth: Some("b".repeat(64)),
            },
            &command_tx,
        );
        assert_eq!(wrong.error.as_ref().map(|error| error.code), Some(-32001));

        let accepted = state.handle_rpc(
            RpcRequest {
                id: Some(json!(3)),
                method: "health".to_owned(),
                params: json!({}),
                auth: Some(token),
            },
            &command_tx,
        );
        assert!(accepted.error.is_none());
        assert_eq!(
            accepted
                .result
                .as_ref()
                .and_then(|value| value["bridge"].as_str()),
            Some("ready")
        );
    }

    #[test]
    fn orchestrator_requires_review_before_completion() {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_codex_event(
            CodexEvent::Connection(CodexConnection::Ready {
                version: "test".to_owned(),
            }),
            &command_tx,
        );
        let created = state
            .create_task(&json!({"title": "Task", "goal": "Do work", "effort": "high"}))
            .unwrap();
        let task_id = created["id"].as_str().unwrap().to_owned();
        state.dispatch_next(&command_tx);
        match command_rx.try_recv().unwrap() {
            CodexCommand::Send {
                text,
                attachments,
                effort,
                model,
            } => {
                assert!(text.contains(&task_id));
                assert!(attachments.is_empty());
                assert_eq!(effort, "high");
                assert!(model.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::Preparing
        );
        state.handle_codex_event(
            CodexEvent::TurnStarted {
                thread_id: "thread".to_owned(),
                turn_id: "turn".to_owned(),
            },
            &command_tx,
        );
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::RunningCodex
        );
        state.handle_codex_event(
            CodexEvent::TurnCompleted {
                thread_id: "thread".to_owned(),
                turn_id: "turn".to_owned(),
                status: "completed".to_owned(),
            },
            &command_tx,
        );
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::NeedsReview
        );
        state
            .approve_task(&json!({"task_id": task_id.clone()}))
            .unwrap();
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::Completed
        );
    }
}
