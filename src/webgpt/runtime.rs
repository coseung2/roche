//! Native desktop adapter that polls the loopback Web GPT bridge.

use std::{
    collections::HashMap,
    sync::mpsc::{Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{transport::rpc_call, types::next_local_chat_id};

const UI_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
        let local_id = next_local_chat_id();
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
