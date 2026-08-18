//! Public Web GPT browser façade and host-process entrypoint.

use std::{
    io::{BufRead, BufReader, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::web_browser_protocol::{WebGptTurnCorrelation, WebGptTurnRequest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebGptBrowserState {
    Starting,
    LoginRequired,
    LoggedIn,
    Offline(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebGptBrowserEvent {
    State(WebGptBrowserState),
    WakeSubmitted {
        request_id: String,
    },
    ChatSubmitted {
        correlation: WebGptTurnCorrelation,
    },
    ChatProgress {
        correlation: WebGptTurnCorrelation,
        text: Option<String>,
        activity: Option<String>,
        thinking: bool,
    },
    ChatAnswered {
        correlation: WebGptTurnCorrelation,
        text: String,
    },
    ChatCancelled {
        correlation: WebGptTurnCorrelation,
    },
    ChatFailed {
        correlation: WebGptTurnCorrelation,
        message: String,
    },
    /// A not-yet-leased queue item was explicitly cancelled/removed.
    ChatQueueCancelled {
        request: WebGptTurnRequest,
    },
    Error(String),
}

fn cancel_script_event(
    value: &str,
    correlation: &WebGptTurnCorrelation,
) -> Option<WebGptBrowserEvent> {
    match value.trim().trim_matches('"') {
        "cancelled" => Some(WebGptBrowserEvent::ChatCancelled {
            correlation: correlation.clone(),
        }),
        "pending" => None,
        _ => Some(WebGptBrowserEvent::Error(format!(
            "Web GPT request {} was not the active browser turn",
            correlation.request_id
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BrowserAttachment {
    name: String,
    mime: String,
    data_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BrowserHostCommand {
    ShowLogin,
    Hide,
    Wake {
        request_id: String,
    },
    Chat {
        correlation: WebGptTurnCorrelation,
        text: String,
        attachments: Vec<BrowserAttachment>,
    },
    Cancel {
        correlation: WebGptTurnCorrelation,
    },
    Reload,
    Shutdown,
}

mod host;
#[cfg(windows)]
mod process_transport;
mod runtime;
#[cfg(windows)]
mod scripts;

pub use runtime::SharedWebGptBrowser;

fn browser_attachments_from_paths(paths: &[std::path::PathBuf]) -> Vec<BrowserAttachment> {
    paths
        .iter()
        .filter_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())?;
            let mime = match path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                "txt" | "md" | "rs" | "toml" | "json" | "csv" => "text/plain",
                _ => "application/octet-stream",
            }
            .to_owned();
            Some(BrowserAttachment {
                name,
                mime,
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect()
}

enum BrowserBackend {
    #[cfg(windows)]
    Process(process_transport::Controller),
    InProcess(host::Controller),
}

pub struct WebGptBrowserController {
    backend: BrowserBackend,
}

impl WebGptBrowserController {
    pub fn disabled(message: &str) -> Self {
        Self {
            backend: BrowserBackend::InProcess(host::Controller::disabled(message)),
        }
    }

    pub fn spawn() -> Self {
        #[cfg(windows)]
        {
            Self {
                backend: BrowserBackend::Process(process_transport::Controller::spawn()),
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                backend: BrowserBackend::InProcess(host::Controller::spawn()),
            }
        }
    }

    pub fn spawn_in_process() -> Self {
        Self {
            backend: BrowserBackend::InProcess(host::Controller::spawn()),
        }
    }

    pub fn show_login(&self) {
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => controller.show_login(),
            BrowserBackend::InProcess(controller) => controller.show_login(),
        }
    }

    pub fn hide(&self) {
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => controller.hide(),
            BrowserBackend::InProcess(controller) => controller.hide(),
        }
    }

    pub fn wake(&self, request_id: String) {
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => controller.wake(request_id),
            BrowserBackend::InProcess(controller) => controller.wake(request_id),
        }
    }

    pub fn submit_chat(&self, correlation: WebGptTurnCorrelation, text: String) {
        self.submit_chat_with_attachments(correlation, text, Vec::new());
    }

    pub fn submit_chat_with_attachments(
        &self,
        correlation: WebGptTurnCorrelation,
        text: String,
        paths: Vec<std::path::PathBuf>,
    ) {
        let attachments = browser_attachments_from_paths(&paths);
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => {
                controller.submit_chat(correlation, text, attachments)
            }
            BrowserBackend::InProcess(controller) => {
                controller.submit_chat(correlation, text, attachments)
            }
        }
    }

    pub fn cancel_chat(&self, correlation: WebGptTurnCorrelation) {
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => controller.cancel_chat(correlation),
            BrowserBackend::InProcess(controller) => controller.cancel_chat(correlation),
        }
    }

    pub fn reload(&self) {
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => controller.reload(),
            BrowserBackend::InProcess(controller) => controller.reload(),
        }
    }

    pub fn drain(&self) -> Vec<WebGptBrowserEvent> {
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => controller.drain(),
            BrowserBackend::InProcess(controller) => controller.drain(),
        }
    }
}

impl Default for WebGptBrowserController {
    fn default() -> Self {
        Self::spawn()
    }
}

#[cfg(windows)]
pub fn run_browser_host() -> Result<(), String> {
    let controller = host::Controller::spawn();
    let (command_tx, command_rx) = mpsc::channel::<BrowserHostCommand>();
    thread::Builder::new()
        .name("roche-webgpt-host-stdin".to_owned())
        .spawn(move || {
            for line in BufReader::new(std::io::stdin()).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        if let Ok(command) = serde_json::from_str::<BrowserHostCommand>(&line)
                            && command_tx.send(command).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = command_tx.send(BrowserHostCommand::Shutdown);
        })
        .map_err(|error| format!("Could not start Web GPT host stdin reader: {error}"))?;

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    loop {
        for event in controller.drain() {
            serde_json::to_writer(&mut stdout, &event)
                .map_err(|error| format!("Could not encode Web GPT host event: {error}"))?;
            stdout
                .write_all(b"\n")
                .map_err(|error| format!("Could not write Web GPT host event: {error}"))?;
            stdout
                .flush()
                .map_err(|error| format!("Could not flush Web GPT host event: {error}"))?;
        }

        match command_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(BrowserHostCommand::ShowLogin) => controller.show_login(),
            Ok(BrowserHostCommand::Hide) => controller.hide(),
            Ok(BrowserHostCommand::Wake { request_id }) => controller.wake(request_id),
            Ok(BrowserHostCommand::Chat {
                correlation,
                text,
                attachments,
            }) => controller.submit_chat(correlation, text, attachments),
            Ok(BrowserHostCommand::Cancel { correlation }) => controller.cancel_chat(correlation),
            Ok(BrowserHostCommand::Reload) => controller.reload(),
            Ok(BrowserHostCommand::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn run_browser_host() -> Result<(), String> {
    Err("Web GPT browser host requires Windows WebView2".to_owned())
}

pub const LOGIN_PROBE_INTERVAL: Duration = Duration::from_secs(2);
