use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::web_browser::SharedWebGptBrowser;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexThreadTarget {
    Current,
    New,
    Existing(String),
}

#[derive(Debug)]
pub(crate) enum CodexCommand {
    Send {
        text: String,
        attachments: Vec<PathBuf>,
        effort: String,
        model: Option<String>,
        target: CodexThreadTarget,
    },
    ReadThread {
        thread_id: String,
    },
    Interrupt,
    Shutdown,
}

#[derive(Clone)]
struct CodexEventSink {
    ui: Sender<CodexEvent>,
    orchestrator: Sender<CodexEvent>,
}

impl CodexEventSink {
    fn send(&self, event: CodexEvent) -> Result<(), ()> {
        let _ = self.orchestrator.send(event.clone());
        self.ui.send(event).map_err(|_| ())
    }
}

#[derive(Debug)]
enum Inbound {
    Message(Value),
    Stderr(String),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingRequest {
    Initialize,
    ThreadStart,
    ThreadResume(String),
    ThreadList,
    ThreadRead(String),
    TurnStart,
    Steer,
    Interrupt,
}

pub struct CodexRuntimeController {
    commands: Sender<CodexCommand>,
    events: Receiver<CodexEvent>,
}

#[derive(Debug)]
pub(crate) struct CodexWorkerRuntime {
    commands: Sender<CodexCommand>,
    events: Receiver<CodexEvent>,
}

impl CodexRuntimeController {
    pub fn spawn(project_root: PathBuf) -> Self {
        Self::spawn_with_web_browser(
            project_root,
            SharedWebGptBrowser::disabled("Web GPT browser was not attached to this runtime"),
        )
    }

    pub fn spawn_with_web_browser(project_root: PathBuf, web_browser: SharedWebGptBrowser) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (orchestrator_event_tx, orchestrator_event_rx) = mpsc::channel();
        let bridge_event_tx = event_tx.clone();
        let event_sink = CodexEventSink {
            ui: event_tx,
            orchestrator: orchestrator_event_tx,
        };
        let worker_root = project_root.clone();
        thread::Builder::new()
            .name("roche-codex-runtime".to_owned())
            .spawn(move || codex_worker(worker_root, command_rx, event_sink))
            .expect("failed to start Roche Codex runtime worker");
        if let Err(message) = crate::webgpt::spawn_orchestrator_bridge(
            project_root,
            command_tx.clone(),
            orchestrator_event_rx,
            web_browser,
        ) {
            let _ = bridge_event_tx.send(CodexEvent::Error(format!(
                "Web GPT bridge startup failed: {message}"
            )));
        }
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn send(&self, text: String, effort: String, model: Option<String>) {
        self.send_with_attachments(text, Vec::new(), effort, model);
    }

    pub fn send_with_attachments(
        &self,
        text: String,
        attachments: Vec<PathBuf>,
        effort: String,
        model: Option<String>,
    ) {
        let _ = self.commands.send(CodexCommand::Send {
            text,
            attachments,
            effort,
            model,
            target: CodexThreadTarget::Current,
        });
    }

    pub fn send_with_attachments_to_thread(
        &self,
        text: String,
        attachments: Vec<PathBuf>,
        effort: String,
        model: Option<String>,
        thread_id: Option<String>,
    ) {
        let target = thread_id
            .map(CodexThreadTarget::Existing)
            .unwrap_or(CodexThreadTarget::New);
        let _ = self.commands.send(CodexCommand::Send {
            text,
            attachments,
            effort,
            model,
            target,
        });
    }

    pub fn read_thread(&self, thread_id: String) {
        let _ = self.commands.send(CodexCommand::ReadThread { thread_id });
    }

    pub fn interrupt(&self) {
        let _ = self.commands.send(CodexCommand::Interrupt);
    }

    pub fn drain(&self) -> Vec<CodexEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for CodexRuntimeController {
    fn drop(&mut self) {
        let _ = self.commands.send(CodexCommand::Shutdown);
    }
}

impl CodexWorkerRuntime {
    pub(crate) fn spawn(project_root: PathBuf) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (discard_tx, _discard_rx) = mpsc::channel();
        let event_sink = CodexEventSink {
            ui: event_tx,
            orchestrator: discard_tx,
        };
        thread::Builder::new()
            .name("roche-codex-session-worker".to_owned())
            .spawn(move || codex_worker(project_root, command_rx, event_sink))
            .expect("failed to start Roche Codex session worker");
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub(crate) fn send(&self, text: String, effort: String, model: Option<String>) {
        let _ = self.commands.send(CodexCommand::Send {
            text,
            attachments: Vec::new(),
            effort,
            model,
            target: CodexThreadTarget::Current,
        });
    }

    pub(crate) fn interrupt(&self) {
        let _ = self.commands.send(CodexCommand::Interrupt);
    }

    pub(crate) fn drain(&self) -> Vec<CodexEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for CodexWorkerRuntime {
    fn drop(&mut self) {
        let _ = self.commands.send(CodexCommand::Shutdown);
    }
}

#[derive(Debug)]
struct QueuedMessage {
    text: String,
    attachments: Vec<PathBuf>,
    effort: String,
    model: Option<String>,
    target: CodexThreadTarget,
}

struct Worker {
    project_root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    inbound: Receiver<Inbound>,
    events: CodexEventSink,
    pending: HashMap<u64, PendingRequest>,
    queued_messages: VecDeque<QueuedMessage>,
    next_request_id: u64,
    initialized: bool,
    thread_start_pending: bool,
    thread_resume_pending: bool,
    turn_start_pending: bool,
    thread_id: Option<String>,
    active_turn_id: Option<String>,
    version: String,
}

impl Worker {
    fn start(project_root: PathBuf, events: CodexEventSink) -> Result<Self, String> {
        let codex_bin = resolve_codex_binary();
        let version =
            read_codex_version(&codex_bin).unwrap_or_else(|_| "codex app-server".to_owned());
        let mut command = codex_command(&codex_bin);
        command
            .arg("app-server")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start Codex app-server: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Codex app-server stdin was not available".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Codex app-server stdout was not available".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Codex app-server stderr was not available".to_owned())?;
        let (inbound_tx, inbound_rx) = mpsc::channel();
        spawn_stdout_reader(stdout, inbound_tx.clone());
        spawn_stderr_reader(stderr, inbound_tx);
        let mut worker = Self {
            project_root,
            child,
            stdin,
            inbound: inbound_rx,
            events,
            pending: HashMap::new(),
            queued_messages: VecDeque::new(),
            next_request_id: 1,
            initialized: false,
            thread_start_pending: false,
            thread_resume_pending: false,
            turn_start_pending: false,
            thread_id: None,
            active_turn_id: None,
            version,
        };
        worker.send_initialize()?;
        Ok(worker)
    }

    fn send_initialize(&mut self) -> Result<(), String> {
        let id = self.next_id();
        self.pending.insert(id, PendingRequest::Initialize);
        self.write(json!({
            "method": "initialize",
            "id": id,
            "params": {"clientInfo": {
                "name": "roche_workstation",
                "title": "Roche AI Workstation",
                "version": env!("CARGO_PKG_VERSION")
            }}
        }))
    }

    fn handle_command(&mut self, command: CodexCommand) -> bool {
        let result = match command {
            CodexCommand::Send {
                text,
                attachments,
                effort,
                model,
                target,
            } => self.queue_or_send(text, attachments, effort, model, target),
            CodexCommand::ReadThread { thread_id } => self.read_thread(thread_id),
            CodexCommand::Interrupt => self.interrupt(),
            CodexCommand::Shutdown => return false,
        };
        if let Err(error) = result {
            let _ = self.events.send(CodexEvent::Error(error));
        }
        true
    }

    fn queue_or_send(
        &mut self,
        text: String,
        attachments: Vec<PathBuf>,
        effort: String,
        model: Option<String>,
        target: CodexThreadTarget,
    ) -> Result<(), String> {
        let message = QueuedMessage {
            text,
            attachments,
            effort,
            model,
            target,
        };
        if !self.initialized
            || self.thread_start_pending
            || self.thread_resume_pending
            || self.turn_start_pending
        {
            self.queued_messages.push_back(message);
            return Ok(());
        }
        self.dispatch_message(message)
    }

    fn dispatch_message(&mut self, mut message: QueuedMessage) -> Result<(), String> {
        match &message.target {
            CodexThreadTarget::Current => {
                if self.thread_id.is_none() {
                    self.queued_messages.push_front(message);
                    return self.start_thread();
                }
                if self.active_turn_id.is_some() {
                    return self.steer(message.text, message.attachments);
                }
            }
            CodexThreadTarget::New => {
                if self.active_turn_id.is_some() {
                    self.queued_messages.push_back(message);
                    return Ok(());
                }
                self.thread_id = None;
                message.target = CodexThreadTarget::Current;
                self.queued_messages.push_front(message);
                return self.start_thread();
            }
            CodexThreadTarget::Existing(thread_id) => {
                if self.thread_id.as_deref() != Some(thread_id.as_str()) {
                    if self.active_turn_id.is_some() {
                        self.queued_messages.push_back(message);
                        return Ok(());
                    }
                    let thread_id = thread_id.clone();
                    self.queued_messages.push_front(message);
                    return self.resume_thread(thread_id);
                }
                if self.active_turn_id.is_some() {
                    return self.steer(message.text, message.attachments);
                }
            }
        }
        self.start_turn(
            message.text,
            message.attachments,
            message.effort,
            message.model,
        )
    }

    fn start_thread(&mut self) -> Result<(), String> {
        if self.thread_start_pending || self.thread_resume_pending || self.thread_id.is_some() {
            return Ok(());
        }
        let id = self.next_id();
        self.pending.insert(id, PendingRequest::ThreadStart);
        self.thread_start_pending = true;
        self.write(json!({
            "method": "thread/start",
            "id": id,
            "params": {
                "cwd": self.project_root.to_string_lossy(),
                "approvalPolicy": "never"
            }
        }))
    }

    fn resume_thread(&mut self, thread_id: String) -> Result<(), String> {
        if self.thread_start_pending || self.thread_resume_pending {
            return Ok(());
        }
        if self.active_turn_id.is_some() {
            return Err("Cannot switch Codex threads while a turn is active".to_owned());
        }
        let id = self.next_id();
        self.pending
            .insert(id, PendingRequest::ThreadResume(thread_id.clone()));
        self.thread_resume_pending = true;
        self.write(json!({
            "method": "thread/resume",
            "id": id,
            "params": {
                "threadId": thread_id,
                "cwd": self.project_root.to_string_lossy(),
                "approvalPolicy": "never",
                "excludeTurns": true
            }
        }))
    }

    fn list_threads(&mut self) -> Result<(), String> {
        let id = self.next_id();
        self.pending.insert(id, PendingRequest::ThreadList);
        self.write(json!({
            "method": "thread/list",
            "id": id,
            "params": {
                "cwd": self.project_root.to_string_lossy(),
                "limit": 200,
                "sortKey": "updated_at",
                "sortDirection": "desc"
            }
        }))
    }

    fn read_thread(&mut self, thread_id: String) -> Result<(), String> {
        if !self.initialized {
            return Err("Codex is not initialized yet".to_owned());
        }
        let id = self.next_id();
        self.pending
            .insert(id, PendingRequest::ThreadRead(thread_id.clone()));
        self.write(json!({
            "method": "thread/read",
            "id": id,
            "params": {
                "threadId": thread_id,
                "includeTurns": true
            }
        }))
    }

    fn dispatch_next_queued_turn(&mut self) -> Result<(), String> {
        if self.active_turn_id.is_some()
            || self.thread_start_pending
            || self.thread_resume_pending
            || self.turn_start_pending
        {
            return Ok(());
        }
        let Some(message) = self.queued_messages.pop_front() else {
            return Ok(());
        };
        self.dispatch_message(message)
    }

    fn start_turn(
        &mut self,
        text: String,
        attachments: Vec<PathBuf>,
        effort: String,
        model: Option<String>,
    ) -> Result<(), String> {
        let thread_id = self
            .thread_id
            .clone()
            .ok_or_else(|| "Codex thread has not started yet".to_owned())?;
        let id = self.next_id();
        self.pending.insert(id, PendingRequest::TurnStart);
        self.turn_start_pending = true;
        self.write(json!({
            "method": "turn/start",
            "id": id,
            "params": turn_start_params(&thread_id, text, &attachments, effort, model)
        }))
    }

    fn steer(&mut self, text: String, attachments: Vec<PathBuf>) -> Result<(), String> {
        let thread_id = self
            .thread_id
            .clone()
            .ok_or_else(|| "Codex thread has not started yet".to_owned())?;
        let turn_id = self
            .active_turn_id
            .clone()
            .ok_or_else(|| "Codex turn is not active".to_owned())?;
        let id = self.next_id();
        self.pending.insert(id, PendingRequest::Steer);
        let input = codex_user_input(text, &attachments);
        self.write(json!({
            "method": "turn/steer",
            "id": id,
            "params": {
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": input
            }
        }))
    }

    fn interrupt(&mut self) -> Result<(), String> {
        let (Some(thread_id), Some(turn_id)) =
            (self.thread_id.clone(), self.active_turn_id.clone())
        else {
            let _ = self.events.send(CodexEvent::Notice(
                "Codex has no active turn to stop".to_owned(),
            ));
            return Ok(());
        };
        let id = self.next_id();
        self.pending.insert(id, PendingRequest::Interrupt);
        self.write(json!({
            "method": "turn/interrupt",
            "id": id,
            "params": {"threadId": thread_id, "turnId": turn_id}
        }))
    }

    fn handle_inbound(&mut self, inbound: Inbound) -> Result<bool, String> {
        match inbound {
            Inbound::Message(message) => self.handle_message(message)?,
            Inbound::Stderr(line) => {
                if line.contains("ERROR") || line.contains("error") {
                    let _ = self
                        .events
                        .send(CodexEvent::Notice(format!("Codex: {line}")));
                }
            }
            Inbound::Closed => return Ok(false),
        }
        Ok(true)
    }

    fn handle_message(&mut self, message: Value) -> Result<(), String> {
        if let Some(id) = message.get("id").and_then(Value::as_u64)
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            return self.handle_response(id, message);
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        if message.get("id").is_some() {
            let _ = self.events.send(CodexEvent::Error(format!(
                "Codex requested unsupported client action: {method}"
            )));
            return Ok(());
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        self.handle_notification(method, &params)
    }

    fn handle_response(&mut self, id: u64, message: Value) -> Result<(), String> {
        let pending = self.pending.remove(&id);
        if let Some(error) = message.get("error") {
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown Codex app-server error")
                .to_owned();
            match pending.as_ref() {
                Some(PendingRequest::ThreadStart) => {
                    self.thread_start_pending = false;
                }
                Some(PendingRequest::ThreadResume(thread_id)) => {
                    self.thread_resume_pending = false;
                    self.thread_id = None;
                    for queued in &mut self.queued_messages {
                        if matches!(
                            &queued.target,
                            CodexThreadTarget::Existing(target) if target == thread_id
                        ) {
                            queued.target = CodexThreadTarget::Current;
                        }
                    }
                    let _ = self.events.send(CodexEvent::ThreadResumeFailed {
                        thread_id: thread_id.clone(),
                        message: detail,
                    });
                    self.dispatch_next_queued_turn()?;
                    return Ok(());
                }
                Some(PendingRequest::ThreadList) => {
                    let _ = self.events.send(CodexEvent::Notice(format!(
                        "Codex stored thread list unavailable: {detail}"
                    )));
                    return Ok(());
                }
                Some(PendingRequest::ThreadRead(thread_id)) => {
                    let _ = self.events.send(CodexEvent::Notice(format!(
                        "Could not read Codex thread {thread_id}: {detail}"
                    )));
                    return Ok(());
                }
                Some(PendingRequest::TurnStart) => {
                    self.turn_start_pending = false;
                }
                _ => {}
            }
            return Err(format!("Codex request failed: {detail}"));
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        match pending {
            Some(PendingRequest::Initialize) => {
                self.initialized = true;
                self.write(json!({"method": "initialized", "params": {}}))?;
                let version = self.version.clone();
                let _ = self
                    .events
                    .send(CodexEvent::Connection(CodexConnection::Ready { version }));
                self.publish_catalog();
                self.list_threads()?;
                self.dispatch_next_queued_turn()?;
            }
            Some(PendingRequest::ThreadStart) => {
                self.thread_start_pending = false;
                let thread_id = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "thread/start response did not contain thread.id".to_owned())?
                    .to_owned();
                let model = result
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.thread_id = Some(thread_id.clone());
                let _ = self
                    .events
                    .send(CodexEvent::ThreadStarted { thread_id, model });
                self.dispatch_next_queued_turn()?;
            }
            Some(PendingRequest::ThreadResume(requested_thread_id)) => {
                self.thread_resume_pending = false;
                let thread_id = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .unwrap_or(requested_thread_id.as_str())
                    .to_owned();
                let model = result
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.thread_id = Some(thread_id.clone());
                let _ = self
                    .events
                    .send(CodexEvent::ThreadStarted { thread_id, model });
                self.dispatch_next_queued_turn()?;
            }
            Some(PendingRequest::ThreadList) => {
                let threads = codex_stored_threads_from_result(&result);
                let _ = self.events.send(CodexEvent::StoredThreads { threads });
            }
            Some(PendingRequest::ThreadRead(requested_thread_id)) => {
                let thread_id = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .unwrap_or(requested_thread_id.as_str())
                    .to_owned();
                let messages = codex_history_from_result(&result);
                let _ = self.events.send(CodexEvent::ThreadHistoryLoaded {
                    thread_id,
                    messages,
                });
            }
            Some(PendingRequest::TurnStart) => {
                self.turn_start_pending = false;
                if let Some(turn_id) = result.pointer("/turn/id").and_then(Value::as_str) {
                    self.active_turn_id = Some(turn_id.to_owned());
                }
            }
            Some(PendingRequest::Steer) => {
                let _ = self.events.send(CodexEvent::Notice(
                    "Follow-up sent to the active Codex turn".to_owned(),
                ));
            }
            Some(PendingRequest::Interrupt) => {
                let _ = self
                    .events
                    .send(CodexEvent::Notice("Stop requested for Codex".to_owned()));
            }
            None => {}
        }
        Ok(())
    }

    fn publish_catalog(&mut self) {
        match read_codex_catalog_models() {
            Ok((source, models)) => {
                let _ = self
                    .events
                    .send(CodexEvent::CatalogLoaded { source, models });
            }
            Err(message) => {
                let _ = self
                    .events
                    .send(CodexEvent::Notice(format!("Codex catalog: {message}")));
            }
        }
    }

    fn handle_notification(&mut self, method: &str, params: &Value) -> Result<(), String> {
        match method {
            "thread/started" => {
                if let Some(thread_id) = params.pointer("/thread/id").and_then(Value::as_str) {
                    self.thread_id = Some(thread_id.to_owned());
                }
            }
            "turn/started" => {
                if let (Some(thread_id), Some(turn_id)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params.pointer("/turn/id").and_then(Value::as_str),
                ) {
                    self.active_turn_id = Some(turn_id.to_owned());
                    let _ = self.events.send(CodexEvent::TurnStarted {
                        thread_id: thread_id.to_owned(),
                        turn_id: turn_id.to_owned(),
                    });
                }
            }
            "item/agentMessage/delta" => {
                if let (Some(thread_id), Some(turn_id), Some(delta)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params.get("turnId").and_then(Value::as_str),
                    params.get("delta").and_then(Value::as_str),
                ) {
                    let _ = self.events.send(CodexEvent::AssistantDelta {
                        thread_id: thread_id.to_owned(),
                        turn_id: turn_id.to_owned(),
                        delta: delta.to_owned(),
                    });
                }
            }
            "item/started" | "item/completed" => self.handle_item_event(method, params),
            "turn/completed" => {
                if let (Some(thread_id), Some(turn_id)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params.pointer("/turn/id").and_then(Value::as_str),
                ) {
                    let status = params
                        .pointer("/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .to_owned();
                    if self.active_turn_id.as_deref() == Some(turn_id) {
                        self.active_turn_id = None;
                    }
                    let _ = self.events.send(CodexEvent::TurnCompleted {
                        thread_id: thread_id.to_owned(),
                        turn_id: turn_id.to_owned(),
                        status,
                    });
                    self.dispatch_next_queued_turn()?;
                }
            }
            "error" => {
                let message = params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex reported an error")
                    .to_owned();
                let _ = self.events.send(CodexEvent::Error(message));
            }
            "warning" => {
                if let Some(message) = params.get("message").and_then(Value::as_str) {
                    let _ = self.events.send(CodexEvent::Notice(message.to_owned()));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_item_event(&self, method: &str, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            return;
        };
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if item_type == "agentMessage" && method == "item/completed" {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                let _ = self.events.send(CodexEvent::AssistantCompleted {
                    thread_id: thread_id.to_owned(),
                    turn_id: turn_id.to_owned(),
                    text: text.to_owned(),
                });
            }
            return;
        }
        if let Some(activity) = codex_activity_from_item(method, item, turn_id) {
            let _ = self.events.send(CodexEvent::Activity {
                thread_id: thread_id.to_owned(),
                turn_id: turn_id.to_owned(),
                activity,
            });
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn write(&mut self, message: Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.stdin, &message)
            .map_err(|error| format!("Could not encode Codex request: {error}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|error| format!("Could not write to Codex app-server: {error}"))?;
        self.stdin
            .flush()
            .map_err(|error| format!("Could not flush Codex app-server stdin: {error}"))
    }
}

fn codex_stored_threads_from_result(result: &Value) -> Vec<CodexStoredThread> {
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|thread| {
            let thread_id = thread.get("id")?.as_str()?.to_owned();
            let cwd = thread.get("cwd")?.as_str().map(PathBuf::from)?;
            let name = thread
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let preview = thread
                .get("preview")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            Some(CodexStoredThread {
                thread_id,
                name,
                preview,
                cwd,
                parent_thread_id: thread
                    .get("parentThreadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                created_at: thread
                    .get("createdAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                updated_at: thread
                    .get("updatedAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn codex_history_from_result(result: &Value) -> Vec<CodexHistoryMessage> {
    let Some(turns) = result.pointer("/thread/turns").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for turn in turns {
        let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("userMessage") => {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        messages.push(CodexHistoryMessage {
                            role: CodexHistoryRole::User,
                            text,
                            turn_id: turn_id.clone(),
                        });
                    }
                }
                Some("agentMessage") => {
                    if item.get("phase").and_then(Value::as_str) == Some("commentary") {
                        continue;
                    }
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    if !text.is_empty() {
                        messages.push(CodexHistoryMessage {
                            role: CodexHistoryRole::Assistant,
                            text,
                            turn_id: turn_id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    messages
}

fn codex_activity_from_item(method: &str, item: &Value, turn_id: &str) -> Option<CodexActivity> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    let phase = match item.get("status").and_then(Value::as_str) {
        Some(status)
            if matches!(
                status.to_ascii_lowercase().as_str(),
                "failed" | "declined" | "error" | "cancelled" | "canceled"
            ) =>
        {
            CodexActivityPhase::Failed
        }
        _ if method == "item/started" => CodexActivityPhase::Running,
        _ => CodexActivityPhase::Completed,
    };
    let (kind, title, detail, worker_thread_id, worker_status) = match item_type {
        "commandExecution" => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("command")
                .to_owned();
            (
                CodexActivityKind::Terminal,
                "명령 실행".to_owned(),
                command,
                None,
                None,
            )
        }
        "fileChange" => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let paths = changes
                .iter()
                .filter_map(|change| {
                    change
                        .get("path")
                        .or_else(|| change.get("filePath"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>();
            let count = changes.len();
            let detail = if paths.is_empty() {
                format!("{count}개 파일")
            } else {
                paths.join("\n")
            };
            (
                CodexActivityKind::FileChange,
                format!("{count}개 파일 변경"),
                detail,
                None,
                None,
            )
        }
        "mcpToolCall" | "dynamicToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
            let tool = item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let title = if server == "mcp" {
                tool.to_owned()
            } else {
                format!("{server}/{tool}")
            };
            (
                CodexActivityKind::ToolCall,
                title.clone(),
                title,
                None,
                None,
            )
        }
        "webSearch" => {
            let query = item
                .get("query")
                .or_else(|| item.get("searchQuery"))
                .and_then(Value::as_str)
                .unwrap_or("웹 검색")
                .to_owned();
            (
                CodexActivityKind::WebSearch,
                "웹 검색".to_owned(),
                query,
                None,
                None,
            )
        }
        "collabToolCall" | "collabAgentToolCall" => {
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("worker");
            let worker_thread_id = item
                .get("newThreadId")
                .or_else(|| item.get("receiverThreadId"))
                .or_else(|| item.get("new_thread_id"))
                .or_else(|| item.get("receiver_thread_id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let worker_status = item
                .get("agentStatus")
                .or_else(|| item.get("agent_status"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let prompt = item
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(tool)
                .to_owned();
            let title = match tool {
                "spawn_agent" => "워커 생성",
                "send_input" => "워커 입력",
                "resume_agent" => "워커 재개",
                "wait" => "워커 대기",
                "close_agent" => "워커 종료",
                _ => "워커 작업",
            }
            .to_owned();
            (
                CodexActivityKind::Worker,
                title,
                prompt,
                worker_thread_id,
                worker_status,
            )
        }
        _ => return None,
    };
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{turn_id}:{item_type}:{title}:{detail}"));
    Some(CodexActivity {
        item_id,
        kind,
        phase,
        title,
        detail,
        worker_thread_id,
        worker_status,
    })
}

fn codex_user_input(text: String, attachments: &[PathBuf]) -> Vec<Value> {
    let mut input = Vec::with_capacity(1 + attachments.len());
    if !text.is_empty() {
        input.push(json!({"type": "text", "text": text, "textElements": []}));
    }
    for path in attachments {
        let path_text = path.to_string_lossy().into_owned();
        if is_image_attachment(path) {
            input.push(json!({"type": "localImage", "path": path_text}));
        } else {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("attachment")
                .to_owned();
            input.push(json!({"type": "mention", "name": name, "path": path_text}));
        }
    }
    input
}

fn is_image_attachment(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| {
            matches!(
                extension.as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
}

fn turn_start_params(
    thread_id: &str,
    text: String,
    attachments: &[PathBuf],
    effort: String,
    model: Option<String>,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": codex_user_input(text, attachments),
        "effort": effort
    });
    if let Some(model) = model {
        params["model"] = Value::String(model);
    }
    params
}

fn codex_worker(project_root: PathBuf, commands: Receiver<CodexCommand>, events: CodexEventSink) {
    let _ = events.send(CodexEvent::Connection(CodexConnection::Starting));
    let mut worker = match Worker::start(project_root, events.clone()) {
        Ok(worker) => worker,
        Err(message) => {
            let _ = events.send(CodexEvent::Connection(CodexConnection::Offline { message }));
            return;
        }
    };
    let mut running = true;
    while running {
        while let Ok(inbound) = worker.inbound.try_recv() {
            match worker.handle_inbound(inbound) {
                Ok(keep_running) => running &= keep_running,
                Err(message) => {
                    let _ = worker.events.send(CodexEvent::Error(message));
                }
            }
        }
        if !running {
            break;
        }
        match commands.recv_timeout(Duration::from_millis(40)) {
            Ok(command) => running = worker.handle_command(command),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        match worker.child.try_wait() {
            Ok(Some(status)) => {
                let _ = worker
                    .events
                    .send(CodexEvent::Connection(CodexConnection::Offline {
                        message: format!("Codex app-server exited with {status}"),
                    }));
                break;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = worker.events.send(CodexEvent::Error(format!(
                    "Could not inspect Codex app-server: {error}"
                )));
            }
        }
    }
    let _ = worker.child.kill();
    let _ = worker.child.wait();
}

fn resolve_codex_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("ROCHE_CODEX_BIN") {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    {
        if let Ok(output) = Command::new("where.exe").arg("codex").output()
            && output.status.success()
        {
            let output = String::from_utf8_lossy(&output.stdout);
            let candidates = output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            if let Some(path) = candidates.iter().find(|path| {
                path.extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            }) {
                return path.clone();
            }
            if let Some(path) = candidates.iter().find(|path| {
                path.extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("cmd")
                            || extension.eq_ignore_ascii_case("bat")
                    })
            }) {
                return path.clone();
            }
            if let Some(path) = candidates.first() {
                return path.clone();
            }
        }
    }
    PathBuf::from("codex")
}

fn codex_command(binary: &Path) -> Command {
    #[cfg(windows)]
    if binary
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "cmd" | "bat"))
    {
        let mut command = Command::new("cmd.exe");
        command.arg("/D").arg("/C").arg(binary);
        return command;
    }
    Command::new(binary)
}

fn read_codex_version(binary: &Path) -> Result<String, String> {
    let output = codex_command(binary)
        .arg("--version")
        .output()
        .map_err(|error| format!("Could not read Codex version: {error}"))?;
    if !output.status.success() {
        return Err(format!("Codex --version exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let profile = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_default();
            profile.join(".codex")
        })
}

fn configured_model_catalog_path(config_toml: &str) -> Option<PathBuf> {
    config_toml.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("model_catalog_json")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let value = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
        let value = value
            .strip_suffix('"')
            .or_else(|| value.strip_suffix('\''))?;
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
}

fn parse_catalog_models(root: &Value) -> Vec<CodexCatalogModel> {
    let Some(models) = root.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut catalog = models
        .iter()
        .filter_map(|entry| {
            if let Some(slug) = entry.as_str() {
                return Some(CodexCatalogModel {
                    slug: slug.to_owned(),
                    display_name: slug.to_owned(),
                    description: None,
                    default_reasoning_level: None,
                    supported_reasoning_levels: Vec::new(),
                    priority: None,
                });
            }
            let slug = entry
                .get("slug")
                .or_else(|| entry.get("id"))
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)?;
            let display_name = entry
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(slug);
            Some(CodexCatalogModel {
                slug: slug.to_owned(),
                display_name: display_name.to_owned(),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                default_reasoning_level: entry
                    .get("default_reasoning_level")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                supported_reasoning_levels: entry
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| {
                                let effort = level.get("effort").and_then(Value::as_str)?;
                                Some(CodexReasoningLevel {
                                    effort: effort.to_owned(),
                                    description: level
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                priority: entry.get("priority").and_then(Value::as_i64),
            })
        })
        .collect::<Vec<_>>();
    catalog.sort_by_key(|model| std::cmp::Reverse(model.priority));
    catalog
}

fn read_codex_catalog_models() -> Result<(String, Vec<CodexCatalogModel>), String> {
    let home = codex_home();
    let configured = std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| configured_model_catalog_path(&text));

    let mut candidates = Vec::new();
    if let Some(path) = configured {
        candidates.push(path);
    }
    for name in ["opencodex-catalog.json", "models_cache.json"] {
        candidates.push(home.join(name));
    }
    for name in ["codex-plus-opencode-go.json", "opencode-go.json"] {
        candidates.push(home.join("model-catalogs").join(name));
    }

    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let models = parse_catalog_models(&root);
        if models.is_empty() {
            continue;
        }
        let source = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("codex catalog")
            .to_owned();
        return Ok((source, models));
    }

    Err(format!(
        "no readable model catalog under {}",
        home.display()
    ))
}

fn spawn_stdout_reader(stdout: impl std::io::Read + Send + 'static, tx: Sender<Inbound>) {
    thread::Builder::new()
        .name("roche-codex-stdout".to_owned())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(message) => {
                            let _ = tx.send(Inbound::Message(message));
                        }
                        Err(error) => {
                            let _ = tx.send(Inbound::Stderr(format!(
                                "Could not parse app-server JSONL: {error}: {line}"
                            )));
                        }
                    },
                    Err(error) => {
                        let _ = tx.send(Inbound::Stderr(format!(
                            "Could not read Codex stdout: {error}"
                        )));
                        break;
                    }
                }
            }
            let _ = tx.send(Inbound::Closed);
        })
        .expect("failed to start Codex stdout reader");
}

fn spawn_stderr_reader(stderr: impl std::io::Read + Send + 'static, tx: Sender<Inbound>) {
    thread::Builder::new()
        .name("roche-codex-stderr".to_owned())
        .spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => {
                        let _ = tx.send(Inbound::Stderr(line));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .expect("failed to start Codex stderr reader");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_text_input_matches_app_server_v2_shape() {
        let input = json!({"type": "text", "text": "hello", "textElements": []});
        assert_eq!(input["type"], "text");
        assert_eq!(input["text"], "hello");
        assert_eq!(input["textElements"], json!([]));
    }

    #[test]
    fn turn_start_params_include_selected_model_override() {
        let params = turn_start_params(
            "thread-1",
            "hello".to_owned(),
            &[],
            "high".to_owned(),
            Some("gpt-5.6-sol".to_owned()),
        );
        assert_eq!(params["threadId"], "thread-1");
        assert_eq!(params["effort"], "high");
        assert_eq!(params["model"], "gpt-5.6-sol");
        assert_eq!(params["input"][0]["text"], "hello");
    }

    #[test]
    fn turn_start_params_omit_model_for_configured_default() {
        let params = turn_start_params("thread-1", "hello".to_owned(), &[], "low".to_owned(), None);
        assert!(params.get("model").is_none());
    }

    #[test]
    fn turn_start_params_encode_local_images_and_file_mentions() {
        let attachments = vec![
            PathBuf::from(r"C:\tmp\screen.png"),
            PathBuf::from(r"C:\tmp\notes.pdf"),
        ];
        let params = turn_start_params(
            "thread-1",
            "check these".to_owned(),
            &attachments,
            "high".to_owned(),
            None,
        );
        assert_eq!(params["input"][1]["type"], "localImage");
        assert_eq!(params["input"][1]["path"], r"C:\tmp\screen.png");
        assert_eq!(params["input"][2]["type"], "mention");
        assert_eq!(params["input"][2]["name"], "notes.pdf");
    }

    #[test]
    fn catalog_parses_object_and_string_entries() {
        let root = json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6-Sol",
                    "description": "Reasoning model",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": [
                        {"effort": "medium", "description": "Balanced"},
                        {"effort": "high", "description": "Deep"}
                    ],
                    "priority": 10
                },
                {"id": "opencode-go/deepseek-v4-flash"},
                "xai/grok-4.6"
            ]
        });
        let models = parse_catalog_models(&root);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].slug, "gpt-5.6-sol");
        assert_eq!(models[0].display_name, "GPT-5.6-Sol");
        assert_eq!(models[0].default_reasoning_level.as_deref(), Some("high"));
        assert_eq!(models[0].supported_reasoning_levels.len(), 2);
        assert_eq!(
            models[0].supported_reasoning_levels[1]
                .description
                .as_deref(),
            Some("Deep")
        );
        assert_eq!(models[1].display_name, "opencode-go/deepseek-v4-flash");
        assert_eq!(models[2].slug, "xai/grok-4.6");
    }

    #[test]
    fn activity_classification_keeps_kind_separate_from_phase() {
        let terminal = json!({
            "id": "item-terminal",
            "type": "commandExecution",
            "status": "inProgress",
            "command": "git status --short"
        });
        let started = codex_activity_from_item("item/started", &terminal, "turn-1").unwrap();
        assert_eq!(started.item_id, "item-terminal");
        assert_eq!(started.kind, CodexActivityKind::Terminal);
        assert_eq!(started.phase, CodexActivityPhase::Running);
        assert_eq!(started.detail, "git status --short");

        let completed = codex_activity_from_item("item/completed", &terminal, "turn-1").unwrap();
        assert_eq!(completed.item_id, started.item_id);
        assert_eq!(completed.kind, CodexActivityKind::Terminal);
        assert_eq!(completed.phase, CodexActivityPhase::Completed);

        let tool = json!({
            "id": "item-tool",
            "type": "mcpToolCall",
            "server": "roche",
            "tool": "read",
            "status": "completed"
        });
        let tool = codex_activity_from_item("item/completed", &tool, "turn-1").unwrap();
        assert_eq!(tool.kind, CodexActivityKind::ToolCall);
        assert_eq!(tool.title, "roche/read");
    }

    #[test]
    fn collab_tool_call_exposes_worker_thread_identity() {
        let started = json!({
            "id": "item-worker",
            "type": "collabToolCall",
            "tool": "spawn_agent",
            "status": "inProgress",
            "prompt": "Inspect the Web GPT bridge"
        });
        let started = codex_activity_from_item("item/started", &started, "turn-1").unwrap();
        assert_eq!(started.kind, CodexActivityKind::Worker);
        assert_eq!(started.phase, CodexActivityPhase::Running);
        assert_eq!(started.title, "워커 생성");
        assert!(started.worker_thread_id.is_none());

        let completed = json!({
            "id": "item-worker",
            "type": "collabToolCall",
            "tool": "spawn_agent",
            "status": "completed",
            "prompt": "Inspect the Web GPT bridge",
            "newThreadId": "thread-worker-1",
            "agentStatus": "running"
        });
        let completed = codex_activity_from_item("item/completed", &completed, "turn-1").unwrap();
        assert_eq!(completed.item_id, started.item_id);
        assert_eq!(
            completed.worker_thread_id.as_deref(),
            Some("thread-worker-1")
        );
        assert_eq!(completed.worker_status.as_deref(), Some("running"));
    }

    #[test]
    fn non_activity_items_are_not_mislabeled_as_tools() {
        let reasoning = json!({"id": "reasoning-1", "type": "reasoning", "text": "hidden"});
        assert!(codex_activity_from_item("item/completed", &reasoning, "turn-1").is_none());

        let assistant = json!({"id": "message-1", "type": "agentMessage", "text": "hello"});
        assert!(codex_activity_from_item("item/completed", &assistant, "turn-1").is_none());
    }

    #[test]
    fn config_model_catalog_path_is_parsed() {
        let toml = "model = \"x\"\nmodel_catalog_json = \"C:\\\\Users\\\\.codex\\\\opencodex-catalog.json\"\n";
        let path = configured_model_catalog_path(toml).expect("configured path");
        assert_eq!(
            path,
            PathBuf::from(r"C:\Users\.codex\opencodex-catalog.json")
        );
    }
}
