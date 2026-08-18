@C:\Users\coseung2\.codex\RTK.md

<!-- CODEGRAPH_START -->
## CodeGraph

In repositories indexed by CodeGraph (a `.codegraph/` directory exists at the repo root), reach for it BEFORE grep/find or reading files when you need to understand or locate code:

- **MCP tool** (when available): `codegraph_explore` answers most code questions in one call — the relevant symbols' verbatim source plus the call paths between them, including dynamic-dispatch hops grep can't follow. Name a file or symbol in the query to read its current line-numbered source. If it's listed but deferred, load it by name via tool search.
- **Shell** (always works): `codegraph explore "<symbol names or question>"` prints the same output.

If there is no `.codegraph/` directory, skip CodeGraph entirely — indexing is the user's decision.
<!-- CODEGRAPH_END -->

## Orchestration terminology

- Unqualified requests for `오케스트레이션`, `오케스트레이트`, `지휘`, workers, or subagents mean Codex native worker orchestration.
- Do not infer an external web-GPT workflow from those terms. Such a workflow requires the user to request it explicitly by name.
- DevSpace is independent workspace infrastructure and should be used only when the user explicitly places it in scope.

--- project-doc ---

# Roche AI Workstation repository instructions

## Product invariants

- The app is a direct-Codex chat workstation; there is no external agent runtime (Herdr) dependency.
- Codex CLI `app-server` is the only chat runtime: the app spawns it, owns its process lifetime, and tears it down on exit.
- `LOCAL CODEX READY` requires a completed `app-server` initialize handshake. A model row is "connected" only when the CLI handshake and catalog load actually succeeded; otherwise it is labeled name-only.
- Model catalogs are loaded from the Codex CLI config (`model_catalog_json`) with documented fallbacks; the catalog must never be presented as live proof of per-model availability.
- Keep Codex I/O, process management, and file reads off the egui UI thread.
- Preserve viewport-only rendering and bounded terminal memory in the performance fixtures.

## Chat runtime

The desktop app spawns a `codex app-server --stdio` child in a background worker and speaks the JSONL protocol (initialize → thread/start → turn/start → turn/steer → turn/interrupt). Assistant deltas, tool activity, and turn completion are drained as events into the chat transcript.

The same Rust runtime now exposes a loopback-only Web GPT / Orchestrator bridge. Web GPT must use the high-level `roche_web` control plane (`chat.*`, `task.*`, `project.snapshot`) through the connected Roche plugin and must not speak the Codex app-server protocol directly. Rust owns deterministic Task scheduling and completion state; a successful Codex turn stops at `needs_review` until approved or revised.

The `[WEB] GPT-5.6 Sol` native UI wiring may consume the mailbox contract in `docs/WEBGPT_BRIDGE.md`: `chat.submit` / `chat.poll` on the app side and `chat.wait` / `chat.respond` on the Web GPT side. No WebView, Oracle, or OpenAI API is part of this bridge.

## Verification

Before claiming a runtime change is complete, run:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo check --all-targets
```

For Codex integration changes, also verify the app-server initialize handshake against the installed CLI (`codex --version`, and optionally `codex app-server --stdio` probe) and confirm the catalog source resolves. Do not send real model turns unless the user asked for a live smoke.
