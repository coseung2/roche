# Web GPT Multi-account and Bidirectional Orchestration Plan

Status: Draft implementation plan<br>
Last updated: 2026-08-19<br>
Decision record: [`ADR-0003`](adr/0003-roche-owned-webgpt-account-pool-and-secure-mcp.md)<br>
Runtime contract: [`WEBGPT_BRIDGE.md`](WEBGPT_BRIDGE.md)

## Outcome

Roche must be the only desktop application the user operates for normal Web GPT/Codex orchestration. After one-time enrollment of each ChatGPT account/workspace, Roche preserves its browser login and developer-mode app connection, automatically restores the shared Secure MCP tunnel, and routes new work across healthy accounts and bounded WebView slots.

The completed product supports:

- multiple independent Codex workers in separate `codex app-server` processes,
- two Web GPT turns concurrently by default inside one isolated helper process,
- Codex and Web GPT running concurrently,
- Web GPT spawning Codex or Web GPT workers through Roche MCP,
- Codex spawning either runtime through the same worker catalog,
- account switching and conservative automatic routing without repeating setup,
- deterministic Task/Session state and explicit `needs_review` approval.

## Non-goals

- Bypassing ChatGPT login, developer-mode permission, workspace policy or first app approval.
- Copying cookies, access grants or browser profile bytes between accounts.
- Exposing the loopback bridge or Codex app-server protocol to the public internet.
- Unlimited WebView or worker concurrency.
- Silently replaying an in-flight Web GPT turn on another account.
- Claiming exact ChatGPT model, reasoning level or quota without observable evidence.

## Current baseline

| Capability | Current state | Target |
| --- | --- | --- |
| Codex worker execution | Independent process per worker | Preserve |
| Web GPT worker execution | One shared browser, FIFO | Bounded slot pool |
| Codex → Web GPT spawn | Available through Codex stdio MCP | Live E2E verified |
| Web GPT → Codex spawn | Session graph only; no ChatGPT MCP connection | Developer-mode app through tunnel |
| ChatGPT accounts | One persistent profile | Enrolled account pool |
| Remote MCP lifecycle | Not implemented | Owned by Roche |
| Restart recovery | Native chat/Codex partial recovery | Account, tunnel, Task and session recovery |

## Fixed contracts

### Trust boundary

- `127.0.0.1` remains the only bind scope for the Rust bridge.
- `tunnel-client` initiates outbound HTTPS; no public inbound port is opened.
- Remote requests terminate at a Roche MCP gateway and use only high-level `worker.*`, `session.*`, `task.*` and `project.snapshot` operations.
- The `.ai-bridge` capability never crosses the MCP/tunnel boundary.
- Remote session references are opaque, short-lived or signed, scoped to one Roche instance/project, and replay-protected.

### Account identity

An account record is keyed by a Roche-generated stable ID, not by mutable display text alone:

```rust
struct WebGptAccount {
    id: AccountId,
    label: String,
    profile_dir: PathBuf,
    workspace_id: Option<String>,
    login_state: LoginState,
    enrollment_state: EnrollmentState,
    health_state: AccountHealth,
    paused: bool,
    last_used_at: Option<SystemTime>,
}
```

Persist profile paths and non-secret status in the Roche state store. Store tunnel runtime keys and future refresh credentials in Windows-protected secret storage. Do not persist ChatGPT cookies outside the WebView2 profile.

### Browser slots

```rust
struct BrowserPool {
    slots: Vec<BrowserSlot>,
    pending: VecDeque<QueuedBrowserTurn>,
    max_active: usize,
}

struct BrowserSlot {
    id: SlotId,
    account_id: AccountId,
    state: SlotState,
    active_request_id: Option<String>,
    conversation_url: Option<String>,
    last_activity_at: Instant,
}
```

Every helper command and event carries `slot_id`, `account_id`, `request_id` and the Roche session/task correlation needed by Rust. A late event is ignored unless all active correlation fields match.

### Scheduling

- Native foreground chat has priority over background worker research only when both compete for the same account/slot class.
- Default Web GPT concurrency is two active slots across all accounts.
- A third slot is disabled by default until resource and live-service evidence supports it.
- Login-required, enrollment-required, paused, rate-limited and offline accounts are not selected.
- Failover applies to queued/new work. Submitted turns stay bound to their original account and slot until terminal or explicitly cancelled.
- Automatic routing records the reason, source account, destination account and whether the original prompt was ever submitted.

## Milestones

### M0 — Contract and baseline evidence

Deliverables:

- Accept or revise ADR-0003.
- Record current single-slot CPU, memory, submit latency and 500 ms probe cost. Initial logged-out idle evidence: [`performance/WEBGPT_BROWSER_BASELINE_2026-08-19.md`](performance/WEBGPT_BROWSER_BASELINE_2026-08-19.md).
- Build a no-model deterministic helper harness that can simulate two slots and late/out-of-order events.
- Confirm whether multiple WebViews can safely share one WebView2 environment/profile in the existing Wry helper process.

Current evidence: the production-disconnected `webgpt_multi_webview_smoke` created and probed two child WebViews under one temporary profile and event loop without a model turn. Authenticated concurrent generation and longer soak remain M1 validation.

The pure `web_browser_pool` scheduler is now wired into the crate without replacing the production FIFO path. Fifteen behavior tests cover two-slot reverse completion, strict one-slot FIFO, two-phase cancellation/draining and acknowledgement, cancellation isolation, queued cancellation, duplicate enqueue rejection, zero-capacity behavior, and stale rejection for wrong slot/account/request or an old slot generation, including deliberate reuse of the same request ID.

Acceptance:

- Baseline report includes process tree and resident memory for main app, helper and Codex workers.
- The spike does not change the production FIFO path.
- A failed shared-profile experiment leaves the per-account/per-slot isolated fallback documented.

M1 entry gate:

- The current one-slot FIFO path must first reject stale or uncorrelated terminal events. Only a request-scoped answer, cancellation acknowledgement or failure that exactly matches the active request may release the slot and dispatch the next turn; a runtime-wide error must not consume active work.
- Cancellation is a draining state, not an immediate free. A slot remains occupied until the matching browser acknowledgement or a matching terminal completion arrives.
- The live command/event contract must carry typed slot, generation, account, session, task and request correlation before the scheduler can replace the production FIFO.
- The scheduler may enter production first only in strict capacity-1 compatibility mode; parallel enablement remains a separate gate.

Current hardening result: the production FIFO uses structured full-correlation chat events, releases only on a matching request-scoped terminal, keeps generic and cancellation-control errors nonterminal, preserves independent wake/queued-cancel routing, and drops late chat events before UI/worker delivery. `WebGptTurnCorrelation` carries slot generation, account, session, optional task and request across browser-host `Chat`/`Cancel` commands and every active chat event; native UI and worker consumers validate the same owner/lease before mutation. `SharedWebGptBrowser` now uses `web_browser_pool` as its scheduling authority in capacity-1 mode, including two-phase cancellation, deferred dispatch while login/offline, duplicate preservation and owner-checked queued cancellation. Focused regressions and the full 149-test Rust suite pass. Capacity-1 integration is go; M1 is still no-go for parallel production use until the multi-controller helper/runtime work below is implemented.

### M1 — BrowserPool and bounded parallel Web GPT

Deliverables:

- Replace global `active_request_id` with slot-scoped state.
- Add slot IDs to browser-host JSONL commands/events.
- Lazily create and reuse WebViews in one helper event loop.
- Add adaptive probing: fast while generating, slow/disabled while idle.
- Implement slot-scoped submit, progress, final, cancel, reload and crash recovery.
- Preserve an explicit `Running -> Cancelling -> terminal` lifecycle; timeout recovery may quarantine/recreate a stuck slot but must never submit the next turn into a WebView that has not drained.

Current progress: the typed live correlation contract and scheduler-backed single-slot compatibility path are complete. `SharedWebGptBrowser` leases slot 0 through `WebGptPoolScheduler`, browser-host JSONL echoes the full owner/lease, and stale generation/account/session/task events are rejected. A cancel request remains `Cancelling` until a real stop acknowledgement or terminal completion; login/offline/clean host EOF reconcile the active lease and defer the next physical submit until `LoggedIn`. Multiple live controllers/slots, lazy WebView creation, adaptive probing and per-slot crash recovery remain unimplemented.

Remaining 2-slot blockers:

- `ChatQueueCancelled` is an unleased event; same-ID reuse needs an enqueue identity or an explicit non-reuse contract before delayed delivery is safe across concurrent callers.
- Repeated malformed/missing probe correlation needs bounded diagnostics and per-slot quarantine/recovery instead of silent retry forever.
- Runtime-wide state/error routing must become slot/account aware; the current prefix/single-active fallback is compatibility-only.

Acceptance:

- Two deterministic fake turns can complete in reverse order without transcript cross-talk.
- Cancelling one slot does not cancel or advance another slot.
- A late answer from a cancelled/recycled request is ignored.
- One-slot compatibility mode passes all existing browser and bridge tests.
- Live two-turn smoke is run only with explicit authorization and produces exact per-request markers.

### M2 — WebGptAccountPool and enrollment persistence

Deliverables:

- Add/list/rename/pause/remove account UI and account health rows near the existing OCX pool UX.
- Assign a persistent WebView2 profile directory per account.
- Detect login-required and best-effort account/workspace identity from visible page state.
- Persist enrollment metadata separately from browser cookies and secrets.
- Add manual active-account selection and a deterministic account selector API.

Acceptance:

- Account A and B remain isolated and opening one never changes the other's session.
- Restart restores both accounts without login when their Web sessions remain valid.
- Removing an account terminates its slots before deleting only its Roche metadata; destructive profile deletion requires explicit confirmation.
- Switching between already enrolled accounts does not invoke tunnel/app setup.

### M3 — Roche-owned Secure MCP Tunnel

Deliverables:

- Add a `TunnelSupervisor` that locates or installs a verified official `tunnel-client` release.
- Configure one tunnel profile targeting `roche_mcp` over stdio or a dedicated loopback MCP adapter.
- Start the helper hidden, monitor doctor/health/ready state, reconnect recoverable failures and stop it on app exit.
- Store the runtime API key in Windows-protected secret storage and redact logs/support output.
- Add a native setup wizard for tunnel identity, Platform organization/workspace associations and ChatGPT developer-mode app approval.
- Store account/workspace enrollment status against the shared tunnel ID.

Acceptance:

- Roche starts with no inbound public listener.
- Tunnel loss is visible and does not mark running tasks completed.
- Restart restores a healthy tunnel without repeating setup when credentials remain valid.
- A second enrolled account/workspace reuses the same tunnel ID.
- A new or revoked account receives a precise enrollment-required state.

### M4 — Bidirectional worker routing

Deliverables:

- Expose the existing Multi-Agent Spawn v1 lifecycle through the tunnel-backed Roche MCP gateway.
- Add opaque caller/session correlation for Web GPT tool calls.
- Ensure `worker_catalog` reports live Codex, Web GPT slot and account capacity.
- Route Web GPT `spawn_agent(runtime="codex")` to an independent Codex runtime.
- Route Web GPT/Codex `spawn_agent(runtime="web_gpt")` through the account/slot scheduler.
- Preserve `needs_review`, revision, approval, cancel and close semantics across both runtimes.

Acceptance:

- Web GPT → Codex implementation/review worker completes at `needs_review`.
- Codex → Web GPT research/review worker completes at `needs_review`.
- One Codex and two Web GPT workers run concurrently without event cross-talk.
- Recursive depth, active worker count and cancellation limits are enforced.
- The remote caller cannot invoke private Codex protocol methods or use another project capability.

### M5 — Conservative automatic account routing

Deliverables:

- Define account health signals: logged in, enrolled, slot availability, explicit limit UI, known submit failure, cooldown and manual pause.
- Reuse OCX interaction patterns for priority, pause, manual selection and auto-switch threshold where the signal is trustworthy.
- Add routing history and explainable destination selection.
- Add cooldown and circuit-breaker behavior for repeated account failures.

Acceptance:

- Queued work moves to the next healthy enrolled account after a demonstrated pre-submit failure.
- Post-submit uncertainty never causes automatic duplicate replay.
- Manual pause and account priority are respected.
- No UI text alone is treated as quota truth without a named probe and regression fixture.

### M6 — Persistence, hardening and release gate

Deliverables:

- Persist Task/Session/account/tunnel events in SQLite or an append-only event store.
- Restore nonterminal work as explicit interrupted/review-required states; do not invent completion.
- Add Windows ACLs for runtime descriptors and state directories.
- Define multi-instance ownership for account profiles, tunnel supervisor and bridge descriptors.
- Add secret rotation, app unlink/revoke and account removal runbooks.

Acceptance:

- App restart does not lose completed/review results or rerun submitted turns.
- Two Roche instances cannot concurrently own the same WebView2 account profile or tunnel supervisor.
- Logs and diagnostics contain no tunnel key, bridge capability, browser cookies or prompt bodies by default.
- Full Rust quality gates pass, plus deterministic browser-pool, account-pool, tunnel-supervisor and orchestration integration suites.

## Verification matrix

| Scenario | Required evidence |
| --- | --- |
| Two Web GPT sessions | Exact request markers and isolated final answers |
| Codex + Web GPT concurrency | Overlapping running intervals and correct Task IDs |
| Account restart recovery | No login/setup UI for a still-valid enrolled account |
| New account | One-time enrollment-required state and guided approval |
| Existing account switch | No tunnel recreation or app reconnection |
| Tunnel outage | Explicit offline state, no false completion, successful recovery |
| Limit/rate event | New work rerouted; submitted work not duplicated |
| App revocation | Account removed from scheduler until re-enrolled |
| Multi-instance | Single owner lock for profiles/tunnel/descriptor |
| Security boundary | Loopback-only listener and no secret leakage |

## Required quality gates

For every implementation milestone:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo check --all-targets
git diff --check
```

Live ChatGPT turns, account enrollment and tunnel association change external state or consume account capacity. Run those smokes only with explicit authorization and record which account/workspace was used without recording credentials or prompt bodies.

## Open decisions before M3

1. Windows secret store: Credential Manager, DPAPI-protected local vault, or a small abstraction supporting both.
2. Tunnel provisioning boundary: fully in-app control-plane API if officially supported, otherwise a Roche-guided one-time Platform settings handoff followed by app-owned runtime lifecycle.
3. Account identity evidence: stable workspace/account identifiers available from allowed visible/session state without brittle private API dependence.
4. Shared-profile topology: multiple WebViews under one `WebContext` versus one retained context per active slot after the M0 spike.
5. Safe health and limit probes: which visible states are reliable enough for automatic routing versus display-only diagnostics.
