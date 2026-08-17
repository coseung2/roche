# Incident Management

Incident 문서는 장애를 비난이 아니라 **재현 가능한 사실, 영향, 복구, 재발 방지** 단위로 기록하기 위한 운영 체계입니다.

## 무엇을 Incident로 기록하는가

다음 중 하나라도 해당하면 Incident 후보입니다.

- GUI 재시작으로 살아 있어야 할 agent가 종료됨
- Herdr session 복구 실패 또는 잘못된 session mapping
- Task 상태가 실제 runtime과 불일치
- 잘못된 Task가 Completed 처리됨
- worktree 격리가 깨져 다른 Task의 변경을 덮어씀
- 자동화가 의도하지 않은 merge / destructive Git 작업을 수행함
- event / artifact / test result 유실 또는 corruption
- 대용량 history 때문에 UI가 실사용 불가능한 수준으로 정지
- security boundary 위반 또는 secret 노출 위험
- 동일 원인으로 반복되는 심각한 regression

일반적인 작은 버그는 Bug Issue로 관리하고 Incident로 과대 분류하지 않습니다.

## Severity

### SEV-0 — Critical

- 데이터 또는 repository를 광범위하게 손상시킬 수 있음
- 잘못된 자동 merge / destructive action이 실제 발생
- security compromise가 확인됨

대응: 영향 확대를 먼저 차단하고 관련 자동화를 중단합니다.

### SEV-1 — High

- 실행 중 agent를 복구할 수 없음
- 여러 Task에 영향을 주는 runtime/state corruption
- 핵심 Milestone 흐름이 사실상 사용 불가

### SEV-2 — Medium

- workaround가 있으나 주요 기능이 잘못 동작
- 특정 조건에서 repeatable한 심각한 성능 / 상태 문제

### SEV-3 — Low

- 영향이 제한적이며 정상 작업을 계속할 수 있음
- 운영 기록 가치가 있지만 긴급 대응이 필요하지 않음

## Incident lifecycle

```text
Detected
  -> Contained
  -> Diagnosing
  -> Mitigated
  -> Resolved
  -> Follow-up
  -> Closed
```

### 1. Detected

최초 증상과 시간을 기록합니다. 추측보다 관측값을 우선합니다.

### 2. Contained

추가 손상을 막습니다. 예:

- 문제 기능 비활성화
- scheduler 일시 중단
- 자동 merge 금지
- 영향을 받는 worktree 보존
- 관련 log / event snapshot 보존

### 3. Diagnosing

다음 축을 분리해서 확인합니다.

- Rust App State
- Orchestrator state
- Herdr runtime state
- Git / worktree state
- persistence / event state

### 4. Mitigated

사용자가 안전하게 작업을 계속할 임시 우회 또는 복구 경로를 확보합니다.

### 5. Resolved

원인 수정과 검증을 완료합니다.

### 6. Follow-up

재발 방지 항목을 Issue / ADR / test / runbook으로 변환합니다.

## 파일 규칙

새 Incident는 아래 형식으로 생성합니다.

```text
docs/incidents/YYYY-MM-DD-short-title.md
```

[`TEMPLATE.md`](TEMPLATE.md)를 복사해 사용합니다.

Incident ID는 문서 제목에서 `INC-YYYYMMDD-NN` 형식을 사용합니다.

예:

```text
INC-20260818-01 — Herdr session recovery mismatch
```

## Blameless 원칙

문서에는 개인 비난보다 시스템 조건을 기록합니다.

좋은 표현:

> snapshot과 event subscription 사이의 sequence gap을 검증하는 guard가 없었다.

피할 표현:

> 누군가 event 처리를 잘못 구현했다.

## 종료 조건

Incident는 다음 조건이 모두 충족되어야 Closed 처리합니다.

- [ ] 영향 범위가 확인됐다.
- [ ] 원인이 사실 또는 검증된 가설 수준으로 정리됐다.
- [ ] mitigation / fix가 적용됐다.
- [ ] regression test 또는 검증 절차가 추가됐다.
- [ ] 필요한 후속 Issue / ADR이 연결됐다.
- [ ] 민감 정보가 문서에 포함되지 않았는지 확인했다.
