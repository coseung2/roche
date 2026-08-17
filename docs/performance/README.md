# Performance PoC

Phase 0의 목적은 UI framework를 먼저 선택하는 것이 아니라, Roche가 장기 실행 상태에서도 지켜야 할 성능 계약을 재현 가능한 workload로 고정하는 것입니다.

## Phase 0A — Framework-independent core

현재 구현된 표준 workload:

- 100,000 conversation messages
- 100,000 tool events
- 1,000 sessions
- 50 MiB terminal output
- 100,000회 streaming tail append 시뮬레이션

핵심 invariant:

1. Conversation / tool event materialization 수는 전체 history 크기가 아니라 viewport + overscan에 비례해야 합니다.
2. Streaming append 시 이전 전체 history를 다시 materialize하지 않습니다.
3. Terminal UI resident history는 설정된 최근 line 수로 제한합니다.
4. 실제 시간(ms)은 개발 환경별 진단값으로 기록하며 CI 합격/실패 임계값으로 사용하지 않습니다.

실행:

```bash
cargo run --release --bin perf_poc
```

정확성/구조적 회귀는 일반 테스트에 포함됩니다.

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## Phase 0B — UI framework comparison

Phase 0A workload와 동일한 조건을 각 UI 후보에 연결합니다.

후보:

- egui
- iced
- Slint
- GPUI 계열

각 prototype에서 최소한 다음을 측정합니다.

- 100,000-row conversation scroll
- jump to bottom
- streaming append
- 1,000-session sidebar filter / selection
- tool call expand / collapse
- task switching
- Markdown parsing / cached layout
- text selection
- Windows IME
- memory growth
- terminal viewport integration 가능성

후보별 구현이 서로 다른 workload를 사용하면 비교 결과를 신뢰하지 않습니다. 공통 synthetic generator와 virtualization 계약은 `src/perf/`를 사용합니다.

## Baseline 기록

측정 결과는 `BASELINE_YYYY-MM-DD.md` 형식으로 이 디렉터리에 기록합니다.

Baseline은 절대 성능 SLA가 아니라 회귀 분석과 UI framework 비교를 위한 참고점입니다. 최종 framework 선택은 별도 ADR로 확정합니다.
