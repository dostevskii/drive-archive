//! 인덱스 데이터베이스. 하드 목록과 파일·폴더 메타데이터를 보관한다.
//!
//! 파일 내용은 저장하지 않는다. 이름·경로·크기·수정 시각만 기록하므로
//! DB는 항목 10만 개당 20~30MB 수준으로 유지된다.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::volume::Volume;

/// 인덱스 파일이 놓이는 폴더: `%LOCALAPPDATA%\drive-archive`
pub fn data_dir() -> Result<PathBuf> {
    let base = std::env::var("LOCALAPPDATA")
        .context("LOCALAPPDATA 환경 변수를 읽을 수 없습니다. Windows에서 실행해야 합니다.")?;
    Ok(PathBuf::from(base).join("drive-archive"))
}

/// 인덱스 DB 파일 경로.
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("index.db"))
}

/// DB를 열고 스키마를 준비한다. 파일이 없으면 새로 만든다.
pub fn open() -> Result<Connection> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("데이터 폴더를 만들 수 없습니다: {}", dir.display()))?;
    let path = dir.join("index.db");
    let conn = Connection::open(&path)
        .with_context(|| format!("인덱스를 열 수 없습니다: {}", path.display()))?;
    init_schema(&conn)?;
    Ok(conn)
}

/// 스키마를 생성하고 성능 관련 PRAGMA를 설정한다. 이미 있으면 아무것도 하지 않는다.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS drives (
            id            INTEGER PRIMARY KEY,
            volume_serial TEXT    NOT NULL UNIQUE,
            label         TEXT    NOT NULL,
            total_bytes   INTEGER NOT NULL DEFAULT 0,
            free_bytes    INTEGER NOT NULL DEFAULT 0,
            first_seen    TEXT    NOT NULL,
            last_seen     TEXT    NOT NULL,
            last_scan_at  TEXT
        );

        CREATE TABLE IF NOT EXISTS entries (
            id       INTEGER PRIMARY KEY,
            drive_id INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
            path     TEXT    NOT NULL,
            name     TEXT    NOT NULL,
            is_dir   INTEGER NOT NULL,
            size     INTEGER NOT NULL DEFAULT 0,
            mtime    INTEGER NOT NULL DEFAULT 0,
            UNIQUE(drive_id, path)
        );

        CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name);
        CREATE INDEX IF NOT EXISTS idx_entries_drive ON entries(drive_id);
        "#,
    )
    .context("스키마를 만들 수 없습니다")?;
    Ok(())
}

/// 현재 시각을 DB에 저장할 문자열로 만든다.
fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 볼륨을 등록하거나 기존 등록 정보를 갱신하고 `drives.id`를 돌려준다.
///
/// 볼륨 시리얼이 같으면 같은 하드로 본다. 라벨을 바꿔 달았거나
/// 파일을 지워 여유 공간이 달라졌다면 그 값이 갱신된다.
pub fn upsert_drive(conn: &Connection, vol: &Volume) -> Result<i64> {
    let ts = now();
    conn.execute(
        r#"
        INSERT INTO drives (volume_serial, label, total_bytes, free_bytes, first_seen, last_seen)
        VALUES (?1, ?2, ?3, ?4, ?5, ?5)
        ON CONFLICT(volume_serial) DO UPDATE SET
            label       = excluded.label,
            total_bytes = excluded.total_bytes,
            free_bytes  = excluded.free_bytes,
            last_seen   = excluded.last_seen
        "#,
        params![vol.serial, vol.label, vol.total_bytes as i64, vol.free_bytes as i64, ts],
    )
    .context("하드 정보를 저장할 수 없습니다")?;

    let id: i64 = conn.query_row(
        "SELECT id FROM drives WHERE volume_serial = ?1",
        params![vol.serial],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// 특정 하드에 지금 인덱싱되어 있는 항목 수.
///
/// 새 스캔 결과가 믿을 만한지 판단하는 기준값으로 쓴다.
pub fn entry_count(conn: &Connection, drive_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE drive_id = ?1",
        params![drive_id],
        |r| r.get(0),
    )?)
}

/// 스캔이 끝난 시각을 기록한다.
pub fn mark_scanned(conn: &Connection, drive_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE drives SET last_scan_at = ?1 WHERE id = ?2",
        params![now(), drive_id],
    )?;
    Ok(())
}

/// 파일 하나 또는 폴더 하나. 스캔 결과이자 DB 저장 단위.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// 드라이브 루트 기준 상대 경로 (예: `작업\2024\최종.psd`)
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 수정 시각 (Unix epoch 초)
    pub mtime: i64,
}

/// 변경 여부 비교에 쓰는 최소 정보.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EntryMeta {
    is_dir: bool,
    size: u64,
    mtime: i64,
}

/// 한 번의 스캔이 인덱스를 얼마나 바꿨는지.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct DiffStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
}

impl DiffStats {
    /// 인덱스에 실제로 반영된 변경이 있었는지.
    pub fn has_changes(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }
}

/// 스캔 결과를 인덱스에 반영한다.
///
/// 기존 인덱스와 비교해 새로 생긴 항목은 넣고, 크기나 수정 시각이 달라진
/// 항목은 갱신하고, 이번 스캔에서 보이지 않은 항목은 지운다.
/// 전체가 한 트랜잭션이므로 도중에 중단돼도 인덱스가 반쯤 망가지지 않는다.
pub fn apply_scan(conn: &mut Connection, drive_id: i64, scanned: &[Entry]) -> Result<DiffStats> {
    let mut existing: HashMap<String, (i64, EntryMeta)> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, path, is_dir, size, mtime FROM entries WHERE drive_id = ?1")?;
        let rows = stmt.query_map(params![drive_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                EntryMeta {
                    is_dir: r.get::<_, i64>(2)? != 0,
                    size: r.get::<_, i64>(3)? as u64,
                    mtime: r.get::<_, i64>(4)?,
                },
            ))
        })?;
        for row in rows {
            let (id, path, meta) = row?;
            existing.insert(path, (id, meta));
        }
    }

    let mut stats = DiffStats::default();
    let tx = conn.transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO entries (drive_id, path, name, is_dir, size, mtime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut update =
            tx.prepare("UPDATE entries SET name = ?1, is_dir = ?2, size = ?3, mtime = ?4 WHERE id = ?5")?;

        for e in scanned {
            match existing.remove(&e.path) {
                None => {
                    insert.execute(params![
                        drive_id,
                        e.path,
                        e.name,
                        e.is_dir as i64,
                        e.size as i64,
                        e.mtime
                    ])?;
                    stats.added += 1;
                }
                Some((id, meta)) => {
                    let same = meta.is_dir == e.is_dir
                        && meta.mtime == e.mtime
                        && (e.is_dir || meta.size == e.size);
                    if same {
                        stats.unchanged += 1;
                    } else {
                        update.execute(params![
                            e.name,
                            e.is_dir as i64,
                            e.size as i64,
                            e.mtime,
                            id
                        ])?;
                        stats.updated += 1;
                    }
                }
            }
        }

        // 남은 항목은 이번 스캔에서 보이지 않았으므로 삭제된 것이다.
        let mut delete = tx.prepare("DELETE FROM entries WHERE id = ?1")?;
        for (id, _) in existing.values() {
            delete.execute(params![id])?;
            stats.removed += 1;
        }
    }
    tx.commit().context("인덱스 갱신을 저장할 수 없습니다")?;

    Ok(stats)
}

/// 검색 결과 한 줄.
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    /// 자료가 들어 있는 하드의 라벨. 사용자가 서랍에서 찾을 이름.
    pub drive_label: String,
    pub drive_serial: String,
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 수정 시각을 `YYYY-MM-DD`로 표기한 것. 시각을 읽을 수 없으면 `None`.
    pub modified: Option<String>,
}

/// LIKE 패턴에서 특별한 뜻을 갖는 문자를 이스케이프한다.
///
/// 사용자가 검색어에 `%`나 `_`를 넣어도 글자 그대로 찾도록 한다.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Unix epoch 초를 `YYYY-MM-DD` 문자열로 바꾼다.
fn format_date(mtime: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(mtime, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

/// 이름 또는 경로에 검색어가 들어간 항목을 찾는다.
///
/// 이름에 들어간 결과를 경로에만 들어간 결과보다 먼저, 폴더를 파일보다 먼저 보여준다.
/// 찾는 대상이 대개 프로젝트 폴더이기 때문이다.
pub fn search(
    conn: &Connection,
    query: &str,
    drive: Option<&str>,
    dirs_only: bool,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let pattern = format!("%{}%", escape_like(query));

    let mut sql = String::from(
        r#"
        SELECT d.label, d.volume_serial, e.path, e.name, e.is_dir, e.size, e.mtime,
               (e.name LIKE ?1 ESCAPE '\') AS name_hit
        FROM entries e
        JOIN drives d ON d.id = e.drive_id
        WHERE (e.name LIKE ?1 ESCAPE '\' OR e.path LIKE ?1 ESCAPE '\')
        "#,
    );
    if dirs_only {
        sql.push_str(" AND e.is_dir = 1");
    }
    if drive.is_some() {
        sql.push_str(r#" AND (d.label LIKE ?3 ESCAPE '\' OR d.volume_serial = ?3)"#);
    }
    sql.push_str(" ORDER BY name_hit DESC, e.is_dir DESC, d.label, e.path LIMIT ?2");

    let mut stmt = conn.prepare(&sql)?;
    let map_row = |r: &rusqlite::Row| -> rusqlite::Result<SearchHit> {
        let mtime: i64 = r.get(6)?;
        Ok(SearchHit {
            drive_label: r.get(0)?,
            drive_serial: r.get(1)?,
            path: r.get(2)?,
            name: r.get(3)?,
            is_dir: r.get::<_, i64>(4)? != 0,
            size: r.get::<_, i64>(5)? as u64,
            modified: format_date(mtime),
        })
    };

    let hits: Vec<SearchHit> = match drive {
        Some(d) => {
            let dp = format!("%{}%", escape_like(d));
            stmt.query_map(params![pattern, limit as i64, dp], map_row)?
                .collect::<rusqlite::Result<_>>()?
        }
        None => stmt
            .query_map(params![pattern, limit as i64], map_row)?
            .collect::<rusqlite::Result<_>>()?,
    };
    Ok(hits)
}

/// 인덱스에 등록된 하드 하나.
#[derive(Debug, Clone, Serialize)]
pub struct DriveRow {
    pub label: String,
    pub serial: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub entry_count: i64,
    pub first_seen: String,
    pub last_seen: String,
    pub last_scan_at: Option<String>,
    /// 지금 이 컴퓨터에 연결되어 있는지. 조회 시점에 실제로 확인한 값이다.
    pub connected: bool,
    /// 연결되어 있을 때의 드라이브 문자.
    pub letter: Option<char>,
}

/// 인덱스에 등록된 모든 하드를 반환한다. 연결 여부는 호출 시점에 판별한다.
pub fn list_drives(conn: &Connection) -> Result<Vec<DriveRow>> {
    let live = crate::volume::list_external_volumes();

    let mut stmt = conn.prepare(
        r#"
        SELECT d.label, d.volume_serial, d.total_bytes, d.free_bytes,
               (SELECT COUNT(*) FROM entries e WHERE e.drive_id = d.id),
               d.first_seen, d.last_seen, d.last_scan_at
        FROM drives d
        ORDER BY d.label
        "#,
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(DriveRow {
            label: r.get(0)?,
            serial: r.get(1)?,
            total_bytes: r.get::<_, i64>(2)? as u64,
            free_bytes: r.get::<_, i64>(3)? as u64,
            entry_count: r.get(4)?,
            first_seen: r.get(5)?,
            last_seen: r.get(6)?,
            last_scan_at: r.get(7)?,
            connected: false,
            letter: None,
        })
    })?;

    let mut drives: Vec<DriveRow> = rows.collect::<rusqlite::Result<_>>()?;
    for d in &mut drives {
        if let Some(v) = live.iter().find(|v| v.serial == d.serial) {
            d.connected = true;
            d.letter = Some(v.letter);
        }
    }
    Ok(drives)
}

/// 인덱스 전체 통계.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub drive_count: i64,
    pub entry_count: i64,
    pub file_count: i64,
    pub dir_count: i64,
    pub db_bytes: u64,
    pub db_path: String,
}

/// 인덱스 규모를 요약한다.
pub fn stats(conn: &Connection) -> Result<Stats> {
    let drive_count: i64 = conn.query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))?;
    let entry_count: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    let dir_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM entries WHERE is_dir = 1", [], |r| r.get(0))?;
    let path = db_path()?;
    let db_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    Ok(Stats {
        drive_count,
        entry_count,
        file_count: entry_count - dir_count,
        dir_count,
        db_bytes,
        db_path: path.display().to_string(),
    })
}

/// 하드 하나를 인덱스에서 완전히 제거한다.
///
/// 라벨 또는 볼륨 시리얼로 지정한다. 지운 항목 수를 돌려준다.
pub fn forget(conn: &Connection, name: &str) -> Result<Option<(String, i64)>> {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, label FROM drives WHERE label = ?1 OR volume_serial = ?1",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let Some((id, label)) = found else {
        return Ok(None);
    };

    let removed: i64 =
        conn.query_row("SELECT COUNT(*) FROM entries WHERE drive_id = ?1", params![id], |r| {
            r.get(0)
        })?;
    conn.execute("DELETE FROM entries WHERE drive_id = ?1", params![id])?;
    conn.execute("DELETE FROM drives WHERE id = ?1", params![id])?;
    Ok(Some((label, removed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn test_drive(conn: &Connection, serial: &str, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO drives (volume_serial, label, first_seen, last_seen) VALUES (?1, ?2, 'x', 'x')",
            params![serial, label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn file(path: &str, size: u64, mtime: i64) -> Entry {
        Entry {
            path: path.to_string(),
            name: path.rsplit('\\').next().unwrap().to_string(),
            is_dir: false,
            size,
            mtime,
        }
    }

    #[test]
    fn 첫_스캔은_모든_항목을_추가한다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");

        let scanned = vec![file("a.txt", 10, 100), file("작업\\b.psd", 20, 200)];
        let s = apply_scan(&mut conn, id, &scanned).unwrap();

        assert_eq!(s.added, 2);
        assert_eq!(s.updated, 0);
        assert_eq!(s.removed, 0);
        assert!(s.has_changes());
    }

    #[test]
    fn 바뀌지_않은_항목은_그대로_둔다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        let scanned = vec![file("a.txt", 10, 100)];

        apply_scan(&mut conn, id, &scanned).unwrap();
        let s = apply_scan(&mut conn, id, &scanned).unwrap();

        assert_eq!(s.unchanged, 1);
        assert_eq!(s.added, 0);
        assert!(!s.has_changes());
    }

    #[test]
    fn 크기나_수정시각이_바뀌면_갱신한다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");

        apply_scan(&mut conn, id, &[file("a.txt", 10, 100)]).unwrap();
        let s = apply_scan(&mut conn, id, &[file("a.txt", 99, 300)]).unwrap();

        assert_eq!(s.updated, 1);
        assert_eq!(s.added, 0);

        let size: i64 = conn
            .query_row("SELECT size FROM entries WHERE path = 'a.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(size, 99);
    }

    #[test]
    fn 사라진_항목은_지운다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");

        apply_scan(&mut conn, id, &[file("a.txt", 10, 100), file("b.txt", 10, 100)]).unwrap();
        let s = apply_scan(&mut conn, id, &[file("a.txt", 10, 100)]).unwrap();

        assert_eq!(s.removed, 1);
        assert_eq!(s.unchanged, 1);

        let n: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn 하드마다_인덱스가_분리된다() {
        let mut conn = test_db();
        let a = test_drive(&conn, "AAAA0001", "PROJECT-A");
        let b = test_drive(&conn, "BBBB0002", "BACKUP-02");

        apply_scan(&mut conn, a, &[file("a.txt", 10, 100)]).unwrap();
        // B를 스캔해도 A의 항목이 "사라진 것"으로 취급되지 않아야 한다.
        let s = apply_scan(&mut conn, b, &[file("b.txt", 10, 100)]).unwrap();

        assert_eq!(s.removed, 0);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn 폴더는_크기_변화를_무시한다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        let dir = |size: u64| Entry {
            path: "작업".into(),
            name: "작업".into(),
            is_dir: true,
            size,
            mtime: 100,
        };

        apply_scan(&mut conn, id, &[dir(0)]).unwrap();
        // 폴더 크기는 파일시스템마다 다르게 보고되므로 비교에서 제외한다.
        let s = apply_scan(&mut conn, id, &[dir(4096)]).unwrap();

        assert_eq!(s.unchanged, 1);
        assert_eq!(s.updated, 0);
    }

    #[test]
    fn 이름과_경로_양쪽에서_찾는다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        apply_scan(
            &mut conn,
            id,
            &[file("브랜딩\\최종.psd", 10, 100), file("기타\\메모.txt", 10, 100)],
        )
        .unwrap();

        let hits = search(&conn, "브랜딩", None, false, 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].drive_label, "PROJECT-A");
        assert_eq!(hits[0].name, "최종.psd");
    }

    #[test]
    fn 이름이_일치하는_결과가_먼저_나온다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        apply_scan(
            &mut conn,
            id,
            &[file("메모\\기타.txt", 10, 100), file("기타\\메모.txt", 10, 100)],
        )
        .unwrap();

        let hits = search(&conn, "메모", None, false, 50).unwrap();
        assert_eq!(hits.len(), 2);
        // 이름이 "메모.txt"인 쪽이 경로에만 "메모"가 있는 쪽보다 앞에 온다.
        assert_eq!(hits[0].name, "메모.txt");
    }

    #[test]
    fn 검색어의_와일드카드는_글자로_취급한다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        apply_scan(&mut conn, id, &[file("100%_완료.txt", 10, 100), file("기타.txt", 10, 100)])
            .unwrap();

        // "%"를 와일드카드로 해석하면 두 건이 모두 걸린다.
        let hits = search(&conn, "100%", None, false, 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "100%_완료.txt");
    }

    #[test]
    fn 폴더만_검색할_수_있다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        apply_scan(
            &mut conn,
            id,
            &[
                Entry { path: "브랜딩".into(), name: "브랜딩".into(), is_dir: true, size: 0, mtime: 1 },
                file("브랜딩.txt", 10, 100),
            ],
        )
        .unwrap();

        let hits = search(&conn, "브랜딩", None, true, 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_dir);
    }

    #[test]
    fn 특정_하드로_검색을_한정할_수_있다() {
        let mut conn = test_db();
        let a = test_drive(&conn, "AAAA0001", "PROJECT-A");
        let b = test_drive(&conn, "BBBB0002", "BACKUP-02");
        apply_scan(&mut conn, a, &[file("공통.txt", 10, 100)]).unwrap();
        apply_scan(&mut conn, b, &[file("공통.txt", 10, 100)]).unwrap();

        let hits = search(&conn, "공통", Some("BACKUP"), false, 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].drive_label, "BACKUP-02");
    }

    #[test]
    fn 하드를_잊으면_항목도_함께_사라진다() {
        let mut conn = test_db();
        let id = test_drive(&conn, "AAAA0001", "PROJECT-A");
        apply_scan(&mut conn, id, &[file("a.txt", 10, 100), file("b.txt", 10, 100)]).unwrap();

        let removed = forget(&conn, "PROJECT-A").unwrap();
        assert_eq!(removed, Some(("PROJECT-A".to_string(), 2)));

        let n: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn 없는_하드를_잊으면_아무_일도_없다() {
        let conn = test_db();
        assert_eq!(forget(&conn, "없는하드").unwrap(), None);
    }

    #[test]
    fn 같은_시리얼은_같은_하드로_본다() {
        let conn = test_db();
        let vol = Volume {
            letter: 'E',
            serial: "AAAA0001".into(),
            label: "PROJECT-A".into(),
            total_bytes: 1000,
            free_bytes: 500,
        };
        let first = upsert_drive(&conn, &vol).unwrap();

        // 라벨을 바꿔 달고 드라이브 문자가 달라져도 같은 행을 갱신해야 한다.
        let renamed = Volume { letter: 'F', label: "PROJECT-A2".into(), free_bytes: 400, ..vol };
        let second = upsert_drive(&conn, &renamed).unwrap();

        assert_eq!(first, second);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);

        let label: String = conn
            .query_row("SELECT label FROM drives WHERE id = ?1", params![first], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "PROJECT-A2");
    }
}
