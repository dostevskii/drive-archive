//! 작업 스케줄러에 이벤트 트리거를 등록한다.
//!
//! 상주 프로그램을 띄우는 대신, Windows가 볼륨 마운트 이벤트를 기록했을 때만
//! 스케줄러가 이 프로그램을 깨운다. 그래서 평소 리소스 사용량이 0이다.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// 작업 스케줄러에 등록되는 이름.
pub const TASK_NAME: &str = "drive-archive sync";

/// 볼륨이 마운트될 때 Windows가 System 로그에 남기는 이벤트.
///
/// 예: `볼륨 J: (\Device\HarddiskVolume27) 정상입니다. 작업이 필요 없습니다.`
/// 섀도 복사본에서도 발생하지만, `sync`가 USB 볼륨만 걸러내므로 문제없다.
///
/// 프로바이더 이름은 `Ntfs`지만 NTFS 전용이 아니다. exFAT 볼륨에서도 이 이벤트가
/// 발생하는 것을 확인했다. 다만 모든 파일 시스템에서 발생한다고 보장할 수는 없어,
/// 로그온 트리거를 함께 걸어 놓친 연결을 메운다.
const MOUNT_EVENT_QUERY: &str = "&lt;QueryList&gt;&lt;Query Id=&quot;0&quot; Path=&quot;System&quot;&gt;&lt;Select Path=&quot;System&quot;&gt;*[System[Provider[@Name='Microsoft-Windows-Ntfs'] and EventID=98]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;";

/// 작업 정의 XML을 만든다.
///
/// `LogonType`이 `S4U`인 이유는 콘솔 창 때문이다. `InteractiveToken`으로 두면 스케줄러가
/// 깨울 때마다 대화형 세션에 검은 콘솔 창이 떠서, 사용자가 아무것도 하지 않았는데 화면에
/// 창이 나타난다. S4U는 같은 계정으로 실행하되 대화형 세션에 붙지 않으므로 창이 아예 뜨지
/// 않고, `Password`와 달리 비밀번호를 저장할 필요도 없다.
///
/// 마운트 이벤트에는 어느 볼륨이 붙었는지가 `DriveName`(`L:` 형태)으로 들어 있다. 값 쿼리로
/// 그 값을 꺼내 `sync --drive`에 넘기면 방금 꽂은 하드만 훑는다. 로그온 트리거나
/// `schtasks /Run`처럼 값이 없는 경로로 실행되면 치환되지 않은 문자열이 그대로 넘어오는데,
/// `sync` 쪽에서 드라이브 문자로 읽히지 않는 값은 무시하고 연결된 하드를 전부 확인한다.
fn task_xml(exe: &Path) -> Result<String> {
    let user = std::env::var("USERNAME").context("USERNAME 환경 변수를 읽을 수 없습니다")?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".to_string());
    let exe = exe.display().to_string();

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>외장하드가 연결되면 drive-archive 인덱스를 갱신합니다. 평소에는 실행되지 않습니다.</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>{MOUNT_EVENT_QUERY}</Subscription>
      <Delay>PT15S</Delay>
      <ValueQueries>
        <Value name="DriveName">Event/EventData/Data[@Name='DriveName']</Value>
      </ValueQueries>
    </EventTrigger>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Delay>PT2M</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{domain}\{user}</UserId>
      <LogonType>S4U</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT4H</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>sync --drive $(DriveName)</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    ))
}

/// `schtasks.exe`는 UTF-16LE로 인코딩된 XML만 받는다.
fn to_utf16le_bom(s: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// 지금 실행 중인 실행 파일의 절대 경로.
pub fn current_exe() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")
}

/// 작업을 등록한다. 이미 있으면 새 정의로 덮어쓴다.
///
/// 작업 스케줄러에 쓰려면 관리자 권한이 필요하다. 권한이 없으면
/// schtasks가 영문 오류만 내놓으므로, 미리 확인해 무엇을 해야 하는지 알려 준다.
pub fn setup(exe: &Path) -> Result<()> {
    if !crate::elevation::is_elevated() {
        bail!(
            "작업 스케줄러에 등록하려면 관리자 권한이 필요합니다.\n\
             PowerShell을 마우스 오른쪽 버튼으로 눌러 '관리자 권한으로 실행'한 뒤 다시 시도하세요."
        );
    }

    let xml = task_xml(exe)?;
    let tmp = std::env::temp_dir().join("drive-archive-task.xml");
    std::fs::write(&tmp, to_utf16le_bom(&xml))
        .with_context(|| format!("작업 정의를 쓸 수 없습니다: {}", tmp.display()))?;

    let out = Command::new("schtasks")
        .args(["/Create", "/TN", TASK_NAME, "/XML"])
        .arg(&tmp)
        .arg("/F")
        .output()
        .context("schtasks를 실행할 수 없습니다")?;

    let _ = std::fs::remove_file(&tmp);

    if !out.status.success() {
        bail!("작업 스케줄러 등록에 실패했습니다:\n{}", decode_console(&out.stderr, &out.stdout));
    }
    Ok(())
}

/// 등록된 작업을 지금 한 번 실행한다.
///
/// 마운트 이벤트는 하드를 꽂는 순간에만 발생한다. 등록하는 시점에 이미 꽂혀 있던
/// 하드는 그 이벤트를 놓친 뒤이므로, 등록 직후 작업을 한 번 깨워 지금 연결된 하드를
/// 인덱싱하게 한다. 사용자가 하드를 뽑았다 다시 꽂지 않아도 되게 하는 것이 목적이다.
///
/// 작업은 `LeastPrivilege`로 등록되어 있어, 상승된 권한에서 호출해도 스캔 자체는
/// 일반 사용자 권한으로 돈다. 실행에는 관리자 권한이 필요 없다.
pub fn run_now() -> Result<()> {
    let out = Command::new("schtasks")
        .args(["/Run", "/TN", TASK_NAME])
        .output()
        .context("schtasks를 실행할 수 없습니다")?;

    if !out.status.success() {
        bail!("작업을 시작하지 못했습니다:\n{}", decode_console(&out.stderr, &out.stdout));
    }
    Ok(())
}

/// 등록된 작업을 제거한다. 없으면 조용히 넘어간다.
pub fn remove() -> Result<bool> {
    if !exists() {
        return Ok(false);
    }
    if !crate::elevation::is_elevated() {
        bail!(
            "작업 스케줄러 등록을 해제하려면 관리자 권한이 필요합니다.\n\
             PowerShell을 '관리자 권한으로 실행'한 뒤 다시 시도하세요."
        );
    }
    let out = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()
        .context("schtasks를 실행할 수 없습니다")?;

    if !out.status.success() {
        bail!("작업 삭제에 실패했습니다:\n{}", decode_console(&out.stderr, &out.stdout));
    }
    Ok(true)
}

/// 작업이 등록되어 있는지 확인한다.
pub fn exists() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 콘솔 출력은 UTF-8이 아닐 수 있다. 깨지더라도 원인은 읽히도록 한다.
fn decode_console(stderr: &[u8], stdout: &[u8]) -> String {
    let pick = if stderr.is_empty() { stdout } else { stderr };
    String::from_utf8_lossy(pick).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml에_실행_경로와_sync_인자가_들어간다() {
        let xml = task_xml(Path::new("C:\\Tools\\drive-archive.exe")).unwrap();
        assert!(xml.contains("<Command>C:\\Tools\\drive-archive.exe</Command>"));
        assert!(xml.contains("<Arguments>sync --drive $(DriveName)</Arguments>"));
    }

    #[test]
    fn xml에_마운트된_볼륨을_넘기는_값_쿼리가_있다() {
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains(r#"<Value name="DriveName">Event/EventData/Data[@Name='DriveName']</Value>"#));
    }

    #[test]
    fn 콘솔_창이_뜨지_않도록_s4u로_실행된다() {
        // InteractiveToken이면 스케줄러가 깨울 때마다 화면에 콘솔 창이 뜬다.
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains("<LogonType>S4U</LogonType>"));
        assert!(!xml.contains("InteractiveToken"));
    }

    #[test]
    fn xml에_마운트_이벤트_트리거가_들어간다() {
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains("Microsoft-Windows-Ntfs"));
        assert!(xml.contains("EventID=98"));
        assert!(xml.contains("<LogonTrigger>"));
    }

    #[test]
    fn 동시_실행을_막도록_설정된다() {
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }

    #[test]
    fn 관리자_권한_없이_실행된다() {
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    }

    #[test]
    fn utf16le는_bom으로_시작한다() {
        let bytes = to_utf16le_bom("AB");
        assert_eq!(bytes, vec![0xFF, 0xFE, 0x41, 0x00, 0x42, 0x00]);
    }

    #[test]
    fn utf16le는_한글도_인코딩한다() {
        let bytes = to_utf16le_bom("한");
        // U+D55C -> LE 바이트 순서는 5C D5
        assert_eq!(bytes, vec![0xFF, 0xFE, 0x5C, 0xD5]);
    }
}
