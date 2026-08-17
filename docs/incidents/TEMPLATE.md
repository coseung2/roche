# INC-YYYYMMDD-NN — Short title

Status: `Detected | Contained | Diagnosing | Mitigated | Resolved | Closed`

Severity: `SEV-0 | SEV-1 | SEV-2 | SEV-3`

Started: `YYYY-MM-DD HH:MM TZ`

Resolved: `YYYY-MM-DD HH:MM TZ | N/A`

## Summary

한 문단으로 무엇이 발생했고 어떤 영향이 있었는지 적습니다.

## Impact

- 영향받은 기능:
- 영향받은 Task / Session:
- 데이터 / Git 영향:
- 사용자 영향:

## Detection

어떤 signal, log, 사용자 행동, test가 문제를 처음 드러냈는지 기록합니다.

## Timeline

| Time | Event |
| --- | --- |
| HH:MM | 최초 증상 관측 |
| HH:MM | containment 수행 |
| HH:MM | 원인 범위 축소 |
| HH:MM | mitigation 적용 |
| HH:MM | fix 검증 |

## Evidence

추측하지 말고 재현 가능한 evidence를 기록합니다.

- 관련 log / event ID:
- 관련 commit:
- 관련 Task:
- 관련 test:

민감한 token, credential, private payload는 첨부하지 않습니다.

## Root Cause

직접 원인과 이를 허용한 시스템 조건을 구분합니다.

### Direct cause

...

### Contributing conditions

...

## Containment

추가 영향을 막기 위해 취한 조치입니다.

## Resolution

영구 수정과 검증 방법을 적습니다.

## Recovery Verification

- [ ] runtime 상태와 App State가 일치한다.
- [ ] 관련 Task / Session이 올바르게 복구된다.
- [ ] Git / worktree 상태가 안전하다.
- [ ] regression test가 통과한다.
- [ ] restart / retry 경로를 확인했다.

## Follow-up Actions

| Priority | Action | Tracking | Status |
| --- | --- | --- | --- |
| P0 |  | Issue / ADR | Open |

## Lessons

다음 구현 또는 운영에서 유지해야 할 구체적인 원칙만 기록합니다.
