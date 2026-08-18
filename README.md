# Roche AI Workstation

Rust/egui 기반 Windows 네이티브 AI 채팅 워크스테이션입니다. 외부 Herdr 런타임 없이 Roche가 Codex CLI `app-server`를 직접 소유하고, 동일한 채팅 UI에서 `Codex`와 `[WEB] GPT-5.6 Sol`을 모델 선택으로 전환하는 구조를 사용합니다.

## 현재 상태

현재 단계는 **Unified Native Chat Integration**입니다.

Codex 직접 채팅은 app-server initialize/turn/streaming 경로가 동작합니다. 일반 Web GPT 채팅은 native composer에서 Windows WebView2 ChatGPT 세션으로 직접 입력하고, 실제 assistant 응답을 회수해 같은 native transcript에 표시합니다. JSON-RPC mailbox/Task/Session bridge는 별도의 오케스트레이션 control-plane으로 유지합니다.

현재 구현된 주요 기능:

- Codex CLI 자동 발견 및 `codex app-server --stdio` 연결 (`LOCAL CODEX READY`)
- Codex JSONL 프로토콜 (`initialize`, `thread/start`, `turn/start`, `turn/steer`, `turn/interrupt`)
- streaming assistant delta / final response / tool activity / turn completion 반영
- Codex 모델 카탈로그 로드 (`model_catalog_json` 및 fallback catalog/cache)
- `[WEB] GPT-5.6 Sol` / `Codex` 단일 모델 선택 UX와 추론 강도 설정
- 별도 `--webgpt-browser-host` helper process에서 WebView2 직접 Web GPT 전송/응답 회수 (`ChatSubmitted` / `ChatAnswered`)
- navigation-safe `sessionStorage` request correlation 및 accessibility-text response fallback
- 인증된 Web GPT loopback control bridge (`chat.*`, `session.*`, `task.*`, `project.snapshot`)
- per-process random capability token / runtime descriptor / non-loopback refusal
- Rust Orchestrator Task 상태 (`queued → preparing → running_codex → needs_review → completed`)
- 명시적 review gate (`needs_review`에서만 `task.approve` 가능)
- Web GPT/Codex worker session graph (`session.spawn`, `session.status`, `session.workers`, `session.events`)
- `roche_web` local control CLI
- Windows WebView2 기반 ChatGPT 로그인 profile, login-state probe, hidden wake-up adapter
- Phase 0 성능 코어 (100,000 messages / tool events, 1,000 sessions synthetic)

## 현재 E2E 상태

일반 Web GPT 채팅은 다음 경로로 실제 exact round trip을 통과했습니다.

```text
Roche native composer
  -> JSONL stdio
  -> roche-workstation.exe --webgpt-browser-host
  -> hidden WebView2 ChatGPT composer
  -> ChatGPT response
  -> Rust response extraction
  -> 같은 native chat transcript
```

`webgpt_native_e2e_smoke`에서 `ROCHE_NATIVE_WEBGPT_READY`가 정확히 회수됩니다. mailbox round trip도 별도로 통과하며, 이는 Task/Session 오케스트레이션 control-plane 검증에 사용합니다.

## 아직 완료 판정 전인 핵심 영역

### Codex failure UX

Codex catalog에서 선택한 slug는 이제 실제 `turn/start.params.model` override로 전달됩니다. `gpt-5.6-sol` explicit override live smoke가 `TURN_completed`와 `ROCHE_DIRECT_CODEX_READY`로 통과했습니다.

OCX/Codex가 제공하는 `kiro`, `opencode-go` 등 provider/model 구성은 Roche가 변경하지 않습니다. 개별 provider/model 요청이 upstream에서 실패할 경우 provider 설정을 덮어쓰는 대신 해당 요청의 오류를 명확하게 표시하고 사용자의 선택 상태를 유지하는 UX가 남아 있습니다.

### Persistence

workspace 목록은 저장되지만 chat/session/task runtime 상태는 메모리 resident입니다. 앱 재시작 시 대화/Task/session 복구는 아직 없습니다.

## 핵심 설계 원칙

1. **Codex CLI가 로컬 실행 런타임** — Roche가 `codex app-server` 프로세스 생명주기를 직접 관리합니다.
2. **Web GPT와 Codex는 같은 chat UX** — 별도 화면이 아니라 모델 선택으로 구분합니다.
3. **일반 Web GPT 채팅과 오케스트레이션 control-plane을 분리** — 일반 답변은 WebView2에서 직접 송수신하고, Task/Session 오케스트레이션만 Roche의 authenticated high-level control surface를 사용합니다.
4. **Rust가 Task completion 권한을 보유** — Codex 성공 turn은 `needs_review`까지이며 자동 `completed`가 아닙니다.
5. **Runtime I/O는 UI thread 밖** — Codex stdio, socket, Git/file I/O는 background 경로를 사용하고, Tao/Wry/WebView2는 eframe과 충돌하지 않도록 별도 helper process에 격리합니다.
6. **Visible data only render** — 대용량 conversation/event/terminal 데이터는 virtualization과 bounded memory를 기본으로 합니다.

## 로컬 개발

요구사항:

- Rust 1.95 이상
- Cargo
- PATH에 `codex` CLI
- Windows Web GPT 사용 시 WebView2 Runtime

기본 품질 게이트:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo check --all-targets
```

실행:

```bash
cargo run --release --bin roche-workstation
```

release build:

```bash
cargo build --release --bin roche-workstation
```

Windows 실행 파일:

```text
target\release\roche-workstation.exe
```

Web GPT local bridge CLI:

```bash
cargo build --bin roche_web
```

예시:

```text
roche_web health
roche_web chat-wait --timeout 240
roche_web chat-respond <request-id> --text "answer"
roche_web session-list
roche_web create --title "Task" --goal "Goal" --accept "Acceptance criterion" --effort high
roche_web snapshot
```

상세 계약은 [`docs/WEBGPT_BRIDGE.md`](docs/WEBGPT_BRIDGE.md)를 참고합니다.

## 현재 검증 상태

2026-08-18 기준:

- `codex-cli 0.147.0-alpha.1.2` initialize handshake 통과
- Codex `gpt-5.6-sol` explicit `turn/start.params.model` live smoke 통과
- Web GPT bridge health 통과
- authenticated native mailbox round-trip smoke 통과
- helper-process WebView2 direct Web GPT exact E2E smoke 통과
- eframe + Tao/Wry same-process Windows callback crash (`0xc000041d / 0xc0000005`) 회귀 제거 및 release 앱 12초 생존 검증
- bridge capability rejection unit test 통과
- `cargo fmt --all -- --check` 통과
- `cargo check --all-targets` 통과
- `cargo test --all-targets` 통과
- `cargo clippy --all-targets --all-features -- -D warnings` 통과
- `target\release\roche-workstation.exe` release build 통과

최신 상세 상태와 다음 작업 순서는 [`docs/PROJECT_STATUS.md`](docs/PROJECT_STATUS.md)를 기준으로 관리합니다.

## 저장소 문서

- 프로젝트 상태: [`docs/PROJECT_STATUS.md`](docs/PROJECT_STATUS.md)
- Web GPT bridge: [`docs/WEBGPT_BRIDGE.md`](docs/WEBGPT_BRIDGE.md)
- 개발 절차: [`CONTRIBUTING.md`](CONTRIBUTING.md)
- ADR: [`docs/adr/README.md`](docs/adr/README.md)
- Incident: [`docs/incidents/README.md`](docs/incidents/README.md)
- Performance: [`docs/performance/README.md`](docs/performance/README.md)
- Security: [`SECURITY.md`](SECURITY.md)
- Changelog: [`CHANGELOG.md`](CHANGELOG.md)
