# Security Policy

Roche AI Workstation은 로컬 process, source repository, Codex app-server 프로세스, AI agent를 다루므로 일반 데스크톱 앱보다 강한 local trust boundary가 필요합니다.

## 현재 단계

프로젝트는 초기 개발 단계이며 공식적인 외부 security reporting channel은 아직 설정되지 않았습니다.

보안 취약점에 credential, token, private source, personal data가 포함될 수 있으면 공개 Issue에 원문을 올리지 않습니다. 저장소 관리자가 별도 private reporting channel을 구성하기 전까지 민감 정보를 commit하거나 Issue / Incident 문서에 첨부하지 않는 것을 원칙으로 합니다.

## 보안 invariant

- secret / token / credential을 source 또는 fixture에 commit하지 않습니다.
- 대형 raw tool payload와 terminal log는 민감 정보가 포함될 수 있는 데이터로 취급합니다.
- DevSpace / future MCP 연동에서 사용자 의도 없이 repository 밖의 경로를 노출하지 않습니다.
- agent가 수행하는 destructive Git 작업, merge, delete, credential 접근은 명시적인 product policy와 approval boundary 없이 자동화하지 않습니다.
- UI 표시용 domain ID와 external runtime ID를 분리합니다.
- incident evidence를 남길 때 secret을 redact합니다.
- persistence가 추가되면 database / artifact / log 파일의 저장 위치와 retention 정책을 명시합니다.

## 개발 중 보안 확인

새로운 외부 연결 또는 권한 경계가 추가되는 변경은 최소한 다음을 검토합니다.

- 어떤 process / service가 신뢰 경계 밖인가?
- 어떤 경로와 파일을 읽거나 쓸 수 있는가?
- command / prompt payload가 어디에서 오는가?
- raw terminal / tool output에 secret이 포함될 수 있는가?
- 자동 수행되는 destructive action이 있는가?
- restart / recovery 과정에서 권한 상태가 잘못 복원될 가능성이 있는가?

중요한 security boundary 결정은 ADR로 기록합니다.

## Incident

실제 또는 의심되는 보안 사고는 `docs/incidents/README.md`의 프로세스를 따르되, 민감 evidence는 repository 문서에 원문으로 저장하지 않습니다.
