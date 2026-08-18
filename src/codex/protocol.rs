//! Pure Codex app-server result parsing and request-shape construction.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use super::types::{
    CodexActivity, CodexActivityKind, CodexActivityPhase, CodexHistoryMessage, CodexHistoryRole,
    CodexStoredThread,
};

pub(super) fn codex_stored_threads_from_result(result: &Value) -> Vec<CodexStoredThread> {
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|thread| {
            let thread_id = thread.get("id")?.as_str()?.to_owned();
            let cwd = thread.get("cwd")?.as_str().map(PathBuf::from)?;
            let name = thread
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let preview = thread
                .get("preview")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            Some(CodexStoredThread {
                thread_id,
                name,
                preview,
                cwd,
                parent_thread_id: thread
                    .get("parentThreadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                created_at: thread
                    .get("createdAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                updated_at: thread
                    .get("updatedAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

pub(super) fn codex_history_from_result(result: &Value) -> Vec<CodexHistoryMessage> {
    let Some(turns) = result.pointer("/thread/turns").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for turn in turns {
        let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("userMessage") => {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        messages.push(CodexHistoryMessage {
                            role: CodexHistoryRole::User,
                            text,
                            turn_id: turn_id.clone(),
                        });
                    }
                }
                Some("agentMessage") => {
                    if item.get("phase").and_then(Value::as_str) == Some("commentary") {
                        continue;
                    }
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    if !text.is_empty() {
                        messages.push(CodexHistoryMessage {
                            role: CodexHistoryRole::Assistant,
                            text,
                            turn_id: turn_id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    messages
}

pub(super) fn codex_activity_from_item(
    method: &str,
    item: &Value,
    turn_id: &str,
) -> Option<CodexActivity> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    let phase = match item.get("status").and_then(Value::as_str) {
        Some(status)
            if matches!(
                status.to_ascii_lowercase().as_str(),
                "failed" | "declined" | "error" | "cancelled" | "canceled"
            ) =>
        {
            CodexActivityPhase::Failed
        }
        _ if method == "item/started" => CodexActivityPhase::Running,
        _ => CodexActivityPhase::Completed,
    };
    let (kind, title, detail, worker_thread_id, worker_status) = match item_type {
        "commandExecution" => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("command")
                .to_owned();
            (
                CodexActivityKind::Terminal,
                "명령 실행".to_owned(),
                command,
                None,
                None,
            )
        }
        "fileChange" => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let paths = changes
                .iter()
                .filter_map(|change| {
                    change
                        .get("path")
                        .or_else(|| change.get("filePath"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>();
            let count = changes.len();
            let detail = if paths.is_empty() {
                format!("{count}개 파일")
            } else {
                paths.join("\n")
            };
            (
                CodexActivityKind::FileChange,
                format!("{count}개 파일 변경"),
                detail,
                None,
                None,
            )
        }
        "mcpToolCall" | "dynamicToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
            let tool = item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let title = if server == "mcp" {
                tool.to_owned()
            } else {
                format!("{server}/{tool}")
            };
            (
                CodexActivityKind::ToolCall,
                title.clone(),
                title,
                None,
                None,
            )
        }
        "webSearch" => {
            let query = item
                .get("query")
                .or_else(|| item.get("searchQuery"))
                .and_then(Value::as_str)
                .unwrap_or("웹 검색")
                .to_owned();
            (
                CodexActivityKind::WebSearch,
                "웹 검색".to_owned(),
                query,
                None,
                None,
            )
        }
        "collabToolCall" | "collabAgentToolCall" => {
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("worker");
            let worker_thread_id = item
                .get("newThreadId")
                .or_else(|| item.get("receiverThreadId"))
                .or_else(|| item.get("new_thread_id"))
                .or_else(|| item.get("receiver_thread_id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let worker_status = item
                .get("agentStatus")
                .or_else(|| item.get("agent_status"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let prompt = item
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(tool)
                .to_owned();
            let title = match tool {
                "spawn_agent" => "워커 생성",
                "send_input" => "워커 입력",
                "resume_agent" => "워커 재개",
                "wait" => "워커 대기",
                "close_agent" => "워커 종료",
                _ => "워커 작업",
            }
            .to_owned();
            (
                CodexActivityKind::Worker,
                title,
                prompt,
                worker_thread_id,
                worker_status,
            )
        }
        _ => return None,
    };
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{turn_id}:{item_type}:{title}:{detail}"));
    Some(CodexActivity {
        item_id,
        kind,
        phase,
        title,
        detail,
        worker_thread_id,
        worker_status,
    })
}

pub(super) fn codex_user_input(text: String, attachments: &[PathBuf]) -> Vec<Value> {
    let mut input = Vec::with_capacity(1 + attachments.len());
    if !text.is_empty() {
        input.push(json!({"type": "text", "text": text, "textElements": []}));
    }
    for path in attachments {
        let path_text = path.to_string_lossy().into_owned();
        if is_image_attachment(path) {
            input.push(json!({"type": "localImage", "path": path_text}));
        } else {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("attachment")
                .to_owned();
            input.push(json!({"type": "mention", "name": name, "path": path_text}));
        }
    }
    input
}

pub(super) fn is_image_attachment(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| {
            matches!(
                extension.as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
}

pub(super) fn turn_start_params(
    thread_id: &str,
    text: String,
    attachments: &[PathBuf],
    effort: String,
    model: Option<String>,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": codex_user_input(text, attachments),
        "effort": effort
    });
    if let Some(model) = model {
        params["model"] = Value::String(model);
    }
    params
}
