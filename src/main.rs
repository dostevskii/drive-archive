//! drive-archive — 외장하드에 흩어진 자료를 인덱싱하고 검색하는 도구.
//!
//! 하드가 연결될 때만 잠깐 실행되고 끝나면 완전히 종료된다.
//! 상주 프로세스가 없으므로 평소 리소스 사용량은 0이다.

mod auth;
mod db;
mod elevation;
mod install;
mod mcp;
mod scan;
mod serve;
mod sync;
mod task;
mod volume;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "drive-archive",
    version,
    about = "외장하드 자료를 인덱싱하고, 하드를 연결하지 않아도 검색할 수 있게 해줍니다",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 연결된 외장하드를 확인해 인덱스를 갱신합니다 (작업 스케줄러가 자동 호출)
    Sync {
        /// 방금 스캔한 하드도 다시 훑습니다
        #[arg(long)]
        force: bool,
        /// 이 드라이브만 확인합니다 (예: L:). 생략하면 연결된 하드를 모두 확인합니다
        #[arg(long)]
        drive: Option<String>,
    },

    /// 하드 하나를 수동으로 전체 재스캔합니다
    Scan {
        /// 드라이브 문자 (예: E). 생략하면 연결된 하드를 모두 스캔합니다
        drive: Option<String>,
    },

    /// 파일과 폴더를 이름으로 검색해, 어느 하드에 있는지 보여줍니다
    Search {
        /// 찾을 이름의 일부
        keyword: String,
        /// 특정 하드 안에서만 검색 (라벨 또는 볼륨 시리얼)
        #[arg(long)]
        drive: Option<String>,
        /// 폴더만 검색 (프로젝트 단위로 찾을 때)
        #[arg(long)]
        dirs_only: bool,
        /// 결과 개수 제한
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// JSON으로 출력
        #[arg(long)]
        json: bool,
    },

    /// 인덱스에 등록된 하드 목록과 현재 연결 여부를 보여줍니다
    Drives {
        #[arg(long)]
        json: bool,
    },

    /// 인덱스 통계를 보여줍니다
    Status {
        #[arg(long)]
        json: bool,
    },

    /// 더 이상 쓰지 않는 하드를 인덱스에서 제거합니다
    Forget {
        /// 하드 라벨 또는 볼륨 시리얼
        name: String,
    },

    /// 자동 인덱싱과 Claude 연결을 한 번에 설정합니다 (처음 쓸 때 한 번)
    Install,

    /// 자동 인덱싱과 Claude 연결을 해제합니다 (인덱스는 남습니다)
    Uninstall,

    /// MCP 서버를 실행합니다 (Claude가 자동으로 호출합니다)
    Mcp,

    /// 브라우저로 인덱스를 볼 수 있게 로컬 웹 화면을 띄웁니다
    Serve {
        /// 열 포트
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// 브라우저를 자동으로 열지 않습니다
        #[arg(long)]
        no_open: bool,
    },

    /// 외장하드 연결 시 자동 인덱싱하도록 작업 스케줄러에 등록합니다
    SetupTask,

    /// 작업 스케줄러 등록을 해제합니다
    RemoveTask,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("오류: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Cmd::Sync { force, drive } => cmd_sync(force, drive.as_deref()),
        Cmd::Scan { drive } => cmd_scan(drive),
        Cmd::Search { keyword, drive, dirs_only, limit, json } => {
            cmd_search(&keyword, drive.as_deref(), dirs_only, limit, json)
        }
        Cmd::Drives { json } => cmd_drives(json),
        Cmd::Status { json } => cmd_status(json),
        Cmd::Forget { name } => cmd_forget(&name),
        Cmd::Install => install::install(),
        Cmd::Uninstall => install::uninstall(),
        Cmd::Mcp => mcp::serve(),
        Cmd::Serve { port, no_open } => serve::serve(port, !no_open),
        Cmd::SetupTask => cmd_setup_task(),
        Cmd::RemoveTask => cmd_remove_task(),
    }
}

// ---------------------------------------------------------------- 명령 구현

/// 작업 스케줄러가 넘겨준 드라이브 값을 문자 하나로 읽는다.
///
/// 마운트 이벤트에는 `L:` 형태로 담겨 있다. 값 쿼리가 치환되지 않은 채 `$(DriveName)`처럼
/// 그대로 넘어오거나 빈 문자열이 오는 경우가 있으므로, 알파벳 한 글자로 읽히지 않으면
/// 지정이 없는 것으로 보고 연결된 하드를 전부 확인한다.
fn parse_drive_letter(raw: &str) -> Option<char> {
    let mut chars = raw.trim().chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    match chars.next() {
        None | Some(':') => Some(first),
        Some(_) => None,
    }
}

fn cmd_sync(force: bool, drive: Option<&str>) -> Result<()> {
    // 스케줄러가 깨웠다면 이 시점에 콘솔 창이 화면에 떠 있다. 먼저 치운다.
    sync::hide_console_if_ours();

    let Some(_lock) = sync::InstanceLock::acquire()? else {
        // 이미 다른 인스턴스가 스캔 중이다. 조용히 물러난다.
        sync::log("이미 실행 중이므로 종료합니다");
        return Ok(());
    };
    sync::lower_priority();

    let only = drive.and_then(parse_drive_letter);
    let outcomes = sync::sync_all(force, only)?;
    if outcomes.is_empty() {
        match only {
            Some(letter) => println!("{letter}: 드라이브는 인덱싱 대상이 아닙니다."),
            None => println!("연결된 외장하드가 없습니다."),
        }
        return Ok(());
    }

    for o in &outcomes {
        match o {
            sync::SyncOutcome::Skipped { label, letter } => {
                println!("{label} ({letter}:)  방금 스캔했으므로 건너뜀");
            }
            sync::SyncOutcome::Updated { label, letter, stats } if stats.has_changes() => {
                println!(
                    "{label} ({letter}:)  추가 {} / 변경 {} / 삭제 {}",
                    stats.added, stats.updated, stats.removed
                );
            }
            sync::SyncOutcome::Updated { label, letter, stats } => {
                println!("{label} ({letter}:)  변경 없음 ({}개 항목)", stats.unchanged);
            }
            sync::SyncOutcome::Failed { label, letter, reason } => {
                println!("{label} ({letter}:)  반영하지 않음 - {reason}");
            }
        }
    }
    Ok(())
}

fn cmd_scan(drive: Option<String>) -> Result<()> {
    sync::lower_priority();

    let volumes = match drive {
        Some(d) => {
            let letter = d
                .chars()
                .next()
                .filter(|c| c.is_ascii_alphabetic())
                .context("드라이브 문자를 알파벳 한 글자로 지정하세요 (예: E)")?;
            vec![volume::volume_at(letter)?]
        }
        None => volume::list_external_volumes(),
    };

    if volumes.is_empty() {
        println!("연결된 외장하드가 없습니다.");
        return Ok(());
    }

    let mut conn = db::open()?;
    for vol in &volumes {
        println!("{} ({}:) 스캔 중...", vol.label, vol.letter);
        let stats = sync::sync_volume(&mut conn, vol)?;
        println!(
            "  추가 {} / 변경 {} / 삭제 {} / 그대로 {}",
            stats.added, stats.updated, stats.removed, stats.unchanged
        );
    }
    Ok(())
}

/// 검색 결과에 나온 하드를 "꺼내 와야 하는 것"과 "지금 연결된 것"으로 나눈다.
///
/// 라벨은 겹칠 수 있으므로(하드를 포맷하면 같은 이름의 새 볼륨이 생긴다) 볼륨 시리얼로 판별한다.
/// 반환값은 (연결 필요, 이미 연결됨) 순이고, 연결된 쪽에는 지금 붙어 있는 드라이브 문자를 덧붙인다.
fn split_by_connection(
    hits: &[db::SearchHit],
    live: &[volume::Volume],
) -> (Vec<String>, Vec<String>) {
    let mut drives: Vec<(&str, &str)> =
        hits.iter().map(|h| (h.drive_label.as_str(), h.drive_serial.as_str())).collect();
    drives.sort_unstable();
    drives.dedup();

    let mut needed = Vec::new();
    let mut ready = Vec::new();
    for (label, serial) in drives {
        match live.iter().find(|v| v.serial == serial) {
            Some(v) => ready.push(format!("{label} ({}:)", v.letter)),
            None => needed.push(label.to_string()),
        }
    }
    (needed, ready)
}

fn cmd_search(
    keyword: &str,
    drive: Option<&str>,
    dirs_only: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let conn = db::open()?;
    let hits = db::search(&conn, keyword, drive, dirs_only, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("'{keyword}'에 해당하는 자료를 찾지 못했습니다.");
        let stats = db::stats(&conn)?;
        if stats.entry_count == 0 {
            println!(
                "\n아직 인덱싱된 하드가 없습니다. 외장하드를 연결한 뒤 `drive-archive sync`를 실행하세요."
            );
        }
        return Ok(());
    }

    let label_width = hits.iter().map(|h| display_width(&h.drive_label)).max().unwrap_or(0);
    for h in &hits {
        let kind = if h.is_dir { "폴더".to_string() } else { format_bytes(h.size) };
        let date = h.modified.as_deref().unwrap_or("-");
        let pad = " ".repeat(label_width.saturating_sub(display_width(&h.drive_label)));
        let trailing = if h.is_dir { "\\" } else { "" };
        println!("  [{}]{pad}  {}{trailing}   ({kind}, {date})", h.drive_label, h.path);
    }

    // 어느 하드를 꺼내야 하는지가 이 프로그램의 존재 이유다. 마지막에 다시 짚어 준다.
    // 이미 꽂혀 있는 하드까지 "연결하세요"라고 하면 서랍을 헛되이 뒤지게 만든다.
    let (needed, ready) = split_by_connection(&hits, &volume::list_external_volumes());
    println!();
    if !needed.is_empty() {
        println!("→ {} 하드를 연결하세요.", needed.join(", "));
    }
    if !ready.is_empty() {
        println!("→ 지금 연결되어 있는 하드: {}", ready.join(", "));
    }

    if hits.len() == limit {
        println!("  (결과가 {limit}개에서 잘렸습니다. --limit 으로 늘릴 수 있습니다.)");
    }
    Ok(())
}

fn cmd_drives(json: bool) -> Result<()> {
    let conn = db::open()?;
    let drives = db::list_drives(&conn)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&drives)?);
        return Ok(());
    }

    if drives.is_empty() {
        println!(
            "아직 인덱싱된 하드가 없습니다. 외장하드를 연결한 뒤 `drive-archive sync`를 실행하세요."
        );
        return Ok(());
    }

    for d in &drives {
        let state = match d.letter {
            Some(l) => format!("연결됨 ({l}:)"),
            None => "연결 안 됨".to_string(),
        };
        println!("{}  [{}]", d.label, state);
        println!(
            "  항목 {}개 · {} · 용량 {} 중 {} 남음",
            d.entry_count,
            d.filesystem,
            format_bytes(d.total_bytes),
            format_bytes(d.free_bytes)
        );
        println!(
            "  마지막 연결 {} · 마지막 스캔 {}",
            d.last_seen,
            d.last_scan_at.as_deref().unwrap_or("없음")
        );
        println!("  볼륨 시리얼 {}", d.serial);
        println!();
    }
    Ok(())
}

fn cmd_status(json: bool) -> Result<()> {
    let conn = db::open()?;
    let s = db::stats(&conn)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }

    println!("등록된 하드   {}개", s.drive_count);
    println!("인덱싱된 항목 {}개 (파일 {} · 폴더 {})", s.entry_count, s.file_count, s.dir_count);
    println!("인덱스 크기   {}", format_bytes(s.db_bytes));
    println!("인덱스 위치   {}", s.db_path);
    println!(
        "자동 인덱싱   {}",
        if task::exists() { "켜짐" } else { "꺼짐 (`drive-archive setup-task`로 켤 수 있습니다)" }
    );
    Ok(())
}

fn cmd_forget(name: &str) -> Result<()> {
    let conn = db::open()?;
    match db::forget(&conn, name)? {
        Some((label, removed)) => {
            println!("'{label}' 하드를 인덱스에서 제거했습니다 (항목 {removed}개).");
        }
        None => {
            println!("'{name}'에 해당하는 하드가 인덱스에 없습니다.");
            println!("`drive-archive drives`로 등록된 하드를 확인하세요.");
        }
    }
    Ok(())
}

fn cmd_setup_task() -> Result<()> {
    let exe = task::current_exe()?;
    task::setup(&exe)?;
    println!("자동 인덱싱을 켰습니다.");
    println!("  실행 파일: {}", exe.display());
    println!("  이제 외장하드를 연결하면 인덱스가 자동으로 갱신됩니다.");

    // 지금 꽂혀 있는 하드는 마운트 이벤트가 이미 지나갔으므로 한 번 깨워 준다.
    match task::run_now() {
        Ok(()) => println!("  지금 연결되어 있는 하드도 백그라운드에서 인덱싱을 시작했습니다."),
        Err(e) => eprintln!("  다만 지금 연결된 하드의 인덱싱을 시작하지 못했습니다: {e:#}"),
    }

    println!("\n주의: 실행 파일을 다른 폴더로 옮기면 `setup-task`를 다시 실행해야 합니다.");
    Ok(())
}

fn cmd_remove_task() -> Result<()> {
    if task::remove()? {
        println!("자동 인덱싱을 껐습니다. 인덱스 자체는 그대로 남아 있습니다.");
    } else {
        println!("자동 인덱싱이 등록되어 있지 않습니다.");
    }
    Ok(())
}

// ---------------------------------------------------------------- 출력 보조

/// 바이트 수를 사람이 읽는 단위로 바꾼다.
fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// 터미널에서 차지하는 칸 수. 한글은 두 칸을 쓴다.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) > 0x1100 { 2 } else { 1 }).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 바이트는_단위와_함께_표시된다() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn 한글은_두_칸으로_계산한다() {
        assert_eq!(display_width("ABC"), 3);
        assert_eq!(display_width("가나"), 4);
        assert_eq!(display_width("A가"), 3);
    }

    #[test]
    fn cli_인자가_파싱된다() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn 스케줄러가_넘긴_드라이브_값을_문자로_읽는다() {
        assert_eq!(parse_drive_letter("L:"), Some('L'));
        assert_eq!(parse_drive_letter("L"), Some('L'));
        assert_eq!(parse_drive_letter(" J: "), Some('J'));
    }

    #[test]
    fn 치환되지_않은_값은_지정이_없는_것으로_본다() {
        // 로그온 트리거나 schtasks /Run으로 실행되면 값 쿼리가 치환되지 않는다.
        // 그때는 어느 볼륨인지 알 수 없으므로 연결된 하드를 전부 확인해야 한다.
        assert_eq!(parse_drive_letter("$(DriveName)"), None);
        assert_eq!(parse_drive_letter(""), None);
        assert_eq!(parse_drive_letter("   "), None);
        assert_eq!(parse_drive_letter("12"), None);
    }

    fn hit(label: &str, serial: &str) -> db::SearchHit {
        db::SearchHit {
            drive_label: label.into(),
            drive_serial: serial.into(),
            path: "어딘가\\파일.psd".into(),
            name: "파일.psd".into(),
            is_dir: false,
            size: 0,
            modified: None,
        }
    }

    fn vol(letter: char, serial: &str, label: &str) -> volume::Volume {
        volume::Volume {
            letter,
            serial: serial.into(),
            label: label.into(),
            filesystem: "NTFS".into(),
            total_bytes: 0,
            free_bytes: 0,
        }
    }

    #[test]
    fn 연결된_하드는_연결하라고_하지_않는다() {
        let hits = [hit("Works D", "90FA8BC5")];
        let live = [vol('L', "90FA8BC5", "Works D")];

        let (needed, ready) = split_by_connection(&hits, &live);

        assert!(needed.is_empty());
        assert_eq!(ready, ["Works D (L:)"]);
    }

    #[test]
    fn 연결되지_않은_하드는_꺼내오라고_한다() {
        let hits = [hit("Works E", "0480390F")];

        let (needed, ready) = split_by_connection(&hits, &[]);

        assert_eq!(needed, ["Works E"]);
        assert!(ready.is_empty());
    }

    #[test]
    fn 라벨이_같아도_시리얼이_다르면_다른_하드로_본다() {
        // 하드를 포맷하면 라벨은 그대로여도 볼륨 시리얼이 바뀐다.
        // 인덱스에 남은 옛 항목을 새 볼륨과 같은 하드로 착각하면 안 된다.
        let hits = [hit("Works F", "6C965A22")];
        let live = [vol('I', "C482BDB6", "Works F")];

        let (needed, ready) = split_by_connection(&hits, &live);

        assert_eq!(needed, ["Works F"]);
        assert!(ready.is_empty());
    }

    #[test]
    fn 같은_하드가_여러_번_나와도_한_번만_알린다() {
        let hits = [
            hit("Works E", "0480390F"),
            hit("Works E", "0480390F"),
            hit("Works D", "90FA8BC5"),
        ];
        let live = [vol('L', "90FA8BC5", "Works D")];

        let (needed, ready) = split_by_connection(&hits, &live);

        assert_eq!(needed, ["Works E"]);
        assert_eq!(ready, ["Works D (L:)"]);
    }
}
