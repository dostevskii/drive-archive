//! drive-archive — 외장하드에 흩어진 자료를 인덱싱하고 검색하는 도구.
//!
//! 하드가 연결될 때만 잠깐 실행되고 끝나면 완전히 종료된다.
//! 상주 프로세스가 없으므로 평소 리소스 사용량은 0이다.

mod auth;
mod db;
mod elevation;
mod envpath;
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
    about = "Indexes files on external hard drives so you can find them without connecting the drive",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Checks connected external drives and updates the index (called automatically by Task Scheduler)
    Sync {
        /// Rescan drives even if just scanned
        #[arg(long)]
        force: bool,
        /// Check only this drive (e.g. L:). If omitted, checks all connected drives
        #[arg(long)]
        drive: Option<String>,
    },

    /// Manually does a full rescan of one drive
    Scan {
        /// Drive letter (e.g. E). If omitted, scans all connected drives
        drive: Option<String>,
    },

    /// Searches files and folders by name and shows which drive they're on
    Search {
        /// Part of the name to search for
        keyword: String,
        /// Search within one drive only (label or volume serial)
        #[arg(long)]
        drive: Option<String>,
        /// Search folders only (for finding a project as a whole)
        #[arg(long)]
        dirs_only: bool,
        /// Limit the number of results
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Shows the drives registered in the index and whether each is currently connected
    Drives {
        #[arg(long)]
        json: bool,
    },

    /// Shows index statistics
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Removes a drive you no longer use from the index
    Forget {
        /// Drive label or volume serial
        name: String,
    },

    /// Sets up automatic indexing and the Claude connection in one step (run once, the first time)
    Install,

    /// Removes automatic indexing and the Claude connection (the index is kept)
    Uninstall,

    /// Runs the MCP server (Claude calls this automatically)
    Mcp,

    /// Starts a local web page so you can browse the index in a browser
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Don't open the browser automatically
        #[arg(long)]
        no_open: bool,
    },

    /// Sets or changes the web page password
    Passwd,

    /// Registers automatic indexing and the web page task in Task Scheduler
    SetupTask,

    /// Removes the Task Scheduler registration (automatic indexing, web page)
    RemoveTask,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
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
        Cmd::Passwd => cmd_passwd(),
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

/// 스케줄러가 넘긴 `--drive` 값의 세 갈래.
enum DriveArg {
    /// NTFS 마운트 이벤트가 준 드라이브 문자. 그 하드만 확인한다.
    Letter(char),
    /// 어느 볼륨인지 모르거나(로그온·수동 실행) 볼륨이 붙었다(장치 이벤트).
    /// 연결된 하드를 전부 확인한다.
    CheckAll,
    /// 저장소가 아닌 장치 이벤트(마우스·키보드 등). 디스크를 건드리지 않고 물러난다.
    NotStorage,
}

/// 작업 스케줄러가 넘겨준 `--drive` 값을 분류한다.
///
/// NTFS 마운트 이벤트는 `L:` 같은 드라이브 문자를 준다. 로그온 트리거나
/// `schtasks /Run`은 값 쿼리가 없어 `$(DriveName)`이 그대로, 또는 빈 값이 온다.
/// 장치 구성 이벤트(Kernel-PnP 400)는 장치 식별자를 주는데, exFAT처럼 Ntfs
/// 이벤트 98이 나지 않는 하드를 잡으려고 듣는 것이라 `STORAGE\VOLUME`이면
/// 전부 확인하고, 그 밖의 장치면 물러난다 — 마우스를 꽂았다고 하드를 다시
/// 훑으면 안 된다.
fn classify_drive_arg(raw: &str) -> DriveArg {
    if let Some(letter) = parse_drive_letter(raw) {
        return DriveArg::Letter(letter);
    }
    let t = raw.trim();
    if t.is_empty() || t == "$(DriveName)" {
        return DriveArg::CheckAll;
    }
    if t.get(..14).is_some_and(|p| p.eq_ignore_ascii_case(r"STORAGE\VOLUME")) {
        return DriveArg::CheckAll;
    }
    DriveArg::NotStorage
}

fn cmd_sync(force: bool, drive: Option<&str>) -> Result<()> {
    // 스케줄러가 깨웠다면 이 시점에 콘솔 창이 화면에 떠 있다. 먼저 치운다.
    sync::hide_console_if_ours();

    // 저장소가 아닌 장치 이벤트면 아무것도 하지 않는다. 잠금 파일도 로그도
    // 건드리지 않는다 — 장치를 꽂을 때마다 흔적이 쌓이면 그게 소음이다.
    let arg = match drive {
        Some(raw) => classify_drive_arg(raw),
        None => DriveArg::CheckAll,
    };
    if matches!(arg, DriveArg::NotStorage) {
        return Ok(());
    }

    let Some(_lock) = sync::InstanceLock::acquire()? else {
        // 이미 다른 인스턴스가 스캔 중이다. 조용히 물러난다.
        sync::log("이미 실행 중이므로 종료합니다");
        return Ok(());
    };
    sync::lower_priority();

    let only = match arg {
        DriveArg::Letter(letter) => Some(letter),
        _ => None,
    };
    let outcomes = sync::sync_all(force, only)?;
    if outcomes.is_empty() {
        match only {
            Some(letter) => println!("Drive {letter}: is not indexed."),
            None => println!("No external drives connected."),
        }
        return Ok(());
    }

    for o in &outcomes {
        match o {
            sync::SyncOutcome::Skipped { label, letter } => {
                println!("{label} ({letter}:)  skipped, scanned moments ago");
            }
            sync::SyncOutcome::Updated { label, letter, stats } if stats.has_changes() => {
                println!(
                    "{label} ({letter}:)  added {} / changed {} / removed {}",
                    stats.added, stats.updated, stats.removed
                );
            }
            sync::SyncOutcome::Updated { label, letter, stats } => {
                println!("{label} ({letter}:)  no changes ({} items)", stats.unchanged);
            }
            sync::SyncOutcome::Failed { label, letter, reason } => {
                println!("{label} ({letter}:)  not applied - {reason}");
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
                .context("Specify a single alphabetic drive letter (e.g. E)")?;
            vec![volume::volume_at(letter)?]
        }
        None => volume::list_external_volumes(),
    };

    if volumes.is_empty() {
        println!("No external drives connected.");
        return Ok(());
    }

    let mut conn = db::open()?;
    for vol in &volumes {
        println!("{} ({}:) scanning...", vol.label, vol.letter);
        let stats = sync::sync_volume(&mut conn, vol)?;
        println!(
            "  added {} / changed {} / removed {} / unchanged {}",
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
        println!("No matches found for '{keyword}'.");
        let stats = db::stats(&conn)?;
        if stats.entry_count == 0 {
            println!(
                "\nNo drives have been indexed yet. Connect an external drive, then run `drive-archive sync`."
            );
        }
        return Ok(());
    }

    let label_width = hits.iter().map(|h| display_width(&h.drive_label)).max().unwrap_or(0);
    for h in &hits {
        let kind = if h.is_dir { "folder".to_string() } else { format_bytes(h.size) };
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
        println!("→ Connect these drives: {}.", needed.join(", "));
    }
    if !ready.is_empty() {
        println!("→ Already connected: {}", ready.join(", "));
    }

    if hits.len() == limit {
        println!("  (Results truncated at {limit}. Use --limit to see more.)");
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
            "No drives have been indexed yet. Connect an external drive, then run `drive-archive sync`."
        );
        return Ok(());
    }

    for d in &drives {
        let state = match d.letter {
            Some(l) => format!("connected ({l}:)"),
            None => "not connected".to_string(),
        };
        println!("{}  [{}]", d.label, state);
        println!(
            "  {} items · {} · {} total, {} free",
            d.entry_count,
            d.filesystem,
            format_bytes(d.total_bytes),
            format_bytes(d.free_bytes)
        );
        println!(
            "  Last connected {} · Last scanned {}",
            d.last_seen,
            d.last_scan_at.as_deref().unwrap_or("never")
        );
        println!("  Volume serial {}", d.serial);
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

    println!("Registered drives : {}", s.drive_count);
    println!("Indexed items     : {} (files {} · folders {})", s.entry_count, s.file_count, s.dir_count);
    println!("Index size        : {}", format_bytes(s.db_bytes));
    println!("Index location    : {}", s.db_path);
    println!(
        "Auto indexing     : {}",
        if task::exists() { "on" } else { "off (run `drive-archive setup-task` to enable)" }
    );
    Ok(())
}

fn cmd_forget(name: &str) -> Result<()> {
    let conn = db::open()?;
    match db::forget(&conn, name)? {
        Some((label, removed)) => {
            println!("Removed drive '{label}' from the index ({removed} items).");
        }
        None => {
            println!("No drive matching '{name}' found in the index.");
            println!("Run `drive-archive drives` to see registered drives.");
        }
    }
    Ok(())
}

fn cmd_setup_task() -> Result<()> {
    let exe = task::current_exe()?;
    task::setup(&exe)?;
    println!("Registered automatic indexing and the web page task.");
    println!("  Executable: {}", exe.display());
    println!("  The index will now update automatically when you connect an external drive.");

    // 지금 꽂혀 있는 하드는 마운트 이벤트가 이미 지나갔으므로 한 번 깨워 준다.
    match task::run_now() {
        Ok(()) => println!("  Also started indexing currently connected drives in the background."),
        Err(e) => eprintln!("  However, indexing of currently connected drives could not be started: {e:#}"),
    }

    if !auth::is_configured() {
        eprintln!("\nNote: no web page password is set, so the web page task will exit as soon as it starts.");
        eprintln!("Set a password first with `drive-archive passwd`.");
    }

    println!("\nNote: if you move the executable to a different folder, run `setup-task` again.");
    Ok(())
}

fn cmd_remove_task() -> Result<()> {
    if task::remove()? {
        println!("Removed the automatic indexing and web page task registration. The index itself is kept.");
    } else {
        println!("No automatic indexing or web page task is registered.");
    }
    Ok(())
}

/// 새 비밀번호로 쓸 수 있는지 본다. 통과하면 그 값을 돌려준다.
///
/// 앞뒤 공백을 지우지 않는 것은 사용자가 일부러 넣었을 수 있어서다. 다만 공백만
/// 있는 것은 실수로 본다.
fn check_new_password(first: &str, second: &str) -> Result<String> {
    if first != second {
        anyhow::bail!("The two entries do not match.");
    }
    if first.trim().is_empty() {
        anyhow::bail!("Password is empty.");
    }
    if first.chars().count() < 8 {
        anyhow::bail!("Password must be at least 8 characters. This screen is reachable from outside.");
    }
    Ok(first.to_string())
}

/// 화면에 찍히지 않게 한 줄을 받는다.
///
/// 별표도 찍지 않는다. 터미널에서는 그것이 관례이고, 길이가 어깨너머로 새지 않는다.
/// 파이프로 넘어온 입력에는 콘솔 모드가 없으므로 그때는 그냥 읽는다.
fn read_hidden(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
        STD_INPUT_HANDLE,
    };

    print!("{prompt}");
    std::io::stdout().flush().ok();

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("Could not open standard input")?;
    let mut mode = CONSOLE_MODE::default();
    let is_console = unsafe { GetConsoleMode(handle, &mut mode) }.is_ok();
    if is_console {
        let _ = unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) };
    }

    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).context("Could not read input")?;

    if is_console {
        let _ = unsafe { SetConsoleMode(handle, mode) };
        println!();   // 사용자가 누른 Enter가 화면에 남지 않았으므로 줄을 바꿔 준다
    }

    if n == 0 {
        // 파이프가 닫혔거나 입력이 끝났다. 그냥 빈 문자열을 돌려주면 되묻는
        // 쪽이 영원히 돈다 — 여기서 끊어야 비대화형 호출이 멈추지 않는다.
        anyhow::bail!("Input is closed. Run this from an interactive terminal.");
    }

    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// 새 비밀번호를 두 번 받아 저장한다. `install`도 이 함수를 쓴다.
fn prompt_and_set_password() -> Result<()> {
    let first = read_hidden("New password (8+ characters): ")?;
    let second = read_hidden("Retype password: ")?;
    let password = check_new_password(&first, &second)?;
    auth::set_password(&password)?;
    Ok(())
}

fn cmd_passwd() -> Result<()> {
    if auth::is_configured() {
        println!("Changing the existing password.");
    } else {
        println!("Setting the password for the web page.");
    }
    prompt_and_set_password()?;
    println!("Password saved.");
    println!("Existing web sessions remain active. Restart the server to end them all.");
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
        // 문자로 읽히지 않는 값의 최종 처리(전부 확인/물러남)는 classify가 정한다.
        assert_eq!(parse_drive_letter("$(DriveName)"), None);
        assert_eq!(parse_drive_letter(""), None);
        assert_eq!(parse_drive_letter("   "), None);
        assert_eq!(parse_drive_letter("12"), None);
    }

    #[test]
    fn 문자와_미치환_값의_분류는_종전과_같다() {
        assert!(matches!(classify_drive_arg("L:"), DriveArg::Letter('L')));
        assert!(matches!(classify_drive_arg("$(DriveName)"), DriveArg::CheckAll));
        assert!(matches!(classify_drive_arg(""), DriveArg::CheckAll));
    }

    #[test]
    fn 볼륨_장치_이벤트는_전부_확인한다() {
        // exFAT 하드는 Ntfs 이벤트 98을 내지 않는다 (2026-09-01 실측). 볼륨이
        // 붙은 장치 이벤트라면 파일 시스템과 무관하게 확인해야 한다.
        assert!(matches!(
            classify_drive_arg(r"STORAGE\Volume\{6b2f-abcd}#0000000000100000"),
            DriveArg::CheckAll
        ));
        assert!(matches!(classify_drive_arg(r"storage\volume\x"), DriveArg::CheckAll));
    }

    #[test]
    fn 저장소가_아닌_장치_이벤트는_물러난다() {
        // 마우스를 꽂았다고 연결된 하드를 전부 다시 훑으면 안 된다.
        assert!(matches!(classify_drive_arg(r"HID\VID_046D&PID_C52B"), DriveArg::NotStorage));
        assert!(matches!(classify_drive_arg(r"USB\VID_0781&PID_5581\5583"), DriveArg::NotStorage));
        assert!(matches!(classify_drive_arg("12"), DriveArg::NotStorage));
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

    #[test]
    fn 빈_비밀번호는_거부한다() {
        assert!(check_new_password("", "").is_err());
        assert!(check_new_password("        ", "        ").is_err());
    }

    #[test]
    fn 두_입력이_다르면_거부한다() {
        assert!(check_new_password("충분히긴비밀번호", "충분히긴비밀번호다").is_err());
    }

    #[test]
    fn 너무_짧으면_거부한다() {
        // 밖에 열리는 화면이다. 네 글자짜리는 찍어서 뚫린다.
        assert!(check_new_password("1234", "1234").is_err());
        assert!(check_new_password("일곱글자짜리요", "일곱글자짜리요").is_err());
    }

    #[test]
    fn 쓸_만한_비밀번호는_통과한다() {
        assert!(check_new_password("충분히긴비밀번호", "충분히긴비밀번호").is_ok());
        // 앞뒤 공백은 사용자가 의도한 것일 수 있으므로 지우지 않는다.
        assert_eq!(check_new_password(" abcdefgh", " abcdefgh").unwrap(), " abcdefgh");
    }
}
