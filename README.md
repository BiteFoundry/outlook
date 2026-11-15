# teams & outlook (v0.2.0 Unified Microsoft Teams + Outlook Suite)

**Microsoft Teams**와 **Microsoft Outlook(메일 · 캘린더 · 연락처 · Microsoft To Do)** 을 **터미널에서** 하나로 통합해 읽고, 검색하고, 답장하고, 파일·이미지를 주고받을 수 있는 **CLI · TUI · HTTP JSON API · MCP 서버 통합 도구**입니다.

> **단 한 번의 Microsoft Entra ID 로그인 (`teams login` 또는 `outlook login`)** 으로 Teams와 Outlook 전체 기능을 모두 잠금 해제합니다.
> 두 명령어(`teams`, `outlook`)는 동일한 단일 바이너리(`teams` / `teams.exe`), 동일한 OAuth 2.0 키링 자격 증명, 동일한 **SQLite WAL + FTS5 캐시(`cache.db`)**, 동일한 HTTP API(`teams serve`) 및 MCP 서버(`teams mcp`)를 공유합니다.

---

## 목차

- [주요 기능](#주요-기능)
- [빠른 시작 (Quickstart)](#빠른-시작-quickstart)
  - [설치 방법 (프리빌트 / npm / 프로필)](#설치-방법-프리빌트--npm--프로필)
  - [단일 로그인 (Teams + Outlook 공용)](#단일-로그인-teams--outlook-공용)
  - [커스텀 Azure App (Entra ID) 설정 및 권한 스코프](#커스텀-azure-app-entra-id-설정-및-권한-스코프)
  - [원클릭 업데이트 (`teams update` / `outlook update`)](#원클릭-업데이트-teams-update--outlook-update)
- [Outlook CLI 레퍼런스 (`outlook`)](#outlook-cli-레퍼런스-outlook)
- [Teams CLI 레퍼런스 (`teams`)](#teams-cli-레퍼런스-teams)
- [인라인 터미널 이미지 미리보기 (`v` / `--preview-images`)](#인라인-터미널-이미지-미리보기-v----preview-images)
- [TUI 가이드 (키보드 + 마우스)](#tui-가이드-키보드--마우스)
- [HTTP JSON API (`teams serve`)](#http-json-api-teams-serve)
- [MCP 서버 (`teams mcp`)](#mcp-서버-teams-mcp)
- [빌드, 런처 및 npm 배포](#빌드-런처-및-npm-배포)
- [설정 및 데이터 저장 위치](#설정-및-데이터-저장-위치)

---

## 주요 기능

- **통합 Microsoft Teams (`teams`)**: 1:1 · 그룹 · 모임 채팅, 팀/채널/스레드 조회 및 답글, Markdown 메시지 & `@멘션`, OneDrive/SharePoint 파일 업로드 및 인라인 이미지 전송, 첨부파일 다운로드, 리액션, 수정/삭제, Presence 조회/변경
- **통합 Microsoft Outlook (`outlook`)**: 메일 폴더 · 받은편지함 조회, 메일 본문/첨부파일 읽기 및 다운로드, 새 메일/답장 전송, 캘린더 일정 조회·생성(Teams 온라인 모임 링크 포함)·RSVP(수락/미정/거절), 개인 연락처 및 조직 디렉토리 검색/관리, Microsoft To Do 목록 및 작업 관리
- **인라인 터미널 이미지 미리보기 (`v` / `--preview-images`)**: TUI 내 `v` 키 또는 CLI `--preview-images` 옵션으로 채팅/메일 첨부 이미지를 외부 뷰어 없이 터미널에 직접 렌더링 (Kitty / iTerm2 / WezTerm / Ghostty 그래픽 프로토콜 및 24-bit Truecolor ANSI Half-Block `▀` 지원)
- **원클릭 GitHub Release 자가 업데이터 (`teams update` / `outlook update`)**: 최신 릴리즈 확인, `SHA256SUMS.txt` 무결성 검증, Windows NTFS 실행 중 바이너리 안전 교체를 포함한 원클릭 업데이트
- **로컬 SQLite WAL + FTS5 통합 캐시**: Teams 채팅/채널과 Outlook 메일·캘린더·연락처·To Do 데이터를 단일 `cache.db`에 저장하고, `trigram` + `LIKE` 하이브리드 검색으로 한글/영문 부분 문자열을 오프라인에서도 빠르게 검색
- **에이전트 자동화 (`teams serve` & `teams mcp`)**: 로컬 `axum` HTTP JSON API 서버(`127.0.0.1:8788`, `--token` Bearer 인증) 및 Claude Desktop · Claude Code · Codex 연동용 stdio JSON-RPC 2.0 MCP 서버

---

## 빠른 시작 (Quickstart)

### 설치 방법 (프리빌트 / npm / 프로필)

빌드된 단일 실행 파일(`teams.exe` / `teams`, SQLite·TLS 내장)이 [GitHub Releases](https://github.com/shinick-dev/teams/releases)와 npm(`@shinick/teams`)으로 제공되므로 Rust 컴파일 없이 바로 설치할 수 있습니다. 하나의 바이너리로 `teams`와 `outlook` 두 명령을 모두 실행합니다.

```powershell
# 방법 A (Windows 권장): 저장소 클론 후 PowerShell $PROFILE + PATH 심(`teams`, `outlook`) 동시 등록
gh repo clone shinick-dev/teams
cd teams
pwsh -File .\scripts\install-profile.ps1
. $PROFILE
teams --version
outlook --version
```

```bash
# 방법 B (npm 글로벌 설치): Windows / macOS / Linux 모두 `teams`와 `outlook` 동시 설치
npm install -g @shinick/teams
teams --version
outlook --version

# 또는 일회성 실행
npx @shinick/teams --version
npx --yes -p @shinick/teams outlook --version
```

```powershell
# 방법 C: 소스 클론 없이 프리빌트 단일 실행 파일(teams.exe)만 직접 다운로드
gh release download v0.2.0 -R shinick-dev/teams --pattern "teams.exe" --dir "$HOME\.local\bin" --clobber
```

> GitHub [Releases -> v0.2.0](https://github.com/shinick-dev/teams/releases/tag/v0.2.0)에서 플랫폼별 아카이브(`teams-0.2.0-*.zip` / `.tar.gz`) 또는 `teams.exe`를 내려받아 바로 실행할 수 있습니다.

### 단일 로그인 (Teams + Outlook 공용)

`teams login` 또는 `outlook login` 중 어느 쪽으로든 한 번만 로그인하면 Teams와 Outlook 모두 즉시 사용할 수 있습니다.

```bash
# 1. 기본 브라우저 로그인 (OAuth 2.0 Authorization Code + PKCE)
teams login
# 또는 outlook 페르소나로 로그인해도 동일한 자격 증명에 저장됩니다
outlook login

# 2. 헤드리스/원격 SSH 환경: Device Code 로그인
teams login --device-code

# 3. 이미 발급받은 Graph Access Token 직접 전달
teams login --token "$GRAPH_TOKEN"
# 또는 환경 변수 사용
export TEAMS_ACCESS_TOKEN="$GRAPH_TOKEN"

# 로그인 상태 및 Graph 연결 진단
teams whoami
teams doctor --probe
```

### 커스텀 Azure App (Entra ID) 설정 및 권한 스코프

기본적으로 내장 다중 테넌트 공개 클라이언트 ID를 사용하지만, 사내 보안 정책에 따라 관리자 동의가 필요한 경우 직접 등록한 Entra ID 앱을 지정할 수 있습니다.

```bash
# CLI 플래그로 지정
teams login --client-id "<AZURE_APP_CLIENT_ID>" --tenant-id "<TENANT_ID_OR_COMMON>"

# 또는 환경 변수 지정 (config.toml 보다 우선 적용)
export TEAMS_CLIENT_ID="<AZURE_APP_CLIENT_ID>"
export TEAMS_TENANT_ID="<TENANT_ID>"          # 기본값: common
```

Azure Portal 앱 등록 시 **Mobile and desktop applications** 리디렉션 URI(`http://localhost`)와 **Public client flows**를 허용하고 아래 **Delegated Permissions**를 추가하세요.

| 영역 | Graph 스코프 | 용도 |
| --- | --- | --- |
| **공통 / 인증** | `offline_access` | Refresh Token 발급 및 자동 토큰 갱신 |
| | `User.Read` | 현재 로그인 사용자 프로필(`/me`) 조회 |
| | `User.ReadBasic.All` | 멘션/새 채팅/연락처용 조직 내 사용자 검색(`/users`) |
| **Teams** | `Chat.ReadWrite` | 1:1·그룹·모임 채팅 목록, 메시지 읽기/전송/수정/삭제/리액션/읽음 처리 |
| | `Team.ReadBasic.All` | 가입된 팀 목록(`/me/joinedTeams`) 조회 |
| | `Channel.ReadBasic.All` | 팀 채널 목록 및 멤버 조회 |
| | `ChannelMessage.Read.All` | 채널 게시물, 델타 동기화, 스레드 답글 읽기 |
| | `ChannelMessage.Send` | 채널 루트 게시물 및 스레드 답글 전송/수정/삭제/리액션 |
| | `Files.ReadWrite.All` | OneDrive(`Microsoft Teams Chat Files`) 및 SharePoint 채널 `filesFolder` 업로드/다운로드 |
| | `Presence.ReadWrite` | 내 상태(Presence) 및 대화 상대 실시간 상태 조회/변경 |
| **Outlook (v0.2.0 추가)** | `Mail.ReadWrite` | 메일 폴더, 메시지 읽기, 읽음/플래그/이동/삭제 및 초안·답장 관리 |
| | `Mail.Send` | 새 메일 및 파일 첨부 메일 발송(`/me/sendMail`) |
| | `Calendars.ReadWrite` | 캘린더 일정 조회, 새 일정·Teams 온라인 모임 생성, RSVP(수락/미정/거절) |
| | `Contacts.ReadWrite` | 개인 연락처 조회/생성/삭제(`/me/contacts`) |
| | `Tasks.ReadWrite` | Microsoft To Do 목록 및 작업 조회/생성/완료 처리/삭제(`/me/todo`) |

> `config.toml`에서 `scopes = [...]` 배열을 지정하면 기본 스코프 목록을 덮어쓸 수 있습니다. v0.1.x에서 로그인한 적이 있다면 Outlook 권한 추가를 위해 `teams login` 또는 `outlook login`을 한 번 다시 실행해 새 스코프를 동의하세요.

### 원클릭 업데이트 (`teams update` / `outlook update`)

GitHub Releases(`shinick-dev/teams`)에서 최신 버전을 확인하고 `SHA256SUMS.txt` 해시 검증을 거쳐 현재 실행 바이너리를 안전하게 교체합니다.

```bash
# 새 버전 확인만 수행 (교체 없음)
teams update --check
outlook update --check

# 최신 릴리즈로 원클릭 업데이트
teams update
outlook update

# 동일 버전 재설치 강제
teams update --force
```

---

## Outlook CLI 레퍼런스 (`outlook`)

`outlook` 명령은 내부적으로 `TEAMS_PERSONA=outlook`으로 단일 바이너리를 실행하며, 인자 없이 실행하면 Outlook TUI로 진입합니다. (`teams` 바이너리에서도 동일한 하위 기능을 사용할 수 있습니다.)

| 명령 | 설명 | 주요 사용 예시 |
| --- | --- | --- |
| `outlook` | Outlook 페르소나 TUI 실행 (메일·캘린더·연락처·To Do + Teams 전환 가능) | `outlook` |
| `outlook inbox` | 받은편지함 또는 지정 폴더 메일 목록 조회 | `outlook inbox --limit 30 --json` |
| `outlook read <mail_id>` | 메일 상세 본문·첨부파일 조회 및 인라인 이미지 미리보기 | `outlook read <mail_id> --preview-images` |
| `outlook send` | 새 메일 전송 (제목·수신자·Markdown 본문·파일 첨부) | `outlook send --to alice@contoso.com --subject "주간 보고" "첨부 확인 바랍니다." --attach ./report.pdf` |
| `outlook reply <mail_id>` | 메일 답장 전송 (`--all` 전체 답장) | `outlook reply <mail_id> "확인했습니다. 감사합니다!"` |
| `outlook cal` | 캘린더 일정 조회(`today`/`week`/`list`), Teams 모임 생성, RSVP | `outlook cal today`, `outlook cal create ...`, `outlook cal rsvp <id> accept` |
| `outlook contacts` | 개인 연락처 목록, 조직 디렉토리 통합 검색, 연락처 추가/삭제 | `outlook contacts list`, `outlook contacts search "김철수"` |
| `outlook todo` | Microsoft To Do 목록 조회, 작업 추가, 완료 토글, 삭제 | `outlook todo tasks`, `outlook todo add "배포 점검"`, `outlook todo done <task_id>` |
| `outlook update` | GitHub Release 최신 버전 확인 및 원클릭 자가 업데이트 | `outlook update --check`, `outlook update` |

### Outlook 사용 예시

```bash
# 1. 받은편지함 조회 & 메일 읽기 (첨부 이미지 터미널 미리보기 포함)
outlook inbox --limit 20
outlook read "<message_id>" --preview-images

# 2. 새 메일 전송 (첨부파일 포함) 및 답장
outlook send --to "alice@contoso.com" --subject "v0.2.0 배포 안내" \
  "안녕하세요, 통합 Teams + Outlook CLI v0.2.0 안내드립니다." \
  --attach ./release-notes.md
outlook reply "<message_id>" "공유 감사합니다. 오늘 중 검토하겠습니다."

# 3. 캘린더 일정 조회 · Teams 온라인 회의 생성 · 수락(RSVP)
outlook cal today
outlook cal create --subject "아키텍처 리뷰" \
  --start "2026-11-16T10:00:00" --end "2026-11-16T11:00:00" \
  --to "alice@contoso.com" --teams-meeting
outlook cal rsvp "<event_id>" accept

# 4. 연락처 / 조직 디렉토리 검색 및 Microsoft To Do 관리
outlook contacts search "김철수"
outlook todo tasks
outlook todo add "릴리즈 체크리스트 확인" --due "2026-11-16"
outlook todo done "<task_id>"
```

---

## Teams CLI 레퍼런스 (`teams`)

대부분의 명령에서 `<target>` 자리에는 `conv_key`(`chat:...`, `chan:<team_id>:<channel_id>`), 원본 `chat_id`/`channel_id`, `"팀이름/채널이름"`(예: `"개발팀/일반"`), 또는 대화방/상대방 이름 일부를 그대로 넣을 수 있습니다.

| 명령 | 별칭 | 설명 | 주요 옵션 |
| --- | --- | --- | --- |
| `teams` | - | 인자 없이 실행 시 TUI 시작 | - |
| `teams login` | `in` | Microsoft Entra ID OAuth 2.0 로그인 (Teams + Outlook 공용) | `--client-id`, `--tenant-id`, `--device-code`, `--client-secret`, `--token`, `--port`, `--json` |
| `teams logout` | `out` | 키링 및 fallback 자격 증명 삭제 | `--json` |
| `teams whoami` | - | 현재 로그인 사용자 및 토큰 상태 확인 | `--json` |
| `teams doctor` | - | 설정/DB/키링 경로 및 Graph 연결 진단 | `--probe`, `--json` |
| `teams update` | - | GitHub Releases 최신 버전 확인 및 자가 업데이트 | `--check`, `--force`, `--json` |
| `teams chats` | `ls` | 1:1 · 그룹 · 모임 채팅 목록 | `--limit <N>`, `--refresh`, `--json` |
| `teams new-chat` | - | 새 1:1 또는 그룹 채팅 생성 | `--to <email_or_id>` (반복 가능), `--topic`, `--message`, `--json` |
| `teams teams` | - | 가입된 팀과 채널 목록 | `--refresh`, `--json` |
| `teams channels <team>` | - | 특정 팀의 채널 목록 | `--refresh`, `--json` |
| `teams read <target>` | `cat` | 채팅방 또는 채널 최근 메시지 읽기 | `--limit <N>`, `--offline`, `--json` |
| `teams preview` | - | 메시지 첨부 또는 로컬 이미지 터미널 미리보기 | `--file <path>`, `--attachment <id>`, `--cols`, `--rows` |
| `teams thread <chan> <root_id>` | - | 팀 채널 루트 게시물 + 스레드 답글 조회 | `--limit <N>`, `--offline`, `--json` |
| `teams send <target> <md>` | `msg` | Markdown 메시지 전송 | `--subject`, `--mention <user_id:DisplayName>` (반복), `--important`, `--json` |
| `teams reply <target> <msg_id> <md>` | - | 채널 스레드 답글 또는 채팅 인용 답장 | `--mention <user_id:DisplayName>`, `--important`, `--json` |
| `teams send-file <target> <path>` | `file` | OneDrive/SharePoint 업로드 후 파일 전송 | `--caption`, `--thread <root_id>`, `--inline`, `--json` |
| `teams download <target> <msg_id>` | `dl` | 메시지 첨부파일/인라인 이미지 다운로드 | `--index <N>`, `--thread <root_id>`, `--out <dir>`, `--json` |
| `teams react <target> <msg_id> <type>` | - | 리액션 추가/해제 (`like`, `heart`, `laugh`, `surprised`, `sad`, `angry`) | `--off`, `--thread <root_id>`, `--json` |
| `teams edit <target> <msg_id> <md>` | - | 내가 보낸 메시지 수정 | `--thread <root_id>`, `--json` |
| `teams delete <target> <msg_id>` | - | 내가 보낸 메시지 삭제 (`--json` 시 `--yes` 필수) | `--thread <root_id>`, `--yes`, `--json` |
| `teams members <target>` | - | 대화방/채널 구성원 및 Presence 조회 | `--offline`, `--json` |
| `teams search <query>` | - | 로컬 SQLite FTS5 + LIKE 하이브리드 검색 | `--target <target>`, `--limit <N>`, `--json` |
| `teams sync` | - | 대화방/팀/채널 및 최근 메시지 로컬 동기화 | `--target <target>`, `--limit <N>`, `--json` |
| `teams export <target>` | - | 대화를 파일 또는 표준출력으로 내보내기 | `--format md\|json\|txt`, `--include-threads`, `--limit <N>`, `--offline`, `--out <path>` |
| `teams presence` | - | 내 상태(Presence) 조회 또는 변경 | `--set Available\|Busy\|DoNotDisturb\|BeRightBack\|Away\|Reset`, `--activity`, `--expiration-mins`, `--json` |
| `teams tui` | `chat` | 인터랙티브 TUI 실행 | - |
| `teams serve` | `api` | 에이전트용 로컬 HTTP JSON API 서버 실행 | `--bind <IP>`, `--port <8788>`, `--token <secret>` |
| `teams mcp` | `mcp-server` | 에이전트용 stdio MCP(JSON-RPC 2.0) 서버 실행 | - |

### Teams 파이프라인 & 사용 예시

```bash
# 1. 1:1 채팅 및 그룹 채팅 생성 · 메시지 전송
teams chats --refresh
teams new-chat --to alice@contoso.com --message "안녕하세요!"
teams new-chat --to alice@contoso.com --to bob@contoso.com --topic "백엔드 스프린트"
teams send "백엔드 스프린트" "**릴리즈 v0.2.0** 배포 준비 완료되었습니다."

# 2. 팀 채널 게시 (`팀이름/채널이름` 표기 + 제목 + @멘션)
teams teams --refresh
teams send "개발팀/일반" "안녕하세요 @홍길동 님, 배포 공지 확인 부탁드립니다." \
  --subject "v0.2.0 배포 공지" \
  --mention "user-guid-123:홍길동" \
  --important

# 3. 채널 스레드 조회 및 답글
teams read "개발팀/일반" --json | jq '.[-1].msg_id'
teams thread "개발팀/일반" 1700000000000
teams reply "개발팀/일반" 1700000000000 "확인했습니다. 테스트 통과!"

# 4. 파일 업로드 (OneDrive Chat Files / SharePoint Channel filesFolder) 및 인라인 이미지
teams send-file "홍길동" ./report.pdf --caption "주간 보고서입니다."
teams send-file "개발팀/일반" ./release-notes.md --thread 1700000000000 --caption "릴리즈 노트"
teams send-file "개발팀/일반" ./screenshot.png --inline --caption "대시보드 캡처"

# 5. 첨부파일 다운로드 · 리액션 · 검색 · 내보내기
teams download "홍길동" 1700000000100 --out ./downloads
teams react "개발팀/일반" 1700000000000 heart
teams search "배포" --target "개발팀/일반" --json
teams export "개발팀/일반" --format md --include-threads --out ./general-archive.md
```

---

## 인라인 터미널 이미지 미리보기 (`v` / `--preview-images`)

PNG, JPEG, GIF, WebP 이미지를 다운로드하거나 외부 창을 열지 않고 **터미널 안에서 바로 미리보기**할 수 있습니다.

- **TUI (`v`)**: 이미지 첨부가 포함된 Teams 메시지나 Outlook 메일을 선택한 상태에서 `v` 키를 누르면 인라인 이미지 미리보기 오버레이/뷰가 열립니다 (`config.toml`의 `inline_images = true`가 기본값).
- **CLI (`--preview-images` / `preview`)**: `outlook read <mail_id> --preview-images` 또는 `teams preview <target> <msg_id>`(`preview-file <path>`) 실행 시, 터미널 환경에 맞춰 **Kitty Graphics Protocol**, **iTerm2/WezTerm Inline Image Protocol**, 또는 호환 가능한 **24-bit Truecolor Unicode Half-Block(`▀`) ANSI 시퀀스**로 자동 렌더링합니다.

---

## TUI 가이드 (키보드 + 마우스)

```bash
teams        # Teams 페르소나로 TUI 시작 (Ctrl+1~3 Teams 탭 + Outlook 탭 전환 가능)
outlook      # Outlook 페르소나로 TUI 시작 (메일 · 캘린더 · 연락처 · To Do 탭 우선 표시)
```

### 화면 구성 및 탭

- **좌측 내비게이션 레일** (`Ctrl+B`로 접기/펼치기):
  - **Teams 탭**: 채팅(1:1 · 그룹 · 모임 + 실시간 Presence 점 + `[+ 새 채팅]`), 팀/채널 트리, 활동(`@멘션` 모아보기)
  - **Outlook 탭**: 메일(폴더별 받은편지함/보낸편지함/임시보관함), 캘린더(일정 & Teams 온라인 모임 링크), 연락처(개인 연락처 + 조직 디렉토리), To Do(할 일 목록 & 완료 토글)
- **중앙 메시지 / 메일 뷰**: Markdown 서식, 인용 답장, 리액션 배지, 첨부파일 칩, Adaptive Card / O365 Connector 카드, 인라인 이미지 미리보기(`v`)
- **우측 스레드 / 상세 드로어**: 채널 루트 게시물에서 `t` 또는 `Enter`를 눌러 스레드 답글을 나란히 보며 답글 전송
- **마우스 조작**: 왼쪽 클릭으로 탭/방/메시지/첨부/버튼 선택, 오른쪽 클릭으로 컨텍스트 메뉴 열기, 휠 스크롤로 목록·대화 탐색

### 주요 키보드 단축키

| 영역 | 키 | 동작 |
| --- | --- | --- |
| **전역 / 내비게이션** | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` | 채팅 / 팀·채널 / 활동 탭 전환 (Outlook 탭 전환 지원) |
| | `Tab` / `Shift+Tab` | 다음 / 이전 포커스 영역 이동 |
| | `↑`/`↓` 또는 `k`/`j`, `Home`/`End` | 목록·메시지·메일 이동 |
| | `Left` / `Right` | 좁은 터미널에서 좌/우 패널 전환 |
| | `Ctrl+B` | 좌측 내비게이션 레일 토글 |
| | `PgUp` / `u` | 이전(과거) 메시지 더 불러오기 |
| | `s` / `R` / `F5` | 전체 기록 동기화 / 목록 새로고침 |
| | `q` / `Ctrl+C` | 종료 |
| **메시지 · 스레드 · 이미지** | `t` (또는 `Enter`) | 선택한 채널 게시물의 우측 스레드 드로어 열기 |
| | `r` | 선택한 메시지/메일에 답장 |
| | `v` | 선택한 메시지/메일의 이미지 첨부 인라인 터미널 미리보기 |
| | `e` / `+` / `=` | 리액션 선택 오버레이 열기 |
| | `y` | 선택한 메시지 본문 복사 |
| | `d` / `[` / `]` | 첨부파일 다운로드 / 이전·다음 첨부 선택 |
| | `Del` | 내가 보낸 메시지 삭제 확인 |
| | `m` | 컨텍스트 메뉴 열기 |
| **Markdown 컴포저** | `i` | 입력창 포커스 |
| | `Enter` | 메시지 전송 |
| | `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | 줄바꿈 (터미널이 `Shift+Enter`를 구분하지 못할 때 `Alt+Enter`/`Ctrl+J` 사용) |
| | `Ctrl+S` | 채널 루트 게시물 제목(`subject`) 입력란 토글 |
| | `@` | 대화방/채널 구성원 `@멘션` 자동완성 팝업 |
| | `Ctrl+V` | 클립보드 텍스트 또는 **스크린샷 이미지** 붙여넣기(인라인 업로드 대기열 추가) |
| | `Ctrl+E` | 외부 에디터(`$VISUAL` / `$EDITOR`)로 긴 메시지 작성 |
| | `Ctrl+Z` / `Ctrl+Y` | 실행 취소(Undo) / 다시 실행(Redo) |
| | `Ctrl+A` / `Ctrl+C` / `Ctrl+X` | 전체 선택 / 복사 / 잘라내기 |
| | `Esc` | 작성 취소 또는 포커스 복귀 |
| **오버레이** | `/` 또는 `Ctrl+F` | 메시지 검색 (`Esc`로 닫기) |
| | `Ctrl+K` | 명령 팔레트 |
| | `Ctrl+P` | 내 상태(Presence) 변경 |
| | `Ctrl+O` | 파일 선택 및 전송 오버레이 |
| | `Ctrl+N` | 새 1:1 / 그룹 채팅 만들기 |
| | `Ctrl+T` | 현재 대화 첨부파일 트레이 |
| | `Ctrl+U` | 대화방/채널 정보 및 구성원 보기 |
| | `Ctrl+,` | 설정 오버레이 |
| | `?` | 단축키 도움말 |

---

## HTTP JSON API (`teams serve`)

로컬 AI 에이전트, 웹훅 스크립트, 자동화 도구가 사용할 수 있는 HTTP JSON API 서버를 실행합니다. 기본 바인딩은 `127.0.0.1:8788`입니다. Teams와 Outlook 데이터 모두 동일한 서버와 SQLite 캐시를 통해 제공됩니다.

```bash
teams serve                                # http://127.0.0.1:8788
teams serve --port 9000 --token secret123  # /health 외 모든 요청에 Bearer 토큰 요구
```

```bash
curl http://127.0.0.1:8788/health
curl http://127.0.0.1:8788/conversations?refresh=true
curl "http://127.0.0.1:8788/conversations/개발팀%2F일반/messages?limit=20"
curl -X POST http://127.0.0.1:8788/conversations/홍길동/messages \
  -H "Content-Type: application/json" \
  -d '{"markdown":"안녕하세요!"}'
```

| 메서드 & 경로 | 설명 |
| --- | --- |
| `GET /health` | 서버 상태, `user_id`, 버전 확인 (인증 예외) |
| `GET /conversations` | 대화방·채널 목록 (`?refresh=true`, `?kind=oneOnOne\|group\|meeting\|channel`) |
| `POST /chats` | 새 채팅 생성 (`{"members":[...], "topic":"..."}`) |
| `GET /teams` | 팀 및 채널 목록 (`?refresh=true`) |
| `GET /conversations/{key}/messages` | 메시지 조회 (`?limit=50`, `?offline=true`) |
| `POST /conversations/{key}/messages` | 메시지 전송 (`markdown`, `subject`, `mentions`, `important`) |
| `GET /conversations/{key}/threads/{root_id}` | 채널 스레드 루트 + 답글 조회 |
| `POST /conversations/{key}/messages/{msg_id}/replies` | 스레드 답글 또는 인용 답장 전송 |
| `POST /conversations/{key}/files` | 파일/인라인 이미지 업로드 전송 (`path`, `caption`, `thread_root_id`, `inline`) |
| `POST /conversations/{key}/messages/{msg_id}/attachments/download` | 첨부파일 다운로드 (`index`, `out_dir`, `thread_root_id`) |
| `POST /conversations/{key}/messages/{msg_id}/reactions` | 리액션 설정/해제 (`reaction`, `enable`, `thread_root_id`) |
| `PATCH /conversations/{key}/messages/{msg_id}` | 메시지 수정 (`markdown`, `thread_root_id`) |
| `DELETE /conversations/{key}/messages/{msg_id}` | 메시지 삭제 (`?thread_root_id=...`) |
| `POST /conversations/{key}/read` | 채팅 읽음 처리 |
| `GET /conversations/{key}/members` | 구성원 목록 조회 |
| `GET /search?q=...` | FTS5 메시지 검색 (`&target=...&limit=50`) |
| `GET /presence` · `POST /presence` | 내/사용자 Presence 조회 및 변경 |

> 전체 엔드포인트 명세(Outlook 메일·캘린더·연락처·To Do 포함)는 [docs/API.md](docs/API.md) 및 [docs/openapi.yaml](docs/openapi.yaml)을 참고하세요.

---

## MCP 서버 (`teams mcp`)

`teams mcp`(별칭 `teams mcp-server`)는 **stdio** 기반 **newline-delimited JSON-RPC 2.0** Model Context Protocol 서버입니다. stdout은 프로토콜 전용으로 유지되며 진단 로그는 stderr로만 출력됩니다.

먼저 한 번 로그인(`teams login` 또는 `outlook login`)을 완료한 뒤 MCP 클라이언트에 등록하세요.

```bash
teams login
```

### Claude Desktop 설정 (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "teams": {
      "command": "teams",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code 등록

```bash
claude mcp add teams -- teams mcp
```

### OpenAI Codex CLI 설정 (`~/.codex/config.toml`)

```toml
[mcp_servers.teams]
command = "teams"
args = ["mcp"]
```

- **Teams 도구**: `list_chats`, `create_chat`, `list_teams_channels`, `get_messages`, `get_thread`, `send_message`, `reply_message`, `send_file`, `download_attachment`, `react_message`, `edit_message`, `delete_message`(`confirm: true` 필수), `search_messages`, `list_members`, `mark_read`, `get_presence`, `set_presence`
- **Outlook 도구**: 메일/캘린더/연락처/To Do MCP 도구 상세 목록 및 인자 스키마는 [docs/MCP.md](docs/MCP.md)를 참고하세요.

> npm으로 설치하지 않고 소스 체크아웃에서 바로 실행하려면 `command`에 `C:\\Users\\user\\source\\teams\\teams.cmd` (Windows) 또는 `/path/to/teams/teams` (macOS/Linux)를 지정하면 됩니다.

---

## 빌드, 런처 및 npm 배포

### 소스 빌드 (Rust 1.80+)

```bash
cargo build --release
./target/release/teams --help
TEAMS_PERSONA=outlook ./target/release/teams --help
```

- SQLite는 `rusqlite` `bundled` 기능으로 모든 플랫폼에 정적 포함됩니다.
- TLS는 `rustls`를 사용하므로 Linux/macOS에서 별도의 OpenSSL 개발 패키지가 필요하지 않습니다.
- 단일 `teams` / `teams.exe` 바이너리가 `teams`와 `outlook` 두 명령을 모두 처리합니다 (`TEAMS_PERSONA=outlook` 또는 실행 파일명 `outlook*` 감지).

### 저장소 내장 런처

- **Windows**: `teams.cmd` 및 `outlook.cmd` (UTF-8 `chcp 65001`, 릴리즈 바이너리 미존재 시 `gh` 다운로드 또는 `cargo build --release` 후 실행; `outlook.cmd`는 `TEAMS_PERSONA=outlook` 설정 후 `teams.cmd` 호출)
- **macOS / Linux**: `./teams` 및 `./outlook` (LF 줄바꿈 + `chmod +x`, 첫 빌드 로그는 `stderr`로 출력해 MCP stdout 보호)
- **PowerShell `$PROFILE` 등록**:

```powershell
pwsh -File .\scripts\install-profile.ps1
```

위 스크립트는 `$PROFILE`에 `# >>> teams launcher >>> ... # <<< teams launcher <<<` 블록으로 `function teams`와 `function outlook`을 멱등적으로 추가/갱신하며, `$HOME\Tools\bin`이 존재할 경우 `teams.cmd`와 `outlook.cmd` 심도 함께 생성합니다.

### npm 배포 (`@shinick/teams`)

```bash
npm install -g @shinick/teams
# 글로벌 설치 시 `teams`와 `outlook` 두 명령 모두 등록됩니다
teams --version
outlook --version

# 또는 일회성 실행
npx @shinick/teams --version
npx --yes -p @shinick/teams outlook --version
```

플랫폼별 선택적 의존성(`@shinick/teams-win32-x64`, `win32-arm64`, `darwin-x64`, `darwin-arm64`, `linux-x64`, `linux-arm64`) 구조와 CI 릴리즈 절차는 [docs/NPM.md](docs/NPM.md)를 참고하세요.

---

## 설정 및 데이터 저장 위치

| 항목 | 기본 위치 (Windows / Linux · macOS) | 재정의 환경 변수 |
| --- | --- | --- |
| 설정 파일 (`config.toml`) | `%APPDATA%\teams-cli\config.toml` / `~/.config/teams-cli/config.toml` | `TEAMS_CONFIG_DIR` |
| 임시 저장(Drafts) | `<config_dir>\drafts.json` | `TEAMS_CONFIG_DIR` |
| 자격 증명(OAuth 토큰) | OS Keyring (`teams-cli` / `default`) + fallback `<config_dir>\credentials.json` | `TEAMS_DISABLE_KEYRING=1` |
| 로컬 SQLite 캐시 (`cache.db`) | `%LOCALAPPDATA%\teams-cli\<user_id>\cache.db` / `~/.local/share/teams-cli/<user_id>/cache.db` | `TEAMS_DATA_DIR` |
| 첨부파일 다운로드 폴더 | `~/Downloads/teams` (또는 `config.toml` `download_dir`) | - |
| Graph / Auth 엔드포인트 | `https://graph.microsoft.com/v1.0` / `https://login.microsoftonline.com` | `TEAMS_GRAPH_BASE_URL`, `TEAMS_AUTH_BASE_URL` |
| 실행 페르소나 / 업데이터 저장소 | `teams` (또는 `outlook`) / `shinick-dev/teams` | `TEAMS_PERSONA`, `TEAMS_GITHUB_REPO` |

## 라이선스

MIT
