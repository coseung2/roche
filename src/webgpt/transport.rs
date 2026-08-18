//! Loopback JSON-RPC transport, capability descriptor, and bridge lifecycle.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        mpsc::{Receiver, Sender},
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    codex::{CodexCommand, CodexEvent},
    web_browser::SharedWebGptBrowser,
};

use super::{BridgeState, RpcError, RpcRequest, RpcResponse, WebWorkerCommand};

pub const DEFAULT_WEBGPT_BRIDGE_ADDR: &str = "127.0.0.1:47831";
const BRIDGE_DESCRIPTOR_RELATIVE_PATH: &str = ".ai-bridge/roche-webgpt-runtime.json";
static IN_PROCESS_BRIDGE: OnceLock<BridgeClientConfig> = OnceLock::new();
static BRIDGE_REBIND: OnceLock<Sender<BridgeRebind>> = OnceLock::new();
static BRIDGE_CURRENT_ROOT: OnceLock<Mutex<PathBuf>> = OnceLock::new();

struct BridgeRebind {
    project_root: PathBuf,
    commands: Sender<CodexCommand>,
    codex_events: Receiver<CodexEvent>,
    web_browser: SharedWebGptBrowser,
}

#[derive(Clone, Serialize, Deserialize)]
struct BridgeClientConfig {
    address: String,
    token: String,
    pid: u32,
    project_root: String,
}

pub(crate) fn spawn_orchestrator_bridge(
    project_root: PathBuf,
    commands: Sender<CodexCommand>,
    codex_events: Receiver<CodexEvent>,
    web_browser: SharedWebGptBrowser,
) -> Result<(), String> {
    if let Some(client) = IN_PROCESS_BRIDGE.get() {
        let rebind = BRIDGE_REBIND
            .get()
            .ok_or_else(|| "Roche bridge rebind channel is unavailable".to_owned())?;
        rebind
            .send(BridgeRebind {
                project_root: project_root.clone(),
                commands,
                codex_events,
                web_browser,
            })
            .map_err(|_| "Roche bridge worker is no longer running".to_owned())?;
        let updated_client = BridgeClientConfig {
            project_root: project_root.display().to_string(),
            ..client.clone()
        };
        write_bridge_descriptor(&project_root, &updated_client)?;
        let current_root = BRIDGE_CURRENT_ROOT
            .get()
            .ok_or_else(|| "Roche bridge current root is unavailable".to_owned())?;
        let mut current_root = current_root
            .lock()
            .map_err(|_| "Roche bridge current root lock is poisoned".to_owned())?;
        let previous_root = current_root.clone();
        if previous_root != project_root {
            let _ = fs::remove_file(bridge_descriptor_path(&previous_root));
        }
        *current_root = project_root;
        return Ok(());
    }
    let listener = bind_bridge_listener()?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not configure Roche Web GPT bridge: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("Could not read Roche Web GPT bridge address: {error}"))?
        .to_string();
    let token = generate_bridge_token()?;
    let client = BridgeClientConfig {
        address,
        token: token.clone(),
        pid: std::process::id(),
        project_root: project_root.display().to_string(),
    };
    let (rebind_tx, rebind_rx) = std::sync::mpsc::channel();
    BRIDGE_REBIND
        .set(rebind_tx)
        .map_err(|_| "Roche bridge rebind channel is already initialized".to_owned())?;
    BRIDGE_CURRENT_ROOT
        .set(Mutex::new(project_root.clone()))
        .map_err(|_| "Roche bridge current root is already initialized".to_owned())?;
    IN_PROCESS_BRIDGE
        .set(client.clone())
        .map_err(|_| "Roche Web GPT bridge is already initialized in this process".to_owned())?;
    write_bridge_descriptor(&project_root, &client)?;
    thread::Builder::new()
        .name("roche-webgpt-orchestrator".to_owned())
        .spawn(move || {
            bridge_worker(
                listener,
                project_root,
                commands,
                codex_events,
                web_browser,
                rebind_rx,
                token,
            )
        })
        .map_err(|error| format!("Could not start Roche Web GPT bridge worker: {error}"))?;
    Ok(())
}

fn bridge_worker(
    listener: TcpListener,
    mut project_root: PathBuf,
    mut commands: Sender<CodexCommand>,
    mut codex_events: Receiver<CodexEvent>,
    mut web_browser: SharedWebGptBrowser,
    rebind_rx: Receiver<BridgeRebind>,
    auth_token: String,
) {
    let mut state = BridgeState::new(project_root.clone(), auth_token.clone());
    loop {
        while let Ok(rebind) = rebind_rx.try_recv() {
            project_root = rebind.project_root;
            commands = rebind.commands;
            codex_events = rebind.codex_events;
            web_browser = rebind.web_browser;
            state = BridgeState::new(project_root.clone(), auth_token.clone());
            state.push_event(
                None,
                "runtime.workspace_rebound",
                format!("Roche bridge rebound to {}", project_root.display()),
            );
        }
        while let Ok(event) = codex_events.try_recv() {
            state.handle_codex_event(event, &commands);
        }
        state.drain_worker_events();
        for event in web_browser.drain_worker() {
            state.handle_web_worker_event(event);
        }
        for command in state.drain_web_worker_commands() {
            match command {
                WebWorkerCommand::EnsureRuntime => {}
                WebWorkerCommand::Submit { request, text } => {
                    web_browser.submit_chat(request, text);
                }
                WebWorkerCommand::Cancel { request } => {
                    web_browser.cancel_chat(request);
                }
                WebWorkerCommand::ShowLogin => web_browser.show_login(),
            }
        }
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, &mut state, &commands),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn handle_connection(
    mut stream: TcpStream,
    state: &mut BridgeState,
    commands: &Sender<CodexCommand>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let reader_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<RpcRequest>(&line) {
        Ok(request) => state.handle_rpc(request, commands),
        Err(error) => RpcResponse {
            jsonrpc: "2.0",
            id: None,
            result: None,
            error: Some(RpcError {
                code: -32700,
                message: format!("Invalid JSON request: {error}"),
            }),
        },
    };
    if serde_json::to_writer(&mut stream, &response).is_ok() {
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
    let _ = stream.shutdown(Shutdown::Both);
}

pub fn rpc_call(method: &str, params: Value) -> Result<Value, String> {
    let client = discover_bridge_client()?;
    rpc_call_with_client(&client, method, params)
}

fn rpc_call_with_client(
    client: &BridgeClientConfig,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let address = &client.address;
    let socket_address = address
        .parse::<SocketAddr>()
        .map_err(|error| format!("Invalid Roche bridge address {address}: {error}"))?;
    if !socket_address.ip().is_loopback() {
        return Err(format!(
            "Refusing non-loopback Roche bridge address: {socket_address}"
        ));
    }
    let mut stream =
        TcpStream::connect_timeout(&socket_address, Duration::from_secs(2)).map_err(|error| {
            format!("Roche app orchestrator is not reachable at {address}: {error}")
        })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
        "auth": client.token,
    });
    serde_json::to_writer(&mut stream, &request)
        .map_err(|error| format!("Could not encode Roche bridge request: {error}"))?;
    stream
        .write_all(b"\n")
        .map_err(|error| format!("Could not write Roche bridge request: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("Could not flush Roche bridge request: {error}"))?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| format!("Could not finish Roche bridge request: {error}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("Could not read Roche bridge response: {error}"))?;
    let value: Value = serde_json::from_str(&line)
        .map_err(|error| format!("Invalid Roche bridge response: {error}"))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Roche orchestrator returned an error");
        return Err(message.to_owned());
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| "Roche orchestrator response did not include result".to_owned())
}

pub fn bridge_addr() -> String {
    discover_bridge_client()
        .map(|client| client.address)
        .unwrap_or_else(|_| DEFAULT_WEBGPT_BRIDGE_ADDR.to_owned())
}

fn validated_bridge_addr() -> Result<SocketAddr, String> {
    let configured = std::env::var("ROCHE_WEBGPT_BRIDGE_ADDR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_WEBGPT_BRIDGE_ADDR.to_owned());
    let address = configured
        .parse::<SocketAddr>()
        .map_err(|error| format!("Invalid Roche bridge address {configured}: {error}"))?;
    if !address.ip().is_loopback() {
        return Err(format!(
            "Refusing non-loopback Roche bridge bind address: {address}"
        ));
    }
    Ok(address)
}

fn bind_bridge_listener() -> Result<TcpListener, String> {
    let configured = validated_bridge_addr()?;
    match TcpListener::bind(configured) {
        Ok(listener) => Ok(listener),
        Err(error)
            if error.kind() == std::io::ErrorKind::AddrInUse
                && std::env::var_os("ROCHE_WEBGPT_BRIDGE_ADDR").is_none() =>
        {
            TcpListener::bind("127.0.0.1:0").map_err(|fallback_error| {
                format!(
                    "Could not bind Roche Web GPT bridge at {configured} ({error}) or fallback loopback port ({fallback_error})"
                )
            })
        }
        Err(error) => Err(format!(
            "Could not bind Roche Web GPT bridge at {configured}: {error}"
        )),
    }
}

fn generate_bridge_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not generate Roche bridge capability token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn capability_matches(expected: &str, provided: Option<&str>) -> bool {
    let Some(provided) = provided else {
        return false;
    };
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .iter()
        .zip(provided)
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn bridge_descriptor_path(project_root: &Path) -> PathBuf {
    project_root.join(BRIDGE_DESCRIPTOR_RELATIVE_PATH)
}

fn write_bridge_descriptor(project_root: &Path, client: &BridgeClientConfig) -> Result<(), String> {
    let path = bridge_descriptor_path(project_root);
    let parent = path
        .parent()
        .ok_or_else(|| format!("Invalid Roche bridge descriptor path: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Could not create Roche bridge descriptor directory {}: {error}",
            parent.display()
        )
    })?;
    let encoded = serde_json::to_vec_pretty(client)
        .map_err(|error| format!("Could not encode Roche bridge descriptor: {error}"))?;
    fs::write(&path, encoded).map_err(|error| {
        format!(
            "Could not write Roche bridge descriptor {}: {error}",
            path.display()
        )
    })
}

fn discover_bridge_client() -> Result<BridgeClientConfig, String> {
    if let Some(client) = IN_PROCESS_BRIDGE.get() {
        return Ok(client.clone());
    }

    let mut roots = Vec::new();
    if let Some(root) = std::env::var_os("ROCHE_PROJECT_ROOT") {
        roots.push(PathBuf::from(root));
    }
    if let Ok(current) = std::env::current_dir() {
        roots.push(current);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        roots.extend(parent.ancestors().take(4).map(Path::to_path_buf));
    }

    roots.dedup();
    for root in roots {
        let path = bridge_descriptor_path(&root);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let client = serde_json::from_slice::<BridgeClientConfig>(&bytes).map_err(|error| {
            format!(
                "Invalid Roche bridge descriptor {}: {error}",
                path.display()
            )
        })?;
        let address = client.address.parse::<SocketAddr>().map_err(|error| {
            format!(
                "Invalid Roche bridge descriptor address {}: {error}",
                client.address
            )
        })?;
        if !address.ip().is_loopback() {
            return Err(format!(
                "Refusing non-loopback Roche bridge descriptor address: {address}"
            ));
        }
        if client.token.len() < 64 {
            return Err("Roche bridge descriptor contains an invalid capability token".to_owned());
        }
        return Ok(client);
    }

    Err(format!(
        "Roche bridge descriptor not found. Start Roche in this project first (expected {BRIDGE_DESCRIPTOR_RELATIVE_PATH})."
    ))
}
