# Web GPT ↔ Roche Runtime Boundary

Roche uses two separate Web GPT paths:

1. **Normal native chat** — direct WebView2 ChatGPT send/receive.
2. **Orchestration control plane** — authenticated loopback JSON-RPC for `session.*`, `task.*`, `project.snapshot`, and the optional mailbox contract.

Web GPT never sends Codex `thread/start`, `turn/start`, `turn/steer`, or other app-server protocol messages directly.

## Runtime boundary

```text
Normal chat

Roche native chat UI
        |
        | JSONL stdio / submit_chat
        v
roche-workstation.exe --webgpt-browser-host
        |
        v
Windows WebView2 ChatGPT session
        |
        | assistant response DOM/accessibility text
        v
JSONL Rust ChatAnswered event
        |
        v
Roche native transcript

Orchestration

Web GPT / roche_web
        |
        | authenticated session.* / task.* / project.snapshot
        v
Rust Orchestrator
        |
        | private Codex runtime commands
        v
codex app-server
```

The Rust Orchestrator is not an AI. It owns Task state and serializes Codex work deterministically. A successful managed Codex turn stops at `needs_review`; only `task.approve` crosses the review gate to `completed`.

## Direct native Web GPT chat

`src/web_browser.rs` owns the Windows WebView2 runtime contract and persistent profile. The main eframe process does **not** host Tao/Wry/WebView2 directly; it starts the same executable in `--webgpt-browser-host` mode and exchanges JSONL commands/events over stdio:

```text
%LOCALAPPDATA%\Roche\WebGptProfile
```

The native UI sends the user message to the helper process, which owns the hidden ChatGPT composer. The browser adapter:

- injects the prompt through the live composer,
- correlates the request through same-origin `sessionStorage` so ChatGPT navigation does not lose it,
- waits for the submitted user message and completed assistant response,
- prefers structured message-role DOM when available,
- falls back to ChatGPT accessibility/body text markers when the current DOM omits message-role attributes,
- emits `ChatSubmitted` and `ChatAnswered` back to Rust,
- routes the answer into the same Roche native session transcript.

Verified smoke:

```text
Reply with exactly ROCHE_NATIVE_WEBGPT_READY and nothing else.
  -> ROCHE_NATIVE_WEBGPT_READY
```

Run with:

```powershell
cargo run --bin webgpt_native_e2e_smoke
```

`ROCHE_WEBGPT_DIAGNOSTICS=1` enables additional browser probe diagnostics for DOM regressions. `ROCHE_WEBGPT_BROWSER_HOST_EXE` can override the helper executable path for development smoke binaries.

### Windows process-isolation requirement

Running eframe/winit and the Tao/Wry WebView2 event loop in the same Roche process produced reproducible Windows `0xc000041d` / `0xc0000005` application crashes. WebView2-only smoke remained stable, and the full app remained stable when WebView2 was disabled. The accepted runtime therefore isolates Tao/Wry/WebView2 in the helper process. After the split, the release app survived the regression window and the exact Web GPT E2E smoke continued to return `ROCHE_NATIVE_WEBGPT_READY`.

### Remaining Web GPT caveat

Direct transport is verified, but Roche does not yet prove that the hard-coded `[WEB] GPT-5.6 Sol` label matches the model currently selected by ChatGPT Web, nor does it force the Roche reasoning selector into the ChatGPT Web model/reasoning controls. Those are separate acceptance items.

## Authenticated local orchestration bridge

The running Roche app starts a loopback JSON-RPC bridge together with its Codex runtime.

Preferred address:

```text
127.0.0.1:47831
```

If the default port is occupied and `ROCHE_WEBGPT_BRIDGE_ADDR` was not explicitly set, Roche falls back to an ephemeral loopback port. Non-loopback bind and client addresses are rejected.

At startup Roche generates a random 32-byte capability token and writes the active bridge descriptor to:

```text
.ai-bridge/roche-webgpt-runtime.json
```

The descriptor contains the actual address, capability token, PID, and project root. `.ai-bridge/` is Git-ignored and must remain so.

Every JSON-RPC request requires the capability. Missing or wrong capabilities are rejected with bridge error code `-32001`.

The bridge is not an OpenAI API endpoint and never exposes the Codex app-server protocol.

## `roche_web` control CLI

Build:

```powershell
cargo build --bin roche_web
```

The CLI discovers the runtime descriptor and automatically attaches the capability token.

Examples:

```powershell
.\target\debug\roche_web.exe health
.\target\debug\roche_web.exe chat-wait --timeout 240
.\target\debug\roche_web.exe chat-respond <request-id> --text "answer"
.\target\debug\roche_web.exe session-list
.\target\debug\roche_web.exe session-spawn --runtime codex --title "worker"
.\target\debug\roche_web.exe create --title "Task" --goal "Goal" --accept "Acceptance criterion" --effort high
.\target\debug\roche_web.exe get <task-id>
.\target\debug\roche_web.exe events --task <task-id> --after 0
.\target\debug\roche_web.exe revise <task-id> --prompt "Revision request"
.\target\debug\roche_web.exe cancel <task-id>
.\target\debug\roche_web.exe approve <task-id>
.\target\debug\roche_web.exe snapshot
```

## Optional mailbox contract

The mailbox remains available for orchestration/control workflows but is no longer the normal native-chat transport.

Producer side:

- `chat.submit { text, reasoning_level }`
- `chat.poll { request_id }`
- `chat.cancel { request_id }`

Consumer side:

- `chat.pending`
- `chat.release { request_id }`
- `chat.respond { request_id, text }`

Verified bridge smoke:

```text
chat.submit → chat.pending → chat.respond → chat.poll
```

Run with:

```powershell
cargo run --bin webgpt_bridge_smoke
```

## Managed Task state

```text
queued
  -> preparing
  -> running_codex
  -> needs_review
       -> completed     (explicit approval)
       -> queued        (revision)

Applicable states may become failed or cancelled.
```

The bridge records bounded task/tool events and exposes a Git project snapshot (`status --short`, diff stat, changed files).

## Session graph

Available methods:

- `session.list`
- `session.get`
- `session.spawn`
- `session.status`
- `session.workers`
- `session.events`

A session can recursively spawn `web_gpt` or `codex` worker metadata. This graph is a deterministic control model; it is not yet a complete scheduler that independently launches execution for every spawned node.

## Security follow-up

Capability authentication closes the previous unauthenticated-loopback gap. Remaining hardening work includes:

- explicit Windows ACL restrictions on the runtime descriptor,
- stale descriptor cleanup,
- a defined multi-instance descriptor-selection policy,
- capability rotation/recovery semantics,
- keeping all externally configurable bridge addresses loopback-only.
