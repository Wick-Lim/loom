# Project Loom (`loom-vcs`) 릴리즈 및 배포 가이드

이 문서는 GitHub Releases, Homebrew, npm/npx, Shell/PowerShell 인스톨러, Scoop, Winget, crates.io 전 채널에 `loom-vcs`를 배포하기 위한 일회성 사전 설정과 릴리즈 절차를 정리합니다.

---

## 1. 일회성 사전 설정 (최초 1회)

GitHub Actions 릴리즈 워크플로우([`.github/workflows/release.yml`](./.github/workflows/release.yml))는 아래 저장소 및 GitHub Secrets가 등록되어 있을 때 자동으로 각 패키지 매니저에 배포합니다. (특정 시크릿이 비어 있으면 해당 배포 잡만 안전하게 건너뜁니다.)

### 1.1. 외부 저장소 생성
1. **Homebrew Tap (`Wick-Lim/homebrew-tap`):**
   - GitHub에 공개 저장소 `Wick-Lim/homebrew-tap`을 생성합니다(빈 `README.md` 또는 `Formula/` 폴더 포함).
   - 이후 `brew install wick-lim/tap/loom-vcs`로 설치할 수 있습니다.
2. **Scoop Bucket (`Wick-Lim/scoop-bucket`, 선택):**
   - 공식 버킷 자동 업데이트를 원하면 공개 저장소 `Wick-Lim/scoop-bucket`을 생성합니다.
   - 또는 URL 직접 설치(`scoop install https://raw.githubusercontent.com/Wick-Lim/loom/main/packaging/scoop/loom-vcs.json`)만 사용할 수도 있습니다.

### 1.2. GitHub Repository Secrets 등록 (`Settings → Secrets and variables → Actions`)

| Secret 이름 | 발급처 및 필요한 권한 | 대상 잡 |
| :--- | :--- | :--- |
| `NPM_TOKEN` | npmjs.com Access Token (Automation 또는 Granular Access Token, `loom-vcs` 및 `loom-vcs-*` 쓰기 권한) | `publish-npm` |
| `CARGO_REGISTRY_TOKEN` | crates.io API Token (`publish-new` + `publish-update`) | `publish-crates` |
| `HOMEBREW_TAP_GITHUB_TOKEN` | GitHub PAT (`Wick-Lim/homebrew-tap` `Contents: Read and write` 권한) | `publish-homebrew` |
| `SCOOP_BUCKET_GITHUB_TOKEN` | GitHub PAT (`Wick-Lim/scoop-bucket` `Contents: Read and write` 권한) | `publish-scoop` |

> **참고 (npm 패키지 이름):** `loom-vcs` 및 6개 플랫폼 패키지(`loom-vcs-darwin-arm64`, `loom-vcs-darwin-x64`, `loom-vcs-linux-x64`, `loom-vcs-linux-arm64`, `loom-vcs-win32-x64`, `loom-vcs-win32-arm64`)가 현재 npm에서 사용 가능하므로 첫 `npm publish` 시 자동 등록됩니다. Trusted Publishing(OIDC)을 쓰려는 경우 첫 배포 이후 npm 패키지 설정에서 GitHub Repository(`Wick-Lim/loom`, `release.yml`)를 연동하면 됩니다.

---

## 2. 릴리즈 실행 절차

1. **버전 동기화 확인**
   - [`Cargo.toml`](./Cargo.toml)의 `[workspace.package] version` 및 `[workspace.dependencies]` 내 `loom-core`/`loom-vfs`/`loom-daemon` `version`이 목표 버전(예: `0.1.0`)과 일치하는지 확인합니다.
   - 필요 시 `npm/*/package.json` 버전 동기화:
     ```bash
     node scripts/generate-manifests.mjs --version 0.1.0 --sync-npm-only
     ```
2. **릴리즈 태그 푸시**
   ```bash
   git tag v0.1.0
   git push origin main --tags
   ```
3. **자동 실행 내역 (`.github/workflows/release.yml`)**
   - **6개 타겟 크로스 빌드:** `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`
   - **GitHub Release 생성:** 아카이브 6종 + `SHA256SUMS.txt` + 실제 해시가 반영된 `loom-vcs.rb`, `loom-vcs.scoop.json` 자동 첨부
   - **npm 배포:** 플랫폼 패키지 6종 선배포 후 루트 패키지 `loom-vcs` 배포 (`--provenance` 포함)
   - **Homebrew / Scoop:** `Wick-Lim/homebrew-tap` 및 `Wick-Lim/scoop-bucket` 자동 커밋·푸시
   - **crates.io:** 의존성 순서(`loom-vcs-core` → `loom-vcs-vfs` → `loom-vcs-daemon` → `loom-vcs`)로 순차 배포
4. **(선택) 저장소 내 매니페스트 동기화 & Winget PR 제출**
   - 릴리즈 완료 후 GitHub Release에서 생성된 `SHA256SUMS.txt`를 내려받아 저장소 내 매니페스트를 실제 체크섬으로 업데이트할 수 있습니다:
     ```bash
     node scripts/generate-manifests.mjs --version 0.1.0 --checksums path/to/SHA256SUMS.txt
     ```
   - `packaging/winget/manifests/w/WickLim/LoomVcs/<VER>/` 파일들을 `microsoft/winget-pkgs`에 PR로 제출합니다(`wingetcreate submit`).

---

## 3. GitHub 저장소 이름을 `Wick-Lim/loom` → `Wick-Lim/loom-vcs`로 변경할 경우

현재 저장소 URL은 `https://github.com/Wick-Lim/loom`입니다. 만약 향후 GitHub 저장소 이름을 `Wick-Lim/loom-vcs`로 변경한다면 아래 위치를 함께 업데이트하세요.

- [`Cargo.toml`](./Cargo.toml): `repository`, `homepage`, `documentation`
- [`npm/loom-vcs/package.json`](./npm/loom-vcs/package.json) 및 6개 플랫폼 패키지의 `repository` / `homepage` / `bugs`
- [`npm/loom-vcs/bin/loom.cjs`](./npm/loom-vcs/bin/loom.cjs) 및 [`npm/loom-vcs/test/launcher.test.mjs`](./npm/loom-vcs/test/launcher.test.mjs)
- [`scripts/generate-manifests.mjs`](./scripts/generate-manifests.mjs) 기본 `--repo` 값 (`DEFAULT` 및 생성된 매니페스트)
- [`scripts/install.sh`](./scripts/install.sh) 및 [`scripts/install.ps1`](./scripts/install.ps1) 기본 `LOOM_REPO` 값
- [`README.md`](./README.md) 설치 URL
