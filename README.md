# Project Loom

**Multi-Agent Native, Local-First Version Control System**

**Loom**은 수십 개의 AI 코딩 에이전트(`agy`, Claude Code, OpenCode 등)가 동일 저장소를 동시에 수정할 때 발생하는 작업 공간 격리 비용, 락 경합, 텍스트 라인 기반 병합 충돌을 제거하기 위해 설계된 Rust 기반 차세대 로컬 우선(Local-First) 버전 관리 시스템입니다.

**OS 네이티브 CoW(Copy-on-Write) 뷰포트**, **Tree-sitter 기반 가환 구문 패치(Commutative CST Patch)**, **실시간 추측성 병합 레이더**, **인과 검증 캡슐 및 실패 기억 원장**, **순수 Rust(`gix`) 양방향 Git 투영 계층**을 단일 워크스페이스(`loom` Thin CLI + `loomd` Single-Writer Daemon & MCP Server)로 통합했습니다.

---

## 핵심 기능

### 1. OS 네이티브 CoW 제로카피 뷰포트 (`loom-vfs`)
- 에이전트 세션마다 `.loom/viewports/` 하위에 격리된 작업 디렉터리를 생성하고 수명주기(`Spawned` → `Active` → `Verifying` → `Sealed` / `Dropped` → `GC_Pending`)를 관리합니다.
- 의존성 및 빌드 캐시(`target/`, `node_modules/`, `.venv/`)의 물리 블록은 공유하면서 컴파일러 락 파일(`.cargo-lock`, `*.lock`, `*.lck`)과 증분 빌드 디렉터리(`incremental/`)는 세션별로 격리합니다.
- **크로스 플랫폼 드라이버 지원:**
  - **macOS (APFS):** `clonefile(2)` CoW 복제
  - **Linux:** Reflink `ioctl(FICLONE)` 및 유저스페이스 `fuse-overlayfs` 자동 폴백
  - **Windows:** ReFS 블록 클로닝(`FSCTL_DUPLICATE_EXTENTS_TO_FILE`), 동적 로드(`LoadLibraryW`) 기반 `ProjectedFSLib.dll`(ProjFS), 및 미지원 NTFS 환경용 `WindowsNtfsFallback`(공용 캐시 제로카피 하드링크 + 편집 파일 독립 복사)

### 2. Tree-sitter 4개 언어 가환 구문 패치 엔진 (`loom-core`)
- **지원 언어:** Rust, TypeScript, Go, Python (4개 언어 내장 Tree-sitter 문법)
- **CST + Leading Trivia 보존:** 선언 노드 직전의 주석, Docstring, 어트리뷰트(`#[derive(...)]`), 데코레이터를 노드의 원자적 일부로 함께 보존합니다.
- **줄 번호 독립 주소 지정 (`NodeAnchor`):** `[logical_file_id + symbol_fqn + syntax_kind_id + decl_header_hash]` 조합으로 식별하므로 파일 상단에 코드가 대량 추가되어도 하단 심볼 주소가 깨지지 않습니다.
- **가환 병합 및 1급 충돌 객체:**
  - 동일 블록 내 서로 다른 멤버 삽입(`InsertMember`) 및 서로 다른 `NodeAnchor` 본문 수정(`ReplaceBody`)은 `(LamportTimestamp, PatchHash)` 순으로 무충돌 결정론적 병합을 수행합니다.
  - 동일 노드 본문 동시 수정은 `StructuralConflict`, 시그니처 변경과 외부 호출 규격 불일치는 `SemanticConflict` 1급 충돌 객체로 생성합니다.
- **비코드 / 바이너리 폴백:** `.json`, `.yaml`, `.md` 파일은 `automerge` 기반 Sequence CRDT로, 바이너리 파일은 BLAKE3 CAS 청크 해시로 처리합니다.

### 3. 실시간 추측성 병합 레이더 & 내장 MCP 서버 (`loom-daemon`, `loom-cli`)
- `notify` 파일 워처(기본 15ms 디바운싱, 대량 쓰기 감지 시 배치 모드 자동 전환)가 활성 뷰포트 변경을 포착해 증분 파싱 및 `tags.scm` 기반 `provides`/`consumes` 심볼 계약을 추출합니다.
- 작성 중인 문법 오류(`ERROR` 노드)는 `PENDING_SYNTAX` 상태로 유보하여 거짓 양성(False Positive) 충돌 경보를 방지합니다.
- 모든 활성 뷰포트 간 인메모리 N×N 섀도우 병합(`GREEN` / `YELLOW` / `RED`) 결과를 계산해 MCP 연결 세션에 `notifications/loom/radar_conflict` 알림을 푸시하고, 각 뷰포트 루트의 `.loom/RADAR_STATUS.json`을 원자적으로 갱신합니다.
- `ratatui` 기반 `loom radar --watch` TUI 대시보드와 `rmcp` 기반 내장 MCP 서버(`stdio` / Streamable HTTP)를 제공합니다.

### 4. 인과 검증 캡슐 & 실패 기억 원장 (`CausalCapsule` & `NegativeEdge`)
- 별도 스테이징 영역(`index`) 없이 뷰포트의 파일 저장을 `AstPatch` 체인으로 Append-only WAL에 기록합니다.
- `loom seal -v "<verify_cmd>"` 실행 시 검증 명령의 종료 코드, 실행 시간, 표준출력 다이제스트, 검증 시점 CST 루트 해시를 `VerificationProof`로 서명해 `CausalCapsule`을 봉인하고 트렁크에 가환 병합합니다.
- `loom drop` 또는 MCP `loom_record_deadend` 실행 시 실패한 접근법의 `touched_symbols`, `FailureKind`, 500자 이내 교훈(`lesson_summary`)을 `NegativeEdge`로 `negative_index`에 영구 기록하고 뷰포트를 클린 트렁크 상태로 복원합니다. 이후 다른 에이전트가 `loom_preflight_symbols` 호출 시 해당 경고가 자동 주입됩니다.
- 모든 상태 전이는 `op_log` 테이블에 기록되어 `loom undo [--op <seq_id>]`로 원자적 롤백이 가능하며, 비정상 종료 시 BLAKE3 체크섬 WAL에서 마지막 체크포인트부터 자동 Replay 복구됩니다.

### 5. 순수 Rust(`gix`) 양방향 Git 투영 계층
- 외부 `git` 프로세스 호출 없이 `gitoxide`(`gix`)만으로 표준 Git 객체와 양방향 동기화합니다.
- **Push (`loom git push`):** 봉인된 `CausalCapsule`을 표준 Git 커밋으로 투영하고 커밋 트레일러(`Loom-Capsule-Id`, `Loom-Agent`, `Loom-Verified`) 및 `refs/notes/loom` 네임스페이스의 Git Notes(`CausalCapsule` / `NegativeEdge` 메타데이터)를 함께 전송합니다.
- **Pull (`loom git pull`):** 원격 Git 커밋을 Tree-sitter 디퍼로 역산 흡수하고, `refs/notes/loom`의 `NegativeEdge`를 로컬 `state.redb`로 복원한 뒤 활성 뷰포트를 리베이스합니다.

---

## 아키텍처 개요

```text
+-----------------------------------------------------------------------------+
|                    Multi-Agent User Space (CLI / TUI / MCP)                 |
|       [loom CLI / ratatui TUI]       [Agent A: agy]       [Agent B: agy]    |
+------------------+--------------------------+--------------------+----------+
                   | UDS / Named Pipe         | MCP (Stdio/HTTP)   | File R/W
                   v                          v                    v
+-----------------------------------------------------------------------------+
|                OS Copy-on-Write Viewport Layer (.loom/viewports/)           |
|     [Base Trunk] <--- CoW / Hardlink Share --- [Viewport A]  [Viewport B]   |
+------------------------------------------------------+----------------------+
                                                       | notify (15ms debounce)
                                                       v
+-----------------------------------------------------------------------------+
|                     loomd (Local Single-Writer Daemon)                      |
|  [Incremental CST Differ] -> [Speculative Merge Radar (In-Memory N×N)]      |
|  [Causal Capsule Builder & Negative Index]  -> [MCP Push & RADAR_STATUS]    |
+------------------------------------------------------+----------------------+
                                                       v
+-----------------------------------------------------------------------------+
|                     Storage & Git Projection Layer (.loom/)                 |
|  [.loom/wal/] BLAKE3 WAL  |  [.loom/state.redb] 8 ACID Tables & OpLog       |
|  [.loom/cas/] Zstd-3 CAS  |  [gix Git Bridge] <-> .git & refs/notes/loom    |
+-----------------------------------------------------------------------------+
```

### 저장소 구조

| 경로 | 설명 |
| :--- | :--- |
| `crates/loom-core` | 핵심 데이터 모델, `config.toml`, BLAKE3 WAL, Zstd-3 CAS, `redb` ACID 스토어, 4개 언어 Tree-sitter CST 엔진, `automerge` CRDT, RPC 프레임 |
| `crates/loom-vfs` | `ViewportDriver` 트레이트 및 OS별 CoW 드라이버 (`macos.rs`, `linux.rs`, `windows.rs`) |
| `crates/loom-daemon` | 단일-라이터 `loomd` 데몬, 크래시 복구, 뷰포트 매니저, 실시간 `notify` 레이더, `rmcp` MCP 서버, 캡슐/실패 기억, `gix` Git 브리지 |
| `crates/loom-cli` | `loom` 초경량 CLI(멀티콜 `loomd` 진입 지원) 및 `ratatui` 레이더 TUI |
| `e2e-suite/` | 독립 Opaque-Box E2E 테스트 스위트 (Tier 0~4, 283개 테스트) |
| `benches/` | 독립 멀티 에이전트 성능 SLA 벤치마크 하네스 (`loom-bench`) |
| [`BENCHMARK_REPORT.md`](./BENCHMARK_REPORT.md) | Windows 11 호스트 10-뷰포트 성능 SLA 실측 리포트 |

### `.loom/` 디렉터리 레이아웃

```text
project-root/
├── .git/                    # 표준 Git 저장소 (--colocate-git 사용 시 공존)
└── .loom/
    ├── config.toml          # 데몬, VFS, 레이더, Git 설정
    ├── daemon.sock          # 로컬 IPC 소켓 / Windows Named Pipe 디스크립터
    ├── state.redb           # ACID 스토어 (viewports, patches, capsules, symbol_provides, symbol_consumes, negative_index, op_log, git_map)
    ├── wal/                 # Append-only BLAKE3 체크섬 로그 세그먼트
    ├── cas/                 # Zstd 레벨 3 압축 CST/바이너리 청크
    └── viewports/           # 에이전트 세션별 격리 CoW 작업 공간 (각 .loom/RADAR_STATUS.json 포함)
```

---

## 빌드 및 빠른 시작

### 요구 사항
- **Rust 툴체인:** Rust 2024 Edition 지원 `rustc` / `cargo`
- 별도 `cmake` 설치 불필요 (`zstd` 기본 C 번들 피처 사용)

### 빌드

```bash
# 전체 워크스페이스 빌드
cargo build --workspace

# 릴리즈 CLI 바이너리 빌드 (loom + 내장 __daemon)
cargo build --release --package loom-cli --bin loom
```

### 기본 워크플로우 예시

```bash
# 1. 기존 Git 저장소에 Loom 초기화 (--colocate-git로 .gitignore 자동 등록)
loom init --colocate-git

# 2. 에이전트 전용 격리 CoW 뷰포트 생성 (선택적으로 하위 명령 즉시 실행)
loom spawn -n auth-refactor -m "JWT 검증기를 비동기 Trait 기반으로 전환" -- agy

# 3. 활성 뷰포트 및 레이더 상태 확인
loom list
loom status
loom radar --watch          # 실시간 ratatui TUI 대시보드
loom radar --json           # JSON 스냅샷 출력

# 4. 검증 명령 실행 후 인과 검증 캡슐 봉인 및 트렁크 병합
loom seal --viewport auth-refactor -v "cargo test" -m "비동기 JWT 검증기 구현 완료"

# 5. 실패한 접근법 폐기 및 실패 기억(NegativeEdge) 기록
loom drop --viewport exp-lock -r "RwLock write 가드 보유 중 await 호출로 데드락 발생" -k Deadlock

# 6. 캡슐/심볼 이력 조회, 롤백 및 정리
loom log --capsules --symbol crate::auth::TokenVerifier::verify
loom undo
loom gc --prune-viewports

# 7. 표준 Git 원격과 커밋 + refs/notes/loom 양방향 동기화
loom git push origin main
loom git pull origin main
```

---

## CLI 명령어 레퍼런스

모든 명령은 전역 플래그 `--repo <PATH>`(대상 저장소 지정)와 `--json`(구조화 JSON 출력)을 지원합니다.

| 명령어 및 주요 옵션 | 설명 |
| :--- | :--- |
| `loom init [--colocate-git]` | `.loom/` 저장소 초기화, `.gitignore`에 `.loom/` 등록 및 `loomd` 데몬 기동 |
| `loom spawn -n <NAME> -m <INTENT> [-- <CMD>...]` | CoW 뷰포트 마운트 및 선택적 하위 프로세스 실행 |
| `loom list [--include-gc]` | 활성 뷰포트 목록, 상태, 미봉인 패치 및 레이더 레벨 조회 |
| `loom status` | `loomd` 데몬 상태, 스토리지(CAS/WAL) 및 저장소 요약 출력 |
| `loom radar [--watch] [--viewport <NAME_OR_ID>] [--json]` | 실시간 N×N 심볼 간섭 행렬 및 충돌 내역 조회 (`--watch` 시 TUI) |
| `loom seal [--viewport <NAME_OR_ID>] [-v <CMD>] [-m <SUMMARY>]` | 검증 명령 실행 후 `VerificationProof` 서명, `CausalCapsule` 봉인 및 트렁크 병합 |
| `loom drop [--viewport <NAME_OR_ID>] -r <REASON> [-k <KIND>]` | `NegativeEdge` 기록 후 뷰포트를 클린 트렁크 상태로 복원 및 폐기 |
| `loom log [--capsules] [--symbol <FQN>] [--limit <N>]` | 오퍼레이션 로그, 인과 캡슐 그래프, 특정 심볼 변경 및 실패 기억 조회 |
| `loom undo [--op <SEQ_ID>]` | `op_log` 기반 직전 또는 지정 시퀀스 상태 전이 원자적 롤백 |
| `loom gc [--prune-viewports]` | 종료된 뷰포트 정리, 미참조 CAS 청크 및 컴팩션된 WAL 세그먼트 정리 |
| `loom mcp serve [--viewport <NAME_OR_ID>] [--http <ADDR>]` | 내장 MCP 서버 실행 (기본 `stdio`, `--http` 지정 시 Streamable HTTP) |
| `loom git push [REMOTE] [BRANCH]` | 봉인된 캡슐과 `refs/notes/loom` 노트를 Git 원격으로 투영 전송 |
| `loom git pull [REMOTE] [BRANCH]` | 원격 Git 커밋과 `refs/notes/loom` 노트를 역산 흡수 및 활성 뷰포트 리베이스 |

> **`loom drop -k <KIND>` 지원 카테고리:** `CompileError`, `TestFailure`, `SemanticConflict`, `StructuralConflict`, `PerformanceRegression`, `DesignDeadEnd`, `Deadlock`, `Other`

---

## 내장 MCP 서버 (`loom mcp serve`)

AI 코딩 에이전트(`agy`, Claude Code 등)가 자율 협업할 수 있도록 `rmcp` 기반 MCP 서버를 내장하고 있습니다.

```bash
# Stdio 전송 (기본)
loom --repo /path/to/repo mcp serve --viewport auth-refactor

# Streamable HTTP 전송
loom --repo /path/to/repo mcp serve --viewport auth-refactor --http 127.0.0.1:8787
```

### 제공 툴 및 푸시 알림

| MCP 툴 / 알림 | 파라미터 | 동작 |
| :--- | :--- | :--- |
| `loom_preflight_symbols` | `viewport?`, `symbols?`, `files?` | 수정 전 피어 뷰포트의 활성 심볼 `provides`/`consumes`, 현재 레이더 상태, `negative_index` 실패 경고 조회 |
| `loom_radar_poll` | `viewport?` | 실시간 N×N 병합 레이더 스냅샷(`GREEN`/`YELLOW`/`RED`), `PENDING_SYNTAX` 파일, 충돌 상세 조회 |
| `loom_record_deadend` | `viewport?`, `touched_symbols?`, `failure_kind?`, `lesson_summary`, `restore_clean?` | 500자 이내 실패 교훈을 `NegativeEdge`로 기록하고 기본값(`restore_clean=true`)으로 뷰포트를 클린 트렁크로 복원 |
| `loom_seal_capsule` | `viewport?`, `verify_cmd?`, `summary?` | 뷰포트 내 검증 실행, CST 차분/병합 후 `CausalCapsule` 봉인 |
| `notifications/loom/radar_conflict` | *(Server Push)* | `YELLOW` / `RED` 충돌 상승 감지 시 연결된 MCP 피어에 실시간 푸시 알림 전송 |

---

## 설정 (`.loom/config.toml`)

```toml
schema_version = 1
idle_timeout_secs = 1800          # 활성 뷰포트 0개일 때 유휴 자동 종료 시간(초)
zstd_level = 3                    # CAS Zstd 압축 레벨 (1..=22)
wal_checkpoint_ops = 64
wal_segment_max_bytes = 16777216  # 16 MiB
colocate_git = true

[vfs]
driver = "auto"                   # auto | apfs_clonefile | linux_reflink | linux_fuse_overlay_fs | windows_refs_block_clone | windows_proj_fs | windows_ntfs_fallback | generic_copy_fallback
cache_dirs = ["target", "node_modules", ".venv"]

[radar]
debounce_ms = 15
batch_window_ms = 200
batch_threshold_events = 20
ignore_dirs = [".loom", ".git", "target", "node_modules", ".venv", "dist", "build"]

[git]
default_remote = "origin"
notes_ref = "refs/notes/loom"
```

---

## 테스트 및 성능 벤치마크

### 검증 명령어

```bash
# 정적 분석 (경고 0건 기준)
cargo clippy --workspace --all-targets -- -D warnings

# 워크스페이스 단위·통합 테스트 (284 tests, agy CLI 연동 포함)
cargo test --workspace

# Opaque-Box E2E 테스트 스위트 (Tier 0~4, 283 tests)
cargo test --manifest-path e2e-suite/Cargo.toml -- --test-threads=1

# Windows 10-뷰포트 SLA 벤치마크 실행
cargo build --release --package loom-cli --bin loom
cargo run --release --manifest-path benches/Cargo.toml -- --loom-bin target/release/loom --report BENCHMARK_REPORT.md
```

### 성능 실측 요약 (Windows 11 NTFS / `WindowsNtfsFallback`)

상세 측정 환경, 방법론 및 전체 수치는 [`BENCHMARK_REPORT.md`](./BENCHMARK_REPORT.md)를 참고하세요.

| 지표 | 실측 결과 (`WindowsNtfsFallback`, Windows 11 NTFS) |
| :--- | :--- |
| **뷰포트 스폰 지연 (Warm 순차)** | P50 `121.02 ms` / P99 `239.59 ms` (공용 캐시 116개 제로카피 하드링크, 편집 소스 38개 복사) |
| **실시간 레이더 지연 (`debounce=15ms`)** | `N=5` P50 `69.17 ms` / P99 `92.58 ms` · `N=10` P50 `89.37 ms` / P99 `156.67 ms` |
| **`loomd` 메모리 점유율 (활성 뷰포트 10개)** | 유휴 Working Set **`30.45 MiB`** (Private `14.85 MiB`) / 피크 **`32.18 MiB`** (Private `15.82 MiB`) |
| **스토리지 효율 (`.loom/cas/` vs `.git/objects/`)** | `N=5` `0.877x` (`50,935 B` vs `58,079 B` loose) · `N=10` `0.874x` (`101,578 B` vs `116,219 B` loose) |

---

## 라이선스

본 프로젝트는 `MIT OR Apache-2.0` 듀얼 라이선스로 배포됩니다.
