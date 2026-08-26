//! 외장하드가 연결됐을 때 인덱스를 갱신하는 진입점.
//!
//! 작업 스케줄러가 볼륨 마운트 이벤트를 받으면 이 코드가 실행된다.
//! 사용자에게 보이지 않는 곳에서 도는 만큼, 진행 상황은 로그 파일에 남긴다.

use anyhow::{Context, Result, bail};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use windows::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
};

use crate::db;
use crate::scan;
use crate::volume::{self, Volume};

/// 같은 하드를 이 시간 안에 다시 스캔하지는 않는다.
///
/// 하드 하나에 볼륨이 여러 개면 마운트 이벤트도 여러 번 발생한다.
/// 그때마다 전체를 다시 훑는 것은 낭비다.
const RESCAN_COOLDOWN_SECS: i64 = 120;

/// 실행 중 다른 인스턴스가 있을 때 잡고 있는 잠금 파일.
///
/// 파일을 공유 없이 열어 두는 방식이라, 프로세스가 어떻게 끝나든
/// 핸들이 닫히면서 잠금도 자동으로 풀린다.
pub struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    /// 잠금을 얻는다. 이미 다른 인스턴스가 돌고 있으면 `None`.
    pub fn acquire() -> Result<Option<Self>> {
        let dir = db::data_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("sync.lock");

        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0) // 다른 프로세스는 이 파일을 열 수 없다
            .open(&path)
        {
            Ok(f) => Ok(Some(InstanceLock { _file: f })),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
            Err(e) => Err(e).with_context(|| format!("잠금 파일을 만들 수 없습니다: {}", path.display())),
        }
    }
}

/// 이 프로세스의 우선순위를 낮춘다.
///
/// 스캔은 급하지 않다. 사용자가 하던 작업이 먼저다.
pub fn lower_priority() {
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// 로그 파일에 한 줄 남긴다. 로그를 못 써도 본 작업은 계속한다.
pub fn log(msg: &str) {
    let Ok(dir) = db::data_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let line = format!("[{}] {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join("sync.log")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 스캔 결과를 인덱스에 반영해도 되는지 판단한다.
///
/// 읽지 못한 항목이 있는데 개수까지 크게 줄었다면, 사용자가 파일을 지운 것이 아니라
/// 하드를 제대로 읽지 못한 쪽일 가능성이 높다. 그대로 반영하면 멀쩡한 인덱스를 지우게 된다.
///
/// 오류 없이 끝난 스캔은 결과가 얼마나 줄었든 그대로 믿는다. 사용자가 정말로
/// 하드를 비웠을 수 있고, 그때는 인덱스도 비워지는 것이 맞다.
fn is_scan_trustworthy(scanned: usize, previous: i64, errors: usize) -> bool {
    if errors == 0 || previous <= 0 {
        return true;
    }
    scanned as i64 * 2 >= previous
}

/// 하드 하나를 스캔해 인덱스에 반영한다.
///
/// 스캔 결과를 믿을 수 없으면 반영하지 않고 오류를 낸다. 인덱스는 이전 상태로 남는다.
pub fn sync_volume(conn: &mut rusqlite::Connection, vol: &Volume) -> Result<db::DiffStats> {
    let drive_id = db::upsert_drive(conn, vol)?;
    let previous = db::entry_count(conn, drive_id)?;

    let result = scan::scan_volume(vol)
        .with_context(|| format!("{} 스캔에 실패했습니다", vol.root_path()))?;

    // 스캔 도중 하드가 빠지면 그때까지 읽은 만큼만 결과에 남는다.
    // 반영하면 나머지가 전부 삭제된 것으로 처리된다.
    if !volume::still_connected(vol) {
        bail!("스캔 도중 하드가 분리되었습니다. 인덱스는 그대로 두었습니다");
    }

    if result.errors > 0 {
        log(&format!(
            "{} ({}): 항목 {}개를 읽지 못했습니다",
            vol.label, vol.letter, result.errors
        ));
    }

    if !is_scan_trustworthy(result.entries.len(), previous, result.errors) {
        bail!(
            "스캔 결과를 믿을 수 없어 반영하지 않았습니다 \
             (이전 {previous}개 → 이번 {}개, 읽지 못한 항목 {}개). \
             하드를 다시 연결해 `drive-archive scan {}`을 실행하세요",
            result.entries.len(),
            result.errors,
            vol.letter
        );
    }

    let stats = db::apply_scan(conn, drive_id, &result.entries)?;
    db::mark_scanned(conn, drive_id)?;
    Ok(stats)
}

/// 하드를 방금 스캔했는지 확인한다.
fn scanned_recently(conn: &rusqlite::Connection, serial: &str) -> bool {
    let elapsed: Option<i64> = conn
        .query_row(
            "SELECT CAST((julianday('now', 'localtime') - julianday(last_scan_at)) * 86400 AS INTEGER)
             FROM drives WHERE volume_serial = ?1",
            rusqlite::params![serial],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    matches!(elapsed, Some(secs) if (0..RESCAN_COOLDOWN_SECS).contains(&secs))
}

/// 한 하드에 대한 동기화 결과.
pub enum SyncOutcome {
    /// 스캔해서 인덱스를 갱신했다.
    Updated { label: String, letter: char, stats: db::DiffStats },
    /// 방금 스캔했으므로 건너뛰었다.
    Skipped { label: String, letter: char },
    /// 스캔하지 못했거나 결과를 믿을 수 없었다. 인덱스는 이전 상태 그대로다.
    Failed { label: String, letter: char, reason: String },
}

/// 지금 연결된 외장하드를 모두 확인해 인덱스를 갱신한다.
///
/// 작업 스케줄러가 호출하는 경로이자 `drive-archive sync`가 하는 일이다.
/// `force`가 참이면 방금 스캔한 하드도 다시 훑는다.
pub fn sync_all(force: bool) -> Result<Vec<SyncOutcome>> {
    let volumes = volume::list_external_volumes();
    if volumes.is_empty() {
        log("연결된 외장하드가 없습니다");
        return Ok(Vec::new());
    }

    let mut conn = db::open()?;
    let mut outcomes = Vec::new();

    for vol in &volumes {
        if !force && scanned_recently(&conn, &vol.serial) {
            log(&format!("{} ({}): 방금 스캔했으므로 건너뜀", vol.label, vol.letter));
            outcomes.push(SyncOutcome::Skipped {
                label: vol.label.clone(),
                letter: vol.letter,
            });
            continue;
        }

        log(&format!("{} ({}:) 스캔 시작", vol.label, vol.letter));
        match sync_volume(&mut conn, vol) {
            Ok(stats) => {
                log(&format!(
                    "{} ({}:) 완료 - 추가 {} / 변경 {} / 삭제 {} / 그대로 {}",
                    vol.label, vol.letter, stats.added, stats.updated, stats.removed, stats.unchanged
                ));
                outcomes.push(SyncOutcome::Updated {
                    label: vol.label.clone(),
                    letter: vol.letter,
                    stats,
                });
            }
            Err(e) => {
                let reason = format!("{e:#}");
                log(&format!("{} ({}:) 실패 - {reason}", vol.label, vol.letter));
                outcomes.push(SyncOutcome::Failed {
                    label: vol.label.clone(),
                    letter: vol.letter,
                    reason,
                });
            }
        }
    }

    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 오류_없는_스캔은_결과가_줄어도_믿는다() {
        // 사용자가 하드를 정리해 파일을 대량으로 지운 정상 상황이다.
        assert!(is_scan_trustworthy(10, 20000, 0));
        assert!(is_scan_trustworthy(0, 20000, 0));
    }

    #[test]
    fn 첫_스캔은_비교할_이전_값이_없으므로_믿는다() {
        assert!(is_scan_trustworthy(0, 0, 5));
        assert!(is_scan_trustworthy(100, 0, 5));
    }

    #[test]
    fn 오류가_있어도_결과가_비슷하면_믿는다() {
        // 파일 몇 개를 못 읽었을 뿐 하드는 정상이다.
        assert!(is_scan_trustworthy(19990, 20000, 10));
        assert!(is_scan_trustworthy(10000, 20000, 10));
    }

    #[test]
    fn 오류가_있고_결과가_절반_아래로_줄면_믿지_않는다() {
        // 하드를 제대로 읽지 못한 쪽일 가능성이 높다.
        assert!(!is_scan_trustworthy(9999, 20000, 1));
        assert!(!is_scan_trustworthy(0, 20000, 1));
    }

    #[test]
    fn 오류가_있는_빈_결과는_믿지_않는다() {
        // 분리된 하드에서 가장 흔하게 나오는 모습이다.
        assert!(!is_scan_trustworthy(0, 1, 1));
    }
}
