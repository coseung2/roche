# Roche AI Workstation

Rust 기반의 장기 실행 AI 개발 워크스테이션입니다. 대화 자체를 상태 저장소로 쓰지 않고 **Project / Task / Session / Event**를 독립적인 제품 상태로 관리하며, Herdr를 Codex 런타임으로 사용하고 Rust 애플리케이션이 control plane 역할을 맡습니다.

## 현재 상태

프로젝트는 **Phase 0 — Performance PoC** 단계입니다. Framework-independent 성능 코어를 먼저 고정했고, 다음으로 UI framework 후보를 동일 workload에서 비교합니다.

현재 구현된 코어:

- 강타입 `ProjectId`, `TaskId`, `SessionId`
- Task / Session 상태 모델
- Herdr adapter 계약
- 결정론적 Orchestrator 상태 전이
- Blocked / attention 우선 Sidebar projection
- Herdr snapshot을 로컬 runtime cache에 반영하는 App state
- 핵심 상태 전이 및 정렬 테스트
- 100,000 messages / 100,000 tool events / 1,000 sessions synthetic workload
- viewport + overscan 기반 virtual window
- 50 MiB terminal history를 최근 500줄로 제한하는 ring buffer
- release-mode Performance PoC runner와 baseline 기록

아직 구현하지 않은 주요 영역:

- Native UI framework 후보별 prototype / 실제 frame-time 비교
- 실제 Herdr Socket API client
- SQLite event store
- worktree / git diff / verifier
- ChatGPT / DevSpace bridge
- Context Engine

상세 진행 상황은 [`docs/PROJECT_STATUS.md`](docs/PROJECT_STATUS.md)를 기준으로 관리합니다.

## 핵심 설계 원칙

1. **Conversation != Project State** — 채팅 기록은 프로젝트 상태 저장소가 아닙니다.
2. **Task is first-class** — 작업, 에이전트, diff, test, approval, artifact는 Task 아래에 묶습니다.
3. **Herdr = runtime** — PTY, Codex process, session lifetime, runtime status는 Herdr가 담당합니다.
4. **Rust = control plane** — GUI, orchestration, persistence, verification, context, UX는 Rust가 담당합니다.
5. **GUI lifetime != Agent lifetime** — GUI가 종료되어도 실행 중인 Codex 작업은 유지되어야 합니다.
6. **Done != Complete** — Herdr의 agent `Done`만으로 Task를 완료 처리하지 않고 검증과 리뷰를 거칩니다.
7. **Visible data only render** — 대용량 conversation, event, terminal history는 virtualization / lazy load를 기본으로 합니다.

## 목표 아키텍처

```text
User
  |
  v
Rust AI Workstation
  |-- Native UI
  |-- App State / Event Bus
  |-- Orchestrator
  |-- Context Engine
  |-- SQLite Event Store
  |-- Git / Worktree / Verifier
  |
  v
Herdr Adapter
  |
  v
Herdr daemon
  |-- Codex process / PTY
  |-- Session / Pane
  |-- Runtime events
  |-- Worktree runtime mapping
```

## 개발 순서

현재 우선순위는 다음 순서를 고정합니다.

1. UI Performance PoC
2. Sidebar / Session model
3. Herdr Socket Client
4. Herdr Session Snapshot
5. Event Subscription
6. Codex Launch / Prompt / Wait
7. Task Model
8. SQLite Event Store
9. Worktree integration
10. Diff / Test / Verification
11. Orchestrator State Machine
12. ChatGPT / DevSpace Bridge
13. Context Engine
14. Native API Mode

## 로컬 개발

요구사항:

- Rust 1.95 이상
- Cargo
- Windows가 우선 target이며, Herdr 연동 단계부터 Herdr daemon이 필요합니다.

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

현재 main binary는 foundation placeholder입니다.

```bash
cargo run
```

Phase 0 성능 workload는 release mode에서 실행합니다.

```bash
cargo run --release --bin perf_poc
```

성능 측정 정책과 baseline은 [`docs/performance/README.md`](docs/performance/README.md)에서 관리합니다.

## 저장소 운영

- 개발 절차: [`CONTRIBUTING.md`](CONTRIBUTING.md)
- 프로젝트 상태: [`docs/PROJECT_STATUS.md`](docs/PROJECT_STATUS.md)
- Architecture Decision Records: [`docs/adr/README.md`](docs/adr/README.md)
- Incident 관리: [`docs/incidents/README.md`](docs/incidents/README.md)
- Performance PoC: [`docs/performance/README.md`](docs/performance/README.md)
- 보안 원칙: [`SECURITY.md`](SECURITY.md)
- 변경 이력: [`CHANGELOG.md`](CHANGELOG.md)

## 첫 Milestone 완료 정의

Milestone 1은 다음 흐름이 실제로 동작할 때 완료입니다.

```text
Rust app start
 -> Herdr connect
 -> running Codex sessions appear in sidebar
 -> select session and read recent terminal output
 -> launch a new Codex task
 -> Working / Blocked / Idle / Done update live
 -> close GUI while Codex keeps running
 -> reopen GUI and recover sessions
```

이 완료 기준은 [`docs/PROJECT_STATUS.md`](docs/PROJECT_STATUS.md)의 체크리스트로 추적합니다.
