# ADR-0003: Roche-owned Web GPT account pool and Secure MCP connection

- Status: Proposed
- Date: 2026-08-19

## Context

Roche already owns the local Codex `app-server`, a loopback-only Rust Orchestrator bridge, a stdio MCP adapter and one persistent WebView2 ChatGPT profile. Codex can discover `codex` and `web_gpt` as sibling worker runtimes, but ChatGPT Web cannot call a local stdio or loopback MCP server directly.

The current Web GPT execution path also serializes native chat and worker turns through one browser and one FIFO queue. Roche can represent multiple Web GPT sessions, but it cannot yet run them concurrently. A product-level requirement is to operate multiple enrolled ChatGPT accounts similarly to the OCX account pool: enroll an account once, preserve its login and app connection, then switch or automatically route later work without repeating setup.

OpenAI Secure MCP Tunnel provides an outbound-only path from a private MCP server to supported OpenAI products. One tunnel can be associated with multiple Platform organizations or ChatGPT workspaces, while tunnel RBAC and ChatGPT developer-mode permission remain separate authorization boundaries.

## Decision

Roche will own both the local and remote sides of the Web GPT orchestration connection.

1. Keep the Rust bridge loopback-only. Do not expose its listener, descriptor capability or Codex app-server protocol externally.
2. Supervise the official OpenAI `tunnel-client` as an app-owned outbound helper connected to the Roche MCP gateway.
3. Reuse one tunnel identity across all explicitly associated Platform organizations and ChatGPT workspaces.
4. Treat each ChatGPT account/workspace combination as a separately enrolled identity with its own persistent WebView2 profile and app-connection state.
5. Require interactive approval only when an account/workspace is first enrolled or its permission is revoked. Normal switching and restart recovery must reuse the recorded enrollment.
6. Run multiple Web GPT turns through a bounded WebView slot pool inside one isolated helper process. Default concurrency is two; higher capacity requires measured validation.
7. Route only new work during account failover. Never silently replay or migrate an in-flight Web GPT turn.
8. Keep Rust authoritative for Task/Session state, review gates, cancellation and parent/child propagation.
9. Store tunnel credentials with Windows-protected secret storage. Persist non-secret account, slot, session and enrollment metadata separately.

## Consequences

### Positive

- Users operate Roche only; they do not maintain a separate tunnel CLI or public MCP server.
- Codex and Web GPT see one worker catalog and can call each other through the same high-level lifecycle.
- Multiple Web GPT sessions can run concurrently without starting one browser helper process per worker.
- ChatGPT accounts can be switched or selected automatically after one-time enrollment.
- The local capability and private Codex protocol remain outside the remote trust boundary.

### Negative / Trade-offs

- A new account/workspace can still require ChatGPT login, developer-mode permission and explicit app approval.
- Tunnel availability depends on OpenAI control-plane connectivity and organization/workspace association.
- Multiple WebViews increase memory, CPU and DOM-regression exposure; concurrency must remain bounded and measured.
- ChatGPT Web quota/limit state may not have a stable machine-readable API, so automatic routing must use conservative observable evidence.
- Account/profile recovery, revocation and multi-instance ownership add persistence and security complexity.

## Alternatives Considered

### Public Roche MCP endpoint

- Advantage: conventional stable HTTPS MCP endpoint and easier public distribution.
- Disadvantage: requires public ingress, server operations and a larger attack surface.
- Rejected for the private workstation path because Roche should remain local-first and outbound-only.

### One tunnel and browser helper per account

- Advantage: simple account isolation.
- Disadvantage: duplicated processes, credentials and health management; higher resource use.
- Rejected because one tunnel can serve multiple associated workspaces and one helper can host a bounded slot pool.

### Copy one account's cookies or app authorization to other accounts

- Advantage: appears to remove enrollment steps.
- Disadvantage: violates account/workspace authorization boundaries and creates unsafe credential handling.
- Rejected. Each account/workspace is enrolled explicitly once.

### Keep the single WebView FIFO

- Advantage: current proven request-marker and profile safety.
- Disadvantage: no Web GPT parallelism and poor mixed-worker throughput.
- Retained only as the initial fallback and rollback mode.

## Validation

The ADR can move to Accepted after all of the following are demonstrated:

1. Two Web GPT slots return independently correlated answers in one helper process.
2. A registered account survives Roche restart without login or app reconnection.
3. Switching between two previously enrolled accounts does not repeat tunnel or app setup.
4. A new or revoked account enters an explicit enrollment-required state instead of borrowing another account's credentials.
5. One tunnel serves at least two explicitly associated test account/workspace identities.
6. Web GPT can call `spawn_agent(runtime="codex")`, and Codex can call `spawn_agent(runtime="web_gpt")`.
7. The local bridge remains loopback-only and remote callers never receive its descriptor capability.
8. Tunnel/client crashes recover without losing completed Task state or duplicating an in-flight turn.
9. Resource measurements establish a safe default slot count and a rollback to single-slot FIFO.
10. Every live browser command and terminal event is correlated by slot generation, account, session, task and request; stale events cannot release another turn or write to another transcript.
11. Cancellation keeps a slot occupied until a matching acknowledgement or terminal completion, including crash/timeout recovery tests.

## Follow-up

- [ ] Execute [`WEBGPT_MULTI_ACCOUNT_ORCHESTRATION_PLAN.md`](../WEBGPT_MULTI_ACCOUNT_ORCHESTRATION_PLAN.md).
- [ ] Choose and document the Windows secret-store implementation.
- [ ] Define account identity and workspace association probes without persisting sensitive page content.
- [ ] Define signed/opaque remote session handles and replay protection.
- [ ] Promote this ADR only after live multi-account and bidirectional worker E2E evidence.
