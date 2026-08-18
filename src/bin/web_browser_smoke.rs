use std::{
    thread,
    time::{Duration, Instant},
};

use roche_workstation::web_browser::{
    WebGptBrowserController, WebGptBrowserEvent, WebGptBrowserState,
};

fn main() {
    let browser = WebGptBrowserController::spawn_in_process();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut terminal_state = None;
    while Instant::now() < deadline {
        for event in browser.drain() {
            println!("{event:?}");
            if let WebGptBrowserEvent::State(state) = event
                && !matches!(state, WebGptBrowserState::Starting)
            {
                terminal_state = Some(state);
            }
        }
        if terminal_state.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    if let Ok(seconds) = std::env::var("ROCHE_WEB_BROWSER_SMOKE_HOLD_SECONDS")
        && let Ok(seconds) = seconds.parse::<u64>()
        && seconds > 0
    {
        let hold_deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < hold_deadline {
            for event in browser.drain() {
                println!("{event:?}");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    match terminal_state {
        Some(WebGptBrowserState::LoggedIn) => println!("WEBVIEW2_CHATGPT_LOGGED_IN"),
        Some(WebGptBrowserState::LoginRequired) => println!("WEBVIEW2_CHATGPT_LOGIN_REQUIRED"),
        Some(WebGptBrowserState::Offline(message)) => {
            eprintln!("WEBVIEW2_OFFLINE {message}");
            std::process::exit(2);
        }
        Some(WebGptBrowserState::Starting) | None => {
            eprintln!("WEBVIEW2_STATUS_TIMEOUT");
            std::process::exit(3);
        }
    }
}
