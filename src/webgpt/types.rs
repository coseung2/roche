//! Shared Web GPT orchestration models and deterministic helpers.

use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sessions::{SessionRuntime, SessionStatus};

static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorTaskStatus {
    Queued,
    Preparing,
    RunningCodex,
    RunningWebGpt,
    NeedsReview,
    Failed,
    Cancelled,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorTask {
    pub id: String,
    pub session_id: Option<String>,
    pub title: String,
    pub goal: String,
    pub acceptance: Vec<String>,
    pub effort: String,
    pub status: OrchestratorTaskStatus,
    pub turn_id: Option<String>,
    pub result: Option<String>,
    pub tool_activity: Vec<String>,
    pub revision_count: u32,
    pub cancel_requested: bool,
    pub created_at_ms: u128,
    pub updated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorEvent {
    pub seq: u64,
    pub task_id: Option<String>,
    pub event: String,
    pub summary: String,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub root: String,
    pub status_short: String,
    pub diff_stat: String,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebChatStatus {
    Pending,
    Claimed,
    Answered,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebChatRequest {
    pub id: String,
    pub text: String,
    pub reasoning_level: String,
    pub status: WebChatStatus,
    pub response: Option<String>,
    pub created_at_ms: u128,
    pub updated_at_ms: u128,
}

pub(super) fn orchestrator_prompt(task: &OrchestratorTask) -> String {
    let acceptance = if task.acceptance.is_empty() {
        "- Preserve existing behavior outside the requested change.\n- Run relevant deterministic checks before reporting done."
            .to_owned()
    } else {
        task.acceptance
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Roche Task {}\nTitle: {}\n\nGoal:\n{}\n\nAcceptance criteria:\n{}\n\nRuntime contract:\n- Work only in the current Roche project root.\n- Inspect applicable AGENTS.md before editing.\n- Implement the goal; do not redesign unrelated UI or architecture.\n- Run relevant tests/checks.\n- Finish with a concise summary of changes and verification.\n- Do not mark the product task complete yourself; Rust Orchestrator owns completion state.",
        task.id, task.title, task.goal, acceptance
    )
}

pub(super) fn project_snapshot(root: &Path) -> Result<ProjectSnapshot, String> {
    let status_short = git_output(root, ["status", "--short"])?;
    let diff_stat = git_output(root, ["diff", "--stat"])?;
    let changed_files = git_output(root, ["diff", "--name-only"])?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(ProjectSnapshot {
        root: root.display().to_string(),
        status_short,
        diff_stat,
        changed_files,
    })
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("Could not run git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(super) fn required_string(params: &Value, name: &str) -> Result<String, String> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Missing non-empty parameter: {name}"))
}

pub(super) fn parse_session_runtime(value: &str) -> Result<SessionRuntime, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "unified" | "mixed" => Ok(SessionRuntime::Unified),
        "web" | "web_gpt" | "webgpt" | "gpt" | "gpt-5.6" => Ok(SessionRuntime::WebGpt),
        "codex" => Ok(SessionRuntime::Codex),
        other => Err(format!("Unsupported session runtime: {other}")),
    }
}

pub(super) fn parse_session_status(value: &str) -> Result<SessionStatus, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "idle" => Ok(SessionStatus::Idle),
        "running" => Ok(SessionStatus::Running),
        "waiting_on_workers" | "waiting" => Ok(SessionStatus::WaitingOnWorkers),
        "needs_input" => Ok(SessionStatus::NeedsInput),
        "completed" => Ok(SessionStatus::Completed),
        "failed" => Ok(SessionStatus::Failed),
        "cancelled" | "canceled" => Ok(SessionStatus::Cancelled),
        "offline" => Ok(SessionStatus::Offline),
        other => Err(format!("Unsupported session status: {other}")),
    }
}

pub(super) fn normalize_effort(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" | "fast" | "빠름" => Ok("low".to_owned()),
        "medium" => Ok("medium".to_owned()),
        "high" | "높음" => Ok("high".to_owned()),
        "xhigh" | "very_high" | "very-high" | "매우 높음" => Ok("xhigh".to_owned()),
        other => Err(format!("Unsupported reasoning effort: {other}")),
    }
}

pub(super) fn next_task_id() -> String {
    let count = next_counter();
    format!("task-{}-{count}", now_ms())
}

pub(super) fn next_chat_id() -> String {
    format!("chat-{}-{}", now_ms(), next_counter())
}

pub(super) fn next_local_chat_id() -> String {
    format!("local-chat-{}-{}", now_ms(), next_counter())
}

fn next_counter() -> u64 {
    TASK_COUNTER.fetch_add(1, Ordering::Relaxed)
}

pub(super) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter_suffix(id: &str) -> u64 {
        id.rsplit_once('-')
            .expect("counter suffix")
            .1
            .parse()
            .expect("numeric counter")
    }

    #[test]
    fn task_chat_and_local_ids_share_one_monotonic_counter() {
        let task = next_task_id();
        let chat = next_chat_id();
        let local = next_local_chat_id();
        assert!(task.starts_with("task-"));
        assert!(chat.starts_with("chat-"));
        assert!(local.starts_with("local-chat-"));
        assert!(counter_suffix(&task) < counter_suffix(&chat));
        assert!(counter_suffix(&chat) < counter_suffix(&local));
    }
}
