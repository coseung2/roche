# Project Status

마지막 갱신: 2026-08-19

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
- [x] `roche_mcp` stdio MCP server / Multi-Agent Spawn v1 worker adapter (`worker.catalog`, `worker.*`, MCP `spawn_agent` lifecycle)
- [x] MCP context resources (`roche://workers/catalog`, `roche://sessions`, `roche://project/snapshot`)
- [x] Codex global MCP registration에서 `roche` stdio server enabled 확인
- [x] 현재 설치 Codex CLI (`codex-cli 0.147.0-alpha.1.2`) initialize + bridge smoke 통과
- [x] Windows release `target\release\roche-workstation.exe` build
- [x] 현재 작업 트리 전체 Rust 품질 게이트 green

## Web GPT transport 분리

### 일반 native chat

```text
Roche native composer
  -> WebGptBrowserController::submit_chat
  -> JSONL stdio
  -> roche-workstation.exe --webgpt-browser-host
  -> hidden WebView2 ChatGPT composer
  -> ChatGPT response
  -> DOM/accessibility-text response probe (500 ms)
  -> JSONL WebGptBrowserEvent::ChatProgress / ChatAnswered
  -> 같은 Roche native transcript에서 생각 중 / 활동 / 응답 중 / 최종 답변 갱신
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

### Web GPT in-flight UX

WebView2 direct chat은 완료된 답변만 한 번에 넣던 경로에서, 생성 중 DOM을 주기적으로 읽어 `ChatProgress`를 native transcript에 반영하는 경로까지 구현되었습니다.

- [x] 전송 확인 직후 native assistant placeholder에 `생각 중…` 표시
- [x] 생성 중 assistant 가시 텍스트를 동일 transcript row에 증분 갱신
- [x] ChatGPT Web에서 관측 가능한 tool/search/connector/status activity를 별도 activity row로 반영
- [x] Web assistant 본문에 섞인 `inProgress/completed/failed` 실행 로그를 제거하고 structured activity group으로 접어 표시
- [x] 완료 시 streaming row를 `ChatAnswered` 최종 텍스트로 확정
- [ ] 실제 ChatGPT Web live turn에서 현재 DOM의 progress/tool selector E2E 재검증
- [ ] DOM 변경 regression fixture 추가

숨겨진 chain-of-thought를 추출하는 것이 아니라 ChatGPT Web UI에 실제로 노출되는 상태/활동/텍스트만 native UI에 반영합니다.

### Native chat activity / layout UX

- [x] Codex app-server item type을 `terminal / file change / tool call / web search / worker` activity로 구조화
- [x] `item/started → item/completed`를 `item_id`로 동일 row에 갱신
- [x] 연속된 동일 종류 activity를 한 개의 접힌 summary row로 묶고 상세 명령/항목은 펼쳤을 때만 표시
- [x] reasoning/assistant/runtime stderr를 tool activity로 오분류하지 않음
- [x] sidebar workspace / provider quota 사이 draggable splitter 및 split ratio persistence
- [x] user bubble 내부 좌측 정렬, `Enter=전송 / Shift+Enter=줄바꿈` widget-level invariant
- [x] chat content height가 `CHAT_BOTTOM_MARGIN`을 항상 예약하는 regression test 추가
- [ ] 실제 Windows build에서 splitter, 짧은 bubble 정렬, Enter/IME, 하단 여백 수동 smoke

### Session graph / worker execution

`session.spawn/status/workers/events`의 worker graph에 실제 Codex execution lifecycle이 연결되었습니다. `session.spawn --runtime codex --goal ...`은 메인 채팅용 runtime과 분리된 **worker 전용 `codex app-server --stdio` 프로세스**를 생성하므로 여러 Codex worker가 독립 thread/process로 동시에 실행될 수 있습니다.

- [x] Codex worker session metadata → 독립 app-server execution 연결
- [x] worker별 command/event channel 분리; 메인 Codex turn과 `steer` 공유 방지
- [x] worker turn/result/structured activity → Task 및 Session status 반영
- [x] Codex app-server `collabToolCall`의 child thread를 native worker session으로 승격해 상단 session tab에 즉시 반영
- [x] worker revision/cancel/approval → 해당 runtime lifecycle 연결
- [x] `roche_web session-spawn`에 `--goal/--accept/--effort/--model` execution 인자 추가
- [x] `session.spawn --runtime web_gpt --goal ...` → 공유 hidden WebView2 실행 및 Task/Session 상태 연결
- [x] native Web GPT 채팅과 Web GPT worker가 단일 브라우저/FIFO turn queue를 공유해 프로필·요청 슬롯 충돌 방지
- [x] Web GPT visible progress/activity/final/cancel/revise/approve → worker Task lifecycle 반영
- [x] Web GPT child review/terminal 상태 → direct parent Session 상태 반영
- [x] workspace 전환 시 기존 bridge/shared browser를 새 Codex runtime에 rebind
- [ ] 실제 모델 turn으로 2개 이상 worker 동시 실행 smoke
- [ ] parent/child completion 및 cancel propagation 정책 완성
- [ ] native UI에서 worker별 transcript/activity 경계 명확화

### Web GPT 멀티 계정 / 병렬 오케스트레이션 목표

현재 Web GPT session graph는 여러 worker metadata와 Task를 만들 수 있지만 실제 browser 실행은 하나의 `SharedWebGptBrowser`와 FIFO turn queue에 직렬화됩니다. 목표 구조는 하나의 격리된 WebView2 helper process 안에서 소수의 browser slot을 운영하고, 계정별 영구 profile과 공통 Roche MCP tunnel을 결합하는 것입니다.

- [x] Codex에서 `roche_mcp`를 통해 `codex` / `web_gpt` worker catalog 발견 가능
- [x] 여러 Codex worker를 독립 `app-server` process로 실행 가능한 구조
- [x] 단일 Web GPT browser/FIFO가 request marker와 profile 충돌을 막는 현재 안전 기준
- [x] production-disconnected 2-WebView shared-profile 생성/navigation/JS probe smoke
- [x] slot/account/request/generation correlation과 cancellation drain/ack를 검증하는 pure `BrowserPool` scheduler 15개 test
- [x] 현재 단일 FIFO에서 matching request terminal만 다음 turn을 배정하고, stale terminal/runtime error/cancel-control error가 active turn을 소비하지 않는 회귀 방지
- [x] browser-host `Chat`/`Cancel` 및 active chat event에 slot generation/account/session/task/request 전체 correlation 적용
- [x] production `SharedWebGptBrowser`가 pure `BrowserPool` scheduler를 capacity 1로 사용하고 cancel request/ack 사이 slot 점유 유지
- [ ] WebView2 helper 내부 `BrowserPool` / slot별 `request_id`, session, conversation 격리
- [ ] 기본 2개, 설정 가능 최대 3개의 bounded Web GPT 동시 실행
- [ ] 계정별 WebView2 profile과 enrollment 상태를 보존하는 `WebGptAccountPool`
- [ ] 동일 계정 재시작/전환 시 로그인과 app 연결 상태 복구
- [ ] 계정 health/한도/일시 제한에 따른 다음 작업 자동 라우팅; in-flight turn 강제 이전 금지
- [ ] Roche가 OpenAI `tunnel-client` process와 local MCP target의 lifecycle/health를 소유
- [ ] 공통 tunnel을 등록된 여러 Platform organization / ChatGPT workspace와 연결
- [ ] ChatGPT developer-mode app에서 Roche `spawn_agent` 실제 호출 E2E
- [ ] Web GPT → Codex 및 Codex → Web GPT 양방향 worker 실행 E2E

계정/워크스페이스별 최초 권한 승인은 보안 경계이므로 복사하거나 우회하지 않습니다. 대신 승인 완료 상태를 계정 enrollment로 저장해 평상시 계정 전환과 앱 재시작에서는 같은 과정을 반복하지 않습니다. 상세 구현 계획은 [`WEBGPT_MULTI_ACCOUNT_ORCHESTRATION_PLAN.md`](WEBGPT_MULTI_ACCOUNT_ORCHESTRATION_PLAN.md), 설계 결정은 [`ADR-0003`](adr/0003-roche-owned-webgpt-account-pool-and-secure-mcp.md)에 있습니다.

### Persistence

- [ ] SQLite schema / append-only event store
- [x] native chat UI state autosave + Codex persisted thread list/read/resume recovery
- [ ] Web GPT/task control-plane restart recovery
- [ ] migration framework

## 남은 런타임 리스크

- Web GPT direct response extraction은 현재 ChatGPT DOM의 message-role attribute가 없는 경우 접근성 본문 텍스트 (`나의 말:` / `ChatGPT의 말:` 등)를 fallback으로 사용합니다. 사이트 DOM/문구 변경에 대한 회귀 보호가 더 필요합니다.
- WebView2는 eframe/Tao/Wry same-process crash를 피하기 위해 별도 helper process로 격리됩니다. 메인 앱 종료 시 helper도 종료하며, stdio protocol의 crash/restart recovery는 추가 보강 대상입니다.
- bridge descriptor에는 capability token이 포함되므로 `.ai-bridge/`는 계속 Git ignore 대상이어야 합니다. 장기적으로 Windows ACL을 명시적으로 제한하는 것이 바람직합니다.
- second Roche instance는 기본 포트 충돌 시 ephemeral loopback port로 분리되지만, 어떤 descriptor가 consumer에 선택되는지 multi-instance 정책을 더 명확히 해야 합니다.
- Web GPT 병렬성은 아직 구현되지 않았습니다. 현재 여러 Web GPT worker는 한 browser/FIFO에서 직렬 실행됩니다.
- ChatGPT Web의 사용량/일시 제한은 안정적인 공개 quota API가 아니라 관측 가능한 UI와 실패 상태에 의존할 수 있으므로, 자동 계정 전환은 보수적이고 증거 기반이어야 합니다.
- Secure MCP Tunnel의 tunnel 권한과 ChatGPT developer-mode 권한은 별도이며 계정/워크스페이스 정책에 따라 최초 enrollment가 차단될 수 있습니다.
- tunnel runtime API key와 계정 연결 metadata는 bridge descriptor나 일반 JSON 설정에 평문 저장하면 안 됩니다.

## 다음 작업 순서

1. **BrowserPool runtime** — bounded 2-slot 병렬 실행, lazy WebView, adaptive probe 및 개별 cancel/reload 구현.
2. **BrowserPool recovery** — per-slot login/offline/crash quarantine, malformed probe 제한, queued-cancel reuse 방지 및 soak 검증.
3. **WebGptAccountPool** — 계정별 persistent profile, enrollment/health/paused 상태와 수동 전환 UI 구현.
4. **Roche-owned Secure MCP** — `tunnel-client` supervisor, secret storage, doctor/health, app 연결 wizard 및 공통 tunnel association 구현.
5. **양방향 worker E2E** — Web GPT → Codex, Codex → Web GPT, Codex + Web GPT 병렬 실행과 `needs_review` gate 검증.
6. **자동 계정 라우팅** — 새 작업만 안전하게 다음 healthy account로 이동하고, 재시작 시 등록 상태를 복구.
7. **재귀 propagation / persistence** — parent/child completion·cancel 정책과 SQLite/event-store recovery 완성.
8. **기존 UX hardening** — Codex failure UX, Web GPT model/reasoning 증거화, DOM lifecycle, IME/selection/Markdown 검증.

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
- Codex worker execution과 Web GPT worker execution은 각각 독립 app-server 및 공유 hidden WebView2 경로로 연결됐지만 이번 변경에서는 실제 모델 turn을 보내는 live worker smoke를 실행하지 않았습니다.
- Web GPT progress/tool extraction은 코드/컴파일 검증까지 완료됐지만 이번 변경에서는 실제 ChatGPT Web live turn을 보내지 않아 현재 DOM에 대한 E2E 재검증이 남아 있습니다.
- `roche_mcp`는 Codex stdio MCP로 등록/발견 가능하지만 Roche-owned Secure MCP Tunnel과 ChatGPT developer-mode app enrollment가 아직 없어 Web GPT가 동일 worker surface를 직접 호출하지는 못합니다.
- native chat UI와 Codex thread는 재시작 복구되지만 Web GPT/task control-plane 상태는 아직 메모리 resident라 앱 재시작 복구가 없습니다.
- egui/eframe 장기 확정 전 Windows IME/selection/Markdown 검증이 남아 있습니다.

## 품질 게이트

필수:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo check --all-targets
```

2026-08-19 현재 `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets` 149개(10 suites), `cargo check --all-targets`, `git diff --check`가 모두 통과했습니다. production-disconnected 2-WebView smoke도 같은 임시 profile에서 두 view의 navigation/probe 완료를 확인했습니다. 이전 `CARGO_TARGET_DIR=target/final cargo build --bin roche-workstation` 및 2026-08-18 live model smoke 기록은 유효하지만, 이번 M0/FIFO/M1 capacity-1 실행에서는 실제 모델 turn이나 계정/tunnel 외부 상태 변경을 수행하지 않았습니다.

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
