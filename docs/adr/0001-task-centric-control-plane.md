# ADR-0001 — Task-centric Rust control plane over Herdr runtime

Status: `Accepted`

Date: `2026-08-18`

## Context

이 제품은 장시간 누적되는 AI 대화, tool call, terminal history 때문에 UI와 conversation 자체가 project state를 사실상 떠안는 구조를 피해야 합니다.

동시에 다음 요구가 있습니다.

- 여러 Codex session을 병렬로 관리해야 한다.
- GUI가 종료되어도 agent process는 계속 살아 있어야 한다.
- agent runtime 상태와 제품의 Task 완료 상태는 동일하지 않다.
- 대화가 길어지더라도 project의 장기 상태가 conversation lifetime에 종속되면 안 된다.
- Codex process / PTY / session 유지 기능을 다시 구현하지 않고 Herdr를 활용해야 한다.
- 대용량 history는 visible data 중심으로 렌더링하여 UI memory와 layout cost를 제한해야 한다.

## Decision

다음 architecture boundary를 기준선으로 채택합니다.

### Rust application is the control plane

Rust 애플리케이션이 다음을 소유합니다.

- Native GUI
- Project / Task domain state
- Orchestrator
- persistence / event store
- verification
- Git / worktree policy
- context management
- approval / retry / cancel / merge UX

### Herdr is the agent runtime

Herdr가 다음을 소유합니다.

- Codex process 실행
- PTY
- runtime session / pane
- agent runtime status
- terminal history runtime access
- GUI process와 독립적인 agent lifetime

Rust는 Herdr 내부 ID를 domain identity로 노출하지 않고 adapter를 통해 mapping합니다.

### Task is the first-class work unit

Conversation이 아니라 Task를 작업의 상위 객체로 둡니다.

Task 아래에 다음 관계를 둡니다.

- conversation
- Codex session
- Herdr runtime mapping
- worktree
- changed files / diff
- tests
- approval
- artifacts
- activity

### Runtime Done is not Task Completed

Herdr의 `Done`은 Codex runtime 상태일 뿐입니다.

Task는 별도의 deterministic Orchestrator state machine을 통해 최소한 verification과 review 단계를 거친 뒤 `Completed`가 됩니다.

### GUI lifetime does not own Agent lifetime

GUI 종료는 Herdr daemon 또는 실행 중 Codex process의 종료 명령이 아닙니다.

GUI 시작 시 snapshot을 받고 이후 event stream을 적용하여 runtime state를 복구하는 구조를 기본으로 합니다.

### UI framework remains undecided until Performance PoC

`egui`, `iced`, `Slint`, GPUI 계열 등의 후보는 architecture core와 분리합니다. 100,000-row history, 1,000 sessions, 50MB terminal log 등의 workload를 검증한 뒤 별도 ADR로 선택합니다.

## Consequences

### Positive

- conversation 길이와 project state lifetime을 분리할 수 있습니다.
- GUI crash / restart와 agent execution을 분리할 수 있습니다.
- Herdr runtime을 재사용하면서 제품-level completion semantics를 Rust에서 통제할 수 있습니다.
- domain logic과 UI framework가 분리되어 Performance PoC 결과에 따라 UI 기술을 교체할 수 있습니다.
- Task 단위 worktree / verification / approval 확장이 자연스럽습니다.

### Negative / Trade-offs

- Herdr runtime state와 Rust domain state를 동기화하는 reconciliation이 필요합니다.
- snapshot 이후 event ordering / gap 처리 규칙을 설계해야 합니다.
- Task와 Herdr 내부 ID의 mapping persistence가 필요합니다.
- runtime `Done`과 Task `Completed`가 다르므로 UI에 상태 모델을 명확하게 표현해야 합니다.

## Alternatives Considered

### Codex runtime을 Rust 앱 내부에서 직접 구현

장점:

- runtime을 완전히 통제할 수 있습니다.

단점:

- PTY, process lifetime, session recovery, agent detection, worktree runtime 기능을 중복 구현해야 합니다.

결론: 초기 제품 목표에 비해 범위가 과도하여 채택하지 않습니다.

### Conversation을 장기 project state로 사용

장점:

- 초기 구현이 단순합니다.

단점:

- 장기 대화가 느려지고, context와 state가 섞이며, restart/recovery 및 Task isolation이 취약해집니다.

결론: 제품의 핵심 문제를 다시 만들기 때문에 채택하지 않습니다.

### UI framework를 먼저 고정

장점:

- 화면 구현을 즉시 시작하기 쉽습니다.

단점:

- 핵심 요구인 대용량 virtualization과 streaming 성능을 검증하지 않고 장기 기술 선택을 고정하게 됩니다.

결론: Performance PoC 이후 별도 ADR로 결정합니다.

## Validation

다음으로 이 결정을 검증합니다.

- domain / Herdr adapter가 UI framework 없이 compile / test 가능
- direct `RunningCodex -> Completed` 전이 차단 test
- Blocked / attention sidebar 우선순위 test
- Milestone 1에서 GUI restart 후 Herdr session 복구 end-to-end 검증
- Phase 0에서 대용량 UI workload benchmark

## Follow-up

- [ ] ADR-0002: UI framework selection after Performance PoC
- [ ] Herdr protocol contract 확정
- [ ] snapshot / event reconciliation 정책 확정
- [ ] SQLite event store schema ADR 필요 여부 검토
