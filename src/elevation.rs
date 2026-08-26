//! 관리자 권한 확인과 UAC 상승 재실행.
//!
//! 작업 스케줄러에 작업을 등록하려면 관리자 권한이 필요하다.
//! 검색이나 스캔에는 필요 없으므로, 설치할 때 한 번만 쓴다.

use anyhow::{Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

/// 이 프로세스가 관리자 권한으로 실행되고 있는지 확인한다.
///
/// 관리자 계정이라도 UAC 때문에 평소에는 권한이 걸러진 토큰으로 실행된다.
/// 계정이 관리자인지가 아니라, 지금 권한이 있는지를 본다.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok()
    };

    unsafe {
        let _ = CloseHandle(token);
    }

    ok && elevation.TokenIsElevated != 0
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 같은 프로그램을 관리자 권한으로 다시 실행한다.
///
/// UAC 동의 창이 뜨고, 사용자가 승인하면 새 프로세스가 시작된다.
/// 이 함수는 새 프로세스를 기다리지 않는다. 호출한 쪽은 곧바로 종료해야 한다.
pub fn relaunch_as_admin(args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_w = wide(&exe.display().to_string());
    let args_w = wide(&args.join(" "));
    let verb_w = wide("runas");

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW는 성공하면 32보다 큰 값을 돌려준다. 오래된 API의 관례다.
    if result.0 as usize <= 32 {
        bail!("관리자 권한으로 다시 실행하지 못했습니다. UAC 창에서 취소했을 수 있습니다.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 권한_확인이_예외없이_끝난다() {
        // 결과는 실행 환경에 따라 다르다. 호출 자체가 안전한지만 본다.
        let _ = is_elevated();
    }
}
