# Changelog

이 프로젝트의 주요 변경 사항을 기록합니다.

형식은 Keep a Changelog의 범주를 참고하되, 버전 정책이 확정되기 전까지 `Unreleased`를 중심으로 관리합니다.

## [Unreleased]

### Added

- Rust crate foundation
- Phase 0 synthetic performance workload for 100,000 messages, 100,000 tool events, and 1,000 sessions
- Framework-independent viewport virtualization core and streaming-tail invariant tests
- 50 MiB terminal ingest simulation with a bounded 500-line resident ring buffer
- Release-mode `perf_poc` runner and performance baseline documentation
- egui/eframe native desktop shell with virtualized conversation and tool views
- ADR-0002 documenting the Phase 0B desktop framework decision
- Direct Codex CLI discovery and `codex app-server --stdio` background worker
- Codex JSONL protocol client (initialize / thread / turn / steer / interrupt)
- Live model catalog loading from `model_catalog_json` with documented fallbacks
- Codex catalog selection wired to `turn/start.params.model` runtime override
- Chat model/reasoning popover with Lucide chevron icons and connection badges
- `[WEB] GPT-5.6 Sol` native mailbox transport and loopback JSON-RPC bridge
- Rust Orchestrator task queue with explicit `needs_review` approval gate
- Web GPT / Codex recursive session graph and `roche_web` control CLI
- Windows WebView2 ChatGPT login profile, state probe, and native-request wake-up adapter
- WebView2 `--webgpt-browser-host` helper-process isolation with JSONL stdio command/event transport

### Changed

- Migrated from the Herdr runtime dependency to a direct Codex CLI runtime
- Unified Web GPT and Codex behind the same native chat UI/model selector

### Fixed

- Windows `0xc000041d / 0xc0000005` crash caused by running eframe and Tao/Wry WebView2 event loops in the same process

### Security

- Web GPT bridge is loopback-only and protected by a random per-process capability token discovered through the ignored runtime descriptor
- Missing or incorrect bridge capability is rejected before RPC dispatch

### Removed

- Herdr adapter / CLI wrapper transport
- Herdr background polling runtime and session/terminal/Task(worktree) UI
- `roche_ctl` Web GPT control bridge
- Herdr-based Task / Session / Orchestrator domain scaffolding
- Herdr-related documentation (quickstart, ADR-0001, status/README/AGENTS references)

## Release 작성 규칙

릴리스가 생기면 `Unreleased`의 내용을 새 버전으로 이동하고 날짜를 기록합니다.

```text
## [0.1.0] - YYYY-MM-DD
```

사용자/운영 관점에서 의미 없는 내부 편집은 모두 기록할 필요가 없지만, 다음은 반드시 기록합니다.

- public behavior 변경
- state / persistence migration
- breaking protocol 변경
- recovery / process lifetime 변경
- 주요 성능 개선 또는 회귀 수정
- security fix
