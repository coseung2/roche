# Architecture Decision Records

ADR은 장기적으로 유지되어야 하는 설계 결정을 코드와 별도로 추적합니다.

## 언제 ADR을 쓰는가

다음과 같이 이후 구현 선택을 제약하는 결정에 사용합니다.

- UI framework 선택
- Codex runtime / transport 선택
- persistence / event-store 구조
- chat / thread lifecycle 의미
- process lifetime / recovery 정책
- context selection / AI backend 경계
- security boundary
- 주요 성능 trade-off

작은 구현 상세나 쉽게 되돌릴 수 있는 refactor는 ADR이 필요하지 않습니다.

## 상태

각 ADR은 다음 중 하나의 상태를 가집니다.

- `Proposed` — 검토 중
- `Accepted` — 현재 적용되는 결정
- `Superseded` — 새로운 ADR로 대체됨
- `Deprecated` — 더 이상 적용하지 않음
- `Rejected` — 검토했으나 채택하지 않음

## 파일 이름

```text
NNNN-short-kebab-title.md
```

예:

```text
0001-direct-codex-chat-runtime.md
0002-ui-framework-selection.md
```

번호는 재사용하지 않습니다.

## 변경 규칙

Accepted ADR의 과거 결론을 조용히 수정하지 않습니다.

결정이 바뀌면:

1. 새로운 ADR을 작성합니다.
2. 이전 ADR을 `Superseded`로 변경합니다.
3. 양쪽 문서에 서로의 번호를 연결합니다.

오탈자나 링크 수정처럼 의미가 바뀌지 않는 편집은 허용합니다.

## 필수 내용

[`TEMPLATE.md`](TEMPLATE.md)를 사용하고 최소한 다음을 기록합니다.

- Context
- Decision
- Consequences
- Alternatives considered
- Validation / follow-up

ADR은 구현이 아니라 **결정과 trade-off**를 기록합니다.
