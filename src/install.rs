//! 일반 사용자를 위한 원커맨드 설치.
//!
//! 자동 인덱싱(작업 스케줄러)과 Claude 연결(MCP)을 한 번에 준비한다.
//! 사용자가 직접 설정 파일을 편집하거나 명령을 외울 필요가 없도록 하는 것이 목적이다.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::elevation;
use crate::task;

/// MCP 설정에 등록될 서버 이름.
const MCP_SERVER_NAME: &str = "drive-archive";

/// Claude Desktop 설정 파일 이름.
const DESKTOP_CONFIG_NAME: &str = "claude_desktop_config.json";

/// Claude Desktop 설정 파일 경로들.
///
/// 배포판에 따라 위치가 다르다.
///
/// - 웹에서 받은 설치판: `%APPDATA%\Claude\claude_desktop_config.json`
/// - Microsoft Store(MSIX)판: 앱이 `%APPDATA%`에 쓴다고 여기는 내용이 패키지
///   폴더 안으로 리디렉션되어, 실제 파일은
///   `%LOCALAPPDATA%\Packages\Claude_<게시자>\LocalCache\Roaming\Claude\`에 놓인다.
///   Store판만 깔려 있으면 `%APPDATA%\Claude`는 아예 생기지 않는다.
///
/// 두 판을 함께 깔 수 있으므로 찾은 곳을 모두 돌려준다.
fn claude_desktop_configs() -> Vec<(&'static str, PathBuf)> {
    let mut found = Vec::new();

    if let Ok(appdata) = std::env::var("APPDATA") {
        found.push((
            "Claude Desktop",
            PathBuf::from(appdata).join("Claude").join(DESKTOP_CONFIG_NAME),
        ));
    }

    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        found.extend(
            store_desktop_configs(&PathBuf::from(local).join("Packages"))
                .into_iter()
                .map(|p| ("Claude Desktop (Store)", p)),
        );
    }

    found
}

/// `%LOCALAPPDATA%\Packages` 아래에서 Store판 Claude의 설정 파일을 찾는다.
///
/// 패키지 폴더 이름에는 게시자 해시가 붙으므로(`Claude_pzs8sxrjxfjjc`) 이름을
/// 박아 두지 않고 `Claude_`로 시작하는 폴더를 훑는다. 없거나 읽을 수 없으면
/// 빈 목록을 돌려준다 — Store판을 안 쓰는 것이 정상적인 경우다.
fn store_desktop_configs(packages: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(packages) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("Claude_"))
        .map(|e| {
            e.path()
                .join("LocalCache")
                .join("Roaming")
                .join("Claude")
                .join(DESKTOP_CONFIG_NAME)
        })
        // 같은 이름으로 시작하는 다른 패키지가 섞일 수 있으므로, 설정 폴더가
        // 실제로 있는 것만 남긴다.
        .filter(|p| p.parent().is_some_and(Path::exists))
        .collect()
}

/// Claude Code 설정 파일 경로: `%USERPROFILE%\.claude.json`
fn claude_code_config() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(PathBuf::from(home).join(".claude.json"))
}

/// 이 프로그램을 MCP 서버로 실행하는 설정 조각.
fn mcp_entry(exe: &Path) -> Value {
    json!({
        "command": exe.display().to_string(),
        "args": ["mcp"],
    })
}

/// 설정 파일 한 곳에 MCP 서버를 등록한다.
///
/// 이미 있는 설정은 건드리지 않는다. `mcpServers` 아래 우리 항목만 넣거나 바꾼다.
/// 파일이 없으면 새로 만든다. Claude를 아직 설치하지 않았을 수 있으므로,
/// 부모 폴더가 없으면 등록을 건너뛴다.
fn register_mcp(config_path: &Path, exe: &Path) -> Result<bool> {
    let Some(parent) = config_path.parent() else {
        return Ok(false);
    };
    if !parent.exists() {
        return Ok(false);
    }

    let mut root: Value = if config_path.exists() {
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("설정을 읽을 수 없습니다: {}", config_path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text).with_context(|| {
                format!(
                    "설정 파일이 올바른 JSON이 아닙니다: {}\n\
                     직접 고친 뒤 다시 시도하거나, 파일을 백업하고 지운 뒤 실행하세요.",
                    config_path.display()
                )
            })?
        }
    } else {
        json!({})
    };

    if !root.is_object() {
        anyhow::bail!("설정 파일의 최상위가 객체가 아닙니다: {}", config_path.display());
    }

    let servers = root
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        anyhow::bail!("설정 파일의 mcpServers가 객체가 아닙니다: {}", config_path.display());
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_NAME.to_string(), mcp_entry(exe));

    std::fs::write(config_path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("설정을 저장할 수 없습니다: {}", config_path.display()))?;
    Ok(true)
}

/// 설정 파일 한 곳에서 MCP 서버 등록을 지운다.
fn unregister_mcp(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(config_path)?;
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        // 우리가 만들지 않은 형식이면 손대지 않는다.
        return Ok(false);
    };

    let removed = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|s| s.remove(MCP_SERVER_NAME))
        .is_some();

    if removed {
        std::fs::write(config_path, serde_json::to_string_pretty(&root)?)?;
    }
    Ok(removed)
}

/// 설치를 진행한다.
///
/// 작업 스케줄러 등록에는 관리자 권한이 필요하므로, 권한이 없으면
/// UAC 동의를 받아 자신을 다시 실행한다.
pub fn install() -> Result<()> {
    let exe = task::current_exe()?;

    if !elevation::is_elevated() {
        println!("자동 인덱싱을 등록하려면 관리자 권한이 필요합니다.");
        println!("UAC 창이 뜨면 '예'를 눌러 주세요.");
        elevation::relaunch_as_admin(&["install"])?;
        println!("\n관리자 권한 창에서 설치가 이어집니다. 이 창은 닫으셔도 됩니다.");
        return Ok(());
    }

    println!("drive-archive 설치를 시작합니다.");
    println!("  실행 파일: {}\n", exe.display());

    task::setup(&exe)?;
    println!("[1/2] 자동 인덱싱을 등록했습니다.");
    println!("      이제 외장하드를 연결하면 인덱스가 자동으로 갱신됩니다.");

    // 지금 꽂혀 있는 하드는 마운트 이벤트가 이미 지나갔다. 등록만 하고 끝내면
    // 다음 연결이나 로그온까지 인덱싱되지 않으므로, 여기서 한 번 깨워 준다.
    match task::run_now() {
        Ok(()) => {
            println!("      지금 연결되어 있는 하드도 백그라운드에서 인덱싱을 시작했습니다.");
            println!("      진행 상황은 `drive-archive status`로 확인할 수 있습니다.");
        }
        Err(e) => {
            eprintln!("      다만 지금 연결된 하드의 인덱싱을 시작하지 못했습니다: {e:#}");
            eprintln!("      `drive-archive sync`를 직접 실행하세요. 자동 인덱싱 등록은 끝났습니다.");
        }
    }

    let mut registered = Vec::new();
    for (name, path) in claude_config_targets() {
        match register_mcp(&path, &exe) {
            Ok(true) => registered.push(name),
            Ok(false) => {}
            Err(e) => eprintln!("      {name} 설정을 건드리지 못했습니다: {e:#}"),
        }
    }

    if registered.is_empty() {
        println!("\n[2/2] Claude를 찾지 못해 MCP 연결은 건너뛰었습니다.");
        println!("      Claude Desktop이나 Claude Code를 설치한 뒤 이 명령을 다시 실행하세요.");
    } else {
        println!("\n[2/2] {}에 연결했습니다.", registered.join(", "));
        println!("      Claude를 완전히 종료했다가 다시 켜면 적용됩니다.");
        println!("      이제 Claude에게 이렇게 물어볼 수 있습니다:");
        println!("        \"외장하드에서 작년 브랜딩 프로젝트 어디 있어?\"");
    }

    println!("\n설치가 끝났습니다. 외장하드를 연결해 보세요.");
    println!("주의: 이 실행 파일을 다른 폴더로 옮기면 `install`을 다시 실행해야 합니다.");
    Ok(())
}

/// 설치를 되돌린다. 인덱스 자체는 지우지 않는다.
pub fn uninstall() -> Result<()> {
    if !elevation::is_elevated() {
        println!("자동 인덱싱 등록을 해제하려면 관리자 권한이 필요합니다.");
        println!("UAC 창이 뜨면 '예'를 눌러 주세요.");
        elevation::relaunch_as_admin(&["uninstall"])?;
        println!("\n관리자 권한 창에서 제거가 이어집니다. 이 창은 닫으셔도 됩니다.");
        return Ok(());
    }

    if task::remove()? {
        println!("[1/2] 자동 인덱싱 등록을 해제했습니다.");
    } else {
        println!("[1/2] 자동 인덱싱이 등록되어 있지 않았습니다.");
    }

    let mut removed = Vec::new();
    for (name, path) in claude_config_targets() {
        match unregister_mcp(&path) {
            Ok(true) => removed.push(name),
            Ok(false) => {}
            Err(e) => eprintln!("      {name} 설정을 건드리지 못했습니다: {e:#}"),
        }
    }

    if removed.is_empty() {
        println!("[2/2] Claude에 등록된 연결이 없었습니다.");
    } else {
        println!("[2/2] {}에서 연결을 지웠습니다.", removed.join(", "));
    }

    let index = crate::db::db_path()?;
    println!("\n인덱스는 그대로 남겨 두었습니다: {}", index.display());
    println!("완전히 지우려면 위 폴더를 직접 삭제하세요.");
    Ok(())
}

/// MCP를 등록할 대상 설정 파일 목록.
fn claude_config_targets() -> Vec<(&'static str, PathBuf)> {
    let mut targets = claude_desktop_configs();
    if let Some(path) = claude_code_config() {
        targets.push(("Claude Code", path));
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn 설정_파일이_없으면_새로_만든다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("claude_desktop_config.json");

        assert!(register_mcp(&cfg, Path::new("C:\\Tools\\drive-archive.exe")).unwrap());

        let root = read(&cfg);
        assert_eq!(root["mcpServers"]["drive-archive"]["args"], json!(["mcp"]));
        assert_eq!(
            root["mcpServers"]["drive-archive"]["command"],
            "C:\\Tools\\drive-archive.exe"
        );
    }

    #[test]
    fn 기존_설정은_그대로_둔다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.json");
        write(
            &cfg,
            r#"{"theme":"dark","mcpServers":{"other-tool":{"command":"other.exe"}}}"#,
        );

        register_mcp(&cfg, Path::new("x.exe")).unwrap();

        let root = read(&cfg);
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["mcpServers"]["other-tool"]["command"], "other.exe");
        assert_eq!(root["mcpServers"]["drive-archive"]["command"], "x.exe");
    }

    #[test]
    fn 다시_설치하면_경로만_바뀐다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.json");

        register_mcp(&cfg, Path::new("old.exe")).unwrap();
        register_mcp(&cfg, Path::new("new.exe")).unwrap();

        let servers = read(&cfg)["mcpServers"].as_object().unwrap().clone();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["drive-archive"]["command"], "new.exe");
    }

    #[test]
    fn 빈_파일도_처리한다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.json");
        write(&cfg, "   ");

        assert!(register_mcp(&cfg, Path::new("x.exe")).unwrap());
        assert_eq!(read(&cfg)["mcpServers"]["drive-archive"]["command"], "x.exe");
    }

    #[test]
    fn 깨진_json은_덮어쓰지_않고_알린다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.json");
        write(&cfg, "{ 이건 JSON이 아님");

        let err = register_mcp(&cfg, Path::new("x.exe")).unwrap_err();
        assert!(format!("{err:#}").contains("올바른 JSON이 아닙니다"));
        // 원본이 보존되어야 한다.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{ 이건 JSON이 아님");
    }

    #[test]
    fn claude가_없는_폴더면_건너뛴다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("설치안됨").join("c.json");

        assert!(!register_mcp(&cfg, Path::new("x.exe")).unwrap());
        assert!(!cfg.exists());
    }

    #[test]
    fn 제거하면_우리_항목만_사라진다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.json");
        write(&cfg, r#"{"mcpServers":{"other-tool":{"command":"other.exe"}}}"#);
        register_mcp(&cfg, Path::new("x.exe")).unwrap();

        assert!(unregister_mcp(&cfg).unwrap());

        let servers = read(&cfg)["mcpServers"].as_object().unwrap().clone();
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("other-tool"));
    }

    #[test]
    fn 등록되지_않은_설정을_제거해도_문제없다() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.json");
        write(&cfg, r#"{"mcpServers":{}}"#);

        assert!(!unregister_mcp(&cfg).unwrap());
    }

    #[test]
    fn 없는_파일을_제거해도_문제없다() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!unregister_mcp(&tmp.path().join("없음.json")).unwrap());
    }

    /// `%LOCALAPPDATA%\Packages\<이름>\LocalCache\Roaming\Claude`를 만든다.
    fn 패키지_설정폴더(packages: &Path, 이름: &str) {
        std::fs::create_dir_all(
            packages.join(이름).join("LocalCache").join("Roaming").join("Claude"),
        )
        .unwrap();
    }

    #[test]
    fn store판_설정_경로를_찾는다() {
        let tmp = tempfile::tempdir().unwrap();
        let packages = tmp.path();
        패키지_설정폴더(packages, "Claude_pzs8sxrjxfjjc");

        let found = store_desktop_configs(packages);

        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0],
            packages
                .join("Claude_pzs8sxrjxfjjc")
                .join("LocalCache")
                .join("Roaming")
                .join("Claude")
                .join("claude_desktop_config.json")
        );
    }

    #[test]
    fn store판_아닌_패키지는_건너뛴다() {
        let tmp = tempfile::tempdir().unwrap();
        let packages = tmp.path();
        패키지_설정폴더(packages, "Microsoft.WindowsCalculator_8wekyb3d8bbwe");
        // 이름은 Claude로 시작하지만 설정 폴더가 없는 경우.
        std::fs::create_dir_all(packages.join("Claude_다른앱")).unwrap();

        assert!(store_desktop_configs(packages).is_empty());
    }

    #[test]
    fn packages_폴더가_없어도_문제없다() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(store_desktop_configs(&tmp.path().join("Packages")).is_empty());
    }
}
