#[allow(dead_code)]
#[path = "../codex.rs"]
mod codex;
#[allow(dead_code)]
#[path = "../sessions.rs"]
mod sessions;
#[allow(dead_code)]
#[path = "../web_browser.rs"]
mod web_browser;
#[allow(dead_code)]
#[path = "../web_browser_pool.rs"]
mod web_browser_pool;
#[allow(dead_code)]
#[path = "../web_browser_protocol.rs"]
mod web_browser_protocol;
#[allow(dead_code)]
#[path = "../webgpt.rs"]
mod webgpt;

use std::{path::PathBuf, thread, time::Duration};

use codex::{CodexConnection, CodexEvent, CodexRuntimeController};
use serde_json::json;

fn main() {
    let project_root = std::env::var_os("ROCHE_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let runtime = CodexRuntimeController::spawn_with_web_browser(
        project_root,
        web_browser::SharedWebGptBrowser::disabled("Web GPT disabled for bridge smoke"),
    );
    let mut ready = false;
    for _ in 0..200 {
        for event in runtime.drain() {
            match event {
                CodexEvent::Connection(CodexConnection::Ready { version }) => {
                    eprintln!("CODEX_READY {version}");
                    ready = true;
                }
                CodexEvent::Connection(CodexConnection::Offline { message }) => {
                    eprintln!("CODEX_OFFLINE {message}");
                }
                _ => {}
            }
        }
        if ready {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !ready {
        eprintln!("Codex initialize handshake did not become ready");
        std::process::exit(2);
    }
    thread::sleep(Duration::from_millis(100));
    let health = match webgpt::rpc_call("health", json!({})) {
        Ok(value) => value,
        Err(message) => {
            eprintln!("BRIDGE_ERROR {message}");
            std::process::exit(3);
        }
    };
    println!("{}", serde_json::to_string_pretty(&health).unwrap());

    let submitted = webgpt::rpc_call(
        "chat.submit",
        json!({"text": "bridge smoke", "reasoning_level": "very_high"}),
    )
    .expect("chat.submit failed");
    let request_id = submitted["id"]
        .as_str()
        .expect("missing chat id")
        .to_owned();
    let pending = webgpt::rpc_call("chat.pending", json!({})).expect("chat.pending failed");
    assert_eq!(pending["id"].as_str(), Some(request_id.as_str()));
    webgpt::rpc_call(
        "chat.respond",
        json!({"request_id": request_id, "text": "WEBGPT_BRIDGE_READY"}),
    )
    .expect("chat.respond failed");
    let polled = webgpt::rpc_call(
        "chat.poll",
        json!({"request_id": submitted["id"].as_str().unwrap()}),
    )
    .expect("chat.poll failed");
    assert_eq!(polled["response"].as_str(), Some("WEBGPT_BRIDGE_READY"));
    println!("CHAT_READY WEBGPT_BRIDGE_READY");
}
