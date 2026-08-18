use std::{
    io::{self, BufRead, Write},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

use crate::webgpt::rpc_call;

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_WAIT_MS: u64 = 60_000;
const DEFAULT_WAIT_MS: u64 = 30_000;
const WAIT_POLL_MS: u64 = 200;

pub fn run_stdio() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("Could not read MCP stdin: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Value>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_message(
                    &mut stdout,
                    &jsonrpc_error(Value::Null, -32700, format!("Invalid JSON: {error}")),
                )?;
                continue;
            }
        };
        if let Some(response) = handle_message(&request) {
            write_message(&mut stdout, &response)?;
        }
    }
    Ok(())
}

fn write_message(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| format!("Could not encode MCP response: {error}"))?;
    writer
        .write_all(b"\n")
        .map_err(|error| format!("Could not write MCP response: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("Could not flush MCP response: {error}"))
}

fn handle_message(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str)?;
    let id = request.get("id").cloned()?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => call_tool(&params),
        "resources/list" => Ok(json!({"resources": resource_definitions()})),
        "resources/read" => read_resource(&params),
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => {
            return Some(jsonrpc_error(
                id,
                -32601,
                format!("Method not found: {method}"),
            ));
        }
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => jsonrpc_error(id, -32602, message),
    })
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSION);
    let protocol_version = if requested.is_empty() {
        MCP_PROTOCOL_VERSION
    } else {
        requested
    };
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {"listChanged": false},
            "resources": {"subscribe": false, "listChanged": false},
            "prompts": {"listChanged": false}
        },
        "serverInfo": {
            "name": "roche-multi-agent",
            "title": "Roche Multi-Agent Spawn v1",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Roche exposes Codex and Web GPT as sibling Multi-Agent Spawn v1 workers. Call worker_catalog before routing. Use spawn_agent with agent_type=worker and runtime=codex or web_gpt. fork_context=true is unsupported; pass a self-contained goal. A successful worker stops at needs_review until approve_agent or a revision through send_input. Codex supports in-flight send_input steering; Web GPT currently accepts send_input only after needs_review/failed."
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "worker_catalog",
            "List Roche Multi-Agent Spawn v1 worker runtimes and their live availability/capabilities. Both Codex and Web GPT are returned as spawnable worker choices.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        tool(
            "spawn_agent",
            "Spawn a Roche worker using the v1 lifecycle. runtime=codex starts an independent Codex app-server worker; runtime=web_gpt routes through the authenticated Roche Web GPT browser worker.",
            json!({
                "type": "object",
                "properties": {
                    "agent_type": {"type": "string", "enum": ["worker"], "default": "worker"},
                    "runtime": {"type": "string", "enum": ["codex", "web_gpt"]},
                    "goal": {"type": "string", "minLength": 1},
                    "title": {"type": "string"},
                    "parent_session_id": {"type": "string"},
                    "acceptance": {"type": "array", "items": {"type": "string"}},
                    "reasoning_effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh"]},
                    "model": {"type": "string", "description": "Codex-only model override. Web GPT rejects model overrides."},
                    "fork_context": {"type": "boolean", "enum": [false], "default": false}
                },
                "required": ["runtime", "goal"],
                "additionalProperties": false
            }),
        ),
        tool(
            "get_agent",
            "Read one Roche worker session plus its task/result state.",
            agent_id_schema(),
        ),
        tool(
            "send_input",
            "Send follow-up input to a Roche worker. Active Codex workers are steered in-flight. Web GPT workers accept revisions after needs_review/failed, not during generation.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "message": {"type": "string", "minLength": 1},
                    "reasoning_effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh"]}
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
        ),
        tool(
            "resume_agent",
            "Resume a failed Roche worker. Active or needs_review workers are returned unchanged; completed/cancelled workers are terminal.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "message": {"type": "string"},
                    "reasoning_effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh"]}
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        ),
        tool(
            "wait_agent",
            "Poll a Roche worker until it reaches needs_review or a terminal state, bounded to 60 seconds per call.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": DEFAULT_WAIT_MS}
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        ),
        tool(
            "approve_agent",
            "Approve a needs_review worker result and mark the task completed.",
            agent_id_schema(),
        ),
        tool(
            "cancel_agent",
            "Cancel a queued or running Roche worker and preserve its session record.",
            agent_id_schema(),
        ),
        tool(
            "close_agent",
            "Close a Roche worker execution slot. Running work is cancelled; review results are preserved without implicit approval.",
            agent_id_schema(),
        ),
        tool(
            "session_list",
            "List Roche root and worker sessions for the active project.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        tool(
            "project_snapshot",
            "Read the active Roche project Git status, diff stat, and changed-file list for context.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

fn agent_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"agent_id": {"type": "string"}},
        "required": ["agent_id"],
        "additionalProperties": false
    })
}

fn call_tool(params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call requires a tool name".to_owned())?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err("tools/call arguments must be an object".to_owned());
    }
    let result = match name {
        "worker_catalog" => rpc_call("worker.catalog", json!({})),
        "spawn_agent" => {
            let mut arguments = object(arguments)?;
            arguments
                .entry("agent_type".to_owned())
                .or_insert_with(|| Value::String("worker".to_owned()));
            arguments
                .entry("fork_context".to_owned())
                .or_insert(Value::Bool(false));
            if let Some(effort) = arguments.remove("reasoning_effort") {
                arguments.insert("effort".to_owned(), effort);
            }
            rpc_call("worker.spawn", Value::Object(arguments))
        }
        "get_agent" => rpc_call("worker.get", arguments),
        "send_input" => rpc_call("worker.send_input", arguments),
        "resume_agent" => rpc_call("worker.resume", arguments),
        "approve_agent" => rpc_call("worker.approve", arguments),
        "cancel_agent" => rpc_call("worker.cancel", arguments),
        "close_agent" => rpc_call("worker.close", arguments),
        "wait_agent" => wait_agent(&arguments),
        "session_list" => rpc_call("session.list", json!({})),
        "project_snapshot" => rpc_call("project.snapshot", json!({})),
        other => return Err(format!("Unknown Roche MCP tool: {other}")),
    };
    match result {
        Ok(value) => Ok(tool_result(value, false)),
        Err(message) => Ok(tool_error(message)),
    }
}

fn wait_agent(arguments: &Value) -> Result<Value, String> {
    let agent_id = arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "wait_agent requires agent_id".to_owned())?;
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT_MS)
        .min(MAX_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let value = rpc_call("worker.get", json!({"agent_id": agent_id}))?;
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            status,
            "needs_input" | "completed" | "failed" | "cancelled" | "offline"
        ) {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Ok(json!({"timed_out": true, "agent": value}));
        }
        thread::sleep(Duration::from_millis(WAIT_POLL_MS));
    }
}

fn object(value: Value) -> Result<Map<String, Value>, String> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "Expected object arguments".to_owned())
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let structured = if value.is_object() {
        value.clone()
    } else {
        json!({"value": value})
    };
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": is_error
    })
}

fn tool_error(message: String) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "structuredContent": {"error": message},
        "isError": true
    })
}

fn resource_definitions() -> Vec<Value> {
    vec![
        json!({"uri": "roche://workers/catalog", "name": "worker-catalog", "title": "Roche worker catalog", "mimeType": "application/json"}),
        json!({"uri": "roche://sessions", "name": "sessions", "title": "Roche sessions", "mimeType": "application/json"}),
        json!({"uri": "roche://project/snapshot", "name": "project-snapshot", "title": "Roche project snapshot", "mimeType": "application/json"}),
    ]
}

fn read_resource(params: &Value) -> Result<Value, String> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| "resources/read requires uri".to_owned())?;
    let value = match uri {
        "roche://workers/catalog" => rpc_call("worker.catalog", json!({}))?,
        "roche://sessions" => rpc_call("session.list", json!({}))?,
        "roche://project/snapshot" => rpc_call("project.snapshot", json!({}))?,
        other => return Err(format!("Unknown Roche resource URI: {other}")),
    };
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
}

fn jsonrpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_tools_and_resources() {
        let response = handle_message(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": MCP_PROTOCOL_VERSION}
        }))
        .expect("initialize response");
        assert_eq!(
            response
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str),
            Some(MCP_PROTOCOL_VERSION)
        );
        assert!(response.pointer("/result/capabilities/tools").is_some());
        assert!(response.pointer("/result/capabilities/resources").is_some());
    }

    #[test]
    fn worker_choices_are_visible_in_spawn_schema() {
        let tools = tool_definitions();
        let spawn = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("spawn_agent"))
            .expect("spawn_agent tool");
        assert_eq!(
            spawn.pointer("/inputSchema/properties/runtime/enum"),
            Some(&json!(["codex", "web_gpt"]))
        );
    }

    #[test]
    fn context_resources_are_advertised() {
        let resources = resource_definitions();
        assert!(
            resources
                .iter()
                .any(|resource| resource.get("uri").and_then(Value::as_str)
                    == Some("roche://workers/catalog"))
        );
        assert!(
            resources
                .iter()
                .any(|resource| resource.get("uri").and_then(Value::as_str)
                    == Some("roche://project/snapshot"))
        );
    }

    #[test]
    fn notifications_do_not_emit_stdout_responses() {
        assert!(
            handle_message(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
    }
}
