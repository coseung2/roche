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

Codex / roche_web
        |
        | authenticated session.* / task.* / project.snapshot
        v
Rust Orchestrator
        |
        | private Codex runtime commands
        v
codex app-server
```

The target Web GPT path adds a Roche-owned outbound Secure MCP Tunnel in front of the same high-level worker surface. The tunnel never exposes the raw loopback capability or Codex app-server protocol:

```text
ChatGPT developer-mode app
        |
        | OpenAI-hosted tunnel endpoint
        v
OpenAI tunnel-client (owned by Roche)
        |
        | local stdio/HTTP MCP
        v
Roche MCP gateway
        |
        | authenticated high-level worker.* / session.* / task.*
        v
Rust Orchestrator
```

The Rust Orchestrator is not an AI. It owns Task state and serializes Codex work deterministically. A successful managed Codex turn stops at `needs_review`; only `task.approve` crosses the review gate to `completed`.

## Direct native Web GPT chat

`src/web_browser.rs` owns the Windows WebView2 runtime contract and persistent profile. The main eframe process does **not** host Tao/Wry/WebView2 directly; it starts the same executable in `--webgpt-browser-host` mode and exchanges JSONL commands/events over stdio:

```text
%LOCALAPPDATA%\Roche\WebGptProfile
```

The native UI sends the user message to the helper process, which owns the hidden ChatGPT composer. The browser adapter:

- injects the prompt through the live composer,
- leases the current FIFO turn as slot 0 with a monotonically increasing generation,
- carries slot generation, account, session, optional task and request identity through browser-host `Chat`/`Cancel` JSONL commands and every active chat event,
- persists that full correlation through same-origin `sessionStorage` so ChatGPT navigation does not lose it,
- waits for the submitted user message and assistant generation lifecycle,
- prefers structured message-role DOM when available,
- falls back to ChatGPT accessibility/body text markers when the current DOM omits message-role attributes,
- probes the generating page at 500 ms cadence and emits `ChatProgress` for visible assistant text plus visible tool/search/connector/status activity,
- emits fully correlated `ChatSubmitted`, `ChatProgress`, `ChatAnswered`, `ChatCancelled` and `ChatFailed` events back to Rust,
- accepts an event only when its complete lease/owner equals the active turn; a reused request ID, stale generation or wrong account/session/task is ignored,
- keeps one native assistant row streaming from `생각 중…` through visible intermediate output to the final answer, while visible tool activity is rendered as activity rows.

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

The loopback-only rule remains an invariant after remote MCP support. Roche must connect outward through the official tunnel client; it must not relax `ROCHE_WEBGPT_BRIDGE_ADDR` to a public bind address.

## Target Secure MCP ownership

Roche, not the user, owns the normal runtime lifecycle of the Secure MCP connection:

- install or locate a verified OpenAI `tunnel-client`,
- start it hidden with the Roche MCP stdio command or a loopback HTTP adapter,
- keep its health/ready state visible in the native UI,
- reconnect it after recoverable failure,
- terminate it with the Roche app,
- keep runtime API keys in OS-protected secret storage,
- preserve one tunnel identity across registered ChatGPT accounts/workspaces.

OpenAI documents that one tunnel can be associated with multiple Platform organizations or ChatGPT workspaces without creating another tunnel. Tunnel RBAC and ChatGPT developer-mode permission are separate. Roche therefore treats account/workspace app approval as a one-time enrollment boundary, not as a step repeated on every account switch. A newly added account may still require interactive login, workspace permission and app approval; Roche must not copy cookies or bypass that approval.

Source: [OpenAI Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels).

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

A session can recursively spawn `web_gpt` or `codex` workers. A Codex worker spawn with a non-empty `goal` launches an independent `codex app-server --stdio` runtime. A Web GPT worker spawn with a non-empty `goal` submits the orchestrator prompt through the authenticated hidden WebView2 ChatGPT runtime and maps visible progress, activity, final answer, cancellation, revision, review, and approval back into Task/Session state. The native chat UI and Web GPT workers share one browser controller and one FIFO turn queue, preventing concurrent turns from overwriting the page-local request marker or opening a second WebView2 process against the same profile. Worker routing uses the typed task owner carried by the turn correlation rather than a request-ID prefix. Workspace changes rebind the existing loopback bridge and shared browser to the new Codex runtime instead of launching a second WebView2 profile owner. Spawns without a `goal` remain metadata-only. Full recursive propagation beyond the direct Web GPT child/parent status remains follow-up work.

## Target browser and account pool

The current single browser/FIFO behavior is a safety baseline, not the final concurrency model. It already uses the production `WebGptPoolScheduler` at capacity 1, so cancellation drain, slot generation and stale-event rules match the future pool while one physical WebView remains active. The target keeps one isolated helper process and raises that bounded slot pool only after the remaining runtime gates:

```text
WebGptBrowserHost
  BrowserPool
    Slot 0 -> account A / conversation 1 / request X
    Slot 1 -> account B / conversation 2 / request Y
    Slot 2 -> lazy optional capacity
```

Each slot owns its request marker, conversation URL, progress probe, cancellation and recovery state. WebViews for the same enrolled account may share that account's WebView2 environment/profile only after a spike proves concurrent access is stable. Different accounts always use distinct persistent profile directories. The default maximum is two active Web GPT turns; three is opt-in only after memory/CPU and live DOM validation.

The account pool routes only new work. Roche does not silently move an in-flight ChatGPT turn to another account because submission may already have consumed quota or produced an answer. Login expiry, app unlink, workspace policy denial and rate-limit evidence move an account to an explicit unhealthy/paused state and trigger routing of subsequent tasks.

Detailed milestones and acceptance checks are in [`WEBGPT_MULTI_ACCOUNT_ORCHESTRATION_PLAN.md`](WEBGPT_MULTI_ACCOUNT_ORCHESTRATION_PLAN.md).

## Security follow-up

Capability authentication closes the previous unauthenticated-loopback gap. Remaining hardening work includes:

- explicit Windows ACL restrictions on the runtime descriptor,
- stale descriptor cleanup,
- a defined multi-instance descriptor-selection policy,
- capability rotation/recovery semantics,
- keeping all externally configurable bridge addresses loopback-only,
- Windows-protected storage for tunnel runtime keys and account metadata separation,
- per-account enrollment/revocation audit events,
- external opaque session handles that cannot be replayed as the local bridge capability.
