//! 드라이브 전체를 훑어 파일·폴더 목록을 만든다.
//!
//! 여러 스레드로 병렬 순회하되 프로세스 우선순위를 낮춰 두므로,
//! 스캔 중에도 사용자가 하던 작업이 느려지지 않는다.

use anyhow::Result;
use jwalk::WalkDir;
use std::time::UNIX_EPOCH;

use crate::db::Entry;
use crate::volume::Volume;

/// 인덱싱에서 제외할 폴더 이름.
///
/// 사용자 자료가 아니고, 일부는 접근 권한도 없어 오류만 낸다.
const EXCLUDED_DIRS: &[&str] = &[
    "System Volume Information",
    "$RECYCLE.BIN",
    "$Recycle.Bin",
    "Config.Msi",
    "$WinREAgent",
    "Recovery",
    "found.000",
];

fn is_excluded(name: &str) -> bool {
    EXCLUDED_DIRS.iter().any(|e| e.eq_ignore_ascii_case(name))
}

/// 한 번의 스캔 결과.
pub struct ScanResult {
    pub entries: Vec<Entry>,
    /// 권한 문제 등으로 읽지 못한 항목 수. 스캔 자체는 계속 진행된다.
    pub errors: usize,
}

/// 볼륨 전체를 훑어 모든 파일과 폴더를 수집한다.
///
/// 경로는 드라이브 루트 기준 상대 경로로 저장한다. 드라이브 문자는
/// 연결할 때마다 바뀌므로 인덱스에 넣으면 안 된다.
pub fn scan_volume(vol: &Volume) -> Result<ScanResult> {
    scan_root(&vol.root_path())
}

/// 지정한 경로 아래를 모두 훑는다. 테스트에서는 임시 폴더를 넘긴다.
pub fn scan_root(root: &str) -> Result<ScanResult> {
    let root_path = std::path::Path::new(root).to_path_buf();
    let mut entries = Vec::new();
    let mut errors = 0usize;

    let walker = WalkDir::new(&root_path)
        .skip_hidden(false)
        .follow_links(false) // 심볼릭 링크를 따라가면 순환에 빠질 수 있다
        .sort(false)
        .process_read_dir(|_depth, _path, _state, children| {
            children.retain(|child| match child {
                Ok(e) => !(e.file_type.is_dir() && is_excluded(&e.file_name.to_string_lossy())),
                Err(_) => true, // 오류는 여기서 거르지 않고 아래에서 센다
            });
        });

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => {
                errors += 1;
                continue;
            }
        };

        // 첫 항목은 루트 자기 자신이므로 인덱스에 넣지 않는다.
        if entry.depth() == 0 {
            continue;
        }

        let full = entry.path();
        let Ok(rel) = full.strip_prefix(&root_path) else {
            continue;
        };
        let rel = rel.to_string_lossy().to_string();
        if rel.is_empty() {
            continue;
        }

        let is_dir = entry.file_type().is_dir();
        let (size, mtime) = match entry.metadata() {
            Ok(m) => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (if is_dir { 0 } else { m.len() }, mtime)
            }
            Err(_) => {
                errors += 1;
                (0, 0)
            }
        };

        entries.push(Entry {
            name: entry.file_name().to_string_lossy().to_string(),
            path: rel,
            is_dir,
            size,
            mtime,
        });
    }

    Ok(ScanResult { entries, errors })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn 파일과_폴더를_모두_수집한다() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("작업")).unwrap();
        fs::write(tmp.path().join("작업\\메모.txt"), b"hello").unwrap();
        fs::write(tmp.path().join("루트.txt"), b"hi").unwrap();

        let r = scan_root(&tmp.path().to_string_lossy()).unwrap();
        let mut paths: Vec<_> = r.entries.iter().map(|e| e.path.clone()).collect();
        paths.sort();

        assert_eq!(paths, vec!["루트.txt", "작업", "작업\\메모.txt"]);
        assert_eq!(r.errors, 0);
    }

    #[test]
    fn 경로는_루트_기준_상대경로다() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a\\b")).unwrap();
        fs::write(tmp.path().join("a\\b\\c.txt"), b"x").unwrap();

        let r = scan_root(&tmp.path().to_string_lossy()).unwrap();
        let c = r.entries.iter().find(|e| e.name == "c.txt").unwrap();

        assert_eq!(c.path, "a\\b\\c.txt");
        // 드라이브 문자나 임시 폴더 경로가 새어 들어가면 안 된다.
        assert!(!c.path.contains(':'));
    }

    #[test]
    fn 파일_크기와_폴더_구분이_기록된다() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("폴더")).unwrap();
        fs::write(tmp.path().join("파일.bin"), vec![0u8; 1234]).unwrap();

        let r = scan_root(&tmp.path().to_string_lossy()).unwrap();
        let dir = r.entries.iter().find(|e| e.name == "폴더").unwrap();
        let f = r.entries.iter().find(|e| e.name == "파일.bin").unwrap();

        assert!(dir.is_dir);
        assert_eq!(dir.size, 0);
        assert!(!f.is_dir);
        assert_eq!(f.size, 1234);
        assert!(f.mtime > 0);
    }

    #[test]
    fn 시스템_폴더는_건너뛴다() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("System Volume Information")).unwrap();
        fs::write(tmp.path().join("System Volume Information\\숨김.dat"), b"x").unwrap();
        fs::write(tmp.path().join("내자료.txt"), b"x").unwrap();

        let r = scan_root(&tmp.path().to_string_lossy()).unwrap();
        let names: Vec<_> = r.entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"내자료.txt"));
        assert!(!names.contains(&"System Volume Information"));
        assert!(!names.contains(&"숨김.dat"));
    }

    #[test]
    fn 루트_자신은_포함되지_않는다() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"x").unwrap();

        let r = scan_root(&tmp.path().to_string_lossy()).unwrap();
        assert_eq!(r.entries.len(), 1);
    }

    #[test]
    fn 빈_폴더도_결과가_비어_있을_뿐_실패하지_않는다() {
        let tmp = tempfile::tempdir().unwrap();
        let r = scan_root(&tmp.path().to_string_lossy()).unwrap();
        assert!(r.entries.is_empty());
    }

    #[test]
    fn 제외_판정은_대소문자를_가리지_않는다() {
        assert!(is_excluded("$RECYCLE.BIN"));
        assert!(is_excluded("$Recycle.Bin"));
        assert!(is_excluded("system volume information"));
        assert!(!is_excluded("내 프로젝트"));
    }
}
