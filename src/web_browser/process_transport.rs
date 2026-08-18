//! Windows child-process JSONL transport for the Web GPT browser host.

use super::{
    BrowserAttachment, BrowserHostCommand, WebGptBrowserEvent, WebGptBrowserState,
    WebGptTurnCorrelation,
};

#[cfg(windows)]
mod process_backend {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Child, Command, Stdio},
        sync::mpsc::{self, Receiver, Sender},
        thread,
    };

    use super::{
        BrowserHostCommand, WebGptBrowserEvent, WebGptBrowserState, WebGptTurnCorrelation,
    };

    pub struct Controller {
        commands: Sender<BrowserHostCommand>,
        events: Receiver<WebGptBrowserEvent>,
        child: Option<Child>,
    }

    impl Controller {
        pub fn spawn() -> Self {
            let (event_tx, event_rx) = mpsc::channel();
            let (command_tx, command_rx) = mpsc::channel::<BrowserHostCommand>();
            let executable = if let Some(path) =
                std::env::var_os("ROCHE_WEBGPT_BROWSER_HOST_EXE").map(std::path::PathBuf::from)
            {
                path
            } else {
                let current = match std::env::current_exe() {
                    Ok(path) => path,
                    Err(error) => {
                        let _ = event_tx.send(WebGptBrowserEvent::State(
                            WebGptBrowserState::Offline(format!(
                                "Could not locate Roche executable for Web GPT host: {error}"
                            )),
                        ));
                        return Self {
                            commands: command_tx,
                            events: event_rx,
                            child: None,
                        };
                    }
                };
                if current.file_stem().and_then(std::ffi::OsStr::to_str)
                    == Some("roche-workstation")
                {
                    current
                } else {
                    let sibling = current.with_file_name("roche-workstation.exe");
                    if sibling.is_file() {
                        sibling
                    } else {
                        let _ = event_tx.send(WebGptBrowserEvent::State(
                            WebGptBrowserState::Offline(
                                "Could not locate roche-workstation.exe for Web GPT host; set ROCHE_WEBGPT_BROWSER_HOST_EXE"
                                    .to_owned(),
                            ),
                        ));
                        return Self {
                            commands: command_tx,
                            events: event_rx,
                            child: None,
                        };
                    }
                }
            };
            let mut child = match Command::new(executable)
                .arg("--webgpt-browser-host")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    let _ = event_tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                        format!("Could not start Roche Web GPT host process: {error}"),
                    )));
                    return Self {
                        commands: command_tx,
                        events: event_rx,
                        child: None,
                    };
                }
            };

            let Some(mut stdin) = child.stdin.take() else {
                let _ = event_tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                    "Roche Web GPT host stdin was unavailable".to_owned(),
                )));
                let _ = child.kill();
                return Self {
                    commands: command_tx,
                    events: event_rx,
                    child: None,
                };
            };
            let Some(stdout) = child.stdout.take() else {
                let _ = event_tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                    "Roche Web GPT host stdout was unavailable".to_owned(),
                )));
                let _ = child.kill();
                return Self {
                    commands: command_tx,
                    events: event_rx,
                    child: None,
                };
            };

            thread::Builder::new()
                .name("roche-webgpt-host-writer".to_owned())
                .spawn(move || {
                    while let Ok(command) = command_rx.recv() {
                        if serde_json::to_writer(&mut stdin, &command).is_err()
                            || stdin.write_all(b"\n").is_err()
                            || stdin.flush().is_err()
                        {
                            break;
                        }
                        if matches!(command, BrowserHostCommand::Shutdown) {
                            break;
                        }
                    }
                })
                .ok();

            let reader_events = event_tx.clone();
            thread::Builder::new()
                .name("roche-webgpt-host-reader".to_owned())
                .spawn(move || {
                    let mut reported_stream_error = false;
                    for line in BufReader::new(stdout).lines() {
                        match line {
                            Ok(line) if line.trim().is_empty() => {}
                            Ok(line) => match serde_json::from_str::<WebGptBrowserEvent>(&line) {
                                Ok(event) => {
                                    let _ = reader_events.send(event);
                                }
                                Err(error) => {
                                    let _ = reader_events.send(WebGptBrowserEvent::Error(format!(
                                        "Invalid Web GPT host event: {error}"
                                    )));
                                }
                            },
                            Err(error) => {
                                let _ = reader_events.send(WebGptBrowserEvent::State(
                                    WebGptBrowserState::Offline(format!(
                                        "Web GPT host event stream closed: {error}"
                                    )),
                                ));
                                reported_stream_error = true;
                                break;
                            }
                        }
                    }
                    if !reported_stream_error {
                        let _ = reader_events.send(WebGptBrowserEvent::State(
                            WebGptBrowserState::Offline(
                                "Web GPT host event stream ended".to_owned(),
                            ),
                        ));
                    }
                })
                .ok();

            Self {
                commands: command_tx,
                events: event_rx,
                child: Some(child),
            }
        }

        pub fn show_login(&self) {
            let _ = self.commands.send(BrowserHostCommand::ShowLogin);
        }

        pub fn hide(&self) {
            let _ = self.commands.send(BrowserHostCommand::Hide);
        }

        pub fn wake(&self, request_id: String) {
            let _ = self.commands.send(BrowserHostCommand::Wake { request_id });
        }

        pub fn submit_chat(
            &self,
            correlation: WebGptTurnCorrelation,
            text: String,
            attachments: Vec<super::BrowserAttachment>,
        ) {
            let _ = self.commands.send(BrowserHostCommand::Chat {
                correlation,
                text,
                attachments,
            });
        }

        pub fn cancel_chat(&self, correlation: WebGptTurnCorrelation) {
            let _ = self
                .commands
                .send(BrowserHostCommand::Cancel { correlation });
        }

        pub fn reload(&self) {
            let _ = self.commands.send(BrowserHostCommand::Reload);
        }

        pub fn drain(&self) -> Vec<WebGptBrowserEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            let _ = self.commands.send(BrowserHostCommand::Shutdown);
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub(super) use process_backend::Controller;
