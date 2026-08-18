use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::web_browser_pool::{PoolEffect, PoolTurn, Slot, SlotEvent, WebGptPoolScheduler};
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

#[cfg(windows)]
mod platform {
    use std::{
        path::PathBuf,
        sync::mpsc::{self, Receiver, Sender},
        thread,
        time::Duration,
    };

    use tao::{
        dpi::LogicalSize,
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
        platform::windows::EventLoopBuilderExtWindows,
        window::WindowBuilder,
    };
    use wry::{
        BackgroundThrottlingPolicy, PageLoadEvent, Rect, WebContext, WebViewBuilder,
        dpi::{LogicalPosition as WryLogicalPosition, LogicalSize as WryLogicalSize},
    };

    use super::{WebGptBrowserEvent, WebGptBrowserState, WebGptTurnCorrelation};

    const CHATGPT_URL: &str = "https://chatgpt.com/";

    #[derive(Debug, Clone)]
    enum BrowserCommand {
        ShowLogin,
        Hide,
        Probe,
        Wake {
            request_id: String,
        },
        Chat {
            correlation: WebGptTurnCorrelation,
            text: String,
            attachments: Vec<super::BrowserAttachment>,
        },
        Cancel {
            correlation: WebGptTurnCorrelation,
        },
        Reload,
        Shutdown,
    }

    pub struct Controller {
        proxy: Option<EventLoopProxy<BrowserCommand>>,
        events: Receiver<WebGptBrowserEvent>,
    }

    impl Controller {
        pub fn disabled(message: &str) -> Self {
            let (event_tx, event_rx) = mpsc::channel();
            let _ = event_tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                message.to_owned(),
            )));
            Self {
                proxy: None,
                events: event_rx,
            }
        }

        pub fn spawn() -> Self {
            let (event_tx, event_rx) = mpsc::channel();
            let (proxy_tx, proxy_rx) = mpsc::sync_channel(1);
            let thread_events = event_tx.clone();
            thread::Builder::new()
                .name("roche-webgpt-webview2".to_owned())
                .spawn(move || browser_thread(proxy_tx, thread_events))
                .expect("failed to start Roche Web GPT browser thread");

            let proxy = match proxy_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(proxy) => Some(proxy),
                Err(error) => {
                    let _ = event_tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                        format!("Web GPT browser startup failed: {error}"),
                    )));
                    None
                }
            };
            Self {
                proxy,
                events: event_rx,
            }
        }

        pub fn show_login(&self) {
            self.send(BrowserCommand::ShowLogin);
        }

        pub fn hide(&self) {
            self.send(BrowserCommand::Hide);
        }

        pub fn wake(&self, request_id: String) {
            self.send(BrowserCommand::Wake { request_id });
        }

        pub fn submit_chat(
            &self,
            correlation: WebGptTurnCorrelation,
            text: String,
            attachments: Vec<super::BrowserAttachment>,
        ) {
            self.send(BrowserCommand::Chat {
                correlation,
                text,
                attachments,
            });
        }

        pub fn cancel_chat(&self, correlation: WebGptTurnCorrelation) {
            self.send(BrowserCommand::Cancel { correlation });
        }

        pub fn reload(&self) {
            self.send(BrowserCommand::Reload);
        }

        pub fn drain(&self) -> Vec<WebGptBrowserEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }

        fn send(&self, command: BrowserCommand) {
            if let Some(proxy) = &self.proxy {
                let _ = proxy.send_event(command);
            }
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            self.send(BrowserCommand::Shutdown);
        }
    }

    fn browser_thread(
        proxy_tx: mpsc::SyncSender<EventLoopProxy<BrowserCommand>>,
        events: Sender<WebGptBrowserEvent>,
    ) {
        let _ = events.send(WebGptBrowserEvent::State(WebGptBrowserState::Starting));

        let mut builder = EventLoopBuilder::<BrowserCommand>::with_user_event();
        builder.with_any_thread(true);
        let event_loop = builder.build();
        let proxy = event_loop.create_proxy();
        if proxy_tx.send(proxy.clone()).is_err() {
            return;
        }

        let window = match WindowBuilder::new()
            .with_title("Roche · Web GPT 로그인")
            .with_visible(false)
            .with_inner_size(LogicalSize::new(1080.0, 820.0))
            .build(&event_loop)
        {
            Ok(window) => window,
            Err(error) => {
                let _ = events.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                    format!("Could not create Web GPT login window: {error}"),
                )));
                return;
            }
        };

        let profile_dir = web_profile_dir();
        if let Err(error) = std::fs::create_dir_all(&profile_dir) {
            let _ = events.send(WebGptBrowserEvent::Error(format!(
                "Could not create Web GPT profile directory {}: {error}",
                profile_dir.display()
            )));
        }
        let mut web_context = WebContext::new(Some(profile_dir));
        let page_events = events.clone();
        let webview = match WebViewBuilder::new_with_web_context(&mut web_context)
            .with_url(CHATGPT_URL)
            .with_background_throttling(BackgroundThrottlingPolicy::Disabled)
            .with_on_page_load_handler(move |event, url| {
                if matches!(event, PageLoadEvent::Finished)
                    && (url.contains("/auth/") || url.contains("/login"))
                {
                    let _ = page_events
                        .send(WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired));
                }
            })
            .build_as_child(&window)
        {
            Ok(webview) => webview,
            Err(error) => {
                let _ = events.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                    format!("Could not create WebView2: {error}"),
                )));
                return;
            }
        };

        let probe_proxy = proxy.clone();
        thread::Builder::new()
            .name("roche-webgpt-login-probe".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(500));
                    if probe_proxy.send_event(BrowserCommand::Probe).is_err() {
                        break;
                    }
                }
            })
            .ok();

        // Keep the context alive for as long as the WebView exists.
        let _web_context = web_context;
        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::UserEvent(BrowserCommand::ShowLogin) => {
                    window.set_visible(true);
                    window.set_focus();
                    probe_login_state(&webview, events.clone());
                }
                Event::UserEvent(BrowserCommand::Hide) => {
                    window.set_visible(false);
                }
                Event::UserEvent(BrowserCommand::Probe) => {
                    probe_login_state(&webview, events.clone());
                    probe_chat_state(&webview, events.clone());
                }
                Event::UserEvent(BrowserCommand::Chat {
                    correlation,
                    text,
                    attachments,
                }) => {
                    submit_chat_prompt(&webview, correlation, text, attachments, events.clone());
                }
                Event::UserEvent(BrowserCommand::Cancel { correlation }) => {
                    cancel_chat_prompt(&webview, correlation, events.clone());
                }
                Event::UserEvent(BrowserCommand::Reload) => {
                    if let Err(error) = webview.load_url(CHATGPT_URL) {
                        let _ = events.send(WebGptBrowserEvent::Error(format!(
                            "Could not reload ChatGPT: {error}"
                        )));
                    }
                }
                Event::UserEvent(BrowserCommand::Wake { request_id }) => {
                    submit_wake_prompt(&webview, request_id, events.clone());
                }
                Event::UserEvent(BrowserCommand::Shutdown) => {
                    *control_flow = ControlFlow::Exit;
                }
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    // Closing the login surface only hides it. The persisted ChatGPT
                    // session remains available to the hidden wake-up runtime.
                    window.set_visible(false);
                }
                Event::WindowEvent {
                    event: WindowEvent::Resized(size),
                    ..
                } => {
                    let size = size.to_logical::<u32>(window.scale_factor());
                    let _ = webview.set_bounds(Rect {
                        position: WryLogicalPosition::new(0, 0).into(),
                        size: WryLogicalSize::new(size.width, size.height).into(),
                    });
                }
                _ => {}
            }
        });
    }

    fn probe_login_state(webview: &wry::WebView, events: Sender<WebGptBrowserEvent>) {
        const SCRIPT: &str = r#"(() => {
            const composer = document.querySelector('#prompt-textarea')
                || document.querySelector('textarea[placeholder]')
                || document.querySelector('[contenteditable="true"][role="textbox"]');
            const loginControl = document.querySelector(
                'a[href*="/auth/login"], a[href*="/auth/"], button[data-testid="login-button"]'
            );
            return Boolean(composer) && !loginControl;
        })()"#;
        let callback_events = events.clone();
        if let Err(error) = webview.evaluate_script_with_callback(SCRIPT, move |value| {
            let logged_in = value.trim().eq_ignore_ascii_case("true");
            let state = if logged_in {
                WebGptBrowserState::LoggedIn
            } else {
                WebGptBrowserState::LoginRequired
            };
            let _ = callback_events.send(WebGptBrowserEvent::State(state));
        }) {
            let _ = events.send(WebGptBrowserEvent::Error(format!(
                "Could not inspect ChatGPT login state: {error}"
            )));
        }
    }

    fn submit_wake_prompt(
        webview: &wry::WebView,
        request_id: String,
        events: Sender<WebGptBrowserEvent>,
    ) {
        let wake_text = format!(
            "Use the Roche app for native request {request_id}. Read that pending request through Roche MCP, orchestrate any Web GPT or Codex worker sessions you need, and post the final user-facing answer back through Roche. Rust is the deterministic session/task source of truth."
        );
        submit_prompt(
            webview,
            None,
            request_id,
            wake_text,
            Vec::new(),
            events,
            false,
        );
    }

    fn submit_chat_prompt(
        webview: &wry::WebView,
        correlation: WebGptTurnCorrelation,
        text: String,
        attachments: Vec<super::BrowserAttachment>,
        events: Sender<WebGptBrowserEvent>,
    ) {
        let request_id = correlation.request_id.clone();
        submit_prompt(
            webview,
            Some(correlation),
            request_id,
            text,
            attachments,
            events,
            true,
        );
    }

    fn cancel_chat_prompt(
        webview: &wry::WebView,
        correlation: WebGptTurnCorrelation,
        events: Sender<WebGptBrowserEvent>,
    ) {
        let request_id = correlation.request_id.clone();
        let encoded_request_id =
            serde_json::to_string(&request_id).unwrap_or_else(|_| "\"roche-web\"".into());
        let encoded_correlation =
            serde_json::to_string(&correlation).unwrap_or_else(|_| "null".to_owned());
        let script = format!(
            r#"(() => {{
                const requestId = {encoded_request_id};
                const correlation = {encoded_correlation};
                let pending = null;
                try {{ pending = JSON.parse(sessionStorage.getItem('__rochePendingChat') || 'null'); }} catch {{}}
                if (!pending || pending.requestId !== requestId) return false;
                if (JSON.stringify(pending.correlation) !== JSON.stringify(correlation)) return false;
                pending.cancelRequested = true;
                sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                const stop = document.querySelector('[data-testid="stop-button"]')
                    || document.querySelector('button[aria-label*="Stop"]')
                    || document.querySelector('button[aria-label*="중지"]');
                if (!stop) return 'pending';
                stop.click();
                sessionStorage.removeItem('__rochePendingChat');
                return 'cancelled';
            }})()"#
        );
        let callback_events = events.clone();
        let callback_correlation = correlation.clone();
        if let Err(error) = webview.evaluate_script_with_callback(&script, move |value| {
            if let Some(event) = super::cancel_script_event(&value, &callback_correlation) {
                let _ = callback_events.send(event);
            }
        }) {
            let _ = events.send(WebGptBrowserEvent::Error(format!(
                "Could not cancel Web GPT request {request_id}: {error}"
            )));
        }
    }

    fn submit_prompt(
        webview: &wry::WebView,
        correlation: Option<WebGptTurnCorrelation>,
        request_id: String,
        text: String,
        attachments: Vec<super::BrowserAttachment>,
        events: Sender<WebGptBrowserEvent>,
        capture_answer: bool,
    ) {
        let encoded_text =
            serde_json::to_string(&text).unwrap_or_else(|_| "\"Roche request\"".into());
        let encoded_attachments =
            serde_json::to_string(&attachments).unwrap_or_else(|_| "[]".into());
        let encoded_request_id =
            serde_json::to_string(&request_id).unwrap_or_else(|_| "\"roche-web\"".into());
        let encoded_correlation = correlation
            .as_ref()
            .map(|corr| serde_json::to_string(corr).unwrap_or_else(|_| "null".to_owned()))
            .unwrap_or_else(|| "null".to_owned());
        let capture = if capture_answer { "true" } else { "false" };
        let script = format!(
            r#"(() => {{
                const rawText = {encoded_text};
                const attachments = {encoded_attachments};
                const text = rawText || (attachments.length ? '첨부 파일을 확인해 주세요.' : rawText);
                const requestId = {encoded_request_id};
                const correlation = {encoded_correlation};
                const composer = document.querySelector('#prompt-textarea')
                    || document.querySelector('textarea[placeholder]')
                    || document.querySelector('[contenteditable="true"][role="textbox"]');
                if (!composer) return 'login_required';
                if (attachments.length) {{
                    const fileInput = document.querySelector('input[type="file"]');
                    if (!fileInput) return 'attachment_input_unavailable';
                    const transfer = new DataTransfer();
                    for (const attachment of attachments) {{
                        const binary = atob(attachment.data_base64);
                        const bytes = new Uint8Array(binary.length);
                        for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
                        transfer.items.add(new File([bytes], attachment.name, {{ type: attachment.mime }}));
                    }}
                    fileInput.files = transfer.files;
                    fileInput.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    fileInput.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }}
                if ({capture}) {{
                    sessionStorage.setItem('__rochePendingChat', JSON.stringify({{
                        requestId,
                        correlation,
                        text,
                        clicked: false,
                        failed: false,
                        submittedEmitted: false,
                        cancelRequested: false,
                        attempts: 0,
                        lastText: '',
                        lastActivity: '',
                        lastThinking: false
                    }}));
                }}
                composer.focus();
                if (composer instanceof HTMLTextAreaElement || composer instanceof HTMLInputElement) {{
                    const proto = composer instanceof HTMLTextAreaElement
                        ? HTMLTextAreaElement.prototype
                        : HTMLInputElement.prototype;
                    const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
                    if (setter) setter.call(composer, text); else composer.value = text;
                    composer.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    composer.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }} else {{
                    const selection = window.getSelection();
                    const range = document.createRange();
                    range.selectNodeContents(composer);
                    selection?.removeAllRanges();
                    selection?.addRange(range);
                    document.execCommand('insertText', false, text);
                    composer.dispatchEvent(new InputEvent('input', {{
                        bubbles: true,
                        inputType: 'insertText',
                        data: text
                    }}));
                }}
                const findSend = () => composer.closest('form')?.querySelector('button[type="submit"]')
                    || document.querySelector('button[data-testid="send-button"]')
                    || document.querySelector('button[data-testid*="send"]')
                    || document.querySelector('button[aria-label*="Send"]')
                    || document.querySelector('button[aria-label*="send"]')
                    || document.querySelector('button[aria-label*="전송"]');
                let attempts = 0;
                const clickWhenReady = () => {{
                    const button = findSend();
                    if (button && !button.disabled) {{
                        button.click();
                        if ({capture}) {{
                            const raw = sessionStorage.getItem('__rochePendingChat');
                            if (raw) {{
                                const pending = JSON.parse(raw);
                                if (pending.requestId === requestId) {{
                                    pending.clicked = true;
                                    pending.attempts = attempts;
                                    sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                                }}
                            }}
                        }}
                        return;
                    }}
                    attempts += 1;
                    if ({capture}) {{
                        const raw = sessionStorage.getItem('__rochePendingChat');
                        if (raw) {{
                            const pending = JSON.parse(raw);
                            if (pending.requestId === requestId) {{
                                pending.attempts = attempts;
                                if (attempts >= 30) pending.failed = true;
                                sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                            }}
                        }}
                    }}
                    if (attempts < 30) setTimeout(clickWhenReady, 100);
                }};
                setTimeout(clickWhenReady, 75);
                return 'scheduled';
            }})()"#
        );
        let callback_events = events.clone();
        let callback_correlation = correlation.clone();
        let callback_request_id = request_id.clone();
        if let Err(error) = webview.evaluate_script_with_callback(&script, move |value| {
            let result = value.trim().trim_matches('"');
            match result {
                "scheduled" | "submitted" if capture_answer => {}
                "scheduled" | "submitted" => {
                    let _ = callback_events.send(WebGptBrowserEvent::WakeSubmitted {
                        request_id: callback_request_id.clone(),
                    });
                }
                "attachment_input_unavailable" => {
                    if let Some(correlation) = &callback_correlation {
                        let _ = callback_events.send(WebGptBrowserEvent::ChatFailed {
                            correlation: correlation.clone(),
                            message: "ChatGPT file input was not available for attachment upload"
                                .to_owned(),
                        });
                    }
                    let _ = callback_correlation.clone();
                }
                "login_required" => {
                    let _ = callback_events
                        .send(WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired));
                }
                other => {
                    if capture_answer {
                        if let Some(correlation) = &callback_correlation {
                            let _ = callback_events.send(WebGptBrowserEvent::ChatFailed {
                                correlation: correlation.clone(),
                                message: format!("ChatGPT request was not submitted: {other}"),
                            });
                        }
                    } else {
                        let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                            "Web GPT wake request {callback_request_id} was not submitted: {other}"
                        )));
                    }
                }
            }
        }) {
            if capture_answer {
                if let Some(correlation) = &correlation {
                    let _ = events.send(WebGptBrowserEvent::ChatFailed {
                        correlation: correlation.clone(),
                        message: format!("Could not submit ChatGPT request: {error}"),
                    });
                }
            } else {
                let _ = events.send(WebGptBrowserEvent::Error(format!(
                    "Could not submit Web GPT wake request {request_id}: {error}"
                )));
            }
        }
    }
    fn probe_chat_state(webview: &wry::WebView, events: Sender<WebGptBrowserEvent>) {
        const SCRIPT: &str = r#"(() => {
            const raw = sessionStorage.getItem('__rochePendingChat');
            if (!raw) return null;
            const pending = JSON.parse(raw);
            if (pending.failed) {
                const result = JSON.stringify({
                    kind: 'error',
                    request_id: pending.requestId,
                    correlation: pending.correlation,
                    detail: `send button unavailable after ${pending.attempts} attempts`
                });
                sessionStorage.removeItem('__rochePendingChat');
                return result;
            }

            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const activityLine = /^\s*(inProgress|completed|failed|warnings?)\s*:/i;
            const runtimeNoiseLine = /^\s*Codex:\s+.*(?:ERROR|WARN|failed to connect|websocket)/i;
            const sanitizeAssistantText = value => (value || '')
                .split(/\r?\n/)
                .filter(line => !activityLine.test(line) && !runtimeNoiseLine.test(line))
                .join('\n')
                .trim();
            const activitySelector = [
                '[data-testid*="tool"]',
                '[data-testid*="search"]',
                '[data-testid*="connector"]',
                '[data-testid*="browse"]',
                '[data-testid*="reasoning"]',
                '[role="status"]'
            ].join(', ');
            const assistantTextWithoutActivity = node => {
                if (!node) return '';
                const clone = node.cloneNode(true);
                clone.querySelectorAll?.(activitySelector).forEach(activity => activity.remove());
                return sanitizeAssistantText(clone.innerText || clone.textContent || node.innerText || node.textContent || '');
            };
            const expected = normalize(pending.text);
            const messages = Array.from(document.querySelectorAll('[data-message-author-role]'));
            let userIndex = -1;
            for (let index = messages.length - 1; index >= 0; index -= 1) {
                const node = messages[index];
                if (node.getAttribute('data-message-author-role') !== 'user') continue;
                const observed = normalize(node.innerText || node.textContent);
                if (observed === expected || observed.includes(expected)) {
                    userIndex = index;
                    break;
                }
            }

            const mainText = normalize(document.querySelector('main')?.innerText || '');
            const promptIndex = mainText.lastIndexOf(expected);
            if (userIndex < 0 && promptIndex < 0) {
                return JSON.stringify({
                    kind: 'probe',
                    request_id: pending.requestId,
                    correlation: pending.correlation,
                    detail: JSON.stringify({
                        href: location.href,
                        clicked: pending.clicked,
                        failed: pending.failed,
                        attempts: pending.attempts,
                        messageCount: messages.length,
                        articleCount: document.querySelectorAll('article').length,
                        mainText: mainText.slice(-1200),
                        bodyTail: normalize(document.body?.innerText || '').slice(-1200),
                        iframeCount: document.querySelectorAll('iframe').length,
                        composerText: normalize((document.querySelector('#prompt-textarea')?.innerText || document.querySelector('#prompt-textarea')?.textContent || document.querySelector('#prompt-textarea')?.value || ''))
                    })
                });
            }

            if (!pending.submittedEmitted) {
                pending.submittedEmitted = true;
                sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                return JSON.stringify({
                    kind: 'submitted',
                    request_id: pending.requestId,
                    correlation: pending.correlation
                });
            }

            const generating = document.querySelector('button[data-testid="stop-button"]')
                || document.querySelector('button[aria-label*="Stop"]')
                || document.querySelector('button[aria-label*="stop"]')
                || document.querySelector('button[aria-label*="중지"]');

            if (pending.cancelRequested && generating) {
                generating.click();
                const result = JSON.stringify({
                    kind: 'cancelled',
                    request_id: pending.requestId,
                    correlation: pending.correlation
                });
                sessionStorage.removeItem('__rochePendingChat');
                return result;
            }

            let text = '';
            let assistantRawText = '';
            if (userIndex >= 0) {
                let assistant = null;
                for (let index = userIndex + 1; index < messages.length; index += 1) {
                    if (messages[index].getAttribute('data-message-author-role') === 'assistant') {
                        assistant = messages[index];
                    }
                }
                assistantRawText = (assistant?.innerText || assistant?.textContent || '').trim();
                text = assistantTextWithoutActivity(assistant);
            }

            if (!text && promptIndex >= 0) {
                const afterPrompt = mainText.slice(promptIndex + expected.length);
                const answerMarkers = ['ChatGPT의 말:', 'ChatGPT said:', 'Assistant:', 'ChatGPT:'];
                let answerStart = -1;
                let markerLength = 0;
                for (const marker of answerMarkers) {
                    const index = afterPrompt.indexOf(marker);
                    if (index >= 0 && (answerStart < 0 || index < answerStart)) {
                        answerStart = index;
                        markerLength = marker.length;
                    }
                }
                if (answerStart >= 0) {
                    let answer = afterPrompt.slice(answerStart + markerLength).trim();
                    const endMarkers = [
                        'ChatGPT는 AI라 실수할 수 있습니다.',
                        'ChatGPT can make mistakes.',
                        'OpenAI OpCo, LLC'
                    ];
                    let answerEnd = answer.length;
                    for (const marker of endMarkers) {
                        const index = answer.indexOf(marker);
                        if (index >= 0) answerEnd = Math.min(answerEnd, index);
                    }
                    text = sanitizeAssistantText(answer.slice(0, answerEnd));
                }
            }

            const visibleText = node => {
                if (!node) return '';
                const rect = node.getBoundingClientRect?.();
                if (rect && (rect.width <= 0 || rect.height <= 0)) return '';
                return normalize(node.innerText || node.textContent || node.getAttribute?.('aria-label') || '');
            };
            const activityNodes = Array.from(document.querySelectorAll('main ' + activitySelector.replaceAll(', ', ', main ')));
            const inlineActivity = assistantRawText
                .split(/\r?\n/)
                .map(line => line.trim())
                .filter(line => activityLine.test(line))
                .slice(-1)[0] || '';
            const activity = activityNodes
                .map(visibleText)
                .filter(value => value && value !== text && value !== expected && !value.includes(expected))
                .filter(value => value.length <= 280)
                .slice(-1)[0] || inlineActivity;

            if (generating) {
                const thinking = !text;
                const changed = text !== (pending.lastText || '')
                    || activity !== (pending.lastActivity || '')
                    || thinking !== Boolean(pending.lastThinking);
                if (!changed) return null;
                pending.lastText = text;
                pending.lastActivity = activity;
                pending.lastThinking = thinking;
                sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                return JSON.stringify({
                    kind: 'progress',
                    request_id: pending.requestId,
                    correlation: pending.correlation,
                    text: text || null,
                    activity: activity || null,
                    thinking
                });
            }

            if (!text) return null;
            const result = JSON.stringify({
                kind: 'answered',
                request_id: pending.requestId,
                correlation: pending.correlation,
                text
            });
            sessionStorage.removeItem('__rochePendingChat');
            return result;
        })()"#;
        let callback_events = events.clone();
        if let Err(error) = webview.evaluate_script_with_callback(SCRIPT, move |value| {
            let Ok(encoded) = serde_json::from_str::<String>(value.trim()) else {
                return;
            };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&encoded) else {
                return;
            };
            let Some(correlation) = payload
                .get("correlation")
                .cloned()
                .and_then(|value| serde_json::from_value::<WebGptTurnCorrelation>(value).ok())
            else {
                return;
            };
            match payload.get("kind").and_then(serde_json::Value::as_str) {
                Some("submitted") => {
                    let _ = callback_events.send(WebGptBrowserEvent::ChatSubmitted {
                        correlation: correlation.clone(),
                    });
                }
                Some("progress") => {
                    let text = payload
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    let activity = payload
                        .get("activity")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    let thinking = payload
                        .get("thinking")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let _ = callback_events.send(WebGptBrowserEvent::ChatProgress {
                        correlation: correlation.clone(),
                        text,
                        activity,
                        thinking,
                    });
                }
                Some("answered") => {
                    let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) else {
                        return;
                    };
                    let _ = callback_events.send(WebGptBrowserEvent::ChatAnswered {
                        correlation: correlation.clone(),
                        text: text.to_owned(),
                    });
                }
                Some("cancelled") => {
                    let _ = callback_events.send(WebGptBrowserEvent::ChatCancelled {
                        correlation: correlation.clone(),
                    });
                }
                Some("error") => {
                    let detail = payload
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown ChatGPT submit error");
                    let _ = callback_events.send(WebGptBrowserEvent::ChatFailed {
                        correlation: correlation.clone(),
                        message: format!("ChatGPT request failed: {detail}"),
                    });
                }
                Some("probe") if std::env::var_os("ROCHE_WEBGPT_DIAGNOSTICS").is_some() => {
                    let detail = payload
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no probe detail");
                    let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                        "ChatGPT probe {}: {detail}",
                        correlation.request_id
                    )));
                }
                _ => {}
            }
        }) {
            let _ = events.send(WebGptBrowserEvent::Error(format!(
                "Could not inspect ChatGPT response state: {error}"
            )));
        }
    }

    fn web_profile_dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Roche")
            .join("WebGptProfile")
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::mpsc::{self, Receiver};

    use super::{WebGptBrowserEvent, WebGptBrowserState, WebGptTurnCorrelation};

    pub struct Controller {
        events: Receiver<WebGptBrowserEvent>,
    }

    impl Controller {
        pub fn disabled(message: &str) -> Self {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                message.to_owned(),
            )));
            Self { events: rx }
        }

        pub fn spawn() -> Self {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(WebGptBrowserEvent::State(WebGptBrowserState::Offline(
                "The embedded Web GPT login runtime currently requires Windows WebView2".to_owned(),
            )));
            Self { events: rx }
        }

        pub fn show_login(&self) {}
        pub fn hide(&self) {}
        pub fn wake(&self, _request_id: String) {}
        pub fn submit_chat(
            &self,
            _correlation: WebGptTurnCorrelation,
            _text: String,
            _attachments: Vec<super::BrowserAttachment>,
        ) {
        }
        pub fn cancel_chat(&self, _correlation: WebGptTurnCorrelation) {}
        pub fn reload(&self) {}

        pub fn drain(&self) -> Vec<WebGptBrowserEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }
    }
}

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
    Process(process_backend::Controller),
    InProcess(platform::Controller),
}

pub struct WebGptBrowserController {
    backend: BrowserBackend,
}

impl WebGptBrowserController {
    pub fn disabled(message: &str) -> Self {
        Self {
            backend: BrowserBackend::InProcess(platform::Controller::disabled(message)),
        }
    }

    pub fn spawn() -> Self {
        #[cfg(windows)]
        {
            Self {
                backend: BrowserBackend::Process(process_backend::Controller::spawn()),
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                backend: BrowserBackend::InProcess(platform::Controller::spawn()),
            }
        }
    }

    pub fn spawn_in_process() -> Self {
        Self {
            backend: BrowserBackend::InProcess(platform::Controller::spawn()),
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

struct SharedBrowserInner {
    controller: WebGptBrowserController,
    ui_events: VecDeque<WebGptBrowserEvent>,
    worker_events: VecDeque<WebGptBrowserEvent>,
    /// Capacity-1 scheduling authority. Slots/queue/generation live here.
    scheduler: WebGptPoolScheduler,
    /// Full owner + message payload for every queued or in-flight turn, keyed by
    /// request id, so scheduler leases can be expanded back to full correlations.
    turn_payloads: HashMap<String, TurnPayload>,
    /// The exact full correlation of the currently leased turn, for event gating.
    active_correlation: Option<WebGptTurnCorrelation>,
    /// Whether the helper can accept a physical submit. A scheduler lease may be
    /// held while unavailable, but it is not sent until LoggedIn resumes it.
    browser_ready: bool,
    /// Distinguishes a scheduler lease from a command already sent to WebView2.
    active_dispatched: bool,
    /// Bounded diagnostics for duplicate/no-capacity/stale rejections that must
    /// not be routed into a UI/worker event queue or mutate another turn.
    diagnostics: VecDeque<String>,
}

struct TurnPayload {
    request: WebGptTurnRequest,
    text: String,
    paths: Vec<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct SharedWebGptBrowser {
    inner: Arc<Mutex<SharedBrowserInner>>,
}

impl SharedWebGptBrowser {
    pub fn spawn() -> Self {
        Self::from_controller(WebGptBrowserController::spawn())
    }

    pub fn disabled(message: &str) -> Self {
        Self::from_controller(WebGptBrowserController::disabled(message))
    }

    fn from_controller(controller: WebGptBrowserController) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedBrowserInner {
                controller,
                ui_events: VecDeque::new(),
                worker_events: VecDeque::new(),
                scheduler: WebGptPoolScheduler::new(1),
                turn_payloads: HashMap::new(),
                active_correlation: None,
                browser_ready: true,
                active_dispatched: false,
                diagnostics: VecDeque::new(),
            })),
        }
    }

    pub fn show_login(&self) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .show_login();
    }

    pub fn hide(&self) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .hide();
    }

    pub fn wake(&self, request_id: String) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .wake(request_id);
    }

    pub fn submit_chat(&self, request: WebGptTurnRequest, text: String) {
        self.submit_chat_with_attachments(request, text, Vec::new());
    }

    pub fn submit_chat_with_attachments(
        &self,
        request: WebGptTurnRequest,
        text: String,
        paths: Vec<std::path::PathBuf>,
    ) {
        let mut inner = self.inner.lock().expect("browser mutex poisoned");
        let pool_turn = request_to_pool_turn(&request);
        let request_id = request.request_id.clone();
        if !inner.turn_payloads.contains_key(&request_id) {
            // Only insert on first sight. A duplicate request id must never
            // overwrite the legitimate active/queued payload.
            inner.turn_payloads.insert(
                request_id.clone(),
                TurnPayload {
                    request,
                    text,
                    paths,
                },
            );
        }
        let effects = inner.scheduler.enqueue(pool_turn);
        process_scheduler_effects(&mut inner, effects);
    }

    pub fn cancel_chat(&self, request: WebGptTurnRequest) {
        let mut inner = self.inner.lock().expect("browser mutex poisoned");
        if let Some(active) = inner.active_correlation.clone()
            && request_matches_correlation(&request, &active)
        {
            let slot_event = correlation_slot_event(&active);
            let effects = inner.scheduler.cancel(slot_event);
            process_scheduler_effects(&mut inner, effects);
            return;
        }
        // Not the active turn. Only a queued turn whose stored owner matches the
        // request exactly may be cancelled; a wrong-owner request sharing a queued
        // request id must not touch the legitimate queued turn.
        match inner.turn_payloads.get(&request.request_id) {
            Some(payload) if payload.request == request => {
                let effects = inner.scheduler.cancel_queued(&request.request_id);
                process_scheduler_effects(&mut inner, effects);
            }
            _ => {
                let request_id = request.request_id;
                inner.push_diagnostic(format!(
                    "Web GPT queued cancel ignored for unknown/mismatched owner: {request_id}"
                ));
            }
        }
    }

    pub fn reload(&self) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .reload();
    }

    pub fn drain_ui(&self) -> Vec<WebGptBrowserEvent> {
        self.drain(false)
    }

    pub fn drain_worker(&self) -> Vec<WebGptBrowserEvent> {
        self.drain(true)
    }

    fn drain(&self, worker: bool) -> Vec<WebGptBrowserEvent> {
        let mut inner = self.inner.lock().expect("browser mutex poisoned");
        for event in inner.controller.drain() {
            handle_shared_event(&mut inner, event);
        }
        if worker {
            inner.worker_events.drain(..).collect()
        } else {
            inner.ui_events.drain(..).collect()
        }
    }
}

impl SharedBrowserInner {
    fn push_diagnostic(&mut self, message: String) {
        const MAX_DIAGNOSTICS: usize = 64;
        if self.diagnostics.len() >= MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(message);
    }
}

fn request_to_pool_turn(request: &WebGptTurnRequest) -> PoolTurn {
    PoolTurn {
        request_id: request.request_id.clone(),
        account: Some(request.account_id.clone()),
    }
}

fn correlation_slot_event(correlation: &WebGptTurnCorrelation) -> SlotEvent {
    SlotEvent {
        slot: Slot {
            index: correlation.lease.slot_id as usize,
            generation: correlation.lease.generation,
        },
        request_id: correlation.request_id.clone(),
        account: Some(correlation.account_id.clone()),
    }
}

fn submit_active_turn_if_ready(inner: &mut SharedBrowserInner) {
    if !inner.browser_ready || inner.active_dispatched {
        return;
    }
    let Some(correlation) = inner.active_correlation.clone() else {
        return;
    };
    let Some(payload) = inner.turn_payloads.get(&correlation.request_id) else {
        inner.push_diagnostic(format!(
            "Web GPT active lease had no stored payload: {}",
            correlation.request_id
        ));
        return;
    };
    let text = payload.text.clone();
    let paths = payload.paths.clone();
    inner.active_dispatched = true;
    inner
        .controller
        .submit_chat_with_attachments(correlation, text, paths);
}

/// Apply the scheduler's observable effects to the single physical controller and
/// the routed event queues. Effects are processed in order: a terminal frees and
/// then a queued dispatch leases the freed slot.
fn process_scheduler_effects(inner: &mut SharedBrowserInner, effects: Vec<PoolEffect>) {
    for effect in effects {
        match effect {
            PoolEffect::Dispatch(leased) => {
                let Some(payload) = inner.turn_payloads.get(&leased.request_id) else {
                    inner.push_diagnostic(format!(
                        "Web GPT dispatch had no stored payload: {}",
                        leased.request_id
                    ));
                    inner.active_correlation = None;
                    continue;
                };
                let correlation = payload
                    .request
                    .clone()
                    .lease(leased.slot.index as u32, leased.slot.generation);
                inner.active_correlation = Some(correlation.clone());
                inner.active_dispatched = false;
                submit_active_turn_if_ready(inner);
            }
            PoolEffect::Complete(leased) | PoolEffect::CancelAck(leased) => {
                if inner
                    .active_correlation
                    .as_ref()
                    .map(|correlation| correlation.request_id.as_str())
                    == Some(leased.request_id.as_str())
                {
                    inner.active_correlation = None;
                    inner.active_dispatched = false;
                }
                inner.turn_payloads.remove(&leased.request_id);
            }
            PoolEffect::CancelRequest(leased) => {
                if let Some(correlation) = inner.active_correlation.clone()
                    && correlation.request_id == leased.request_id
                {
                    inner.controller.cancel_chat(correlation);
                }
            }
            PoolEffect::CancelQueued(pool_turn) => {
                let Some(payload) = inner.turn_payloads.remove(&pool_turn.request_id) else {
                    continue;
                };
                let worker = payload.request.task_id.is_some();
                let event = WebGptBrowserEvent::ChatQueueCancelled {
                    request: payload.request,
                };
                if worker {
                    inner.worker_events.push_back(event);
                } else {
                    inner.ui_events.push_back(event);
                }
            }
            PoolEffect::RejectDuplicate { request_id } => {
                // We never inserted a duplicate payload (preserve the original),
                // so nothing is removed here; only a bounded diagnostic is made.
                inner.push_diagnostic(format!("Web GPT duplicate request rejected: {request_id}"));
            }
            PoolEffect::RejectStale {
                request_id, reason, ..
            } => {
                // A stale terminal/diagnostic must never mutate a running turn,
                // release a lease, or cross-route. It only surfaces a bounded log.
                inner.push_diagnostic(format!(
                    "Web GPT stale event rejected ({reason:?}): {request_id}"
                ));
            }
        }
    }
}

fn handle_shared_event(inner: &mut SharedBrowserInner, event: WebGptBrowserEvent) {
    if let WebGptBrowserEvent::State(state) = &event {
        handle_browser_state(inner, state.clone());
        return;
    }
    if let Some(correlation) = event_chat_correlation(&event).cloned() {
        // Gate on the exact stored full correlation.
        let Some(active) = inner.active_correlation.clone() else {
            inner.push_diagnostic(format!(
                "Web GPT chat event rejected with no active turn: {}",
                correlation.request_id
            ));
            return;
        };
        if active != correlation {
            inner.push_diagnostic(format!(
                "Web GPT stale chat event rejected for request {}",
                correlation.request_id
            ));
            return;
        }

        let terminal = matches!(
            &event,
            WebGptBrowserEvent::ChatAnswered { .. }
                | WebGptBrowserEvent::ChatCancelled { .. }
                | WebGptBrowserEvent::ChatFailed { .. }
        );
        if !terminal {
            // ChatSubmitted / ChatProgress: route only.
            route_correlation_event(inner, event);
            return;
        }

        // Transition first so we only route a terminal that actually freed.
        let slot_event = correlation_slot_event(&correlation);
        let effects = if matches!(&event, WebGptBrowserEvent::ChatCancelled { .. }) {
            inner.scheduler.cancel_ack(slot_event)
        } else {
            inner.scheduler.complete(slot_event)
        };
        if !matches!(
            effects.first(),
            Some(PoolEffect::Complete(_) | PoolEffect::CancelAck(_))
        ) {
            // Scheduler rejected the terminal (e.g. a cancel ack with no prior
            // cancel request): surface a bounded diagnostic and drop the event.
            inner.push_diagnostic(format!(
                "Web GPT terminal rejected for request {}",
                correlation.request_id
            ));
            return;
        }
        route_correlation_event(inner, event);
        process_scheduler_effects(inner, effects);
        return;
    }

    // Wake / State / Error / ChatQueueCancelled.
    route_other_event(inner, event);
}

fn handle_browser_state(inner: &mut SharedBrowserInner, state: WebGptBrowserState) {
    route_other_event(inner, WebGptBrowserEvent::State(state.clone()));
    match state {
        WebGptBrowserState::LoggedIn => {
            inner.browser_ready = true;
            submit_active_turn_if_ready(inner);
        }
        WebGptBrowserState::Starting => {
            inner.browser_ready = false;
        }
        WebGptBrowserState::LoginRequired => {
            inner.browser_ready = false;
            if let Some(correlation) = inner.active_correlation.clone() {
                handle_shared_event(
                    inner,
                    WebGptBrowserEvent::ChatFailed {
                        correlation,
                        message: "ChatGPT login is required before the request can continue"
                            .to_owned(),
                    },
                );
            }
        }
        WebGptBrowserState::Offline(message) => {
            inner.browser_ready = false;
            if let Some(correlation) = inner.active_correlation.clone() {
                handle_shared_event(
                    inner,
                    WebGptBrowserEvent::ChatFailed {
                        correlation,
                        message: format!("Web GPT browser went offline: {message}"),
                    },
                );
            }
        }
    }
}

fn route_correlation_event(inner: &mut SharedBrowserInner, event: WebGptBrowserEvent) {
    if event_chat_correlation(&event).is_some_and(|correlation| correlation.is_worker()) {
        inner.worker_events.push_back(event);
    } else {
        inner.ui_events.push_back(event);
    }
}

fn route_other_event(inner: &mut SharedBrowserInner, event: WebGptBrowserEvent) {
    let active_worker = inner
        .active_correlation
        .as_ref()
        .is_some_and(|correlation| correlation.is_worker());
    match &event {
        WebGptBrowserEvent::WakeSubmitted { request_id } => {
            if request_id.starts_with("web-worker-") {
                inner.worker_events.push_back(event);
            } else {
                inner.ui_events.push_back(event);
            }
        }
        WebGptBrowserEvent::State(_) => {
            inner.ui_events.push_back(event.clone());
            inner.worker_events.push_back(event);
        }
        WebGptBrowserEvent::Error(_) if active_worker => {
            inner.worker_events.push_back(event);
        }
        WebGptBrowserEvent::Error(_) if inner.active_correlation.is_some() => {
            inner.ui_events.push_back(event);
        }
        WebGptBrowserEvent::Error(_) => {
            inner.ui_events.push_back(event.clone());
            inner.worker_events.push_back(event);
        }
        WebGptBrowserEvent::ChatQueueCancelled { request } => {
            if request.task_id.is_some() {
                inner.worker_events.push_back(event);
            } else {
                inner.ui_events.push_back(event);
            }
        }
        WebGptBrowserEvent::ChatSubmitted { .. }
        | WebGptBrowserEvent::ChatProgress { .. }
        | WebGptBrowserEvent::ChatAnswered { .. }
        | WebGptBrowserEvent::ChatCancelled { .. }
        | WebGptBrowserEvent::ChatFailed { .. } => {}
    }
}

fn event_chat_correlation(event: &WebGptBrowserEvent) -> Option<&WebGptTurnCorrelation> {
    match event {
        WebGptBrowserEvent::ChatSubmitted { correlation }
        | WebGptBrowserEvent::ChatProgress { correlation, .. }
        | WebGptBrowserEvent::ChatAnswered { correlation, .. }
        | WebGptBrowserEvent::ChatCancelled { correlation }
        | WebGptBrowserEvent::ChatFailed { correlation, .. } => Some(correlation),
        WebGptBrowserEvent::WakeSubmitted { .. }
        | WebGptBrowserEvent::State(_)
        | WebGptBrowserEvent::Error(_)
        | WebGptBrowserEvent::ChatQueueCancelled { .. } => None,
    }
}

/// Compare the unleased ownership of a request against an active lease.
fn request_matches_correlation(
    request: &WebGptTurnRequest,
    correlation: &WebGptTurnCorrelation,
) -> bool {
    request.account_id == correlation.account_id
        && request.session_id == correlation.session_id
        && request.task_id == correlation.task_id
        && request.request_id == correlation.request_id
}

#[cfg(windows)]
pub fn run_browser_host() -> Result<(), String> {
    let controller = platform::Controller::spawn();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_browser_protocol::WebGptSlotLease;

    fn test_browser() -> SharedWebGptBrowser {
        SharedWebGptBrowser::disabled("test browser")
    }

    fn worker_request(request_id: &str) -> WebGptTurnRequest {
        WebGptTurnRequest::worker(
            "session-a".to_owned(),
            "task-a".to_owned(),
            request_id.to_owned(),
        )
    }

    fn native_request(request_id: &str) -> WebGptTurnRequest {
        WebGptTurnRequest::native_chat("session-a".to_owned(), request_id.to_owned())
    }

    fn active_request_id(inner: &SharedBrowserInner) -> &str {
        &inner
            .active_correlation
            .as_ref()
            .expect("active turn")
            .request_id
    }

    #[test]
    fn stale_terminal_cannot_release_new_fifo_turn_or_route_late_payload() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        // A matching completion frees A and dispatches B.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a.clone(),
                text: "A answer".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);

        // A late terminal from the old lease (active is now B) is dropped.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a,
                text: "late A payload".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);
        assert!(!inner.worker_events.iter().any(|event| {
            matches!(
                event,
                WebGptBrowserEvent::ChatAnswered { text, .. } if text == "late A payload"
            )
        }));
    }

    #[test]
    fn matching_terminal_advances_fifo_exactly_once() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatFailed {
                correlation: active_a.clone(),
                message: "A failed".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);

        // Re-delivering the same terminal for A must not advance again.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatFailed {
                correlation: active_a,
                message: "late A failure".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);
        drop(inner);

        // Request a cancel on B: it must move to Cancelling without freeing.
        browser.cancel_chat(worker_request("req-b"));
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 1);

        // The matching cancel acknowledgment frees B and dispatches C.
        let active_b = inner.active_correlation.clone().expect("active B");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_b.clone(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), 2);

        // Replaying the same ack does not advance again.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_b,
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.worker_events.len(), 2);
    }

    #[test]
    fn generic_error_is_diagnostic_and_does_not_advance_fifo() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::Error("runtime noise".to_owned()),
        );
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert!(matches!(
            inner.worker_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::Error(message)) if message == "runtime noise"
        ));
    }

    #[test]
    fn cancel_control_error_is_nonterminal() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        // A cancel-control failure is routed but never releases the active turn.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::Error(format!(
                "Could not cancel Web GPT request {}: script failed",
                active_a.request_id
            )),
        );
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.queued_count(), 1);
    }

    #[test]
    fn pending_cancel_script_result_is_not_an_acknowledgement() {
        let correlation = worker_request("req-a").lease(0, 3);

        assert_eq!(cancel_script_event("\"pending\"", &correlation), None);
        assert_eq!(
            cancel_script_event("\"cancelled\"", &correlation),
            Some(WebGptBrowserEvent::ChatCancelled { correlation })
        );
    }

    #[test]
    fn unavailable_state_reconciles_active_and_defers_next_submit_until_logged_in() {
        for unavailable in [
            WebGptBrowserState::LoginRequired,
            WebGptBrowserState::Offline("host ended".to_owned()),
        ] {
            let browser = test_browser();
            browser.submit_chat(worker_request("req-a"), "A".to_owned());
            browser.submit_chat(worker_request("req-b"), "B".to_owned());

            let mut inner = browser.inner.lock().expect("browser mutex poisoned");
            let active_a = inner.active_correlation.clone().expect("active A");
            assert!(inner.active_dispatched);
            handle_shared_event(&mut inner, WebGptBrowserEvent::State(unavailable.clone()));

            assert_eq!(active_request_id(&inner), "req-b");
            assert_eq!(inner.scheduler.in_flight_count(), 1);
            assert_eq!(inner.scheduler.queued_count(), 0);
            assert!(!inner.browser_ready);
            assert!(!inner.active_dispatched);
            assert!(inner.worker_events.iter().any(|event| {
                matches!(
                    event,
                    WebGptBrowserEvent::ChatFailed { correlation, .. }
                        if correlation == &active_a
                )
            }));

            handle_shared_event(
                &mut inner,
                WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn),
            );
            assert!(inner.browser_ready);
            assert!(inner.active_dispatched);
            assert_eq!(active_request_id(&inner), "req-b");
        }
    }

    #[test]
    fn wake_submitted_keeps_its_independent_routing_without_active_chat_turn() {
        let browser = test_browser();
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::WakeSubmitted {
                request_id: "web-worker-wake".to_owned(),
            },
        );
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::WakeSubmitted {
                request_id: "web-chat-wake".to_owned(),
            },
        );
        assert!(matches!(
            inner.worker_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::WakeSubmitted { request_id })
                if request_id == "web-worker-wake"
        ));
        assert!(matches!(
            inner.ui_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::WakeSubmitted { request_id }) if request_id == "web-chat-wake"
        ));
    }

    #[test]
    fn queued_cancel_emits_queue_cancelled_only_for_explicit_pending_request() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-active"), "A".to_owned());
        browser.submit_chat(worker_request("req-pending"), "B".to_owned());

        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-active");
        assert_eq!(inner.scheduler.queued_count(), 1);
        let before = inner.worker_events.len();
        drop(inner);

        // Removing a concrete queued request emits ChatQueueCancelled exactly once.
        browser.cancel_chat(worker_request("req-pending"));
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-active");
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), before + 1);
        assert!(matches!(
            inner.worker_events.back(),
            Some(WebGptBrowserEvent::ChatQueueCancelled { request })
                if request.request_id == "req-pending"
        ));

        // Cancelling a request that is neither active nor queued emits nothing.
        drop(inner);
        browser.cancel_chat(worker_request("req-unknown"));
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(inner.worker_events.len(), before + 1);
        // The unknown cancel only surfaced a bounded diagnostic, no cross-routing.
        assert!(!inner.diagnostics.is_empty());
    }

    #[test]
    fn same_request_id_reused_across_generations_is_distinct() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-reused"), "first".to_owned());
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let first = inner.active_correlation.clone().expect("first lease");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: first.clone(),
                text: "one".to_owned(),
            },
        );
        assert!(inner.active_correlation.is_none());
        drop(inner);

        // Reusing the same request id gets a fresh generation on slot 0.
        browser.submit_chat(worker_request("req-reused"), "second".to_owned());
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let second = inner.active_correlation.clone().expect("second lease");
        assert_eq!(first.request_id, second.request_id);
        assert_eq!(first.lease.slot_id, second.lease.slot_id);
        assert_ne!(first.lease.generation, second.lease.generation);

        // A late terminal from the first lease must not release the new turn.
        let before = inner.worker_events.len();
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: first,
                text: "stale".to_owned(),
            },
        );
        assert_eq!(inner.active_correlation, Some(second.clone()));
        assert_eq!(inner.worker_events.len(), before);
    }

    #[test]
    fn mismatched_owner_and_lease_are_rejected() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");

        // Wrong generation on the same slot.
        let wrong_lease = WebGptTurnCorrelation {
            lease: WebGptSlotLease {
                slot_id: active_a.lease.slot_id,
                generation: active_a.lease.generation + 1,
            },
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: wrong_lease,
                text: "wrong lease".to_owned(),
            },
        );

        // Wrong account, session, and task ownership (same request id).
        let wrong_account = WebGptTurnCorrelation {
            account_id: "other-account".to_owned(),
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: wrong_account,
            },
        );
        let wrong_session = WebGptTurnCorrelation {
            session_id: "other-session".to_owned(),
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: wrong_session,
                text: "wrong session".to_owned(),
            },
        );
        let wrong_task = WebGptTurnCorrelation {
            task_id: None,
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: wrong_task,
            },
        );

        // None of the mismatched events were routed or released the active turn.
        assert_eq!(inner.active_correlation, Some(active_a));
        assert!(inner.worker_events.is_empty());
        assert!(inner.ui_events.is_empty());
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert!(!inner.diagnostics.is_empty());
    }

    #[test]
    fn native_and_worker_events_route_by_task_ownership() {
        let browser = test_browser();
        browser.submit_chat(native_request("req-native"), "native".to_owned());
        assert_eq!(browser.inner.lock().unwrap().ui_events.len(), 0);
        assert_eq!(browser.inner.lock().unwrap().worker_events.len(), 0);

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active = inner.active_correlation.clone().expect("active native");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active.clone(),
                text: "native answer".to_owned(),
            },
        );
        assert!(matches!(
            inner.ui_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::ChatAnswered { text, .. }) if text == "native answer"
        ));
        assert!(inner.worker_events.is_empty());
        assert!(inner.active_correlation.is_none());
    }

    #[test]
    fn host_command_serde_round_trip_preserves_full_correlation() {
        let correlation = worker_request("req-a").lease(0, 7);
        let chat = BrowserHostCommand::Chat {
            correlation: correlation.clone(),
            text: "hello".to_owned(),
            attachments: Vec::new(),
        };
        let decoded: BrowserHostCommand =
            serde_json::from_str(&serde_json::to_string(&chat).unwrap()).unwrap();
        assert_eq!(decoded, chat);

        let cancel = BrowserHostCommand::Cancel {
            correlation: correlation.clone(),
        };
        let decoded: BrowserHostCommand =
            serde_json::from_str(&serde_json::to_string(&cancel).unwrap()).unwrap();
        assert_eq!(decoded, cancel);
    }

    #[test]
    fn chat_event_serde_round_trip_preserves_full_correlation() {
        let correlation = worker_request("req-a").lease(1, 9);
        let answered = WebGptBrowserEvent::ChatAnswered {
            correlation: correlation.clone(),
            text: "answer".to_owned(),
        };
        let decoded: WebGptBrowserEvent =
            serde_json::from_str(&serde_json::to_string(&answered).unwrap()).unwrap();
        assert_eq!(decoded, answered);

        let request = worker_request("req-q");
        let queued = WebGptBrowserEvent::ChatQueueCancelled {
            request: request.clone(),
        };
        let decoded: WebGptBrowserEvent =
            serde_json::from_str(&serde_json::to_string(&queued).unwrap()).unwrap();
        assert_eq!(decoded, queued);
    }

    #[test]
    fn cancel_request_alone_does_not_dispatch_queued_turn() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());

        {
            let inner = browser.inner.lock().expect("browser mutex poisoned");
            assert_eq!(active_request_id(&inner), "req-a");
            assert_eq!(inner.scheduler.queued_count(), 1);
            assert_eq!(inner.scheduler.in_flight_count(), 1);
        }

        // Requesting a cancel moves A to Cancelling but must not free it or
        // dispatch the queued C into the draining WebView.
        browser.cancel_chat(worker_request("req-a"));
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 1);
    }

    #[test]
    fn cancel_acknowledgment_dispatches_queued_turn_once() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());
        browser.cancel_chat(worker_request("req-a"));

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        assert_eq!(inner.scheduler.queued_count(), 1);

        // The correlated acknowledgment frees A and dispatches C exactly once.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_a.clone(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), 1);

        // A second (stale) ack does not advance again.
        let before = inner.worker_events.len();
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_a,
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.worker_events.len(), before);
    }

    #[test]
    fn completion_while_cancelling_is_safe() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());
        browser.cancel_chat(worker_request("req-a"));

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        // A natural completion while Cancelling is a safe terminal that frees the
        // slot and schedules the waiting turn.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a,
                text: "completed anyway".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
    }

    #[test]
    fn duplicate_request_rejection_does_not_disturb_active_turn() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        let before = {
            let inner = browser.inner.lock().expect("browser mutex poisoned");
            (inner.worker_events.len(), inner.scheduler.in_flight_count())
        };
        assert_eq!(before.1, 1);

        // Submitting the same request id again is rejected without redispatch.
        browser.submit_chat(worker_request("req-a"), "A again".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), before.0);
        // Only the original active payload remains; the duplicate was removed.
        assert_eq!(
            inner
                .turn_payloads
                .get("req-a")
                .map(|payload| payload.text.as_str()),
            Some("A")
        );
        assert!(!inner.diagnostics.is_empty());

        // The original active turn still completes normally and can dispatch the
        // next FIFO turn, proving the duplicate never corrupted the lease.
        let active_a = inner.active_correlation.clone().expect("active A");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a,
                text: "A final".to_owned(),
            },
        );
        assert!(inner.active_correlation.is_none());
        assert_eq!(inner.scheduler.in_flight_count(), 0);
        drop(inner);

        browser.submit_chat(worker_request("req-c"), "C".to_owned());
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(
            inner
                .turn_payloads
                .get("req-c")
                .map(|payload| payload.text.as_str()),
            Some("C")
        );
    }

    #[test]
    fn wrong_owner_queued_cancel_leaves_queued_turn_untouched() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-active"), "A".to_owned());
        browser.submit_chat(worker_request("req-pending"), "B".to_owned());

        {
            let inner = browser.inner.lock().expect("browser mutex poisoned");
            assert_eq!(inner.scheduler.queued_count(), 1);
            assert_eq!(inner.scheduler.in_flight_count(), 1);
            assert_eq!(inner.turn_payloads.len(), 2);
        }

        // A wrong owner sharing the queued request id (different session) must
        // not cancel the legitimate queued turn.
        let wrong_owner = WebGptTurnRequest::worker(
            "other-session".to_owned(),
            "task-a".to_owned(),
            "req-pending".to_owned(),
        );
        browser.cancel_chat(wrong_owner);

        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-active");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.turn_payloads.len(), 2);
        assert!(
            !inner
                .worker_events
                .iter()
                .any(|event| { matches!(event, WebGptBrowserEvent::ChatQueueCancelled { .. }) })
        );
        assert!(!inner.diagnostics.is_empty());
    }
}
