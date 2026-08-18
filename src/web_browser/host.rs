//! Wry/WebView2 host lifecycle and native event-loop adapter.

use super::{BrowserAttachment, WebGptBrowserEvent, WebGptBrowserState, WebGptTurnCorrelation};

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

    use super::super::scripts::{
        cancel_chat_prompt, probe_chat_state, probe_login_state, submit_chat_prompt,
        submit_wake_prompt,
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

pub(super) use platform::Controller;
