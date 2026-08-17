# Changelog

이 프로젝트의 주요 변경 사항을 기록합니다.

형식은 Keep a Changelog의 범주를 참고하되, 버전 정책이 확정되기 전까지 `Unreleased`를 중심으로 관리합니다.

## [Unreleased]

### Added

- Rust crate foundation
- Project / Task / Session domain model
- Herdr adapter contract
- Deterministic Orchestrator state transition guard
- Sidebar priority projection
- App runtime snapshot cache
- Repository README and development workflow
- ADR management system
- Incident management system
- GitHub Issue / Pull Request templates

### Changed

- None

### Fixed

- None

### Security

- None

## Release 작성 규칙

릴리스가 생기면 `Unreleased`의 내용을 새 버전으로 이동하고 날짜를 기록합니다.

```text
## [0.1.0] - YYYY-MM-DD
```

사용자/운영 관점에서 의미 없는 내부 편집은 모두 기록할 필요가 없지만, 다음은 반드시 기록합니다.

- public behavior 변경
- state / persistence migration
- breaking protocol 변경
- recovery / process lifetime 변경
- 주요 성능 개선 또는 회귀 수정
- security fix
