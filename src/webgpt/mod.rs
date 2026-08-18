//! Roche-owned Web GPT orchestration core and public façade.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::mpsc::Sender,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod chat;
mod runtime;
mod task_store;
mod transport;
mod types;

#[allow(unused_imports)]
pub use runtime::{WebGptRuntimeController, WebGptRuntimeEvent};
pub(crate) use transport::spawn_orchestrator_bridge;
#[allow(unused_imports)]
pub use transport::{DEFAULT_WEBGPT_BRIDGE_ADDR, bridge_addr, rpc_call};
#[allow(unused_imports)]
pub use types::{
    OrchestratorEvent, OrchestratorTask, OrchestratorTaskStatus, ProjectSnapshot, WebChatRequest,
    WebChatStatus,
};

use chat::{ChatMailbox, ChatOutcome};
use task_store::TaskStore;
use transport::capability_matches;
use types::{
    next_task_id, normalize_effort, now_ms, orchestrator_prompt, parse_session_runtime,
    parse_session_status, project_snapshot, required_string,
};

use crate::{
    codex::{CodexCommand, CodexConnection, CodexEvent, CodexWorkerRuntime},
    sessions::{SessionGraph, SessionRuntime, SessionStatus},
    web_browser::{WebGptBrowserEvent, WebGptBrowserState},
    web_browser_protocol::{
        DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID, WebGptTurnCorrelation, WebGptTurnRequest,
    },
};

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
    task_store: TaskStore,
    chat: ChatMailbox,
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
            task_store: TaskStore::default(),
            chat: ChatMailbox::default(),
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
        self.task_store.push_event(task_id, event, summary);
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
            let Some(task) = self.task_store.tasks.get_mut(&task_id) else {
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
        self.task_store.tasks.insert(id.clone(), task);
        self.queue.push_back(id.clone());
        self.push_event(Some(id.clone()), "task.queued", "Task queued by Web GPT");
        Ok(
            serde_json::to_value(self.task_store.tasks.get(&id).expect("inserted task"))
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
                .task_store
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
            serde_json::to_value(self.task_store.tasks.get(&task_id).expect("existing task"))
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
            .task_store
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
                let task = self
                    .task_store
                    .tasks
                    .get_mut(&task_id)
                    .expect("existing task");
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
                return Ok(serde_json::to_value(
                    self.task_store.tasks.get(&task_id).expect("existing task"),
                )
                .expect("task serialization cannot fail"));
            }
            let task = self
                .task_store
                .tasks
                .get_mut(&task_id)
                .expect("existing task");
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
            return Ok(serde_json::to_value(
                self.task_store.tasks.get(&task_id).expect("existing task"),
            )
            .expect("task serialization cannot fail"));
        }

        let active = self.active_task_id.as_deref() == Some(task_id.as_str());
        let task = self
            .task_store
            .tasks
            .get_mut(&task_id)
            .expect("existing task");
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
            serde_json::to_value(self.task_store.tasks.get(&task_id).expect("existing task"))
                .expect("task serialization cannot fail"),
        )
    }

    fn approve_task(&mut self, params: &Value) -> Result<Value, String> {
        let task_id = required_string(params, "task_id")?;
        let session_id = {
            let task = self
                .task_store
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
            serde_json::to_value(self.task_store.tasks.get(&task_id).expect("existing task"))
                .expect("task serialization cannot fail"),
        )
    }

    fn record_chat_outcome(
        &mut self,
        outcome: Result<ChatOutcome, String>,
    ) -> Result<Value, String> {
        let outcome = outcome?;
        if let Some(event) = outcome.event {
            self.push_event(None, event.event, event.summary);
        }
        Ok(outcome.value)
    }

    fn submit_chat(&mut self, params: &Value) -> Result<Value, String> {
        let outcome = self.chat.submit(params);
        self.record_chat_outcome(outcome)
    }

    fn claim_pending_chat(&mut self) -> Result<Value, String> {
        let outcome = self.chat.claim_pending();
        self.record_chat_outcome(outcome)
    }

    fn release_chat(&mut self, params: &Value) -> Result<Value, String> {
        let outcome = self.chat.release(params);
        self.record_chat_outcome(outcome)
    }

    fn respond_chat(&mut self, params: &Value) -> Result<Value, String> {
        let outcome = self.chat.respond(params);
        self.record_chat_outcome(outcome)
    }

    fn poll_chat(&self, params: &Value) -> Result<Value, String> {
        self.chat.poll(params)
    }

    fn cancel_chat(&mut self, params: &Value) -> Result<Value, String> {
        let outcome = self.chat.cancel(params);
        self.record_chat_outcome(outcome)
    }

    fn dispatch_next(&mut self, commands: &Sender<CodexCommand>) {
        if !self.codex_ready || self.codex_busy || self.active_task_id.is_some() {
            return;
        }
        while let Some(task_id) = self.queue.pop_front() {
            let Some(task) = self.task_store.tasks.get_mut(&task_id) else {
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                            && let Some(task) = self.task_store.tasks.get_mut(&task_id)
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
                    if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                        .task_store
                        .tasks
                        .get(&task_id)
                        .and_then(|task| task.turn_id.as_deref())
                        == Some(turn_id.as_str())
                {
                    if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                        .task_store
                        .tasks
                        .get(&task_id)
                        .and_then(|task| task.turn_id.as_deref())
                        == Some(turn_id.as_str())
                {
                    if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                        .task_store
                        .tasks
                        .get(&task_id)
                        .and_then(|task| task.turn_id.as_deref())
                        .is_none_or(|known| known == turn_id);
                    if is_matching {
                        if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                            .task_store
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
            .task_store
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::Failed;
                    task.updated_at_ms = now_ms();
                }
                let _ = self.sessions.set_status(session_id, SessionStatus::Offline);
                self.push_event(Some(task_id), "task.worker_offline", message);
                self.worker_runtimes.remove(session_id);
            }
            CodexEvent::TurnStarted { turn_id, .. } => {
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
                    task.result.get_or_insert_with(String::new).push_str(&delta);
                    task.updated_at_ms = now_ms();
                }
            }
            CodexEvent::AssistantCompleted { text, .. } => {
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                    .task_store
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                    if let Some(session_id) = self.task_store.tasks[&task_id].session_id.clone() {
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
                if self.task_store.tasks[&task_id].cancel_requested {
                    return;
                }
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::RunningWebGpt;
                    task.updated_at_ms = now_ms();
                }
                if let Some(session_id) = self.task_store.tasks[&task_id].session_id.clone() {
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
                if self.task_store.tasks[&task_id].cancel_requested {
                    return;
                }
                let mut activity_event = None;
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                let cancelled = self.task_store.tasks[&task_id].cancel_requested;
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
                if let Some(session_id) = self.task_store.tasks[&task_id].session_id.clone() {
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
                if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
                    task.status = OrchestratorTaskStatus::Cancelled;
                    task.updated_at_ms = now_ms();
                }
                if let Some(session_id) = self.task_store.tasks[&task_id].session_id.clone() {
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
        let task = self.task_store.tasks.get(active_task_id)?;
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
        let task = self.task_store.tasks.get(task_id)?;
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
        let session_id = self.task_store.tasks[&task_id].session_id.clone();
        if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
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
        let cancelled = self.task_store.tasks[&task_id].cancel_requested;
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
        let cancelled = self.task_store.tasks[&task_id].cancel_requested;
        self.finish_web_worker_failure(task_id, message, cancelled);
    }

    fn finish_web_worker_failure(&mut self, task_id: String, message: String, cancelled: bool) {
        if let Some(task) = self.task_store.tasks.get_mut(&task_id) {
            task.status = if cancelled {
                OrchestratorTaskStatus::Cancelled
            } else {
                OrchestratorTaskStatus::Failed
            };
            task.updated_at_ms = now_ms();
        }
        if let Some(session_id) = self.task_store.tasks[&task_id].session_id.clone() {
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
        self.task_store
            .tasks
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
            .task_store
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
                .task_store
                .tasks
                .get(task_id)
                .and_then(|task| task.session_id.clone());
            if let Some(session_id) = session_id
                && let Some(runtime) = self.worker_runtimes.remove(&session_id)
            {
                runtime.interrupt();
            }
            if self.active_web_task_id.as_deref() == Some(task_id.as_str()) {
                if let Some(request) = self.task_store.tasks.get(task_id).and_then(|task| {
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
            self.task_store.tasks.remove(task_id);
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
            self.task_store.tasks.insert(task_id.clone(), task);
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
                "task_count": self.task_store.tasks.len(),
                "pending_chat": self.chat.pending_len(),
                "chat_count": self.chat.len(),
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
                    self.task_store
                        .tasks
                        .get(&id)
                        .map(|task| {
                            serde_json::to_value(task).expect("task serialization cannot fail")
                        })
                        .ok_or_else(|| format!("Unknown task: {id}"))
                })
            }
            "task.list" => Ok(serde_json::to_value(
                self.task_store.tasks.values().collect::<Vec<_>>(),
            )
            .expect("task list serialization cannot fail")),
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
                    .task_store
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
    fn bridge_chat_mailbox_preserves_fifo_release_and_event_contract() {
        let mut state = BridgeState::new(PathBuf::from("C:/repo"), "test-token".to_owned());
        let submitted = state
            .submit_chat(&json!({"text": "question", "reasoning_level": "high"}))
            .expect("submit chat");
        let request_id = submitted["id"].as_str().expect("request id").to_owned();

        let claimed = state.claim_pending_chat().expect("claim chat");
        assert_eq!(claimed["id"], request_id);
        state
            .release_chat(&json!({"request_id": request_id.clone()}))
            .expect("release chat");
        let reclaimed = state.claim_pending_chat().expect("reclaim chat");
        assert_eq!(reclaimed["id"], request_id);
        let answered = state
            .respond_chat(&json!({
                "request_id": request_id.clone(),
                "text": "answer"
            }))
            .expect("answer chat");
        assert_eq!(answered["status"], "answered");
        let cancelled_after_answer = state
            .cancel_chat(&json!({"request_id": request_id}))
            .expect("cancel answered chat");
        assert_eq!(cancelled_after_answer["status"], "answered");

        let event_names = state
            .task_store
            .events
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            event_names,
            [
                "chat.pending",
                "chat.claimed",
                "chat.released",
                "chat.claimed",
                "chat.answered",
                "chat.cancelled",
            ]
        );
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
            state.task_store.tasks[&task_id].status,
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
            state.task_store.tasks[&task_id].status,
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
            state.task_store.tasks[&task_id].status,
            OrchestratorTaskStatus::NeedsReview
        );
        state
            .approve_task(&json!({"task_id": task_id.clone()}))
            .unwrap();
        assert_eq!(
            state.task_store.tasks[&task_id].status,
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
            .task_store
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
            state.task_store.tasks[&task_id].status,
            OrchestratorTaskStatus::RunningWebGpt
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatProgress {
            correlation: correlation.clone(),
            text: Some("Working".to_owned()),
            activity: Some("Searching repository".to_owned()),
            thinking: false,
        });
        assert_eq!(
            state.task_store.tasks[&task_id].result.as_deref(),
            Some("Working")
        );
        assert_eq!(
            state.task_store.tasks[&task_id].tool_activity,
            vec!["Searching repository"]
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation,
            text: "Final evidence".to_owned(),
        });
        assert_eq!(
            state.task_store.tasks[&task_id].status,
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
            state.task_store.tasks[&task_id].status,
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
            .task_store
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
            state.task_store.tasks[&task_id].status,
            OrchestratorTaskStatus::Preparing
        );

        state.handle_web_worker_event(WebGptBrowserEvent::ChatSubmitted {
            correlation: valid.clone(),
        });
        assert_eq!(state.active_web_correlation, Some(valid.clone()));
        assert_eq!(
            state.task_store.tasks[&task_id].status,
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
        assert_eq!(state.task_store.tasks[&task_id].result, None);
        assert_eq!(state.active_web_task_id.as_deref(), Some(task_id.as_str()));

        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation: valid,
            text: "valid answer".to_owned(),
        });
        assert_eq!(
            state.task_store.tasks[&task_id].status,
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
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(second_session.as_str()))
            .unwrap()
            .id
            .clone();
        let first_task = state
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == first["id"].as_str())
            .unwrap()
            .id
            .clone();
        let second_request_id = format!("web-worker-{second_task}-r0");
        state
            .task_store
            .tasks
            .get_mut(&second_task)
            .unwrap()
            .turn_id = Some(second_request_id.clone());
        let wrong = WebGptTurnRequest::worker(
            "session-other".to_owned(),
            second_task.clone(),
            second_request_id.clone(),
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatQueueCancelled { request: wrong });
        assert_eq!(
            state.task_store.tasks[&second_task].status,
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
            state.task_store.tasks[&second_task].status,
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
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session.as_str()))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .task_store
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
            state.task_store.tasks[&first_task].status,
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
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(second_session))
            .unwrap()
            .id
            .clone();
        let third_task = state
            .task_store
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
            state.task_store.tasks[&second_task].status,
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
        assert!(state.task_store.tasks[&first_task].result.is_none());
        state.handle_web_worker_event(WebGptBrowserEvent::ChatCancelled {
            correlation: first_correlation.clone(),
        });
        assert_eq!(
            state.task_store.tasks[&first_task].status,
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
            state.task_store.tasks[&third_task].status,
            OrchestratorTaskStatus::Preparing
        );
        state.handle_web_worker_event(WebGptBrowserEvent::ChatAnswered {
            correlation: first_correlation,
            text: "late".to_owned(),
        });
        assert_eq!(
            state.task_store.tasks[&first_task].status,
            OrchestratorTaskStatus::Cancelled
        );
        assert_eq!(
            state.active_web_task_id.as_deref(),
            Some(second_task.as_str())
        );
        assert_eq!(
            state.task_store.tasks[&third_task].status,
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
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .task_store
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
            state.task_store.tasks[&first_task].status,
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
            state.task_store.tasks[&second_task].status,
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
            .task_store
            .tasks
            .values()
            .find(|task| task.session_id.as_deref() == Some(first_session))
            .unwrap()
            .id
            .clone();
        let second_task = state
            .task_store
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
            state.task_store.tasks[&first_task].status,
            OrchestratorTaskStatus::Preparing
        );
        assert_eq!(
            state.task_store.tasks[&second_task].status,
            OrchestratorTaskStatus::Preparing
        );
        assert!(state.drain_web_worker_commands().is_empty());
        assert!(state.task_store.events.iter().any(|event| {
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
            .task_store
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
            state.task_store.tasks[&task_id].status,
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
