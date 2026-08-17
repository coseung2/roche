# Project Status

마지막 갱신: 2026-08-18

이 문서는 현재 개발 상태의 단일 요약 지점입니다. 세부 설계 결정은 ADR, 장애 기록은 Incident 문서, 코드 변경 이력은 Git과 CHANGELOG에서 관리합니다.

## 현재 단계

**Phase 0B — Usable Desktop Shell**

Framework-independent 성능 workload 위에 egui/eframe 기반 Windows native shell을 연결했습니다. 이제 실제 UI를 실행해 100k-scale views를 조작할 수 있으며, 다음 P0는 Herdr daemon/socket 연동입니다.

## 구현 완료

- [x] Rust crate bootstrap
- [x] Project / Task / Session 강타입 ID
- [x] Task 상태 모델
- [x] Agent runtime 상태 모델
- [x] Herdr adapter trait
- [x] Herdr snapshot / event 기본 계약
- [x] Orchestrator transition guard
- [x] `Done != Completed` invariant 테스트
- [x] Blocked / attention 우선 Sidebar 정렬
- [x] Herdr snapshot -> app runtime cache 적용
- [x] Repository README / contribution / ADR / incident 관리 체계
- [x] 100,000 messages synthetic workload
- [x] 100,000 tool events synthetic workload
- [x] 1,000 sessions synthetic workload
- [x] viewport + overscan virtual window invariant
- [x] 100,000 streaming tail append bounded-materialization test
- [x] 50 MiB terminal ingest / 500-line resident ring buffer test
- [x] release-mode Performance PoC runner / baseline 문서
- [x] egui/eframe desktop shell (ADR-0002)
- [x] 1,000-session virtualized sidebar / search / status filter
- [x] 100,000-message virtualized Chat view
- [x] 100,000-event virtualized Tools view
- [x] 500-line resident Terminal view
- [x] local Task creation UI
- [x] Windows release `.exe` build

## 다음 작업

### P0 — Performance PoC

- [x] 공통 synthetic workload / 측정 runner
- [x] 100,000 messages virtual-window core
- [x] 100,000 tool events virtual-window core
- [x] 1,000 sessions virtual-window core
- [x] 50 MiB terminal ring buffer
- [x] streaming tail bounded-materialization 부하 테스트
- [x] egui/eframe 최소 prototype
- [ ] 실제 scroll / frame-time 측정
- [ ] jump-to-bottom / task switching 측정
- [ ] Markdown parse / layout cache 전략 측정
- [ ] Windows IME / text selection 검증
- [x] Phase 0B framework 선택을 ADR-0002로 기록
- [ ] IME/selection/Markdown 검증 후 장기 framework 결정 확정

### P0 — Herdr Integration

Desktop shell 기준선이 생겼으므로 바로 진행합니다.

- [ ] Herdr daemon 발견 / health check
- [ ] Socket transport
- [ ] `session.snapshot`
- [ ] `events.subscribe`
- [ ] runtime event normalization
- [ ] agent read / prompt / wait
- [ ] GUI restart 후 session recovery

### P1 — Persistence / Task Runtime

- [ ] SQLite schema
- [ ] append-only event store
- [ ] migration framework
- [ ] Task <-> Herdr ID mapping
- [ ] worktree lifecycle
- [ ] git diff
- [ ] verifier

## Milestone 1 완료 조건

- [ ] Rust 앱 실행 시 Herdr에 자동 연결한다.
- [ ] 실행 중 Codex 세션이 Sidebar에 표시된다.
- [ ] 세션을 선택하면 최근 terminal 내용을 읽을 수 있다.
- [ ] 새 Codex Task를 시작할 수 있다.
- [ ] Working / Blocked / Idle / Done 상태가 실시간 갱신된다.
- [ ] Blocked / Needs Attention 세션이 우선 노출된다.
- [ ] GUI를 종료해도 Codex process는 계속 실행된다.
- [ ] GUI 재실행 시 기존 세션을 복구한다.

## 현재 알려진 제약

- egui/eframe은 Phase 0B 선택이며 장기 확정 전 IME/selection/Markdown 검증이 남아 있음
- 실제 Herdr protocol과 아직 연결되지 않음
- persistence 없음
- 현재 desktop shell은 synthetic `LOCAL PERF MODE`

## 품질 게이트

현재 필수:

```text
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Phase 0A의 구조적 성능 invariant는 `cargo test`에 포함합니다. 실제 시간(ms)은 머신 편차가 커 CI pass/fail 기준으로 사용하지 않고, `docs/performance/` baseline과 Phase 0B UI 비교에 기록합니다.

## 상태 갱신 규칙

- 기능이 실제 검증되기 전에는 `[x]`로 바꾸지 않습니다.
- 완료된 큰 설계 결정은 ADR 번호를 옆에 기록합니다.
- incident에서 생긴 후속 작업은 Issue 또는 이 문서의 P0/P1 항목으로 승격합니다.
- milestone 완료는 개별 기능 완료가 아니라 end-to-end acceptance criteria 통과를 기준으로 합니다.
