# teams

Microsoft Teams를 **터미널에서** 읽고, 검색하고, 스레드에 답글을 달고, 파일/이미지를 주고받을 수 있는 CLI · TUI · HTTP JSON API · MCP 서버 통합 도구입니다.

> 공식 **Microsoft Graph v1.0 API**와 **Microsoft Entra ID (OAuth 2.0)** 를 사용합니다.
> 개인/사내 Microsoft 365 계정의 Teams 채팅(1:1·그룹·모임)과 팀/채널/스레드, OneDrive/SharePoint 첨부파일, 상태(Presence)를 터미널과 AI 에이전트에서 안전하게 다룰 수 있습니다.

---

## 목차

- [주요 기능](#주요-기능)
- [빠른 시작 (Quickstart)](#빠른-시작-quickstart)
  - [로그인 방법](#로그인-방법)
  - [커스텀 Azure App (Entra ID) 설정 및 권한 스코프](#커스텀-azure-app-entra-id-설정-및-권한-스코프)
- [CLI 명령어 레퍼런스](#cli-명령어-레퍼런스)
  - [파이프라인 & 사용 예시](#파이프라인--사용-예시)
- [TUI 가이드 (키보드 + 마우스)](#tui-가이드-키보드--마우스)
- [HTTP JSON API (`teams serve`)](#http-json-api-teams-serve)
- [MCP 서버 (`teams mcp`)](#mcp-서버-teams-mcp)
- [빌드, 런처 및 npm 배포](#빌드-런처-및-npm-배포)
- [설정 및 데이터 저장 위치](#설정-및-데이터-저장-위치)

---

## 주요 기능

- **CLI**: 채팅/팀/채널 조회, Markdown 메시지 및 `@멘션` 전송, 채널 스레드 조회/답글, OneDrive/SharePoint 파일 업로드 및 인라인 이미지 전송, 첨부파일 다운로드, 리액션, 수정/삭제, FTS5 로컬 검색, `md`/`json`/`txt` 내보내기, Presence 조회/변경
- **TUI (`ratatui`)**: 3개 내비게이션 탭(채팅 · 팀/채널 트리 · 활동/@멘션), 우측 스레드 드로어, Markdown 컴포저, `@` 멘션 자동완성, `Ctrl+V` 클립보드 스크린샷 붙여넣기, `Ctrl+E` 외부 에디터, Adaptive Card / O365 Connector 카드 렌더링, 마우스 클릭/우클릭 메뉴/휠 스크롤
- **HTTP JSON API (`teams serve`)**: 로컬 에이전트·스크립트 연동용 `axum` REST 서버 (기본 `127.0.0.1:8788`, 선택적 `--token` Bearer 인증)
- **MCP 서버 (`teams mcp`)**: Claude Desktop · Claude Code · Codex 등에 바로 연결할 수 있는 stdio JSON-RPC 2.0 서버
- **로컬 SQLite WAL + FTS5**: `trigram` + `LIKE` 하이브리드 검색으로 한글/영문 부분 문자열 모두 빠르게 검색 (`--offline` 조회 지원)

---

## 빠른 시작 (Quickstart)

### 로그인 방법

```bash
# 1. 기본 브라우저 로그인 (OAuth 2.0 Authorization Code + PKCE)
teams login

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

PowerShell 예시:

```powershell
$env:TEAMS_CLIENT_ID = "<AZURE_APP_CLIENT_ID>"
$env:TEAMS_TENANT_ID = "<TENANT_ID>"
teams login --device-code
```

Azure Portal 앱 등록 시 **Mobile and desktop applications** 리디렉션 URI(`http://localhost`)와 **Public client flows**를 허용하고 아래 **Delegated Permissions**를 추가하세요.

| Graph 스코프 | 용도 |
| --- | --- |
| `offline_access` | Refresh Token 발급 및 자동 토큰 갱신 |
| `User.Read` | 현재 로그인 사용자 프로필(`/me`) 조회 |
| `User.ReadBasic.All` | 멘션/새 채팅용 조직 내 사용자 검색(`/users`) |
| `Chat.ReadWrite` | 1:1·그룹·모임 채팅 목록, 메시지 읽기/전송/수정/삭제/리액션/읽음 처리 |
| `Team.ReadBasic.All` | 가입된 팀 목록(`/me/joinedTeams`) 조회 |
| `Channel.ReadBasic.All` | 팀 채널 목록 및 멤버 조회 |
| `ChannelMessage.Read.All` | 채널 게시물, 델타 동기화, 스레드 답글 읽기 |
| `ChannelMessage.Send` | 채널 루트 게시물 및 스레드 답글 전송/수정/삭제/리액션 |
| `Files.ReadWrite.All` | OneDrive(`Microsoft Teams Chat Files`) 및 SharePoint 채널 `filesFolder` 업로드/다운로드 |
| `Presence.ReadWrite` | 내 상태(Presence) 및 대화 상대 실시간 상태 조회/변경 |

> `config.toml`에서 `scopes = [...]` 배열을 지정하면 기본 스코프 목록을 덮어쓸 수 있습니다.

---

## CLI 명령어 레퍼런스

대부분의 명령에서 `<target>` 자리에는 `conv_key`(`chat:...`, `chan:<team_id>:<channel_id>`), 원본 `chat_id`/`channel_id`, `"팀이름/채널이름"`(예: `"개발팀/일반"`), 또는 대화방/상대방 이름 일부를 그대로 넣을 수 있습니다.

| 명령 | 별칭 | 설명 | 주요 옵션 |
| --- | --- | --- | --- |
| `teams` | - | 인자 없이 실행 시 TUI 시작 | - |
| `teams login` | `in` | Microsoft Entra ID OAuth 2.0 로그인 | `--client-id`, `--tenant-id`, `--device-code`, `--client-secret`, `--token`, `--port`, `--json` |
| `teams logout` | `out` | 키링 및 fallback 자격 증명 삭제 | `--json` |
| `teams whoami` | - | 현재 로그인 사용자 및 토큰 상태 확인 | `--json` |
| `teams doctor` | - | 설정/DB/키링 경로 및 Graph 연결 진단 | `--probe`, `--json` |
| `teams chats` | `ls` | 1:1 · 그룹 · 모임 채팅 목록 | `--limit <N>`, `--refresh`, `--json` |
| `teams new-chat` | - | 새 1:1 또는 그룹 채팅 생성 | `--to <email_or_id>` (반복 가능), `--topic`, `--message`, `--json` |
| `teams teams` | - | 가입된 팀과 채널 목록 | `--refresh`, `--json` |
| `teams channels <team>` | - | 특정 팀의 채널 목록 | `--refresh`, `--json` |
| `teams read <target>` | `cat` | 채팅방 또는 채널 최근 메시지 읽기 | `--limit <N>`, `--offline`, `--json` |
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

### 파이프라인 & 사용 예시

```bash
# 1. 1:1 채팅 및 그룹 채팅 생성 · 메시지 전송
teams chats --refresh
teams new-chat --to alice@contoso.com --message "안녕하세요!"
teams new-chat --to alice@contoso.com --to bob@contoso.com --topic "백엔드 스프린트"
teams send "백엔드 스프린트" "**릴리즈 v1.2** 배포 준비 완료되었습니다."

# 2. 팀 채널 게시 (`팀이름/채널이름` 표기 + 제목 + @멘션)
teams teams --refresh
teams send "개발팀/일반" "안녕하세요 @홍길동 님, 배포 공지 확인 부탁드립니다." \
  --subject "v1.2 배포 공지" \
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
teams export "백엔드 스프린트" --format json | jq '.messages[] | {author: .author_name, text: .body_plain}'
```

---

## TUI 가이드 (키보드 + 마우스)

```bash
teams        # 또는 teams tui / teams chat
```

### 화면 구성 및 탭

- **좌측 내비게이션 레일** (`Ctrl+B`로 접기/펼치기):
  - `Ctrl+1`: **채팅** (1:1 · 그룹 · 모임 채팅 + 실시간 Presence 점 + `[+ 새 채팅]`)
  - `Ctrl+2`: **팀/채널** (`▾ 개발팀` -> `# 일반`, `🔒 비공개` 접기/펼치기 트리)
  - `Ctrl+3`: **활동** (`@멘션` 모아보기)
- **중앙 메시지 뷰**: Markdown 서식, 인용 답장, 리액션 배지, 첨부파일 칩, Adaptive Card / O365 Connector 카드 표시
- **우측 스레드 드로어**: 채널 루트 게시물에서 `t` 또는 `Enter`를 누르면 해당 스레드 답글을 나란히 보면서 바로 답글 전송 가능
- **마우스 조작**: 왼쪽 클릭으로 탭/방/메시지/첨부/버튼 선택, 오른쪽 클릭으로 컨텍스트 메뉴 열기, 휠 스크롤로 목록·대화 탐색

### 키보드 단축키

| 영역 | 키 | 동작 |
| --- | --- | --- |
| **전역 / 내비게이션** | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` | 채팅 / 팀·채널 / 활동 탭 전환 |
| | `Tab` / `Shift+Tab` | 다음 / 이전 포커스 영역 이동 |
| | `↑`/`↓` 또는 `k`/`j`, `Home`/`End` | 목록·메시지 이동 |
| | `Left` / `Right` | 좁은 터미널에서 좌/우 패널 전환 |
| | `Ctrl+B` | 좌측 내비게이션 레일 토글 |
| | `PgUp` / `u` | 이전(과거) 메시지 더 불러오기 |
| | `s` / `R` / `F5` | 전체 기록 동기화 / 목록 새로고침 |
| | `q` / `Ctrl+C` | 종료 |
| **메시지 & 스레드** | `t` (또는 `Enter`) | 선택한 채널 게시물의 우측 스레드 드로어 열기 |
| | `r` | 선택한 메시지에 답장(채널은 스레드 답글, 채팅은 인용 답장) |
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

로컬 AI 에이전트, 웹훅 스크립트, 자동화 도구가 사용할 수 있는 HTTP JSON API 서버를 실행합니다. 기본 바인딩은 `127.0.0.1:8788`입니다.

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

---

## MCP 서버 (`teams mcp`)

`teams mcp`(별칭 `teams mcp-server`)는 **stdio** 기반 **newline-delimited JSON-RPC 2.0** Model Context Protocol 서버입니다. stdout은 프로토콜 전용으로 유지되며 진단 로그는 stderr로만 출력됩니다.

먼저 한 번 로그인을 완료한 뒤 MCP 클라이언트에 등록하세요.

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

제공 도구(17개): `list_chats`, `create_chat`, `list_teams_channels`, `get_messages`, `get_thread`, `send_message`, `reply_message`, `send_file`, `download_attachment`, `react_message`, `edit_message`, `delete_message`(`confirm: true` 필수), `search_messages`, `list_members`, `mark_read`, `get_presence`, `set_presence`.

> npm으로 설치하지 않고 소스 체크아웃에서 바로 실행하려면 `command`에 `C:\\Users\\user\\source\\teams\\teams.cmd` (Windows) 또는 `/path/to/teams/teams` (macOS/Linux)를 지정하면 됩니다.

---

## 빌드, 런처 및 npm 배포

### 소스 빌드 (Rust 1.80+)

```bash
cargo build --release
./target/release/teams --help
```

- SQLite는 `rusqlite` `bundled` 기능으로 모든 플랫폼에 정적 포함됩니다.
- TLS는 `rustls`를 사용하므로 Linux/macOS에서 별도의 OpenSSL 개발 패키지가 필요하지 않습니다.

### 저장소 내장 런처

- **Windows**: `teams.cmd` (UTF-8 `chcp 65001`, 릴리즈 바이너리 미존재 시 자동 `cargo build --release` 후 실행)
- **macOS / Linux**: `./teams` (LF 줄바꿈 + `chmod +x teams`, 첫 빌드 로그는 `stderr`로 출력해 MCP stdout 보호)
- **PowerShell `$PROFILE` 등록**:

```powershell
pwsh -File .\scripts\install-profile.ps1
```

위 스크립트는 `$PROFILE`에 `# >>> teams launcher >>> ... # <<< teams launcher <<<` 블록을 멱등적으로 추가/갱신합니다.

### npm 배포 (`@shinick/teams`)

```bash
npm install -g @shinick/teams
# 또는 일회성 실행
npx @shinick/teams --version
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

## 라이선스

MIT
