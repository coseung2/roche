//! Codex app-server child process, protocol state machine, and reader threads.

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

use serde_json::{Value, json};

use super::{
    catalog::read_codex_catalog_models,
    controller::{CodexCommand, CodexEventSink, CodexThreadTarget, Inbound, PendingRequest},
    protocol::*,
    types::{CodexConnection, CodexEvent},
};

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

pub(super) fn codex_worker(
    project_root: PathBuf,
    commands: Receiver<CodexCommand>,
    events: CodexEventSink,
) {
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
