# Project Status

마지막 갱신: 2026-08-18

이 문서는 현재 개발 상태의 단일 요약 지점입니다. 세부 설계는 ADR/전용 문서, 장애 기록은 Incident, 코드 변경 이력은 Git과 CHANGELOG에서 관리합니다.

## 현재 단계

**Unified Native Chat Integration** — Herdr 없이 Roche 앱이 Codex CLI `app-server`를 직접 소유하고, 동일한 네이티브 채팅 UI에서 `Codex`와 `[WEB] GPT-5.6 Sol`을 모델 선택으로 전환하는 구조입니다.

일반 Web GPT 채팅은 이제 mailbox relay가 아니라 **WebView2 ChatGPT session에 직접 입력하고 실제 assistant 응답을 회수해 native transcript에 넣는 경로**를 사용합니다. 로컬 JSON-RPC mailbox / Task / Session bridge는 Web GPT 오케스트레이션과 control-plane 용도로 유지합니다.

## 검증 완료

- [x] Rust crate bootstrap / Rust 1.95 target
- [x] egui/eframe Windows desktop shell (ADR-0002)
- [x] Phase 0 성능 코어 (100,000 messages / tool events, 1,000 sessions synthetic)
- [x] viewport + overscan virtual-window core 및 streaming-tail invariant test
- [x] 50 MiB terminal ingest / 500-line resident ring buffer test
- [x] release-mode Performance PoC runner / baseline 문서
- [x] Codex CLI 자동 발견 (`ROCHE_CODEX_BIN` / PATH)
- [x] `codex app-server --stdio` background worker
- [x] app-server initialize handshake → `LOCAL CODEX READY`
- [x] `thread/start`, `turn/start`, `turn/steer`, `turn/interrupt` JSONL 경로
- [x] assistant delta / final message / tool activity / turn completion 이벤트 반영
- [x] Codex 모델 카탈로그 로드 (`model_catalog_json` → cache/catalog fallback)
- [x] 선택된 Codex catalog slug → `turn/start.params.model` runtime override
- [x] `gpt-5.6-sol` explicit model override live smoke (`ROCHE_DIRECT_CODEX_READY`)
- [x] 모델/추론 강도 popover 및 `[WEB] GPT-5.6 Sol` / `Codex` 단일 선택 UX
- [x] WebView2 ChatGPT persistent profile / login-state probe / hidden browser runtime
- [x] WebView2 runtime을 `roche-workstation.exe --webgpt-browser-host` helper process로 격리
- [x] eframe + Tao/Wry same-process `0xc000041d / 0xc0000005` crash 제거
- [x] WebView2 native direct chat submit (`ChatSubmitted`)
- [x] ChatGPT navigation을 견디는 `sessionStorage` request correlation
- [x] current ChatGPT accessibility-text DOM fallback response extraction
- [x] helper-process direct Web GPT E2E smoke: `ROCHE_NATIVE_WEBGPT_READY` exact round trip
- [x] Web GPT loopback JSON-RPC bridge (`127.0.0.1:47831` 기본값)
- [x] 32-byte random per-process bridge capability token
- [x] `.ai-bridge/roche-webgpt-runtime.json` runtime descriptor discovery
- [x] missing/wrong capability RPC rejection test (`-32001`)
- [x] non-loopback bind/client refusal
- [x] default port collision 시 ephemeral loopback fallback
- [x] `chat.submit → chat.pending → chat.respond → chat.poll` bridge smoke
- [x] Rust Orchestrator Task queue / cancel / revise / approval gate
- [x] `needs_review → completed` 명시적 승인 invariant test
- [x] Web GPT / Codex 재귀 worker session graph 및 session RPC surface
- [x] `roche_web` control CLI (`chat.*`, `session.*`, `task.*`, `project.snapshot`)
- [x] 현재 설치 Codex CLI (`codex-cli 0.147.0-alpha.1.2`) initialize + bridge smoke 통과
- [x] Windows release `target\release\roche-workstation.exe` build
- [x] 전체 Rust 품질 게이트 green

## Web GPT transport 분리

### 일반 native chat

```text
Roche native composer
  -> WebGptBrowserController::submit_chat
  -> JSONL stdio
  -> roche-workstation.exe --webgpt-browser-host
  -> hidden WebView2 ChatGPT composer
  -> ChatGPT response
  -> DOM/accessibility-text response probe
  -> JSONL WebGptBrowserEvent::ChatAnswered
  -> 같은 Roche native transcript
```

2026-08-18 실제 smoke에서 다음 exact round trip을 확인했습니다.

```text
Reply with exactly ROCHE_NATIVE_WEBGPT_READY and nothing else.
  -> ROCHE_NATIVE_WEBGPT_READY
```

### Orchestration / control bridge

```text
Web GPT / roche_web
  -> authenticated loopback JSON-RPC
  -> session.* / task.* / project.snapshot / optional chat mailbox
  -> Rust Orchestrator
  -> private Codex runtime commands
```

일반 채팅이 bridge mailbox에 의존하지 않으므로 ChatGPT가 스스로 Roche tool을 호출해야만 사용자 답변이 돌아오는 구조가 아닙니다.

## 구현됐지만 완료 판정 전

### Codex model selection

카탈로그 항목 선택은 이제 `selected_codex_slug → CodexCommand::Send.model → turn/start.params.model`로 전달됩니다. 설치된 app-server schema의 `TurnStartParams.model`이 이 turn 및 이후 turn의 모델 override임을 확인했고, `gpt-5.6-sol`을 명시적으로 선택한 live smoke가 `TURN_completed / ROCHE_DIRECT_CODEX_READY`로 통과했습니다.

- [x] 현재 Codex app-server schema에서 `turn/start.params.model` override 위치 확정
- [x] 선택된 slug를 runtime command까지 전달
- [x] explicit `gpt-5.6-sol` live turn 성공 검증
- [ ] 지원하지 않는 catalog 항목 선택 시 명확한 오류/rollback 상태 표시

현재 OCX/Codex 환경은 `kiro`, `opencode-go` 등 여러 provider/model slug를 포함하며 Roche는 그 설정을 변경하지 않습니다. 이번 live smoke에서는 configured-default 경로의 한 요청이 upstream `INVALID_MODEL_ID`를 반환했고, 별도로 `gpt-5.6-sol` explicit override가 정상 완료됐습니다. 이는 provider 설정 변경 근거가 아니라 **선택된 catalog slug가 app-server까지 정확히 전달되는지 확인한 transport 검증 결과**로만 취급합니다.

### Web GPT model / reasoning control

`[WEB] GPT-5.6 Sol` 행의 native direct transport 자체는 동작하지만 현재 Roche의 추론 강도 선택값을 ChatGPT Web UI의 모델/추론 selector에 강제 반영하지는 않습니다.

- [ ] WebView2에서 실제 선택된 ChatGPT model을 증거 기반으로 읽기
- [ ] `[WEB] GPT-5.6 Sol` 라벨과 실제 웹 모델 일치 검증
- [ ] `빠름 / 높음 / 매우 높음`을 웹 UI의 지원 설정과 연결하거나 미지원 상태를 명시
- [ ] ChatGPT DOM 변경 시 selector/accessibility-text extraction regression 보호

### Session graph

`session.spawn/status/workers/events`는 worker 계층을 상태 모델로 생성할 수 있습니다. 다만 session graph 자체가 별도 Web GPT/Codex 프로세스를 자동 실행하는 scheduler는 아닙니다.

- [ ] session metadata와 실제 worker execution lifecycle 연결
- [ ] parent/child completion 및 cancel propagation 정의
- [ ] native UI에서 worker별 transcript/activity 경계 명확화

### Persistence

- [ ] SQLite schema / append-only event store
- [ ] chat/session/task restart recovery
- [ ] migration framework

## 남은 런타임 리스크

- Web GPT direct response extraction은 현재 ChatGPT DOM의 message-role attribute가 없는 경우 접근성 본문 텍스트 (`나의 말:` / `ChatGPT의 말:` 등)를 fallback으로 사용합니다. 사이트 DOM/문구 변경에 대한 회귀 보호가 더 필요합니다.
- WebView2는 eframe/Tao/Wry same-process crash를 피하기 위해 별도 helper process로 격리됩니다. 메인 앱 종료 시 helper도 종료하며, stdio protocol의 crash/restart recovery는 추가 보강 대상입니다.
- bridge descriptor에는 capability token이 포함되므로 `.ai-bridge/`는 계속 Git ignore 대상이어야 합니다. 장기적으로 Windows ACL을 명시적으로 제한하는 것이 바람직합니다.
- second Roche instance는 기본 포트 충돌 시 ephemeral loopback port로 분리되지만, 어떤 descriptor가 consumer에 선택되는지 multi-instance 정책을 더 명확히 해야 합니다.

## 다음 작업 순서

1. **Codex failure UX hardening** — OCX/provider 설정은 그대로 유지하고, 개별 catalog 모델 요청 실패 시 명확한 오류 표시와 선택 상태 보존.
2. **Web GPT model/reasoning 증거화** — hard-coded `[WEB] GPT-5.6 Sol`과 실제 Web UI model 상태를 일치시킴.
3. **Web GPT direct lifecycle 강화** — timeout/cancel/retry, DOM 변경 감지, extraction regression test.
4. **Session graph ↔ 실제 worker execution 연결** — Web GPT와 Codex worker 파생을 실행 모델로 완성.
5. **Persistence** — workspace/session/chat/task 상태를 SQLite/event store로 복구 가능하게 저장.
6. **IME / selection / Markdown** — 장시간 실사용 채팅 UX 검증.
7. **정식 MCP surface / Context Engine** — native control API와 context 공급 계층 정리.

## 제거됨 (Herdr 정리)

- [x] Herdr adapter / CLI wrapper (`src/herdr.rs`)
- [x] Herdr background polling runtime
- [x] Herdr session sidebar / terminal read / Task(worktree) launch UI
- [x] 기존 `roche_ctl` bridge
- [x] Herdr 기반 Task/Session/Orchestrator 스캐폴드 (`app.rs`, `models.rs`, `orchestrator.rs`, `sidebar.rs`)
- [x] Herdr 관련 quickstart / ADR-0001 참조

## 현재 알려진 제약

- 앱이 소유하는 `codex app-server` 프로세스는 앱 종료 시 함께 종료됩니다.
- 카탈로그 항목은 CLI가 아는 모델 목록이며 항목별 실제 제공 가능성을 보장하지 않습니다.
- Codex catalog 선택은 실제 runtime model override에 연결되었습니다. Roche는 OCX가 제공하는 provider/model 구성을 변경하지 않으며, 개별 provider/model의 일시적 또는 upstream 실패는 해당 요청 오류로 처리해야 합니다.
- Web GPT direct chat은 동작하지만 웹 모델/추론 설정과 Roche selector의 강제 동기화는 아직 없습니다.
- chat/session/task control-plane 상태는 메모리 resident이며 앱 재시작 복구가 없습니다.
- egui/eframe 장기 확정 전 Windows IME/selection/Markdown 검증이 남아 있습니다.

## 품질 게이트

필수:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo check --all-targets
```

2026-08-18 현재 네 항목 모두 통과합니다. 추가 smoke도 통과합니다.

```text
cargo run --bin webgpt_bridge_smoke
cargo run --bin web_browser_smoke
cargo run --bin webgpt_native_e2e_smoke
```

## 상태 갱신 규칙

- 코드가 존재하는 것과 실제 E2E 검증 완료를 구분합니다.
- 기능이 acceptance criteria를 통과하기 전에는 완료로 표시하지 않습니다.
- 큰 transport/security/lifecycle 결정은 ADR 또는 전용 설계 문서에 남깁니다.
- incident 후속 작업은 이 문서 또는 Issue로 승격합니다.
- milestone 완료는 개별 함수/모듈 존재가 아니라 실제 사용자 흐름 통과를 기준으로 합니다.
