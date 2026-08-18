# Contributing

이 저장소의 변경은 **작업 단위가 작고, 검증 가능하며, 이유가 추적 가능**해야 합니다.

## 기본 흐름

1. 작업을 Issue 또는 `docs/PROJECT_STATUS.md`의 명시된 항목으로 정의합니다.
2. 필요하면 ADR을 먼저 작성합니다.
3. `main`에서 짧은 수명의 branch를 만듭니다.
4. 구현과 테스트를 함께 변경합니다.
5. 로컬 품질 게이트를 통과합니다.
6. PR 설명에 목표, 범위, 검증 결과, 위험을 기록합니다.
7. 리뷰 후 `main`에 병합합니다.
8. 운영 장애나 회귀가 발생하면 Incident 기록을 남기고 필요한 후속 Issue / ADR을 연결합니다.

## Branch 규칙

권장 형식:

```text
feat/<short-name>
fix/<short-name>
perf/<short-name>
refactor/<short-name>
docs/<short-name>
chore/<short-name>
incident/<short-name>
```

예:

```text
feat/direct-codex-chat
perf/sidebar-virtualization
fix/catalog-path-parse
```

직접 `main`에서 장기 개발하지 않습니다. 다만 초기 repository bootstrap처럼 원격 협업이 시작되기 전의 baseline 구성은 예외로 둘 수 있습니다.

## Commit 규칙

Conventional Commit 형태를 사용합니다.

```text
feat: connect codex app-server chat runtime
fix: reject catalog entries without a resolvable slug
perf: virtualize sidebar rows
docs: add incident response process
chore: bootstrap repository management
```

한 commit에는 가능한 한 하나의 논리적 변경만 담습니다.

## Architecture Decision Record

다음 변경은 구현 전에 ADR을 권장합니다.

- UI framework 선택
- persistence / migration 전략 변경
- Codex runtime / transport 선택 변경
- chat 상태 모델 의미 변경
- process lifetime / recovery 모델 변경
- AI backend 또는 security boundary 변경

ADR 규칙은 [`docs/adr/README.md`](docs/adr/README.md)를 따릅니다.

## Definition of Done

일반 코드 변경의 완료 조건:

- [ ] 사용자/제품 목표가 명확하다.
- [ ] 정상 경로와 실패 경로를 고려했다.
- [ ] 필요한 테스트가 추가되거나 기존 테스트로 검증된다.
- [ ] `cargo fmt --check`가 통과한다.
- [ ] `cargo test`가 통과한다.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`가 통과한다.
- [ ] 공개 계약 또는 아키텍처가 바뀌면 문서를 갱신했다.
- [ ] 위험한 운영 변경이면 rollback 또는 recovery 절차가 있다.
- [ ] 사용자에게 노출되는 상태 변경이면 상태명과 Source of Truth가 명확하다.

성능 관련 변경은 추가로 baseline과 비교 가능한 측정값을 남깁니다.

## Rust 코드 원칙

- UI와 runtime / orchestration 로직을 분리합니다.
- 외부 ID(thread / turn)를 사용자-facing 상태로 부풀리지 않습니다.
- 채팅 전송 / 중단은 Codex runtime controller를 통해서만 수행합니다.
- 큰 payload를 장기 resident memory에 고정하지 않습니다.
- blocking I/O, Git, DB bulk query, socket I/O, 대형 parsing은 UI thread에서 실행하지 않습니다.

## 테스트 전략

우선순위:

1. 순수 domain/state-machine unit test
2. adapter contract / protocol test
3. persistence migration test
4. integration test
5. UI performance / regression benchmark
6. end-to-end recovery test

특히 다음 invariant는 regression test로 보호합니다.

- Codex `Ready`는 app-server initialize 응답 수신 후에만 발생한다.
- 카탈로그 파싱은 object / string 항목을 모두 지원한다.
- 연결되지 않은 모델 항목은 연결 상태로 표시하지 않는다.
- UI thread는 Codex runtime I/O를 수행하지 않는다.

## Pull Request

PR 템플릿의 모든 관련 항목을 채웁니다. 큰 변경을 여러 목적의 PR 하나로 합치지 않습니다.

리뷰 시에는 다음 순서로 봅니다.

1. Correctness
2. State / lifecycle invariants
3. Failure / recovery
4. Performance / bounded memory
5. Maintainability
6. UX

## Incident

장애, 데이터 손실 위험, agent lifetime 손상, 잘못된 자동 merge, 상태 복구 실패 등은 [`docs/incidents/README.md`](docs/incidents/README.md)의 절차를 사용합니다.
