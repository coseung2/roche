use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    codex::{CodexCommand, CodexConnection, CodexEvent, CodexWorkerRuntime},
    sessions::{SessionGraph, SessionRuntime, SessionStatus},
    web_browser::{SharedWebGptBrowser, WebGptBrowserEvent, WebGptBrowserState},
    web_browser_protocol::{
        DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID, WebGptTurnCorrelation, WebGptTurnRequest,
    },
};

pub const DEFAULT_WEBGPT_BRIDGE_ADDR: &str = "127.0.0.1:47831";
const BRIDGE_DESCRIPTOR_RELATIVE_PATH: &str = ".ai-bridge/roche-webgpt-runtime.json";
const MAX_TASK_EVENTS: usize = 2_000;
const UI_POLL_INTERVAL: Duration = Duration::from_millis(150);

static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);
static IN_PROCESS_BRIDGE: OnceLock<BridgeClientConfig> = OnceLock::new();
static BRIDGE_REBIND: OnceLock<Sender<BridgeRebind>> = OnceLock::new();
static BRIDGE_CURRENT_ROOT: OnceLock<Mutex<PathBuf>> = OnceLock::new();

struct BridgeRebind {
    project_root: PathBuf,
    commands: Sender<CodexCommand>,
    codex_events: Receiver<CodexEvent>,
    web_browser: SharedWebGptBrowser,
}

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
    RunningWebGpt,
    NeedsReview,
    Failed,
    Cancelled,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorTask {
    pub id: String,
    pub session_id: Option<String>,
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
enum WebWorkerCommand {
    EnsureRuntime,
    Submit {
        request: WebGptTurnRequest,
        text: String,
    },
    Cancel {
        request: WebGptTurnRequest,
    },
    ShowLogin,
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
    worker_runtimes: HashMap<String, CodexWorkerRuntime>,
    web_worker_ready: bool,
    active_web_task_id: Option<String>,
    active_web_correlation: Option<WebGptTurnCorrelation>,
    web_worker_queue: VecDeque<String>,
    web_worker_commands: VecDeque<WebWorkerCommand>,
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
            worker_runtimes: HashMap::new(),
            web_worker_ready: false,
            active_web_task_id: None,
            active_web_correlation: None,
            web_worker_queue: VecDeque::new(),
            web_worker_commands: VecDeque::new(),
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

    fn queue_web_worker(&mut self, task_id: String) {
        self.web_worker_queue.push_back(task_id);
        self.web_worker_commands
            .push_back(WebWorkerCommand::EnsureRuntime);
        self.dispatch_next_web_worker();
    }

    fn dispatch_next_web_worker(&mut self) {
        if !self.web_worker_ready || self.active_web_task_id.is_some() {
            return;
        }
        while let Some(task_id) = self.web_worker_queue.pop_front() {
            let Some(task) = self.tasks.get_mut(&task_id) else {
                continue;
            };
            if task.status != OrchestratorTaskStatus::Preparing {
                continue;
            }
            let Some(session_id) = task.session_id.clone() else {
                continue;
            };
            let request_id = format!("web-worker-{}-r{}", task.id, task.revision_count);
            task.turn_id = Some(request_id.clone());
            task.updated_at_ms = now_ms();
            let text = orchestrator_prompt(task);
            let request = WebGptTurnRequest::worker(session_id, task.id.clone(), request_id);
            self.active_web_task_id = Some(task_id.clone());
            self.active_web_correlation = None;
            self.web_worker_commands
                .push_back(WebWorkerCommand::Submit { request, text });
            self.push_event(
                Some(task_id),
                "task.web_worker_dispatching",
                "Web GPT worker turn dispatched to the hidden browser runtime",
            );
            break;
        }
    }

    fn drain_web_worker_commands(&mut self) -> Vec<WebWorkerCommand> {
        self.web_worker_commands.drain(..).collect()
    }

    fn refresh_parent_status(&mut self, child_session_id: &str) {
        let Some(parent_id) = self
            .sessions
            .get(child_session_id)
            .and_then(|session| session.parent_session_id.clone())
        else {
            return;
        };
        self.refresh_session_worker_status(&parent_id);
    }

    fn refresh_session_worker_status(&mut self, parent_id: &str) {
        let Ok(workers) = self.sessions.workers_of(parent_id) else {
            return;
        };
        let status = if workers
            .iter()
            .any(|worker| worker.status == SessionStatus::NeedsInput)
        {
            SessionStatus::NeedsInput
        } else if workers.iter().any(|worker| worker.status.is_active()) {
            SessionStatus::WaitingOnWorkers
        } else if workers.iter().any(|worker| {
            matches!(
                worker.status,
                SessionStatus::Failed | SessionStatus::Offline
            )
        }) {
            SessionStatus::NeedsInput
        } else {
            SessionStatus::Idle
        };
        let _ = self.sessions.set_status(parent_id, status);
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
            session_id: None,
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
        let (session_id, worker_prompt, worker_effort) = {
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
            task.status = if task.session_id.is_some() {
                OrchestratorTaskStatus::Preparing
            } else {
                OrchestratorTaskStatus::Queued
            };
            task.turn_id = None;
            task.result = None;
            task.cancel_requested = false;
            task.updated_at_ms = now_ms();
            (
                task.session_id.clone(),
                orchestrator_prompt(task),
                task.effort.clone(),
            )
        };

        if let Some(session_id) = session_id {
            let runtime_kind = self
                .sessions
                .get(&session_id)
                .map(|session| session.runtime)
                .ok_or_else(|| format!("Unknown worker session: {session_id}"))?;
            let _ = self
                .sessions
                .set_status(&session_id, SessionStatus::WaitingOnWorkers);
            match runtime_kind {
                SessionRuntime::Codex => {
                    let root = self.project_root.clone();
                    let runtime = self
                        .worker_runtimes
                        .entry(session_id)
                        .or_insert_with(|| CodexWorkerRuntime::spawn(root));
                    runtime.send(worker_prompt, worker_effort, None);
                    self.push_event(
                        Some(task_id.clone()),
                        "task.worker_revision_started",
                        "Revision sent to the independent Codex worker",
                    );
                }
                SessionRuntime::WebGpt => {
                    self.queue_web_worker(task_id.clone());
                    self.push_event(
                        Some(task_id.clone()),
                        "task.web_worker_revision_queued",
                        "Revision queued for the Web GPT worker runtime",
                    );
                }
                SessionRuntime::Unified => {
                    return Err("Unified sessions cannot own worker tasks".to_owned());
                }
            }
        } else {
            self.queue.push_front(task_id.clone());
            self.push_event(
                Some(task_id.clone()),
                "task.revision_queued",
                "Revision queued by Web GPT",
            );
        }
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
        let worker_session_id = self
            .tasks
            .get(&task_id)
            .ok_or_else(|| format!("Unknown task: {task_id}"))?
            .session_id
            .clone();
        if let Some(session_id) = worker_session_id {
            let runtime_kind = self
                .sessions
                .get(&session_id)
                .map(|session| session.runtime)
                .ok_or_else(|| format!("Unknown worker session: {session_id}"))?;
            if runtime_kind == SessionRuntime::WebGpt {
                let active = self.active_web_task_id.as_deref() == Some(task_id.as_str());
                let task = self.tasks.get_mut(&task_id).expect("existing task");
                if task.cancel_requested {
                    return Ok(
                        serde_json::to_value(&*task).expect("task serialization cannot fail")
                    );
                }
                match task.status {
                    OrchestratorTaskStatus::Preparing if active => {
                        task.cancel_requested = true;
                        if let Some(request_id) = task.turn_id.clone() {
                            let request = WebGptTurnRequest::worker(
                                session_id.clone(),
                                task.id.clone(),
                                request_id,
                            );
                            self.web_worker_commands
                                .push_back(WebWorkerCommand::Cancel { request });
                        } else {
                            task.status = OrchestratorTaskStatus::Cancelled;
                            self.active_web_task_id = None;
                            self.active_web_correlation = None;
                        }
                    }
                    OrchestratorTaskStatus::RunningWebGpt => {
                        task.cancel_requested = true;
                        if let Some(request_id) = task.turn_id.clone() {
                            let request = WebGptTurnRequest::worker(
                                session_id.clone(),
                                task.id.clone(),
                                request_id,
                            );
                            self.web_worker_commands
                                .push_back(WebWorkerCommand::Cancel { request });
                        } else {
                            task.status = OrchestratorTaskStatus::Cancelled;
                            self.active_web_task_id = None;
                            self.active_web_correlation = None;
                        }
                    }
                    OrchestratorTaskStatus::Queued
                    | OrchestratorTaskStatus::Preparing
                    | OrchestratorTaskStatus::NeedsReview
                    | OrchestratorTaskStatus::Failed => {
                        self.web_worker_queue.retain(|queued| queued != &task_id);
                        task.status = OrchestratorTaskStatus::Cancelled;
                    }
                    OrchestratorTaskStatus::Cancelled | OrchestratorTaskStatus::Completed => {}
                    OrchestratorTaskStatus::RunningCodex => {
                        return Err(
                            "Web GPT task entered an invalid Codex running state".to_owned()
                        );
                    }
                }
                task.updated_at_ms = now_ms();
                if task.status == OrchestratorTaskStatus::Cancelled {
                    let _ = self
                        .sessions
                        .set_status(&session_id, SessionStatus::Cancelled);
                    self.refresh_parent_status(&session_id);
                }
                self.push_event(
                    Some(task_id.clone()),
                    "task.cancel_requested",
                    if active {
                        "Cancellation sent to the Web GPT browser runtime"
                    } else {
                        "Queued Web GPT worker task cancelled"
                    },
                );
                self.dispatch_next_web_worker();
                return Ok(
                    serde_json::to_value(self.tasks.get(&task_id).expect("existing task"))
                        .expect("task serialization cannot fail"),
                );
            }
            let task = self.tasks.get_mut(&task_id).expect("existing task");
            match task.status {
                OrchestratorTaskStatus::Preparing | OrchestratorTaskStatus::RunningCodex => {
                    task.cancel_requested = true;
                    if let Some(runtime) = self.worker_runtimes.get(&session_id) {
                        runtime.interrupt();
                    } else {
                        task.status = OrchestratorTaskStatus::Cancelled;
                    }
                }
                OrchestratorTaskStatus::Queued
                | OrchestratorTaskStatus::NeedsReview
                | OrchestratorTaskStatus::Failed => {
                    task.status = OrchestratorTaskStatus::Cancelled;
                    self.worker_runtimes.remove(&session_id);
                }
                OrchestratorTaskStatus::Cancelled | OrchestratorTaskStatus::Completed => {}
                OrchestratorTaskStatus::RunningWebGpt => {
                    return Err("Codex task entered an invalid Web GPT running state".to_owned());
                }
            }
            task.updated_at_ms = now_ms();
            let _ = self
                .sessions
                .set_status(&session_id, SessionStatus::Cancelled);
            self.push_event(
                Some(task_id.clone()),
                "task.cancel_requested",
                "Cancellation sent to independent Codex worker",
            );
            return Ok(
                serde_json::to_value(self.tasks.get(&task_id).expect("existing task"))
                    .expect("task serialization cannot fail"),
            );
        }

        let active = self.active_task_id.as_deref() == Some(task_id.as_str());
        let task = self.tasks.get_mut(&task_id).expect("existing task");
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
            OrchestratorTaskStatus::RunningWebGpt => {
                return Err("Unbound task entered an invalid Web GPT running state".to_owned());
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
        let session_id = {
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
            task.session_id.clone()
        };
        if let Some(session_id) = session_id {
            let _ = self
                .sessions
                .set_status(&session_id, SessionStatus::Completed);
            self.refresh_parent_status(&session_id);
            self.worker_runtimes.remove(&session_id);
            self.web_worker_queue.retain(|queued| queued != &task_id);
            if self.active_web_task_id.as_deref() == Some(task_id.as_str()) {
                self.active_web_task_id = None;
                self.active_web_correlation = None;
            }
        }
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
                    target: crate::codex::CodexThreadTarget::Current,
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
            CodexEvent::Activity {
                turn_id, activity, ..
            } => {
                let summary = if activity.detail.is_empty() {
                    format!("{} · {}", activity.kind.label(), activity.title)
                } else {
                    format!(
                        "{} · {} · {}",
                        activity.kind.label(),
                        activity.title,
                        activity.detail
                    )
                };
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
                    self.push_event(Some(task_id), "task.activity", summary);
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
            | CodexEvent::StoredThreads { .. }
            | CodexEvent::ThreadHistoryLoaded { .. }
            | CodexEvent::ThreadResumeFailed { .. }
            | CodexEvent::AssistantDelta { .. }
            | CodexEvent::CatalogLoaded { .. } => {}
        }
        self.dispatch_next(commands);
    }

    fn drain_worker_events(&mut self) {
        let session_ids = self.worker_runtimes.keys().cloned().collect::<Vec<_>>();
        let mut pending = Vec::new();
        for session_id in session_ids {
            if let Some(runtime) = self.worker_runtimes.get(&session_id) {
                for event in runtime.drain() {
                    pending.push((session_id.clone(), event));
                }
            }
        }
        for (session_id, event) in pending {
            self.handle_worker_event(&session_id, event);
        }
    }

    fn handle_worker_event(&mut self, session_id: &str, event: CodexEvent) {
        let task_id = self
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(session_id))
            .map(|task| task.id.clone());
        let Some(task_id) = task_id else {
            return;
        };

        match event {
            CodexEvent::Connection(CodexConnection::Starting) => {
                let _ = self
                    .sessions
                    .set_status(session_id, SessionStatus::WaitingOnWorkers);
            }
            CodexEvent::Connection(CodexConnection::Ready { version }) => {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::RunningCodex;
                    task.updated_at_ms = now_ms();
                }
                let _ = self.sessions.set_status(session_id, SessionStatus::Running);
                self.push_event(
                    Some(task_id),
                    "task.worker_ready",
                    format!("Codex worker ready: {version}"),
                );
            }
            CodexEvent::Connection(CodexConnection::Offline { message }) => {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::Failed;
                    task.updated_at_ms = now_ms();
                }
                let _ = self.sessions.set_status(session_id, SessionStatus::Offline);
                self.push_event(Some(task_id), "task.worker_offline", message);
                self.worker_runtimes.remove(session_id);
            }
            CodexEvent::TurnStarted { turn_id, .. } => {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::RunningCodex;
                    task.turn_id = Some(turn_id.clone());
                    task.updated_at_ms = now_ms();
                }
                let _ = self.sessions.set_status(session_id, SessionStatus::Running);
                self.push_event(
                    Some(task_id),
                    "task.running_codex",
                    format!("Worker turn started: {turn_id}"),
                );
            }
            CodexEvent::AssistantDelta { delta, .. } => {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.result.get_or_insert_with(String::new).push_str(&delta);
                    task.updated_at_ms = now_ms();
                }
            }
            CodexEvent::AssistantCompleted { text, .. } => {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.result = Some(text);
                    task.updated_at_ms = now_ms();
                }
                self.push_event(
                    Some(task_id),
                    "task.codex_result",
                    "Worker Codex produced a final message",
                );
            }
            CodexEvent::Activity { activity, .. } => {
                let summary = if activity.detail.is_empty() {
                    format!("{} · {}", activity.kind.label(), activity.title)
                } else {
                    format!(
                        "{} · {} · {}",
                        activity.kind.label(),
                        activity.title,
                        activity.detail
                    )
                };
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    if task.tool_activity.len() >= 200 {
                        task.tool_activity.remove(0);
                    }
                    task.tool_activity.push(summary.clone());
                    task.updated_at_ms = now_ms();
                }
                self.push_event(Some(task_id), "task.activity", summary);
            }
            CodexEvent::TurnCompleted { status, .. } => {
                let (task_status, session_status, terminal_event) = if self
                    .tasks
                    .get(&task_id)
                    .is_some_and(|task| task.cancel_requested)
                {
                    (
                        OrchestratorTaskStatus::Cancelled,
                        SessionStatus::Cancelled,
                        "task.cancelled",
                    )
                } else if status.eq_ignore_ascii_case("completed") {
                    (
                        OrchestratorTaskStatus::NeedsReview,
                        SessionStatus::NeedsInput,
                        "task.needs_review",
                    )
                } else {
                    (
                        OrchestratorTaskStatus::Failed,
                        SessionStatus::Failed,
                        "task.failed",
                    )
                };
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = task_status;
                    task.updated_at_ms = now_ms();
                }
                let _ = self.sessions.set_status(session_id, session_status);
                self.push_event(
                    Some(task_id.clone()),
                    terminal_event,
                    format!("Worker Codex turn ended with status {status}"),
                );
                if task_status != OrchestratorTaskStatus::NeedsReview {
                    self.worker_runtimes.remove(session_id);
                }
            }
            CodexEvent::Error(message) => {
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::Failed;
                    task.updated_at_ms = now_ms();
                }
                let _ = self.sessions.set_status(session_id, SessionStatus::Failed);
                self.push_event(Some(task_id), "runtime.error", message);
                self.worker_runtimes.remove(session_id);
            }
            CodexEvent::Notice(message) => {
                self.push_event(Some(task_id), "runtime.notice", message);
            }
            CodexEvent::ThreadStarted { .. }
            | CodexEvent::StoredThreads { .. }
            | CodexEvent::ThreadHistoryLoaded { .. }
            | CodexEvent::ThreadResumeFailed { .. }
            | CodexEvent::CatalogLoaded { .. } => {}
        }
    }

    fn handle_web_worker_event(&mut self, event: WebGptBrowserEvent) {
        match event {
            WebGptBrowserEvent::State(WebGptBrowserState::Starting) => {
                self.web_worker_ready = false;
            }
            WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn) => {
                self.web_worker_ready = true;
                self.push_event(
                    None,
                    "runtime.web_gpt_ready",
                    "Web GPT browser runtime is ready",
                );
                self.dispatch_next_web_worker();
            }
            WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired) => {
                self.web_worker_ready = false;
                if let Some(task_id) = self
                    .active_web_task_id
                    .clone()
                    .or_else(|| self.web_worker_queue.front().cloned())
                {
                    if let Some(session_id) = self.tasks[&task_id].session_id.clone() {
                        let _ = self
                            .sessions
                            .set_status(&session_id, SessionStatus::NeedsInput);
                        self.refresh_parent_status(&session_id);
                    }
                    self.push_event(
                        Some(task_id),
                        "task.web_worker_login_required",
                        "Web GPT login is required; the browser login surface was opened",
                    );
                }
                self.web_worker_commands
                    .push_back(WebWorkerCommand::ShowLogin);
            }
            WebGptBrowserEvent::State(WebGptBrowserState::Offline(message)) => {
                self.web_worker_ready = false;
                self.fail_active_web_worker(message);
            }
            WebGptBrowserEvent::WakeSubmitted { .. } => {}
            WebGptBrowserEvent::ChatSubmitted { correlation } => {
                let Some(task_id) = self.web_task_for_request(&correlation) else {
                    return;
                };
                if self.tasks[&task_id].cancel_requested {
                    return;
                }
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::RunningWebGpt;
                    task.updated_at_ms = now_ms();
                }
                if let Some(session_id) = self.tasks[&task_id].session_id.clone() {
                    let _ = self
                        .sessions
                        .set_status(&session_id, SessionStatus::Running);
                    self.refresh_parent_status(&session_id);
                }
                self.push_event(
                    Some(task_id),
                    "task.running_web_gpt",
                    "Web GPT worker turn started",
                );
            }
            WebGptBrowserEvent::ChatProgress {
                correlation,
                text,
                activity,
                thinking,
            } => {
                let Some(task_id) = self.web_task_for_request(&correlation) else {
                    return;
                };
                if self.tasks[&task_id].cancel_requested {
                    return;
                }
                let mut activity_event = None;
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    if let Some(text) = text.filter(|value| !value.trim().is_empty()) {
                        task.result = Some(text);
                    }
                    if let Some(activity) = activity.filter(|value| !value.trim().is_empty()) {
                        if task.tool_activity.len() >= 200 {
                            task.tool_activity.remove(0);
                        }
                        task.tool_activity.push(activity.clone());
                        activity_event = Some(activity);
                    }
                    task.updated_at_ms = now_ms();
                }
                if let Some(activity) = activity_event {
                    self.push_event(Some(task_id.clone()), "task.activity", activity);
                }
                self.push_event(
                    Some(task_id),
                    "task.web_gpt_progress",
                    if thinking {
                        "Web GPT worker is thinking"
                    } else {
                        "Web GPT worker response updated"
                    },
                );
            }
            WebGptBrowserEvent::ChatAnswered { correlation, text } => {
                let Some(task_id) = self.web_task_for_request(&correlation) else {
                    return;
                };
                let cancelled = self.tasks[&task_id].cancel_requested;
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    if !cancelled {
                        task.result = Some(text);
                    }
                    task.status = if cancelled {
                        OrchestratorTaskStatus::Cancelled
                    } else {
                        OrchestratorTaskStatus::NeedsReview
                    };
                    task.updated_at_ms = now_ms();
                }
                if let Some(session_id) = self.tasks[&task_id].session_id.clone() {
                    let _ = self.sessions.set_status(
                        &session_id,
                        if cancelled {
                            SessionStatus::Cancelled
                        } else {
                            SessionStatus::NeedsInput
                        },
                    );
                    self.refresh_parent_status(&session_id);
                }
                self.active_web_task_id = None;
                self.active_web_correlation = None;
                self.push_event(
                    Some(task_id),
                    if cancelled {
                        "task.cancelled"
                    } else {
                        "task.needs_review"
                    },
                    if cancelled {
                        "Web GPT worker stopped after cancellation was requested"
                    } else {
                        "Web GPT worker produced a final answer and awaits approval"
                    },
                );
                self.dispatch_next_web_worker();
            }
            WebGptBrowserEvent::ChatCancelled { correlation } => {
                let Some(task_id) = self.web_task_for_request(&correlation) else {
                    return;
                };
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::Cancelled;
                    task.updated_at_ms = now_ms();
                }
                if let Some(session_id) = self.tasks[&task_id].session_id.clone() {
                    let _ = self
                        .sessions
                        .set_status(&session_id, SessionStatus::Cancelled);
                    self.refresh_parent_status(&session_id);
                }
                self.active_web_task_id = None;
                self.active_web_correlation = None;
                self.push_event(
                    Some(task_id),
                    "task.cancelled",
                    "Web GPT worker turn was cancelled",
                );
                self.dispatch_next_web_worker();
            }
            WebGptBrowserEvent::ChatFailed {
                correlation,
                message,
            } => self.fail_web_worker_for_correlation(&correlation, message),
            WebGptBrowserEvent::ChatQueueCancelled { request } => {
                self.cancel_queued_web_worker(&request);
            }
            WebGptBrowserEvent::Error(message) => {
                self.push_event(None, "runtime.web_gpt_error", message);
            }
        }
    }

    fn web_task_for_request_owner(&self, correlation: &WebGptTurnCorrelation) -> Option<String> {
        let active_task_id = self.active_web_task_id.as_deref()?;
        if correlation.account_id != DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID
            || correlation.task_id.as_deref() != Some(active_task_id)
        {
            return None;
        }
        let task = self.tasks.get(active_task_id)?;
        if task.session_id.as_deref() != Some(correlation.session_id.as_str())
            || task.turn_id.as_deref() != Some(correlation.request_id.as_str())
        {
            return None;
        }
        Some(task.id.clone())
    }

    fn web_task_for_request(&mut self, correlation: &WebGptTurnCorrelation) -> Option<String> {
        let task_id = self.web_task_for_request_owner(correlation)?;
        match self.active_web_correlation.as_ref() {
            Some(active_correlation) if active_correlation != correlation => None,
            Some(_) => Some(task_id),
            None => {
                self.active_web_correlation = Some(correlation.clone());
                Some(task_id)
            }
        }
    }

    fn queued_web_task_for_request(&self, request: &WebGptTurnRequest) -> Option<String> {
        if request.account_id != DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID {
            return None;
        }
        let task_id = request.task_id.as_deref()?;
        let dispatched_but_unleased = self.active_web_task_id.as_deref() == Some(task_id)
            && self.active_web_correlation.is_none();
        let waiting_in_bridge_queue = self.web_worker_queue.iter().any(|queued| queued == task_id);
        if !dispatched_but_unleased && !waiting_in_bridge_queue {
            return None;
        }
        let task = self.tasks.get(task_id)?;
        if task.status != OrchestratorTaskStatus::Preparing
            || task.session_id.as_deref() != Some(request.session_id.as_str())
            || task.turn_id.as_deref() != Some(request.request_id.as_str())
        {
            return None;
        }
        Some(task.id.clone())
    }

    fn cancel_queued_web_worker(&mut self, request: &WebGptTurnRequest) {
        let Some(task_id) = self.queued_web_task_for_request(request) else {
            return;
        };
        let session_id = self.tasks[&task_id].session_id.clone();
        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.status = OrchestratorTaskStatus::Cancelled;
            task.updated_at_ms = now_ms();
        }
        if self.active_web_task_id.as_deref() == Some(task_id.as_str()) {
            self.active_web_task_id = None;
            self.active_web_correlation = None;
        }
        self.web_worker_queue.retain(|queued| queued != &task_id);
        if let Some(session_id) = session_id {
            let _ = self
                .sessions
                .set_status(&session_id, SessionStatus::Cancelled);
            self.refresh_parent_status(&session_id);
        }
        self.push_event(
            Some(task_id),
            "task.cancelled",
            "Queued Web GPT worker request was cancelled before leasing",
        );
        self.dispatch_next_web_worker();
    }

    fn fail_active_web_worker(&mut self, message: String) {
        let Some(task_id) = self.active_web_task_id.take() else {
            self.push_event(None, "runtime.web_gpt_error", message);
            return;
        };
        self.active_web_correlation = None;
        let cancelled = self.tasks[&task_id].cancel_requested;
        self.finish_web_worker_failure(task_id, message, cancelled);
    }

    fn fail_web_worker_for_correlation(
        &mut self,
        correlation: &WebGptTurnCorrelation,
        message: String,
    ) {
        let Some(task_id) = self.web_task_for_request(correlation) else {
            return;
        };
        let cancelled = self.tasks[&task_id].cancel_requested;
        self.finish_web_worker_failure(task_id, message, cancelled);
    }

    fn finish_web_worker_failure(&mut self, task_id: String, message: String, cancelled: bool) {
        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.status = if cancelled {
                OrchestratorTaskStatus::Cancelled
            } else {
                OrchestratorTaskStatus::Failed
            };
            task.updated_at_ms = now_ms();
        }
        if let Some(session_id) = self.tasks[&task_id].session_id.clone() {
            let _ = self.sessions.set_status(
                &session_id,
                if cancelled {
                    SessionStatus::Cancelled
                } else {
                    SessionStatus::Failed
                },
            );
            self.refresh_parent_status(&session_id);
        }
        if self.active_web_task_id.as_deref() == Some(task_id.as_str()) {
            self.active_web_task_id = None;
            self.active_web_correlation = None;
        }
        self.push_event(
            Some(task_id),
            if cancelled {
                "task.cancelled"
            } else {
                "task.failed"
            },
            message,
        );
        self.dispatch_next_web_worker();
    }

    fn worker_catalog(&self) -> Result<Value, String> {
        Ok(json!({
            "version": "v1",
            "root_session_id": self.root_session_id,
            "workers": [
                {
                    "id": "codex",
                    "runtime": "codex",
                    "label": SessionRuntime::Codex.label(),
                    "available": self.codex_ready,
                    "spawnable": true,
                    "capabilities": {
                        "spawn": true,
                        "send_input": "in_flight_steer_or_review_revision",
                        "resume": true,
                        "close": true,
                        "cancel": true,
                        "approve": true,
                        "model_override": true,
                        "fork_context": false,
                        "reasoning_efforts": ["low", "medium", "high", "xhigh"]
                    }
                },
                {
                    "id": "web_gpt",
                    "runtime": "web_gpt",
                    "label": SessionRuntime::WebGpt.label(),
                    "available": self.web_worker_ready,
                    "spawnable": true,
                    "capabilities": {
                        "spawn": true,
                        "send_input": "review_revision_only",
                        "resume": true,
                        "close": true,
                        "cancel": true,
                        "approve": true,
                        "model_override": false,
                        "fork_context": false,
                        "reasoning_control": "advisory",
                        "reasoning_efforts": ["low", "medium", "high", "xhigh"]
                    }
                }
            ]
        }))
    }

    fn worker_task_for_session(&self, session_id: &str) -> Option<&OrchestratorTask> {
        self.tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(session_id))
    }

    fn worker_handle(&self, session_id: &str) -> Result<Value, String> {
        let session = self
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| format!("Unknown worker session: {session_id}"))?;
        if !session.is_worker() {
            return Err(format!("Session {session_id} is not a worker"));
        }
        let task = self.worker_task_for_session(session_id).cloned();
        Ok(json!({
            "agent_id": session.id,
            "runtime": session.runtime,
            "status": session.status,
            "session": session,
            "task_id": task.as_ref().map(|task| task.id.clone()),
            "task": task,
        }))
    }

    fn worker_get(&self, params: &Value) -> Result<Value, String> {
        let agent_id = required_string(params, "agent_id")?;
        self.worker_handle(&agent_id)
    }

    fn worker_spawn(&mut self, params: &Value) -> Result<Value, String> {
        if params
            .get("fork_context")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(
                "Roche external workers do not support fork_context=true; pass a self-contained goal"
                    .to_owned(),
            );
        }
        if let Some(agent_type) = params.get("agent_type").and_then(Value::as_str)
            && !agent_type.trim().eq_ignore_ascii_case("worker")
        {
            return Err(format!(
                "Unsupported agent_type {agent_type:?}; Roche Multi-Agent Spawn v1 uses agent_type=worker"
            ));
        }
        required_string(params, "goal")?;
        let session = self.session_spawn(params)?;
        let session_id = session
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "session.spawn response did not include id".to_owned())?;
        self.worker_handle(session_id)
    }

    fn worker_send_input(&mut self, params: &Value) -> Result<Value, String> {
        let agent_id = required_string(params, "agent_id")?;
        let message = required_string(params, "message")?;
        let effort_override = params
            .get("reasoning_effort")
            .or_else(|| params.get("effort"))
            .and_then(Value::as_str)
            .map(normalize_effort)
            .transpose()?;
        let session = self
            .sessions
            .get(&agent_id)
            .cloned()
            .ok_or_else(|| format!("Unknown worker session: {agent_id}"))?;
        let task = self
            .worker_task_for_session(&agent_id)
            .cloned()
            .ok_or_else(|| format!("Worker {agent_id} does not have an executable task"))?;

        match session.runtime {
            SessionRuntime::Codex => match task.status {
                OrchestratorTaskStatus::RunningCodex => {
                    let effort = effort_override.unwrap_or_else(|| task.effort.clone());
                    let runtime = self
                        .worker_runtimes
                        .get(&agent_id)
                        .ok_or_else(|| format!("Codex worker runtime is not active: {agent_id}"))?;
                    runtime.send(message, effort, None);
                    self.push_event(
                        Some(task.id),
                        "task.worker_input",
                        "Follow-up input sent to the active Codex worker",
                    );
                }
                OrchestratorTaskStatus::NeedsReview | OrchestratorTaskStatus::Failed => {
                    let mut revision = json!({"task_id": task.id, "prompt": message});
                    if let Some(effort) = effort_override {
                        revision["effort"] = Value::String(effort);
                    }
                    self.revise_task(&revision)?;
                }
                OrchestratorTaskStatus::Preparing | OrchestratorTaskStatus::Queued => {
                    return Err(format!(
                        "Codex worker {agent_id} is still preparing; wait until it is running before send_input"
                    ));
                }
                OrchestratorTaskStatus::Cancelled | OrchestratorTaskStatus::Completed => {
                    return Err(format!(
                        "Codex worker {agent_id} is {:?} and cannot accept input",
                        task.status
                    ));
                }
                OrchestratorTaskStatus::RunningWebGpt => {
                    return Err("Codex worker entered an invalid Web GPT state".to_owned());
                }
            },
            SessionRuntime::WebGpt => match task.status {
                OrchestratorTaskStatus::NeedsReview | OrchestratorTaskStatus::Failed => {
                    let mut revision = json!({"task_id": task.id, "prompt": message});
                    if let Some(effort) = effort_override {
                        revision["effort"] = Value::String(effort);
                    }
                    self.revise_task(&revision)?;
                }
                OrchestratorTaskStatus::Preparing | OrchestratorTaskStatus::RunningWebGpt => {
                    return Err(
                        "Web GPT workers do not support in-flight steer yet; wait for needs_review or cancel the worker"
                            .to_owned(),
                    );
                }
                OrchestratorTaskStatus::Queued => {
                    return Err(format!(
                        "Web GPT worker {agent_id} is queued and cannot accept input yet"
                    ));
                }
                OrchestratorTaskStatus::Cancelled | OrchestratorTaskStatus::Completed => {
                    return Err(format!(
                        "Web GPT worker {agent_id} is {:?} and cannot accept input",
                        task.status
                    ));
                }
                OrchestratorTaskStatus::RunningCodex => {
                    return Err("Web GPT worker entered an invalid Codex state".to_owned());
                }
            },
            SessionRuntime::Unified => {
                return Err("Unified sessions are not spawnable workers".to_owned());
            }
        }
        self.worker_handle(&agent_id)
    }

    fn worker_resume(&mut self, params: &Value) -> Result<Value, String> {
        let agent_id = required_string(params, "agent_id")?;
        let task = self
            .worker_task_for_session(&agent_id)
            .cloned()
            .ok_or_else(|| format!("Worker {agent_id} does not have an executable task"))?;
        match task.status {
            OrchestratorTaskStatus::Failed => {
                let prompt = params
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("Resume from the failed attempt, preserve the original goal, and address the failure before reporting again.");
                let mut revision = json!({"task_id": task.id, "prompt": prompt});
                if let Some(effort) = params
                    .get("reasoning_effort")
                    .or_else(|| params.get("effort"))
                    .and_then(Value::as_str)
                {
                    revision["effort"] = Value::String(normalize_effort(effort)?);
                }
                self.revise_task(&revision)?;
            }
            OrchestratorTaskStatus::Cancelled | OrchestratorTaskStatus::Completed => {
                return Err(format!(
                    "Worker {agent_id} is {:?}; terminal workers cannot be resumed",
                    task.status
                ));
            }
            OrchestratorTaskStatus::Queued
            | OrchestratorTaskStatus::Preparing
            | OrchestratorTaskStatus::RunningCodex
            | OrchestratorTaskStatus::RunningWebGpt
            | OrchestratorTaskStatus::NeedsReview => {}
        }
        self.worker_handle(&agent_id)
    }

    fn worker_close(
        &mut self,
        params: &Value,
        commands: &Sender<CodexCommand>,
    ) -> Result<Value, String> {
        let agent_id = required_string(params, "agent_id")?;
        let task = self
            .worker_task_for_session(&agent_id)
            .cloned()
            .ok_or_else(|| format!("Worker {agent_id} does not have an executable task"))?;
        match task.status {
            OrchestratorTaskStatus::NeedsReview => {
                self.worker_runtimes.remove(&agent_id);
                self.web_worker_queue.retain(|queued| queued != &task.id);
                if self.active_web_task_id.as_deref() == Some(task.id.as_str()) {
                    self.active_web_task_id = None;
                }
                self.push_event(
                    Some(task.id),
                    "task.worker_closed",
                    "Worker execution slot closed; review result preserved",
                );
            }
            OrchestratorTaskStatus::Completed
            | OrchestratorTaskStatus::Cancelled
            | OrchestratorTaskStatus::Failed => {
                self.worker_runtimes.remove(&agent_id);
            }
            OrchestratorTaskStatus::Queued
            | OrchestratorTaskStatus::Preparing
            | OrchestratorTaskStatus::RunningCodex
            | OrchestratorTaskStatus::RunningWebGpt => {
                self.cancel_task(&json!({"task_id": task.id}), commands)?;
            }
        }
        self.worker_handle(&agent_id)
    }

    fn worker_approve(&mut self, params: &Value) -> Result<Value, String> {
        let agent_id = required_string(params, "agent_id")?;
        let task_id = self
            .worker_task_for_session(&agent_id)
            .map(|task| task.id.clone())
            .ok_or_else(|| format!("Worker {agent_id} does not have an executable task"))?;
        self.approve_task(&json!({"task_id": task_id}))?;
        self.worker_handle(&agent_id)
    }

    fn worker_cancel(
        &mut self,
        params: &Value,
        commands: &Sender<CodexCommand>,
    ) -> Result<Value, String> {
        let agent_id = required_string(params, "agent_id")?;
        let task_id = self
            .worker_task_for_session(&agent_id)
            .map(|task| task.id.clone())
            .ok_or_else(|| format!("Worker {agent_id} does not have an executable task"))?;
        self.cancel_task(&json!({"task_id": task_id}), commands)?;
        self.worker_handle(&agent_id)
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

    fn session_create(&mut self, params: &Value) -> Result<Value, String> {
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("New session");
        let session = self.sessions.create_root(
            self.project_root.display().to_string(),
            SessionRuntime::Unified,
            title,
        );
        serde_json::to_value(session)
            .map_err(|error| format!("Could not serialize session: {error}"))
    }

    fn session_rename(&mut self, params: &Value) -> Result<Value, String> {
        let session_id = required_string(params, "session_id")?;
        let title = required_string(params, "title")?;
        let session = self.sessions.rename(&session_id, title)?;
        serde_json::to_value(session)
            .map_err(|error| format!("Could not serialize renamed session: {error}"))
    }

    fn session_delete(&mut self, params: &Value) -> Result<Value, String> {
        let session_id = required_string(params, "session_id")?;
        if session_id == self.root_session_id {
            return Err("The primary Main session cannot be deleted".to_owned());
        }
        let parent_id = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.parent_session_id.clone());
        let subtree_ids = self.sessions.subtree_ids(&session_id)?;
        let task_ids = self
            .tasks
            .values()
            .filter(|task| {
                task.session_id
                    .as_ref()
                    .is_some_and(|id| subtree_ids.iter().any(|session_id| session_id == id))
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();

        for task_id in &task_ids {
            let session_id = self
                .tasks
                .get(task_id)
                .and_then(|task| task.session_id.clone());
            if let Some(session_id) = session_id
                && let Some(runtime) = self.worker_runtimes.remove(&session_id)
            {
                runtime.interrupt();
            }
            if self.active_web_task_id.as_deref() == Some(task_id.as_str()) {
                if let Some(request) = self.tasks.get(task_id).and_then(|task| {
                    Some(WebGptTurnRequest::worker(
                        task.session_id.clone()?,
                        task.id.clone(),
                        task.turn_id.clone()?,
                    ))
                }) {
                    self.web_worker_commands
                        .push_back(WebWorkerCommand::Cancel { request });
                }
                self.active_web_task_id = None;
                self.active_web_correlation = None;
            }
            self.web_worker_queue.retain(|queued| queued != task_id);
            self.tasks.remove(task_id);
        }

        let removed = self.sessions.remove_subtree(&session_id)?;
        if let Some(parent_id) = parent_id {
            self.refresh_session_worker_status(&parent_id);
        }
        self.dispatch_next_web_worker();
        serde_json::to_value(removed)
            .map_err(|error| format!("Could not serialize deleted sessions: {error}"))
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
        let goal = params
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
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
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if runtime == SessionRuntime::WebGpt && model.is_some() {
            return Err(
                "Web GPT workers use the authenticated Roche browser model; --model is unsupported"
                    .to_owned(),
            );
        }
        let session = self
            .sessions
            .spawn_worker(&parent_session_id, runtime, title)?;

        if let Some(goal) = goal {
            let task_id = next_task_id();
            let timestamp = now_ms();
            let task = OrchestratorTask {
                id: task_id.clone(),
                session_id: Some(session.id.clone()),
                title: session.title.clone(),
                goal,
                acceptance,
                effort: effort.clone(),
                status: OrchestratorTaskStatus::Preparing,
                turn_id: None,
                result: None,
                tool_activity: Vec::new(),
                revision_count: 0,
                cancel_requested: false,
                created_at_ms: timestamp,
                updated_at_ms: timestamp,
            };
            let prompt = orchestrator_prompt(&task);
            self.tasks.insert(task_id.clone(), task);
            self.sessions
                .set_status(&session.id, SessionStatus::WaitingOnWorkers)?;
            let _ = self
                .sessions
                .set_status(&parent_session_id, SessionStatus::WaitingOnWorkers);
            match runtime {
                SessionRuntime::Codex => {
                    let worker = CodexWorkerRuntime::spawn(self.project_root.clone());
                    worker.send(prompt, effort, model);
                    self.worker_runtimes.insert(session.id.clone(), worker);
                    self.push_event(
                        Some(task_id),
                        "task.worker_started",
                        format!(
                            "Independent Codex worker started for session {}",
                            session.id
                        ),
                    );
                }
                SessionRuntime::WebGpt => {
                    self.queue_web_worker(task_id.clone());
                    self.push_event(
                        Some(task_id),
                        "task.web_worker_queued",
                        format!("Web GPT worker queued for session {}", session.id),
                    );
                }
                SessionRuntime::Unified => unreachable!("unified workers were rejected"),
            }
        }

        let session = self
            .sessions
            .get(&session.id)
            .cloned()
            .ok_or_else(|| "Spawned worker session disappeared".to_owned())?;
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
                "web_worker_ready": self.web_worker_ready,
                "active_web_task_id": self.active_web_task_id,
                "queued_web_workers": self.web_worker_queue.len(),
                "active_task_id": self.active_task_id,
                "queued": self.queue.len(),
                "task_count": self.tasks.len(),
                "pending_chat": self.pending_chat.len(),
                "chat_count": self.chat_requests.len(),
                "project_root": self.project_root,
                "root_session_id": self.root_session_id,
                "active_sessions": self.sessions.active_count(&self.project_root.display().to_string()),
            })),
            "worker.catalog" => self.worker_catalog(),
            "worker.get" => self.worker_get(&request.params),
            "worker.spawn" => self.worker_spawn(&request.params),
            "worker.send_input" => self.worker_send_input(&request.params),
            "worker.resume" => self.worker_resume(&request.params),
            "worker.close" => self.worker_close(&request.params, commands),
            "worker.approve" => self.worker_approve(&request.params),
            "worker.cancel" => self.worker_cancel(&request.params, commands),
            "session.list" => self.session_list(),
            "session.get" => self.session_get(&request.params),
            "session.create" => self.session_create(&request.params),
            "session.rename" => self.session_rename(&request.params),
            "session.delete" => self.session_delete(&request.params),
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
    web_browser: SharedWebGptBrowser,
) -> Result<(), String> {
    if let Some(client) = IN_PROCESS_BRIDGE.get() {
        let rebind = BRIDGE_REBIND
            .get()
            .ok_or_else(|| "Roche bridge rebind channel is unavailable".to_owned())?;
        rebind
            .send(BridgeRebind {
                project_root: project_root.clone(),
                commands,
                codex_events,
                web_browser,
            })
            .map_err(|_| "Roche bridge worker is no longer running".to_owned())?;
        let updated_client = BridgeClientConfig {
            project_root: project_root.display().to_string(),
            ..client.clone()
        };
        write_bridge_descriptor(&project_root, &updated_client)?;
        let current_root = BRIDGE_CURRENT_ROOT
            .get()
            .ok_or_else(|| "Roche bridge current root is unavailable".to_owned())?;
        let mut current_root = current_root
            .lock()
            .map_err(|_| "Roche bridge current root lock is poisoned".to_owned())?;
        let previous_root = current_root.clone();
        if previous_root != project_root {
            let _ = fs::remove_file(bridge_descriptor_path(&previous_root));
        }
        *current_root = project_root;
        return Ok(());
    }
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
    let (rebind_tx, rebind_rx) = std::sync::mpsc::channel();
    BRIDGE_REBIND
        .set(rebind_tx)
        .map_err(|_| "Roche bridge rebind channel is already initialized".to_owned())?;
    BRIDGE_CURRENT_ROOT
        .set(Mutex::new(project_root.clone()))
        .map_err(|_| "Roche bridge current root is already initialized".to_owned())?;
    IN_PROCESS_BRIDGE
        .set(client.clone())
        .map_err(|_| "Roche Web GPT bridge is already initialized in this process".to_owned())?;
    write_bridge_descriptor(&project_root, &client)?;
    thread::Builder::new()
        .name("roche-webgpt-orchestrator".to_owned())
        .spawn(move || {
            bridge_worker(
                listener,
                project_root,
                commands,
                codex_events,
                web_browser,
                rebind_rx,
                token,
            )
        })
        .map_err(|error| format!("Could not start Roche Web GPT bridge worker: {error}"))?;
    Ok(())
}

fn bridge_worker(
    listener: TcpListener,
    mut project_root: PathBuf,
    mut commands: Sender<CodexCommand>,
    mut codex_events: Receiver<CodexEvent>,
    mut web_browser: SharedWebGptBrowser,
    rebind_rx: Receiver<BridgeRebind>,
    auth_token: String,
) {
    let mut state = BridgeState::new(project_root.clone(), auth_token.clone());
    loop {
        while let Ok(rebind) = rebind_rx.try_recv() {
            project_root = rebind.project_root;
            commands = rebind.commands;
            codex_events = rebind.codex_events;
            web_browser = rebind.web_browser;
            state = BridgeState::new(project_root.clone(), auth_token.clone());
            state.push_event(
                None,
                "runtime.workspace_rebound",
                format!("Roche bridge rebound to {}", project_root.display()),
            );
        }
        while let Ok(event) = codex_events.try_recv() {
            state.handle_codex_event(event, &commands);
        }
        state.drain_worker_events();
        for event in web_browser.drain_worker() {
            state.handle_web_worker_event(event);
        }
        for command in state.drain_web_worker_commands() {
            match command {
                WebWorkerCommand::EnsureRuntime => {}
                WebWorkerCommand::Submit { request, text } => {
                    web_browser.submit_chat(request, text);
                }
                WebWorkerCommand::Cancel { request } => {
                    web_browser.cancel_chat(request);
                }
                WebWorkerCommand::ShowLogin => web_browser.show_login(),
            }
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
    SessionCreated {
        session: crate::sessions::AgentSession,
    },
    SessionRenamed {
        session: crate::sessions::AgentSession,
    },
    SessionDeleted {
        session_ids: Vec<String>,
    },
    WorkerApproved {
        session_id: String,
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
    CreateSession {
        title: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    DeleteSession {
        session_id: String,
    },
    ApproveWorker {
        session_id: String,
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

    pub fn create_session(&self, title: String) {
        let _ = self
            .commands
            .send(WebGptRuntimeCommand::CreateSession { title });
    }

    pub fn rename_session(&self, session_id: String, title: String) {
        let _ = self
            .commands
            .send(WebGptRuntimeCommand::RenameSession { session_id, title });
    }

    pub fn delete_session(&self, session_id: String) {
        let _ = self
            .commands
            .send(WebGptRuntimeCommand::DeleteSession { session_id });
    }

    pub fn approve_worker(&self, session_id: String) {
        let _ = self
            .commands
            .send(WebGptRuntimeCommand::ApproveWorker { session_id });
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
            Ok(WebGptRuntimeCommand::CreateSession { title }) => {
                match rpc_call("session.create", json!({"title": title})) {
                    Ok(value) => match serde_json::from_value(value) {
                        Ok(session) => {
                            let _ = events.send(WebGptRuntimeEvent::SessionCreated { session });
                        }
                        Err(error) => {
                            let _ = events.send(WebGptRuntimeEvent::Error {
                                local_id: None,
                                message: format!("session.create response was invalid: {error}"),
                            });
                        }
                    },
                    Err(message) => {
                        let _ = events.send(WebGptRuntimeEvent::Error {
                            local_id: None,
                            message,
                        });
                    }
                }
            }
            Ok(WebGptRuntimeCommand::RenameSession { session_id, title }) => {
                match rpc_call(
                    "session.rename",
                    json!({"session_id": session_id, "title": title}),
                ) {
                    Ok(value) => match serde_json::from_value(value) {
                        Ok(session) => {
                            let _ = events.send(WebGptRuntimeEvent::SessionRenamed { session });
                        }
                        Err(error) => {
                            let _ = events.send(WebGptRuntimeEvent::Error {
                                local_id: None,
                                message: format!("session.rename response was invalid: {error}"),
                            });
                        }
                    },
                    Err(message) => {
                        let _ = events.send(WebGptRuntimeEvent::Error {
                            local_id: None,
                            message,
                        });
                    }
                }
            }
            Ok(WebGptRuntimeCommand::DeleteSession { session_id }) => {
                match rpc_call("session.delete", json!({"session_id": session_id})) {
                    Ok(value) => {
                        match serde_json::from_value::<Vec<crate::sessions::AgentSession>>(value) {
                            Ok(sessions) => {
                                let session_ids =
                                    sessions.into_iter().map(|session| session.id).collect();
                                let _ =
                                    events.send(WebGptRuntimeEvent::SessionDeleted { session_ids });
                            }
                            Err(error) => {
                                let _ = events.send(WebGptRuntimeEvent::Error {
                                    local_id: None,
                                    message: format!(
                                        "session.delete response was invalid: {error}"
                                    ),
                                });
                            }
                        }
                    }
                    Err(message) => {
                        let _ = events.send(WebGptRuntimeEvent::Error {
                            local_id: None,
                            message,
                        });
                    }
                }
            }
            Ok(WebGptRuntimeCommand::ApproveWorker { session_id }) => {
                match rpc_call("worker.approve", json!({"agent_id": session_id})) {
                    Ok(_) => {
                        let _ = events.send(WebGptRuntimeEvent::WorkerApproved { session_id });
                        next_session_poll = Instant::now();
                    }
                    Err(message) => {
                        let _ = events.send(WebGptRuntimeEvent::Error {
                            local_id: None,
                            message,
                        });
                    }
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

    fn leased(request: &WebGptTurnRequest, generation: u64) -> WebGptTurnCorrelation {
        request.clone().lease(0, generation)
    }

    #[test]
    fn prompt_keeps_rust_as_completion_authority() {
        let task = OrchestratorTask {
            id: "task-test".to_owned(),
            session_id: None,
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
                ..
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
        assert_eq!(
            state.sessions.get(&state.root_session_id).unwrap().status,
            SessionStatus::Idle
        );
    }

    #[test]
    fn worker_catalog_exposes_codex_and_web_gpt_as_v1_workers() {
        let state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        let catalog = state.worker_catalog().unwrap();
        assert_eq!(catalog["version"], "v1");
        assert_eq!(catalog["workers"][0]["runtime"], "codex");
        assert_eq!(catalog["workers"][1]["runtime"], "web_gpt");
        assert_eq!(catalog["workers"][0]["spawnable"], true);
        assert_eq!(catalog["workers"][1]["spawnable"], true);
    }

    #[test]
    fn worker_spawn_adapter_creates_web_gpt_worker_handle() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        let handle = state
            .worker_spawn(&json!({
                "agent_type": "worker",
                "runtime": "web_gpt",
                "goal": "Review the implementation",
                "fork_context": false
            }))
            .unwrap();
        assert_eq!(handle["runtime"], "web_gpt");
        assert_eq!(handle["status"], "waiting_on_workers");
        assert!(handle["agent_id"].as_str().unwrap().starts_with("session-"));
        assert!(handle["task_id"].as_str().unwrap().starts_with("task-"));
    }

    #[test]
    fn session_rename_and_delete_keep_primary_main_safe() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        let primary = state.root_session_id.clone();
        let secondary = state.session_create(&json!({"title": "Scratch"})).unwrap();
        let secondary_id = secondary["id"].as_str().unwrap().to_owned();

        let renamed = state
            .session_rename(&json!({"session_id": secondary_id, "title": "Review"}))
            .unwrap();
        assert_eq!(renamed["title"], "Review");

        let removed = state
            .session_delete(&json!({"session_id": secondary_id}))
            .unwrap();
        assert_eq!(removed.as_array().unwrap().len(), 1);
        assert!(state.sessions.get(&secondary_id).is_none());

        let error = state
            .session_delete(&json!({"session_id": primary}))
            .unwrap_err();
        assert!(error.contains("Main"));
    }

    #[test]
    fn web_gpt_worker_executes_through_browser_commands_and_review_gate() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        let session = state
            .session_spawn(&json!({
                "runtime": "web_gpt",
                "title": "Research",
                "goal": "Inspect the repository",
                "acceptance": ["Return evidence"],
                "effort": "high"
            }))
            .unwrap();
        let session_id = session["id"].as_str().unwrap().to_owned();
        let task_id = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(session_id.as_str()))
            .unwrap()
            .id
            .clone();

        assert!(matches!(
            state.drain_web_worker_commands().as_slice(),
            [WebWorkerCommand::EnsureRuntime]
        ));
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let commands = state.drain_web_worker_commands();
        let (request, prompt) = commands
            .iter()
            .find_map(|command| match command {
                WebWorkerCommand::Submit { request, text } => Some((request.clone(), text.clone())),
                _ => None,
            })
            .expect("logged-in browser should receive one worker submission");
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, WebWorkerCommand::Submit { .. }))
                .count(),
            1
        );
        assert!(prompt.contains("Inspect the repository"));
        let correlation = leased(&request, 1);

        state.handle_web_worker_event(WebGptBrowserEvent::ChatSubmitted {
            correlation: correlation.clone(),
        });
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::RunningWebGpt
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatProgress {
            correlation: correlation.clone(),
            text: Some("Working".to_owned()),
            activity: Some("Searching repository".to_owned()),
            thinking: false,
        });
        assert_eq!(state.tasks[&task_id].result.as_deref(), Some("Working"));
        assert_eq!(
            state.tasks[&task_id].tool_activity,
            vec!["Searching repository"]
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation,
            text: "Final evidence".to_owned(),
        });
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::NeedsReview
        );
        assert_eq!(
            state.sessions.get(&session_id).unwrap().status,
            SessionStatus::NeedsInput
        );
        state
            .approve_task(&json!({"task_id": task_id.clone()}))
            .unwrap();
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::Completed
        );
    }

    #[test]
    fn web_gpt_rejects_wrong_owner_and_stale_generation_correlations() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let session = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "First"}))
            .unwrap();
        let task_id = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == session["id"].as_str())
            .unwrap()
            .id
            .clone();
        let request = state
            .drain_web_worker_commands()
            .into_iter()
            .find_map(|command| match command {
                WebWorkerCommand::Submit { request, .. } => Some(request),
                _ => None,
            })
            .expect("worker request");
        let valid = leased(&request, 11);

        let mut wrong_task = valid.clone();
        wrong_task.task_id = Some("task-other".to_owned());
        let mut wrong_session = valid.clone();
        wrong_session.session_id = "session-other".to_owned();
        let mut wrong_account = valid.clone();
        wrong_account.account_id = "account-other".to_owned();
        for correlation in [wrong_task, wrong_session, wrong_account] {
            state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
                correlation,
                text: "must be ignored".to_owned(),
            });
        }
        assert_eq!(state.active_web_task_id.as_deref(), Some(task_id.as_str()));
        assert_eq!(state.active_web_correlation, None);
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::Preparing
        );

        state.handle_web_worker_event(WebGptBrowserEvent::ChatSubmitted {
            correlation: valid.clone(),
        });
        assert_eq!(state.active_web_correlation, Some(valid.clone()));
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::RunningWebGpt
        );

        let stale_generation = request.clone().lease(0, 10);
        state.handle_web_worker_event(WebGptBrowserEvent::ChatProgress {
            correlation: stale_generation.clone(),
            text: Some("stale".to_owned()),
            activity: Some("stale activity".to_owned()),
            thinking: false,
        });
        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation: stale_generation,
            text: "stale answer".to_owned(),
        });
        assert_eq!(state.tasks[&task_id].result, None);
        assert_eq!(state.active_web_task_id.as_deref(), Some(task_id.as_str()));

        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation: valid,
            text: "valid answer".to_owned(),
        });
        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::NeedsReview
        );
        assert_eq!(state.active_web_task_id, None);
    }

    #[test]
    fn web_gpt_queued_cancellation_requires_exact_unleased_owner() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let first = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "First"}))
            .unwrap();
        let second = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Second"}))
            .unwrap();
        let second_session = second["id"].as_str().unwrap().to_owned();
        let second_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(second_session.as_str()))
            .unwrap()
            .id
            .clone();
        let first_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == first["id"].as_str())
            .unwrap()
            .id
            .clone();
        let second_request_id = format!("web-worker-{second_task}-r0");
        state.tasks.get_mut(&second_task).unwrap().turn_id = Some(second_request_id.clone());
        let wrong = WebGptTurnRequest::worker(
            "session-other".to_owned(),
            second_task.clone(),
            second_request_id.clone(),
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatQueueCancelled { request: wrong });
        assert_eq!(
            state.tasks[&second_task].status,
            OrchestratorTaskStatus::Preparing
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(first_task.as_str())
        );

        let valid =
            WebGptTurnRequest::worker(second_session, second_task.clone(), second_request_id);
        state.handle_web_worker_event(WebGptBrowserEvent::ChatQueueCancelled { request: valid });
        assert_eq!(
            state.tasks[&second_task].status,
            OrchestratorTaskStatus::Cancelled
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(first_task.as_str())
        );
        assert!(!state.web_worker_queue.iter().any(|id| id == &second_task));
    }

    #[test]
    fn browser_queued_cancel_releases_dispatched_but_unleased_worker() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let first = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "First"}))
            .unwrap();
        let second = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Second"}))
            .unwrap();
        let first_session = first["id"].as_str().unwrap().to_owned();
        let first_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session.as_str()))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == second["id"].as_str())
            .unwrap()
            .id
            .clone();
        let first_request = state
            .drain_web_worker_commands()
            .into_iter()
            .find_map(|command| match command {
                WebWorkerCommand::Submit { request, .. } => Some(request),
                _ => None,
            })
            .expect("first worker was dispatched to the shared browser");
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(first_task.as_str())
        );
        assert!(state.active_web_correlation.is_none());

        state.handle_web_worker_event(WebGptBrowserEvent::ChatQueueCancelled {
            request: first_request,
        });

        assert_eq!(
            state.tasks[&first_task].status,
            OrchestratorTaskStatus::Cancelled
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(second_task.as_str())
        );
        assert_eq!(
            state
                .drain_web_worker_commands()
                .into_iter()
                .filter(|command| matches!(command, WebWorkerCommand::Submit { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn web_gpt_workers_serialize_and_cancel_without_accepting_late_answers() {
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let first = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "First"}))
            .unwrap();
        let second = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Second"}))
            .unwrap();
        let third = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Third"}))
            .unwrap();
        let first_session = first["id"].as_str().unwrap();
        let second_session = second["id"].as_str().unwrap();
        let third_session = third["id"].as_str().unwrap();
        let first_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(second_session))
            .unwrap()
            .id
            .clone();
        let third_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(third_session))
            .unwrap()
            .id
            .clone();
        let initial = state.drain_web_worker_commands();
        let first_request = initial
            .iter()
            .find_map(|command| match command {
                WebWorkerCommand::Submit { request, .. } => Some(request.clone()),
                _ => None,
            })
            .unwrap();
        let first_correlation = leased(&first_request, 1);
        assert_eq!(
            initial
                .iter()
                .filter(|command| matches!(command, WebWorkerCommand::Submit { .. }))
                .count(),
            1
        );
        assert_eq!(
            state.tasks[&second_task].status,
            OrchestratorTaskStatus::Preparing
        );

        state.handle_web_worker_event(WebGptBrowserEvent::ChatSubmitted {
            correlation: first_correlation.clone(),
        });
        state
            .cancel_task(&json!({"task_id": first_task.clone()}), &command_tx)
            .unwrap();
        state
            .cancel_task(&json!({"task_id": first_task.clone()}), &command_tx)
            .unwrap();
        let cancel_commands = state.drain_web_worker_commands();
        assert_eq!(
            cancel_commands
                .iter()
                .filter(|command| matches!(command, WebWorkerCommand::Cancel { request } if request.request_id == first_request.request_id))
                .count(),
            1
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatProgress {
            correlation: first_correlation.clone(),
            text: Some("late progress".to_owned()),
            activity: Some("late activity".to_owned()),
            thinking: false,
        });
        assert!(state.tasks[&first_task].result.is_none());
        state.handle_web_worker_event(WebGptBrowserEvent::ChatCancelled {
            correlation: first_correlation.clone(),
        });
        assert_eq!(
            state.tasks[&first_task].status,
            OrchestratorTaskStatus::Cancelled
        );
        let next = state.drain_web_worker_commands();
        assert_eq!(
            next.iter()
                .filter(|command| matches!(command, WebWorkerCommand::Submit { .. }))
                .count(),
            1
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(second_task.as_str())
        );
        assert_eq!(
            state.tasks[&third_task].status,
            OrchestratorTaskStatus::Preparing
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation: first_correlation,
            text: "late".to_owned(),
        });
        assert_eq!(
            state.tasks[&first_task].status,
            OrchestratorTaskStatus::Cancelled
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(second_task.as_str())
        );
        assert_eq!(
            state.tasks[&third_task].status,
            OrchestratorTaskStatus::Preparing
        );
    }

    #[test]
    fn web_gpt_matching_failure_advances_once_and_late_failure_cannot_fail_next_task() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let first = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "First"}))
            .unwrap();
        let second = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Second"}))
            .unwrap();
        let first_session = first["id"].as_str().unwrap();
        let second_session = second["id"].as_str().unwrap();
        let first_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(second_session))
            .unwrap()
            .id
            .clone();
        let initial = state.drain_web_worker_commands();
        let first_request = initial
            .iter()
            .find_map(|command| match command {
                WebWorkerCommand::Submit { request, .. } => Some(request.clone()),
                _ => None,
            })
            .expect("first worker should be submitted");
        let first_correlation = leased(&first_request, 1);

        state.handle_web_worker_event(WebGptBrowserEvent::ChatFailed {
            correlation: first_correlation.clone(),
            message: "first failed".to_owned(),
        });
        assert_eq!(
            state.tasks[&first_task].status,
            OrchestratorTaskStatus::Failed
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(second_task.as_str())
        );
        let next = state.drain_web_worker_commands();
        assert_eq!(
            next.iter()
                .filter(|command| matches!(command, WebWorkerCommand::Submit { .. }))
                .count(),
            1
        );

        state.handle_web_worker_event(WebGptBrowserEvent::ChatFailed {
            correlation: first_correlation,
            message: "late first failure".to_owned(),
        });
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(second_task.as_str())
        );
        assert_eq!(
            state.tasks[&second_task].status,
            OrchestratorTaskStatus::Preparing
        );
        assert!(state.drain_web_worker_commands().is_empty());
    }

    #[test]
    fn web_gpt_generic_error_is_diagnostic_without_consuming_active_task() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn));
        let first = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "First"}))
            .unwrap();
        let second = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Second"}))
            .unwrap();
        let first_session = first["id"].as_str().unwrap();
        let second_session = second["id"].as_str().unwrap();
        let first_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(second_session))
            .unwrap()
            .id
            .clone();
        let _ = state.drain_web_worker_commands();

        state.handle_web_worker_event(WebGptBrowserEvent::Error(
            "cancel diagnostic for web-worker-first".to_owned(),
        ));
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(first_task.as_str())
        );
        assert_eq!(
            state.tasks[&first_task].status,
            OrchestratorTaskStatus::Preparing
        );
        assert_eq!(
            state.tasks[&second_task].status,
            OrchestratorTaskStatus::Preparing
        );
        assert!(state.drain_web_worker_commands().is_empty());
        assert!(state.events.iter().any(|event| {
            event.event == "runtime.web_gpt_error"
                && event.summary == "cancel diagnostic for web-worker-first"
        }));
    }

    #[test]
    fn offline_without_active_worker_does_not_consume_queued_task() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        let session = state
            .session_spawn(&json!({"runtime": "web_gpt", "goal": "Queued"}))
            .unwrap();
        let task_id = state
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == session["id"].as_str())
            .unwrap()
            .id
            .clone();
        assert!(state.active_web_task_id.is_none());
        assert!(
            state
                .web_worker_queue
                .iter()
                .any(|queued| queued == &task_id)
        );

        state.handle_web_worker_event(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
            "host unavailable".to_owned(),
        )));

        assert_eq!(
            state.tasks[&task_id].status,
            OrchestratorTaskStatus::Preparing
        );
        assert!(
            state
                .web_worker_queue
                .iter()
                .any(|queued| queued == &task_id)
        );
    }
}
