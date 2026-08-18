#[cfg(not(windows))]
fn main() {
    eprintln!("WEBGPT_MULTI_WEBVIEW_UNSUPPORTED");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    windows::run();
}

#[cfg(windows)]
mod windows {
    use std::{
        path::PathBuf,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use tao::{
        dpi::LogicalSize,
        event::Event,
        event_loop::{ControlFlow, EventLoopBuilder},
        platform::windows::EventLoopBuilderExtWindows,
        window::WindowBuilder,
    };
    use wry::{
        BackgroundThrottlingPolicy, PageLoadEvent, Rect, WebContext, WebViewBuilder,
        dpi::{LogicalPosition as WryLogicalPosition, LogicalSize as WryLogicalSize},
    };

    const SLOT_COUNT: usize = 2;
    const TIMEOUT: Duration = Duration::from_secs(20);

    #[derive(Debug)]
    enum SmokeCommand {
        Tick,
    }

    #[derive(Debug)]
    enum SlotSignal {
        Loaded(usize),
        Probed(usize),
        Failed(usize, String),
    }

    pub fn run() {
        let profile_dir = std::env::var_os("ROCHE_WEBGPT_MULTI_WEBVIEW_PROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!(
                    "roche-webgpt-multi-webview-smoke-{}",
                    std::process::id()
                ))
            });
        if let Err(error) = std::fs::create_dir_all(&profile_dir) {
            fail(format!(
                "could not create profile {}: {error}",
                profile_dir.display()
            ));
        }

        let mut event_loop_builder = EventLoopBuilder::<SmokeCommand>::with_user_event();
        event_loop_builder.with_any_thread(true);
        let event_loop = event_loop_builder.build();
        let proxy = event_loop.create_proxy();
        let window = WindowBuilder::new()
            .with_title("Roche Web GPT multi-WebView smoke")
            .with_visible(false)
            .with_inner_size(LogicalSize::new(1080.0, 820.0))
            .build(&event_loop)
            .unwrap_or_else(|error| fail(format!("could not create smoke window: {error}")));

        let (signal_tx, signal_rx) = mpsc::channel();
        let mut web_context = WebContext::new(Some(profile_dir.clone()));
        let mut webviews = Vec::with_capacity(SLOT_COUNT);
        for slot_id in 0..SLOT_COUNT {
            let slot_events = signal_tx.clone();
            let webview = WebViewBuilder::new_with_web_context(&mut web_context)
                .with_url("https://chatgpt.com/")
                .with_background_throttling(BackgroundThrottlingPolicy::Disabled)
                .with_on_page_load_handler(move |event, _url| {
                    if matches!(event, PageLoadEvent::Finished) {
                        let _ = slot_events.send(SlotSignal::Loaded(slot_id));
                    }
                })
                .build_as_child(&window)
                .unwrap_or_else(|error| {
                    fail(format!("could not create slot {slot_id} WebView2: {error}"))
                });
            if let Err(error) = webview.set_bounds(Rect {
                position: WryLogicalPosition::new(0, 0).into(),
                size: WryLogicalSize::new(1080, 820).into(),
            }) {
                fail(format!("could not size slot {slot_id}: {error}"));
            }
            webviews.push(webview);
        }

        let tick_proxy = proxy.clone();
        thread::Builder::new()
            .name("roche-multi-webview-smoke-tick".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(100));
                    if tick_proxy.send_event(SmokeCommand::Tick).is_err() {
                        break;
                    }
                }
            })
            .unwrap_or_else(|error| fail(format!("could not start smoke timer: {error}")));

        let started_at = Instant::now();
        let hold_duration = std::env::var("ROCHE_WEBGPT_MULTI_WEBVIEW_HOLD_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_default();
        let mut loaded = [false; SLOT_COUNT];
        let mut probed = [false; SLOT_COUNT];
        let mut probes_started = false;
        let mut ready_at = None;
        let _web_context = web_context;

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            if !matches!(event, Event::UserEvent(SmokeCommand::Tick)) {
                return;
            }

            while let Ok(signal) = signal_rx.try_recv() {
                match signal {
                    SlotSignal::Loaded(slot_id) => loaded[slot_id] = true,
                    SlotSignal::Probed(slot_id) => probed[slot_id] = true,
                    SlotSignal::Failed(slot_id, message) => {
                        eprintln!("WEBGPT_MULTI_WEBVIEW_SLOT_FAILED slot={slot_id} {message}");
                        *control_flow = ControlFlow::ExitWithCode(3);
                        return;
                    }
                }
            }

            if loaded.iter().all(|value| *value) && !probes_started {
                probes_started = true;
                for (slot_id, webview) in webviews.iter().enumerate() {
                    let callback_tx = signal_tx.clone();
                    if let Err(error) = webview.evaluate_script_with_callback(
                        "JSON.stringify({ href: location.href, readyState: document.readyState })",
                        move |value| {
                            let signal = if value.trim().is_empty() || value.trim() == "null" {
                                SlotSignal::Failed(slot_id, "empty probe response".to_owned())
                            } else {
                                SlotSignal::Probed(slot_id)
                            };
                            let _ = callback_tx.send(signal);
                        },
                    ) {
                        let _ = signal_tx.send(SlotSignal::Failed(slot_id, error.to_string()));
                    }
                }
            }

            if probed.iter().all(|value| *value) {
                let ready_since = ready_at.get_or_insert_with(|| {
                    println!(
                        "WEBGPT_MULTI_WEBVIEW_READY slots={SLOT_COUNT} profile={}",
                        profile_dir.display()
                    );
                    Instant::now()
                });
                if ready_since.elapsed() >= hold_duration {
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }

            if started_at.elapsed() >= TIMEOUT {
                eprintln!("WEBGPT_MULTI_WEBVIEW_TIMEOUT loaded={loaded:?} probed={probed:?}");
                *control_flow = ControlFlow::ExitWithCode(4);
            }
        });
    }

    fn fail(message: String) -> ! {
        eprintln!("WEBGPT_MULTI_WEBVIEW_FAILED {message}");
        std::process::exit(1);
    }
}
