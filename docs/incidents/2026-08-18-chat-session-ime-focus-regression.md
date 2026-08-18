# INC-20260818-01 — Chat session selection and IME focus regression

Status: `Mitigated`

Severity: `SEV-2`

Started: `2026-08-18 22:51 KST`

Resolved: `N/A`

## Summary

Roche native chat에서 상단 탭으로 새 세션을 선택해도 주기적인 `session.list` snapshot 반영 직후 첫 root 세션(Main)으로 되돌아가고, 모델/추론 선택 popover를 사용한 뒤 composer가 키보드 포커스를 잃어 한글 IME 입력과 Enter 전송이 함께 동작하지 않는 회귀가 반복됐다. 멀티라인 `TextEdit`이 일반 Enter를 먼저 줄바꿈으로 처리한 뒤 외부에서 전송을 판정하는 구조 때문에 정상 포커스 상태에서도 Enter가 간헐적으로 Shift+Enter처럼 남는 경로가 있었다. 같은 시점에 sidebar의 provider quota/account dashboard 호출도 제거된 상태가 확인됐다.

## Impact

- 영향받은 기능: native chat session tabs, composer IME/Enter, model/reasoning picker, sidebar provider quota visibility
- 영향받은 Chat thread / Codex session: 선택된 모든 native root session에서 재현 가능
- 데이터 / 저장소 영향: 없음
- 사용자 영향: 새 세션 유지 불가, 모델 변경 후 채팅 입력 중단, provider quota를 sidebar에서 확인 불가

## Detection

사용자가 2026-08-18 22:51 KST 실사용 중 세 증상을 연속 보고했다. 코드 점검에서 `apply_session_snapshot`, composer focus gate, sidebar layout에서 각각 deterministic한 원인이 확인됐다.

## Timeline

| Time | Event |
| --- | --- |
| 22:51 | 새 세션 선택이 Main으로 되돌아감, 모델 변경 후 한글/Enter 불가 보고 |
| 22:52 | `apply_session_snapshot()`이 750ms snapshot마다 첫 root를 강제로 선택하는 코드 확인 |
| 22:53 | picker click이 composer focus를 가져가고 `response.has_focus()`가 Enter 전송 조건인 점 확인 |
| 22:54 | IME state가 `Preedit/Commit`만 추적해 focus transition에서 stale state가 남을 수 있음을 확인 |
| 22:56 | sidebar에서 provider quota dashboard 호출이 제거된 상태 확인 |
| 22:58 | live session 선택 보존, composer refocus/IME reset, 말풍선 내부 좌측 정렬, quota dashboard 복구 적용 |

## Evidence

- 관련 log / event ID: `WebGptRuntimeEvent::SessionsUpdated`, `SessionCreated`
- 관련 commit: 현재 working tree 수정분
- 관련 thread / turn: native UI state issue; 특정 model turn에 종속되지 않음
- 관련 test: `session_snapshot_preserves_a_live_non_main_selection`, 전체 Rust quality gate

민감한 token, credential, private payload는 포함하지 않는다.

## Root Cause

### Direct cause

1. `DesktopApp::apply_session_snapshot()`이 현재 선택된 session이 새 snapshot에도 존재하는지 확인하지 않고 첫 root session을 `selected_session_id`로 덮어썼다.
2. model/reasoning popover 선택 컨트롤이 TextEdit의 keyboard focus를 가져간 뒤 composer에 focus를 복원하지 않았다. Enter 전송은 `response.has_focus()`를 요구하므로 한글 입력과 Enter가 동시에 끊겼다.
3. IME composition flag는 `Preedit/Commit`만으로 관리되어 focus loss 경계에서 stale `ime_composing=true`가 남을 수 있었다. egui 0.35에서는 legacy `ImeEvent::Enabled/Disabled`가 신뢰 가능한 lifecycle signal이 아니므로 focus boundary 자체에서 reset해야 한다.
4. multiline `TextEdit`의 기본 return key가 일반 Enter인 상태에서 widget 렌더 이후 별도 `key_pressed(Enter)`로 submit을 판정했다. 따라서 IME/focus 타이밍에 submit gate를 통과하지 못하면 이미 삽입된 newline이 그대로 남았다.
5. sidebar layout 변경에서 `render_ocx_dashboard()` 호출이 제거되고 workspace list가 남은 높이를 점유하면서 provider quota/account UI가 사라졌다.

### Contributing conditions

- ADR/Project Status에는 Windows IME 검증 필요가 기록돼 있었지만 picker focus transition에 대한 concrete invariant와 regression test가 없었다.
- session snapshot은 runtime source of truth로 취급됐지만 local UI selection은 snapshot 데이터와 별개의 user-owned state라는 구분이 코드에 없었다.
- sidebar quota renderer는 삭제되지 않았기 때문에 compile/test가 통과했고, 실제 sidebar composition 회귀를 정적 테스트가 감지하지 못했다.

## Containment

- snapshot 반영 시 현재 선택 session이 live snapshot에 존재하면 selection을 보존한다.
- session/model/reasoning 선택 후 공통 `refocus_composer()`를 통해 stale IME flag를 reset하고 다음 frame에 TextEdit focus를 복원한다.
- composer가 focus를 잃은 frame에서도 IME composition state를 reset한다.
- multiline `TextEdit::return_key`를 `Shift+Enter`로 명시해 일반 Enter가 본문 newline으로 들어가는 경로를 제거한다. 일반 Enter는 외부 submit만 담당한다.
- sidebar에 `render_ocx_dashboard()` 영역을 다시 배치하고 workspace list 높이를 제한한다.

## Resolution

현재 코드 수정은 적용됐다. 사용자 말풍선은 bubble 위치는 오른쪽에 유지하되 bubble 내부 `Label::halign(Align::Min)`까지 명시해 짧은 문장도 좌측 정렬로 고정했다. composer는 `Shift+Enter`만 newline을 생성하고 일반 Enter는 전송만 수행하도록 widget-level return key를 변경했다. session selection 보존 regression test를 추가했다. 실제 Windows 한국어 IME와 새 session 탭 동작은 새 build에서 수동 확인 후 Incident를 `Resolved`로 전환한다.

## Recovery Verification

- [x] selected live session을 snapshot이 Main으로 덮어쓰지 않는 regression test가 존재한다.
- [x] model/reasoning/session 선택 경로가 composer refocus invariant를 사용한다.
- [x] focus loss 시 stale IME composition state를 clear한다.
- [x] provider quota/account dashboard가 sidebar composition에 다시 포함됐다.
- [x] 사용자 말풍선 내부 텍스트가 좌측 정렬이다.
- [ ] 실제 Windows Korean IME로 모델 변경 후 연속 입력을 확인한다.
- [x] 일반 Enter가 TextEdit newline을 생성하지 않고 Shift+Enter만 newline을 생성하도록 설정했다.
- [ ] Enter 전송을 모델 변경 직후 실제 Windows 입력으로 확인한다.
- [ ] 새 root session 탭을 750ms 이상 선택한 상태로 유지되는지 확인한다.
- [ ] sidebar provider quota/active account/rotation threshold가 실제 데이터로 표시되는지 확인한다.

## Follow-up Actions

| Priority | Action | Tracking | Status |
| --- | --- | --- | --- |
| P0 | session selection은 runtime snapshot이 아닌 user-owned UI state로 보존 | regression test | Done |
| P0 | 모든 composer 주변 picker가 선택 종료 시 동일 refocus path 사용 | code invariant | Done |
| P1 | Windows IME/model picker/session switch 수동 smoke를 release checklist에 추가 | PROJECT_STATUS | Open |
| P1 | sidebar composition visual smoke 추가 | UI verification | Open |

## Lessons

주기적인 runtime snapshot은 session 목록과 runtime status의 source of truth이지 현재 사용자가 보고 있는 tab selection의 source of truth가 아니다. TextEdit 주변의 popover/tab/menu는 keyboard focus를 가져갈 수 있으므로 선택 종료 시 composer focus와 IME state를 하나의 invariant로 복구해야 한다. 렌더 함수가 남아 있다는 사실은 실제 화면에 기능이 존재한다는 증거가 아니므로 sidebar composition 자체를 검증해야 한다.
