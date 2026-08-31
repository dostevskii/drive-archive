//! 일반 사용자를 위한 원커맨드 설치.
//!
//! 자동 인덱싱(작업 스케줄러)과 Claude 연결(MCP)을 한 번에 준비한다.
//! 사용자가 직접 설정 파일을 편집하거나 명령을 외울 필요가 없도록 하는 것이 목적이다.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::auth;
use crate::elevation;
use crate::envpath;
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
            .with_context(|| format!("Could not read config: {}", config_path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text).with_context(|| {
                format!(
                    "Config file is not valid JSON: {}\n\
                     Fix it manually and try again, or back up and delete the file, then run again.",
                    config_path.display()
                )
            })?
        }
    } else {
        json!({})
    };

    if !root.is_object() {
        anyhow::bail!("Top level of config file is not an object: {}", config_path.display());
    }

    let servers = root
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        anyhow::bail!("mcpServers in config file is not an object: {}", config_path.display());
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_NAME.to_string(), mcp_entry(exe));

    std::fs::write(config_path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("Could not save config: {}", config_path.display()))?;
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
        println!("Administrator privileges are required to register automatic indexing.");
        println!("When the UAC prompt appears, click 'Yes'.");
        elevation::relaunch_as_admin(&["install"])?;
        println!("\nInstallation continues in the elevated window. You can close this one.");
        return Ok(());
    }

    println!("Starting drive-archive installation.");
    println!("  Executable: {}\n", exe.display());

    // 웹 화면은 밖에서도 들어올 수 있다. 비밀번호 없이 설치를 끝내면 그 상태로
    // 터널이 붙는다. 그래서 여기서 반드시 받는다.
    if auth::is_configured() {
        println!("[1/4] A web page password is already set.");
        println!("      To change it later, run `drive-archive passwd`.");
    } else {
        println!("[1/4] Please set a password for the web page.");
        println!("      The index contains every file name and path. Anyone connecting from");
        println!("      outside will need this password. What you type will not be shown.\n");
        let mut tries = 0;
        loop {
            match crate::prompt_and_set_password() {
                Ok(()) => break,
                Err(e) => {
                    tries += 1;
                    eprintln!("      {e}");
                    if tries >= 3 {
                        anyhow::bail!(
                            "Could not set a password, so installation is stopping.\n\
                             Run `drive-archive install` again."
                        );
                    }
                    eprintln!("      Please try again.\n");
                }
            }
        }
        println!("      Password saved.");
    }

    task::setup(&exe)?;
    println!("\n[2/4] Registered automatic indexing and the web page.");
    println!("      The index will update automatically when you connect an external drive.");
    println!("      The web page opens automatically at login: http://127.0.0.1:8787/");

    // 등록만 하고 끝내면 다음 로그온까지 웹 화면이 없다. v0.2.1에서 sync를
    // 등록 직후 깨운 것과 같은 이유로, 여기서 한 번 띄운다.
    match task::run_serve_now() {
        Ok(()) => println!("      Requested that the web page start now."),
        Err(e) => eprintln!("      Could not start the web page right away ({e:#}). It will start at the next login."),
    }

    // 지금 꽂혀 있는 하드는 마운트 이벤트가 이미 지나갔다. 등록만 하고 끝내면
    // 다음 연결이나 로그온까지 인덱싱되지 않으므로, 여기서 한 번 깨워 준다.
    match task::run_now() {
        Ok(()) => {
            println!("      Also started indexing currently connected drives in the background.");
            println!("      Check progress with `drive-archive status`.");
        }
        Err(e) => {
            eprintln!("      However, indexing of currently connected drives could not be started: {e:#}");
            eprintln!("      Run `drive-archive sync` directly. Automatic indexing registration is complete.");
        }
    }

    // PATH는 편의 기능이지 필수 기능이 아니다. 실패해도 설치를 계속 진행한다.
    // exe는 std::env::current_exe()가 돌려준 절대 경로라 parent()가 없을 수 없다.
    let path_result = exe.parent().context("Could not determine the install folder").and_then(envpath::add);
    match path_result {
        Ok(true) => {
            println!("\n[3/4] Added the install folder to your PATH.");
            println!("      Open a NEW terminal to use the `drive-archive` command directly.");
        }
        Ok(false) => println!("\n[3/4] PATH already contains the install folder."),
        Err(e) => {
            eprintln!("\n[3/4] Could not update PATH: {e:#}");
            eprintln!("      You can still run the exe by its full path.");
        }
    }

    let mut registered = Vec::new();
    for (name, path) in claude_config_targets() {
        match register_mcp(&path, &exe) {
            Ok(true) => registered.push(name),
            Ok(false) => {}
            Err(e) => eprintln!("      Could not update {name} config: {e:#}"),
        }
    }

    if registered.is_empty() {
        println!("\n[4/4] Claude was not found, so the MCP connection was skipped.");
        println!("      Install Claude Desktop or Claude Code, then run this command again.");
    } else {
        println!("\n[4/4] Connected to {}.", registered.join(", "));
        println!("      This takes effect once you fully quit and reopen Claude.");
        println!("      You can now ask Claude things like:");
        println!("        \"Which external drive has last year's branding project?\"");
    }

    println!("\nInstallation complete. Try connecting an external drive.");
    println!("To change the password, run `drive-archive passwd`.");
    println!("To view it from outside, set up a tunnel to http://127.0.0.1:8787/ (see README).");
    println!("Note: if you move this executable to a different folder, run `install` again.");
    Ok(())
}

/// 설치를 되돌린다. 인덱스 자체는 지우지 않는다.
pub fn uninstall() -> Result<()> {
    if !elevation::is_elevated() {
        println!("Administrator privileges are required to remove automatic indexing.");
        println!("When the UAC prompt appears, click 'Yes'.");
        elevation::relaunch_as_admin(&["uninstall"])?;
        println!("\nRemoval continues in the elevated window. You can close this one.");
        return Ok(());
    }

    if task::remove()? {
        println!("[1/3] Removed automatic indexing and web page registration.");
    } else {
        println!("[1/3] No registered task was found.");
    }

    // PATH는 편의 기능이지 필수 기능이 아니다. 실패해도 제거를 계속 진행한다.
    // exe는 std::env::current_exe()가 돌려준 절대 경로라 parent()가 없을 수 없다.
    let exe = task::current_exe()?;
    let path_result = exe.parent().context("Could not determine the install folder").and_then(envpath::remove);
    match path_result {
        Ok(true) => println!("[2/3] Removed the install folder from your PATH."),
        Ok(false) => println!("[2/3] PATH did not contain the install folder."),
        Err(e) => {
            eprintln!("[2/3] Could not update PATH: {e:#}");
            eprintln!("      You can remove it manually from your user environment variables.");
        }
    }

    let mut removed = Vec::new();
    for (name, path) in claude_config_targets() {
        match unregister_mcp(&path) {
            Ok(true) => removed.push(name),
            Ok(false) => {}
            Err(e) => eprintln!("      Could not update {name} config: {e:#}"),
        }
    }

    if removed.is_empty() {
        println!("[3/3] No connections were registered with Claude.");
    } else {
        println!("[3/3] Removed the connection from {}.", removed.join(", "));
    }

    println!("\nThe index and web password remain.");
    println!("To remove everything, delete the %LOCALAPPDATA%\\drive-archive folder.");
    println!("If the web page was running, log off or end drive-archive in Task Manager to fully stop it.");
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
        assert!(format!("{err:#}").contains("not valid JSON"));
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
