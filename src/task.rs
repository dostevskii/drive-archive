//! 작업 스케줄러에 이벤트 트리거를 등록한다.
//!
//! 상주 프로그램을 띄우는 대신, Windows가 볼륨 마운트·디스크 도착 이벤트를 기록했을
//! 때만 스케줄러가 이 프로그램을 깨운다. 그래서 평소 리소스 사용량이 0이다.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// 작업 스케줄러에 등록되는 이름.
pub const SYNC_TASK: &str = "drive-archive sync";
pub const SERVE_TASK: &str = "drive-archive serve";

/// 볼륨이 마운트될 때 Windows가 System 로그에 남기는 이벤트.
///
/// 예: `볼륨 J: (\Device\HarddiskVolume27) 정상입니다. 작업이 필요 없습니다.`
/// 섀도 복사본에서도 발생하지만, `sync`가 USB 볼륨만 걸러내므로 문제없다.
///
/// 사실상 NTFS 전용이다. v0.2.0에서는 exFAT에서도 나는 것으로 관찰했으나
/// 2026-09-01 실측으로 뒤집혔다 — exFAT 하드 두 개를 꽂아도 나지 않았다.
/// 그래서 아래 `DISK_EVENT_QUERY`(디스크 도착, 파일 시스템 무관)를 함께 듣는다.
const MOUNT_EVENT_QUERY: &str = "&lt;QueryList&gt;&lt;Query Id=&quot;0&quot; Path=&quot;System&quot;&gt;&lt;Select Path=&quot;System&quot;&gt;*[System[Provider[@Name='Microsoft-Windows-Ntfs'] and EventID=98]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;";

/// USB 디스크가 도착할 때 파티션 관리자가 남기는 이벤트 (Partition/Diagnostic 1006).
///
/// exFAT 하드는 Ntfs 이벤트 98을 내지 않는다 (2026-09-01 실측 — v0.2.0의 관찰이
/// 뒤집혔다). 처음에는 Kernel-PnP 400(장치 구성)을 들었으나, 그 이벤트는 장치를
/// 처음 구성할 때만 나고 알려진 하드를 다시 꽂을 때는 나지 않았다 (2026-09-05 실측).
/// 이 이벤트는 디스크가 붙을 때마다 파일 시스템과 무관하게 기록된다. 내장 디스크나
/// 가상 디스크(부팅·VHD 마운트)에서도 나므로 쿼리에서 `BusType`이 USB(7)인 것만 고른다.
/// 드라이브 문자 대신 디스크 번호(`DiskNumber`)를 주므로, `sync`가 그 번호로
/// 방금 꽂힌 디스크의 볼륨만 찾아 훑는다.
const DISK_EVENT_QUERY: &str = "&lt;QueryList&gt;&lt;Query Id=&quot;0&quot; Path=&quot;Microsoft-Windows-Partition/Diagnostic&quot;&gt;&lt;Select Path=&quot;Microsoft-Windows-Partition/Diagnostic&quot;&gt;*[System[Provider[@Name='Microsoft-Windows-Partition'] and EventID=1006]] and *[EventData[Data[@Name='BusType']='7']]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;";

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
/// 디스크 도착 이벤트는 같은 `DriveName` 자리에 디스크 번호를 넣어 넘기며, `sync`는
/// 숫자를 디스크 번호로 읽어 그 디스크의 볼륨만 본다.
///
/// 인스턴스 정책은 `Queue`다. `IgnoreNew`로 두면 하드 하나를 훑는 동안 도착한 다른 하드의
/// 발화가 버려진다 — 2026-09-05 실측에서 Works A를 훑는 70초 사이에 꽂힌 Works F와 새 SSD가
/// 인덱싱되지 않았다. 줄을 세우면 차례로 돈다. 같은 하드의 중복 발화(NTFS는 98과 1006이
/// 둘 다 난다)는 뒤의 인스턴스가 120초 쿨다운에 걸려 건너뛴다.
fn task_xml(exe: &Path) -> Result<String> {
    let user = std::env::var("USERNAME").context("Could not read the USERNAME environment variable")?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".to_string());
    let exe = exe.display().to_string();

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>외장하드가 연결되면 drive-archive 인덱스를 갱신합니다. 평소에는 실행되지 않습니다.</Description>
    <URI>\{SYNC_TASK}</URI>
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
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>{DISK_EVENT_QUERY}</Subscription>
      <Delay>PT15S</Delay>
      <ValueQueries>
        <Value name="DriveName">Event/EventData/Data[@Name='DiskNumber']</Value>
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
    <MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>
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

/// 웹 화면을 로그온할 때 띄우는 작업.
///
/// sync 작업과 세 곳이 다르다. 트리거는 로그온뿐이고(하드를 꽂았다고 서버를 다시
/// 띄울 이유가 없다), 인자는 `serve --no-open`이며(로그온마다 브라우저가 뜨면 안
/// 된다), `ExecutionTimeLimit`이 `PT0S`다 — sync의 `PT4H`를 그대로 쓰면 서버가
/// 네 시간 뒤에 죽는다.
fn serve_task_xml(exe: &Path) -> Result<String> {
    let user = std::env::var("USERNAME").context("Could not read the USERNAME environment variable")?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".to_string());
    let exe = exe.display().to_string();

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>drive-archive 웹 화면을 띄웁니다. 컴퓨터가 켜져 있는 동안에만 접속할 수 있습니다.</Description>
    <URI>\{SERVE_TASK}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Delay>PT30S</Delay>
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
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>serve --no-open</Arguments>
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
    std::env::current_exe().context("Could not determine the executable's path")
}

/// 작업 하나를 등록한다. 이미 있으면 새 정의로 덮어쓴다.
fn register(name: &str, xml: &str) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("{}.xml", name.replace(' ', "-")));
    std::fs::write(&tmp, to_utf16le_bom(xml))
        .with_context(|| format!("Could not write task definition: {}", tmp.display()))?;

    let out = Command::new("schtasks")
        .args(["/Create", "/TN", name, "/XML"])
        .arg(&tmp)
        .arg("/F")
        .output()
        .context("Could not run schtasks")?;

    let _ = std::fs::remove_file(&tmp);

    if !out.status.success() {
        bail!(
            "Failed to register {name}:\n{}",
            decode_console(&out.stderr, &out.stdout)
        );
    }
    Ok(())
}

fn delete(name: &str) -> Result<bool> {
    if !exists_named(name) {
        return Ok(false);
    }
    let out = Command::new("schtasks")
        .args(["/Delete", "/TN", name, "/F"])
        .output()
        .context("Could not run schtasks")?;
    if !out.status.success() {
        bail!(
            "Failed to delete {name}:\n{}",
            decode_console(&out.stderr, &out.stdout)
        );
    }
    Ok(true)
}

fn exists_named(name: &str) -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_named(name: &str) -> Result<()> {
    let out = Command::new("schtasks")
        .args(["/Run", "/TN", name])
        .output()
        .context("Could not run schtasks")?;
    if !out.status.success() {
        bail!("Could not start {name} task:\n{}", decode_console(&out.stderr, &out.stdout));
    }
    Ok(())
}

/// 자동 인덱싱과 웹 화면 작업을 함께 등록한다.
pub fn setup(exe: &Path) -> Result<()> {
    if !crate::elevation::is_elevated() {
        bail!(
            "Administrator privileges are required to register with Task Scheduler.\n\
             Right-click PowerShell and choose 'Run as administrator', then try again."
        );
    }
    register(SYNC_TASK, &task_xml(exe)?)?;
    register(SERVE_TASK, &serve_task_xml(exe)?)?;
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
    run_named(SYNC_TASK)
}

/// 등록 직후 웹 화면을 바로 띄울 때 쓴다. 다음 로그온을 기다리게 하지 않는다.
pub fn run_serve_now() -> Result<()> {
    run_named(SERVE_TASK)
}

/// 등록한 작업을 모두 제거한다. 하나라도 지웠으면 참이다.
pub fn remove() -> Result<bool> {
    if !exists_named(SYNC_TASK) && !exists_named(SERVE_TASK) {
        return Ok(false);
    }
    if !crate::elevation::is_elevated() {
        bail!(
            "Administrator privileges are required to remove the Task Scheduler registration.\n\
             Run PowerShell 'as administrator', then try again."
        );
    }
    let a = delete(SYNC_TASK)?;
    let b = delete(SERVE_TASK)?;
    Ok(a || b)
}

/// 작업이 등록되어 있는지 확인한다.
pub fn exists() -> bool {
    exists_named(SYNC_TASK)
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
    fn 디스크_도착_트리거가_함께_들어간다() {
        // exFAT 하드는 Ntfs 이벤트 98을 내지 않아, 파일 시스템을 가리지 않는
        // 디스크 도착 이벤트를 함께 들어야 모든 포맷이 자동으로 인덱싱된다.
        // USB 디스크만 고르고, 드라이브 문자 대신 디스크 번호를 넘긴다.
        let xml = task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert_eq!(xml.matches("<EventTrigger>").count(), 2, "{xml}");
        assert!(xml.contains("Microsoft-Windows-Partition/Diagnostic"));
        assert!(xml.contains("EventID=1006"));
        assert!(xml.contains("Data[@Name='BusType']='7'"));
        assert!(xml.contains(r#"<Value name="DriveName">Event/EventData/Data[@Name='DiskNumber']</Value>"#));
        assert!(!xml.contains("Kernel-PnP"));
    }

    #[test]
    fn xml에_마운트_이벤트_트리거가_들어간다() {
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains("Microsoft-Windows-Ntfs"));
        assert!(xml.contains("EventID=98"));
        assert!(xml.contains("<LogonTrigger>"));
    }

    #[test]
    fn 동시에_뜨지_않고_늦은_발화는_줄을_선다() {
        // IgnoreNew면 한 하드를 훑는 동안 꽂은 다른 하드의 발화가 버려진다 (2026-09-05 실측).
        let xml = task_xml(Path::new("x.exe")).unwrap();
        assert!(xml.contains("<MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>"));
        assert!(!xml.contains("IgnoreNew"));
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

    #[test]
    fn serve_작업은_로그온에_뜨고_시간_제한이_없다() {
        let xml = serve_task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert!(xml.contains("<LogonTrigger>"));
        // sync의 PT4H를 그대로 쓰면 서버가 4시간 뒤에 죽는다.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"), "{xml}");
        assert!(!xml.contains("PT4H"));
    }

    #[test]
    fn serve_작업은_마운트_이벤트를_듣지_않는다() {
        // 하드를 꽂았다고 웹 서버를 다시 띄울 이유가 없다.
        let xml = serve_task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert!(!xml.contains("EventTrigger"));
    }

    #[test]
    fn serve_작업은_창_없이_실행된다() {
        let xml = serve_task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert!(xml.contains("<LogonType>S4U</LogonType>"));
        assert!(xml.contains("<Hidden>true</Hidden>"));
    }

    #[test]
    fn serve_작업은_브라우저를_열지_않는다() {
        // 로그온할 때마다 브라우저가 뜨면 안 된다.
        let xml = serve_task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert!(xml.contains("<Arguments>serve --no-open</Arguments>"), "{xml}");
    }

    #[test]
    fn 두_작업의_이름이_다르다() {
        assert_ne!(SYNC_TASK, SERVE_TASK);
        let sync = task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        let serve = serve_task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert!(sync.contains(&format!("<URI>\\{SYNC_TASK}</URI>")));
        assert!(serve.contains(&format!("<URI>\\{SERVE_TASK}</URI>")));
    }

    #[test]
    fn serve_작업도_한_번만_돈다() {
        // 두 개가 뜨면 뒤엣것이 포트를 못 잡는다.
        let xml = serve_task_xml(Path::new(r"C:\bin\drive-archive.exe")).unwrap();
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }
}
