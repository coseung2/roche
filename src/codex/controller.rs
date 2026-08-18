//! Public desktop controller, worker handles, and internal command/event channels.

use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use serde_json::Value;

use crate::web_browser::SharedWebGptBrowser;

use super::{types::CodexEvent, worker::codex_worker};

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
pub(super) struct CodexEventSink {
    pub(super) ui: Sender<CodexEvent>,
    pub(super) orchestrator: Sender<CodexEvent>,
}

impl CodexEventSink {
    pub(super) fn send(&self, event: CodexEvent) -> Result<(), ()> {
        let _ = self.orchestrator.send(event.clone());
        self.ui.send(event).map_err(|_| ())
    }
}

#[derive(Debug)]
pub(super) enum Inbound {
    Message(Value),
    Stderr(String),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingRequest {
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
