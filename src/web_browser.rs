use std::{
    io::{BufRead, BufReader, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

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
    WakeSubmitted { request_id: String },
    ChatSubmitted { request_id: String },
    ChatAnswered { request_id: String, text: String },
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BrowserAttachment {
    name: String,
    mime: String,
    data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BrowserHostCommand {
    ShowLogin,
    Hide,
    Wake {
        request_id: String,
    },
    Chat {
        request_id: String,
        text: String,
        attachments: Vec<BrowserAttachment>,
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

    use super::{BrowserHostCommand, WebGptBrowserEvent, WebGptBrowserState};

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
                                break;
                            }
                        }
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
            request_id: String,
            text: String,
            attachments: Vec<super::BrowserAttachment>,
        ) {
            let _ = self.commands.send(BrowserHostCommand::Chat {
                request_id,
                text,
                attachments,
            });
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

    use super::{WebGptBrowserEvent, WebGptBrowserState};

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
            request_id: String,
            text: String,
            attachments: Vec<super::BrowserAttachment>,
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
            request_id: String,
            text: String,
            attachments: Vec<super::BrowserAttachment>,
        ) {
            self.send(BrowserCommand::Chat {
                request_id,
                text,
                attachments,
            });
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
                    thread::sleep(Duration::from_secs(2));
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
                    request_id,
                    text,
                    attachments,
                }) => {
                    submit_chat_prompt(&webview, request_id, text, attachments, events.clone());
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
        submit_prompt(webview, request_id, wake_text, Vec::new(), events, false);
    }

    fn submit_chat_prompt(
        webview: &wry::WebView,
        request_id: String,
        text: String,
        attachments: Vec<super::BrowserAttachment>,
        events: Sender<WebGptBrowserEvent>,
    ) {
        submit_prompt(webview, request_id, text, attachments, events, true);
    }

    fn submit_prompt(
        webview: &wry::WebView,
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
        let capture = if capture_answer { "true" } else { "false" };
        let script = format!(
            r#"(() => {{
                const rawText = {encoded_text};
                const attachments = {encoded_attachments};
                const text = rawText || (attachments.length ? '첨부 파일을 확인해 주세요.' : rawText);
                const requestId = {encoded_request_id};
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
                        text,
                        clicked: false,
                        failed: false,
                        submittedEmitted: false,
                        attempts: 0
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
                    let _ = callback_events.send(WebGptBrowserEvent::Error(
                        "ChatGPT file input was not available for attachment upload".to_owned(),
                    ));
                }
                "login_required" => {
                    let _ = callback_events
                        .send(WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired));
                }
                other => {
                    let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                        "ChatGPT request was not submitted: {other}"
                    )));
                }
            }
        }) {
            let _ = events.send(WebGptBrowserEvent::Error(format!(
                "Could not submit ChatGPT request: {error}"
            )));
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
                    detail: `send button unavailable after ${pending.attempts} attempts`
                });
                sessionStorage.removeItem('__rochePendingChat');
                return result;
            }

            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
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
                return JSON.stringify({ kind: 'submitted', request_id: pending.requestId });
            }

            const generating = document.querySelector('button[data-testid="stop-button"]')
                || document.querySelector('button[aria-label*="Stop"]')
                || document.querySelector('button[aria-label*="stop"]')
                || document.querySelector('button[aria-label*="중지"]');
            if (generating) return null;

            let text = '';
            if (userIndex >= 0) {
                let assistant = null;
                for (let index = userIndex + 1; index < messages.length; index += 1) {
                    if (messages[index].getAttribute('data-message-author-role') === 'assistant') {
                        assistant = messages[index];
                    }
                }
                text = (assistant?.innerText || assistant?.textContent || '').trim();
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
                    text = answer.slice(0, answerEnd).trim();
                }
            }

            if (!text) return null;
            const result = JSON.stringify({ kind: 'answered', request_id: pending.requestId, text });
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
            let Some(request_id) = payload
                .get("request_id")
                .and_then(serde_json::Value::as_str)
            else {
                return;
            };
            match payload.get("kind").and_then(serde_json::Value::as_str) {
                Some("submitted") => {
                    let _ = callback_events.send(WebGptBrowserEvent::ChatSubmitted {
                        request_id: request_id.to_owned(),
                    });
                }
                Some("answered") => {
                    let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) else {
                        return;
                    };
                    let _ = callback_events.send(WebGptBrowserEvent::ChatAnswered {
                        request_id: request_id.to_owned(),
                        text: text.to_owned(),
                    });
                }
                Some("error") => {
                    let detail = payload
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown ChatGPT submit error");
                    let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                        "ChatGPT request {request_id} failed: {detail}"
                    )));
                }
                Some("probe") if std::env::var_os("ROCHE_WEBGPT_DIAGNOSTICS").is_some() => {
                    let detail = payload
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no probe detail");
                    let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                        "ChatGPT probe {request_id}: {detail}"
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

    use super::{WebGptBrowserEvent, WebGptBrowserState};

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
            _request_id: String,
            _text: String,
            _attachments: Vec<super::BrowserAttachment>,
        ) {
        }
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

    pub fn submit_chat(&self, request_id: String, text: String) {
        self.submit_chat_with_attachments(request_id, text, Vec::new());
    }

    pub fn submit_chat_with_attachments(
        &self,
        request_id: String,
        text: String,
        paths: Vec<std::path::PathBuf>,
    ) {
        let attachments = browser_attachments_from_paths(&paths);
        match &self.backend {
            #[cfg(windows)]
            BrowserBackend::Process(controller) => {
                controller.submit_chat(request_id, text, attachments)
            }
            BrowserBackend::InProcess(controller) => {
                controller.submit_chat(request_id, text, attachments)
            }
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
                request_id,
                text,
                attachments,
            }) => controller.submit_chat(request_id, text, attachments),
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
