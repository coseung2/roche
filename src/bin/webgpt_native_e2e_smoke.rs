use std::{
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use roche_workstation::web_browser::{
    WebGptBrowserController, WebGptBrowserEvent, WebGptBrowserState,
};
use roche_workstation::web_browser_protocol::WebGptTurnRequest;

const EXPECTED: &str = "ROCHE_NATIVE_WEBGPT_READY";

fn main() {
    let browser = WebGptBrowserController::spawn();
    let login_deadline = Instant::now() + Duration::from_secs(15);
    let mut browser_ready = false;

    while Instant::now() < login_deadline {
        for event in browser.drain() {
            println!("BROWSER {event:?}");
            match event {
                WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn) => browser_ready = true,
                WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired) => {
                    println!("WEBGPT_E2E_ANONYMOUS_SESSION");
                    browser_ready = true;
                }
                WebGptBrowserEvent::State(WebGptBrowserState::Offline(message)) => {
                    eprintln!("WEBGPT_E2E_BROWSER_OFFLINE {message}");
                    std::process::exit(2);
                }
                _ => {}
            }
        }
        if browser_ready {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    if !browser_ready {
        eprintln!("WEBGPT_E2E_BROWSER_NOT_READY");
        std::process::exit(3);
    }

    let request_id = format!(
        "web-smoke-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    // Build an explicit unleased request and lease the single slot (generation 1)
    // so the smoke run correlates the answer against the full ownership.
    let request =
        WebGptTurnRequest::native_chat("web-smoke-session".to_owned(), request_id.clone());
    let correlation = request.lease(0, 1);
    browser.submit_chat(
        correlation.clone(),
        format!("Reply with exactly {EXPECTED} and nothing else."),
    );

    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        for event in browser.drain() {
            println!("BROWSER {event:?}");
            match event {
                WebGptBrowserEvent::ChatAnswered {
                    correlation: answered_correlation,
                    text,
                } if answered_correlation == correlation => {
                    if text.trim() == EXPECTED {
                        println!("WEBGPT_NATIVE_E2E_READY {EXPECTED}");
                        return;
                    }
                    eprintln!("WEBGPT_E2E_WRONG_ANSWER {text:?}");
                    std::process::exit(4);
                }
                WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired) => {
                    eprintln!("WEBGPT_E2E_LOGIN_LOST");
                    std::process::exit(5);
                }
                WebGptBrowserEvent::Error(message) => {
                    eprintln!("WEBGPT_E2E_BROWSER_ERROR {message}");
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    eprintln!("WEBGPT_E2E_TIMEOUT request_id={request_id}");
    std::process::exit(6);
}
