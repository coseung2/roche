use std::{
    thread,
    time::{Duration, Instant},
};

use roche_workstation::codex::{CodexConnection, CodexEvent, CodexRuntimeController};
use roche_workstation::web_browser::SharedWebGptBrowser;

fn main() {
    let root = std::env::current_dir().expect("current directory");
    let runtime = CodexRuntimeController::spawn_with_web_browser(
        root,
        SharedWebGptBrowser::disabled("Web GPT disabled for Codex smoke"),
    );
    let model = std::env::var("ROCHE_CODEX_SMOKE_MODEL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut sent = false;
    let mut answer = String::new();

    while Instant::now() < deadline {
        for event in runtime.drain() {
            match event {
                CodexEvent::Connection(CodexConnection::Ready { version }) if !sent => {
                    println!("CODEX_READY {version}");
                    println!(
                        "MODEL_OVERRIDE {}",
                        model.as_deref().unwrap_or("<configured-default>")
                    );
                    runtime.send(
                        "Do not edit files or run commands. Reply with exactly ROCHE_DIRECT_CODEX_READY."
                            .to_owned(),
                        "low".to_owned(),
                        model.clone(),
                    );
                    sent = true;
                }
                CodexEvent::Connection(CodexConnection::Offline { message }) => {
                    eprintln!("CODEX_OFFLINE {message}");
                    std::process::exit(2);
                }
                CodexEvent::AssistantDelta { delta, .. } => answer.push_str(&delta),
                CodexEvent::AssistantCompleted { text, .. } => answer = text,
                CodexEvent::TurnCompleted { status, .. } => {
                    println!("TURN_{status}");
                    println!("ANSWER {}", answer.trim());
                    if answer.contains("ROCHE_DIRECT_CODEX_READY") {
                        return;
                    }
                    std::process::exit(3);
                }
                CodexEvent::Error(message) => eprintln!("CODEX_ERROR {message}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    eprintln!("CODEX_SMOKE_TIMEOUT answer={}", answer.trim());
    std::process::exit(4);
}
