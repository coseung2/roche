@C:\Users\coseung2\.codex\RTK.md

<!-- CODEGRAPH_START -->
## CodeGraph

In repositories indexed by CodeGraph (a `.codegraph/` directory exists at the repo root), reach for it BEFORE grep/find or reading files when you need to understand or locate code:

- **MCP tool** (when available): `codegraph_explore` answers most code questions in one call — the relevant symbols' verbatim source plus the call paths between them, including dynamic-dispatch hops grep can't follow. Name a file or symbol in the query to read its current line-numbered source. If it's listed but deferred, load it by name via tool search.
- **Shell** (always works): `codegraph explore "<symbol names or question>"` prints the same output.

If there is no `.codegraph/` directory, skip CodeGraph entirely — indexing is the user's decision.
<!-- CODEGRAPH_END -->

<!-- CHATGPT_ORACLE_ROUTING_START -->
## ChatGPT Web Orchestration

When the user explicitly asks to use web GPT, GPT review/edit/orchestration, Pro, Web Multi-GPT, deep research, or comprehensive mode, route the request through the installed ChatGPT Oracle skills instead of inventing a browser workflow.

- The installed `codexpro-automation` package and its `docs/GLOBAL_CHATGPT_ROUTING.md` are the authoritative routing source. Preserve unrelated personal rules when refreshing this block.

- Regular `GPT`/`direct`, `plan`, `review`, `edit`, and `orchestrator` work must use the `chatgpt-thinking-browser` skill and its `chatgpt_oracle_dispatch.py` entrypoint; deep research, Web Multi-GPT, and comprehensive work use their corresponding Oracle skills. Regular modes use the manually registered `DevSpace` ChatGPT app.
- Standalone `Pro` uses the `chatgpt-pro-browser` skill, Oracle attachments only, and never DevSpace.
- Comprehensive mode uses `chatgpt-pro-plan-handoff`; it owns plan, optional Pro/Web Multi, review, implementation, and the final deterministic gate.
- Use `orchestrator` when the goal and approach are already settled and one web-GPT pass should implement and test it.
- Requests such as "use web GPT to finish this", "implement it to completion", `오케스트라`, or `지휘` must trigger `chatgpt-thinking-browser`; never construct a raw `oracle`/`npx @steipete/oracle` command, select GPT 5.5/Pro, reuse a shared manual-login browser, or set `--browser-model-strategy ignore` for a new regular run.
- Before a live orchestrator submission, its dispatcher dry-run must prove `mode=orchestrator`, `transport=devspace`, `model=gpt-5.6`, `model_strategy=select`, `thinking_time=heavy`, app `DevSpace`, zero attachments, and the exact root and absolute mission. The live run must verify visible `GPT-5.6 Sol`, `Extra High`, and `verified=yes`; a mismatch blocks submission instead of permitting a fallback.
- Every regular run is bound to one exact project root and one absolute UTF-8 mission file. Never substitute another root or expose an entire drive.
- Preview with the selected runner's dry-run mode before any real submission. Preserve exact Oracle slug/session identity during recovery; never resubmit a timed-out run as a replacement.
- After a successful dry-run and explicit live authorization, execute the exact installed dispatcher/runtime command selected by the current skill. Do not invent a wrapper, duplicate a run, or use lifecycle behavior from an older installed version.
- If DevSpace is unavailable or ChatGPT reports an app/account connection failure, classify the run as blocked and use only `chatgpt-workspace-setup` diagnosis or repair. Do not claim that implementation is running, do not silently retry a terminal blocked run, and do not fall back to CodexPro, agbrowse, ZIP attachments, Chrome automation, or another workspace.
- Report implementation complete only after the exact run has terminal output and the required task outcome permits completion (`EXECUTED`, or an explicitly allowed compatibility outcome). Browser launch, model verification, relay registration, or transport exit alone is not implementation evidence.
- Treat CodexPro and agbrowse as persisted-run recovery only, never as a fallback for new work.
<!-- CHATGPT_ORACLE_ROUTING_END -->

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
