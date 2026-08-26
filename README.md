# drive-archive

> 외장하드에 흩어진 자료를 인덱싱하고, **하드를 연결하지 않아도** 어느 하드에 무엇이 있는지 검색할 수 있게 해주는 도구

여러 개의 외장하드에 프로젝트 자료를 나눠 보관하다 보면 "그 파일이 어느 하드에 있었지?"라는 문제가 생깁니다. 하드를 하나씩 꽂아 보는 것 말고는 방법이 없습니다.

drive-archive는 외장하드가 **연결될 때마다 자동으로** 그 안의 파일·폴더 목록을 로컬 데이터베이스에 기록합니다. 그 다음부터는 하드가 책상 서랍에 있어도, 컴퓨터에서 검색만 하면 어느 하드에 있는지 바로 알 수 있습니다.

```
$ drive-archive search "2024 브랜딩"

  [PROJECT-A]  작업/2024/브랜딩_리뉴얼/          (폴더, 2024-11-03)
  [PROJECT-A]  작업/2024/브랜딩_리뉴얼/최종.psd   (1.2 GB, 2024-11-03)
  [BACKUP-02]  보관/2024브랜딩_백업.zip          (3.4 GB, 2024-12-20)

  → PROJECT-A 하드를 연결하세요.
```

---

## 특징

**평소에는 리소스를 전혀 쓰지 않습니다.**
상주 프로그램이 아닙니다. Windows 작업 스케줄러가 "외장하드가 연결됨" 이벤트를 감지했을 때만 프로그램이 잠깐 실행되고, 끝나면 완전히 종료됩니다. 백그라운드에 떠 있는 프로세스가 없으므로 평소 메모리·CPU 사용량은 0입니다.

**변경사항이 자동으로 반영됩니다.**
하드를 다시 연결하면 이전 인덱스와 비교해서 새로 생긴 파일, 수정된 파일, 삭제된 파일을 자동으로 갱신합니다.

**Claude와 연결됩니다.**
MCP(Model Context Protocol) 서버를 내장하고 있어, Claude Desktop이나 Claude Code에서 자연어로 물어보면 됩니다.

> "외장하드에서 작년 브랜딩 프로젝트 파일 어디 있어?"
> → Claude가 인덱스를 검색해서 "PROJECT-A 하드에 있습니다"라고 답합니다.

**파일 내용은 저장하지 않습니다.**
이름, 경로, 크기, 수정 날짜만 기록합니다. 데이터베이스는 가볍게 유지되고, 파일 내용이 어딘가로 복사되는 일은 없습니다.

---

## 요구 환경

| 항목 | 요구사항 |
|---|---|
| 운영체제 | **Windows 11 전용** |
| 파일 시스템 | NTFS로 포맷된 외장하드 |
| 연결 방식 | USB |

이 프로그램은 Windows 11에서 개발되었고 Windows 11에서만 동작합니다. Windows 10 이하나 macOS/Linux는 지원하지 않습니다.

---

## 설치

### 1. 다운로드

[Releases 페이지](https://github.com/dostevskii/drive-archive/releases)에서 `drive-archive.exe`를 받아 원하는 폴더에 둡니다.
(예: `C:\Tools\drive-archive\drive-archive.exe`)

### 2. 설치 명령 실행

해당 폴더에서 PowerShell을 열고:

```powershell
.\drive-archive.exe install
```

이 한 줄이 다음을 모두 처리합니다.

- 외장하드 연결 시 자동 인덱싱하도록 작업 스케줄러에 등록
- Claude Desktop과 Claude Code에 MCP 서버로 등록

### 3. 하드 연결

외장하드를 연결하면 자동으로 인덱싱이 시작됩니다. 진행 상황은 다음으로 확인합니다.

```powershell
.\drive-archive.exe drives
```

> 첫 인덱싱은 파일 수에 따라 수 분이 걸릴 수 있습니다. 백그라운드에서 낮은 우선순위로 동작하므로 다른 작업에는 지장이 없습니다.

### 제거

```powershell
.\drive-archive.exe uninstall
```

---

## 사용법

### 검색

```powershell
drive-archive search <키워드>
```

| 옵션 | 설명 |
|---|---|
| `--drive <라벨>` | 특정 하드 안에서만 검색 |
| `--dirs-only` | 폴더(프로젝트 단위)만 검색 |
| `--limit <숫자>` | 결과 개수 제한 (기본 50) |
| `--json` | JSON 출력 (스크립트·MCP용) |

```powershell
# 이름에 "포트폴리오"가 들어간 폴더만 찾기
drive-archive search 포트폴리오 --dirs-only

# BACKUP-02 하드 안에서만 찾기
drive-archive search .psd --drive BACKUP-02
```

### 하드 목록

```powershell
drive-archive drives
```

등록된 모든 하드의 라벨, 용량, 마지막 연결 시각, **지금 연결되어 있는지 여부**를 보여줍니다.

### 그 외 명령

| 명령 | 설명 |
|---|---|
| `drive-archive sync` | 지금 연결된 하드를 모두 확인해 인덱스 갱신 (스케줄러가 자동 호출) |
| `drive-archive scan [드라이브문자]` | 특정 하드를 수동으로 전체 재스캔 |
| `drive-archive status` | 인덱스 통계 (하드 수, 항목 수, DB 크기) |
| `drive-archive forget <라벨>` | 더 이상 쓰지 않는 하드를 인덱스에서 제거 |
| `drive-archive mcp` | MCP 서버 실행 (Claude가 자동으로 호출) |
| `drive-archive setup-task` / `remove-task` | 작업 스케줄러만 따로 등록/해제 |

---

## 동작 방식

```
외장하드 연결
      │
      ▼
Windows가 볼륨 마운트 이벤트 기록
      │
      ▼
작업 스케줄러가 이벤트를 감지 ──▶ drive-archive.exe sync 실행
                                        │
                                        ▼
                              USB + NTFS 볼륨만 골라냄
                              (볼륨 시리얼로 하드 식별)
                                        │
                                        ▼
                              전체 파일·폴더 병렬 스캔
                                        │
                                        ▼
                              기존 인덱스와 비교
                              신규 / 변경 / 삭제 반영
                                        │
                                        ▼
                                   프로세스 종료
                                (다시 리소스 사용 0)
```

하드를 **분리**할 때는 아무것도 하지 않습니다. 인덱스는 이미 저장되어 있고, `drives` 명령이 조회 시점에 실제 연결 여부를 직접 확인하기 때문입니다.

### 하드 식별

드라이브 문자(`E:`, `F:` 등)는 연결 순서에 따라 바뀌므로 식별자로 쓸 수 없습니다. drive-archive는 **NTFS 볼륨 시리얼 번호**로 하드를 식별하고, **볼륨 라벨**을 사람이 읽는 이름으로 표시합니다.

> 외장하드의 볼륨 라벨을 물리적으로 붙여 놓은 라벨과 똑같이 맞춰 두면, 검색 결과에 나온 이름 그대로 서랍에서 찾을 수 있습니다.

### 저장 위치

| 대상 | 경로 |
|---|---|
| 인덱스 DB | `%LOCALAPPDATA%\drive-archive\index.db` |
| 로그 | `%LOCALAPPDATA%\drive-archive\sync.log` |

---

## 직접 빌드하기

```powershell
git clone https://github.com/dostevskii/drive-archive.git
cd drive-archive
cargo build --release
# 결과물: target\release\drive-archive.exe
```

Rust 1.85 이상과 MSVC 빌드 도구가 필요합니다.

---

## 라이선스

이 프로그램은 [GNU General Public License v3.0](LICENSE)에 따라 배포됩니다.

자유롭게 사용·수정·재배포할 수 있으나, 수정판을 배포할 경우 동일한 GPL 3.0 라이선스로 소스 코드를 공개해야 합니다.

```
Copyright (C) 2026 dostevskii

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, version 3.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.
```
