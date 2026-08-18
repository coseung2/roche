use std::{
    env, thread,
    time::{Duration, Instant},
};

use roche_workstation::webgpt::rpc_call;
use serde_json::{Value, json};

fn main() {
    match run() {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            );
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<Value, String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return Err(help());
    };
    match command {
        "health" | "status" => rpc_call("health", json!({})),
        "chat-submit" => {
            let text = flag_value(&args, "--text")?;
            let reasoning_level =
                optional_flag_value(&args, "--reasoning").unwrap_or_else(|| "very_high".to_owned());
            rpc_call(
                "chat.submit",
                json!({"text": text, "reasoning_level": reasoning_level}),
            )
        }
        "chat-pending" => rpc_call("chat.pending", json!({})),
        "chat-wait" => {
            let timeout = optional_flag_value(&args, "--timeout")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("Invalid --timeout seconds: {value}"))
                })
                .transpose()?
                .unwrap_or(240)
                .min(240);
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let pending = rpc_call("chat.pending", json!({}))?;
                if !pending.is_null() {
                    break Ok(pending);
                }
                if Instant::now() >= deadline {
                    break Ok(Value::Null);
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
        "chat-poll" => {
            let request_id = positional(&args, 1, "chat request id")?;
            rpc_call("chat.poll", json!({"request_id": request_id}))
        }
        "chat-release" => {
            let request_id = positional(&args, 1, "chat request id")?;
            rpc_call("chat.release", json!({"request_id": request_id}))
        }
        "chat-respond" => {
            let request_id = positional(&args, 1, "chat request id")?;
            let text = flag_value(&args, "--text")?;
            rpc_call(
                "chat.respond",
                json!({"request_id": request_id, "text": text}),
            )
        }
        "chat-cancel" => {
            let request_id = positional(&args, 1, "chat request id")?;
            rpc_call("chat.cancel", json!({"request_id": request_id}))
        }
        "session-list" => rpc_call("session.list", json!({})),
        "session-get" => {
            let session_id = positional(&args, 1, "session id")?;
            rpc_call("session.get", json!({"session_id": session_id}))
        }
        "session-spawn" => {
            let runtime = flag_value(&args, "--runtime")?;
            let parent_session_id = optional_flag_value(&args, "--parent");
            let title = optional_flag_value(&args, "--title");
            let goal = optional_flag_value(&args, "--goal");
            let effort = optional_flag_value(&args, "--effort");
            let model = optional_flag_value(&args, "--model");
            let acceptance = repeated_flag_values(&args, "--accept");
            rpc_call(
                "session.spawn",
                json!({
                    "runtime": runtime,
                    "parent_session_id": parent_session_id,
                    "title": title,
                    "goal": goal,
                    "effort": effort,
                    "model": model,
                    "acceptance": acceptance,
                }),
            )
        }
        "session-status" => {
            let session_id = positional(&args, 1, "session id")?;
            let status = flag_value(&args, "--status")?;
            rpc_call(
                "session.status",
                json!({"session_id": session_id, "status": status}),
            )
        }
        "session-workers" => {
            let session_id = positional(&args, 1, "session id")?;
            rpc_call("session.workers", json!({"session_id": session_id}))
        }
        "session-events" => {
            let after = optional_flag_value(&args, "--after")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("Invalid --after sequence: {value}"))
                })
                .transpose()?
                .unwrap_or(0);
            rpc_call("session.events", json!({"after": after}))
        }
        "list" => rpc_call("task.list", json!({})),
        "get" => {
            let task_id = positional(&args, 1, "task id")?;
            rpc_call("task.get", json!({"task_id": task_id}))
        }
        "create" => {
            let goal = flag_value(&args, "--goal")?;
            let title =
                optional_flag_value(&args, "--title").unwrap_or_else(|| "Web GPT task".to_owned());
            let effort =
                optional_flag_value(&args, "--effort").unwrap_or_else(|| "high".to_owned());
            let acceptance = repeated_flag_values(&args, "--accept");
            rpc_call(
                "task.create",
                json!({
                    "title": title,
                    "goal": goal,
                    "effort": effort,
                    "acceptance": acceptance,
                }),
            )
        }
        "revise" => {
            let task_id = positional(&args, 1, "task id")?;
            let prompt = flag_value(&args, "--prompt")?;
            let effort = optional_flag_value(&args, "--effort");
            rpc_call(
                "task.revise",
                json!({"task_id": task_id, "prompt": prompt, "effort": effort}),
            )
        }
        "cancel" => {
            let task_id = positional(&args, 1, "task id")?;
            rpc_call("task.cancel", json!({"task_id": task_id}))
        }
        "approve" => {
            let task_id = positional(&args, 1, "task id")?;
            rpc_call("task.approve", json!({"task_id": task_id}))
        }
        "events" => {
            let after = optional_flag_value(&args, "--after")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("Invalid --after sequence: {value}"))
                })
                .transpose()?
                .unwrap_or(0);
            let task_id = optional_flag_value(&args, "--task");
            rpc_call("task.events", json!({"after": after, "task_id": task_id}))
        }
        "snapshot" | "diff" => rpc_call("project.snapshot", json!({})),
        "help" | "--help" | "-h" => Err(help()),
        other => Err(format!("Unknown command: {other}\n\n{}", help())),
    }
}

fn positional(args: &[String], index: usize, label: &str) -> Result<String, String> {
    args.get(index)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
        .map(str::to_owned)
        .ok_or_else(|| format!("Missing {label}"))
}

fn flag_value(args: &[String], flag: &str) -> Result<String, String> {
    optional_flag_value(args, flag).ok_or_else(|| format!("Missing {flag} <value>"))
}

fn optional_flag_value(args: &[String], flag: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + 1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
        .map(str::to_owned)
}

fn repeated_flag_values(args: &[String], flag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == flag
            && let Some(value) = args.get(index + 1)
            && !value.trim().is_empty()
            && !value.starts_with("--")
        {
            values.push(value.trim().to_owned());
            index += 2;
            continue;
        }
        index += 1;
    }
    values
}

fn help() -> String {
    [
        "Roche Web GPT -> Rust Orchestrator control CLI",
        "",
        "Commands:",
        "  roche_web health",
        "  roche_web chat-submit --text <message> [--reasoning very_high]",
        "  roche_web chat-pending",
        "  roche_web chat-wait [--timeout <seconds, max 240>]",
        "  roche_web chat-poll <request-id>",
        "  roche_web chat-release <request-id>",
        "  roche_web chat-respond <request-id> --text <answer>",
        "  roche_web chat-cancel <request-id>",
        "  roche_web session-list",
        "  roche_web session-get <session-id>",
        "  roche_web session-spawn --runtime web_gpt|codex [--parent <session-id>] [--title <title>] [--goal <goal>] [--accept <criterion>]... [--effort low|high|xhigh] [--model <slug>]",
        "  roche_web session-status <session-id> --status idle|running|waiting_on_workers|needs_input|completed|failed|cancelled|offline",
        "  roche_web session-workers <session-id>",
        "  roche_web session-events [--after <seq>]",
        "  roche_web create --title <title> --goal <goal> [--accept <criterion>]... [--effort low|high|xhigh]", 
        "  roche_web list",
        "  roche_web get <task-id>",
        "  roche_web revise <task-id> --prompt <revision> [--effort low|high|xhigh]",
        "  roche_web cancel <task-id>",
        "  roche_web approve <task-id>",
        "  roche_web events [--task <task-id>] [--after <seq>]",
        "  roche_web snapshot",
        "",
        "This CLI never speaks the Codex app-server protocol. It only calls the Rust Orchestrator bridge owned by the running Roche app.",
    ]
    .join("\n")
}
