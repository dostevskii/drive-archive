//! 사용자 PATH(`HKCU\Environment`의 `Path` 값)에 설치 폴더를 등록/제거한다.
//!
//! `setx`나 PowerShell의 `[Environment]::SetEnvironmentVariable`를 쓰지 않는다. 둘 다
//! `Path` 값을 항상 `REG_SZ`로 다시 쓰기 때문에, 기존 값이 `REG_EXPAND_SZ`였다면
//! (`%SystemRoot%\...`처럼 展開이 필요한 항목이 하나라도 있으면) 그 항목들이 더 이상
//! 展開되지 않는 문자 그대로의 문자열로 굳어버린다. 레지스트리를 직접 읽고 써서
//! 원래 있던 타입을 그대로 유지한다.

use anyhow::{Context, Result};
use std::path::Path;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
    REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows::core::PCWSTR;

const ENV_KEY: &str = "Environment";
const PATH_VALUE: &str = "Path";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---------------------------------------------------------- 순수 문자열 로직

/// `path_value`(세미콜론으로 구분된 PATH 문자열)에 `dir`이 이미 들어 있는지 본다.
///
/// 대소문자를 구분하지 않고, 끝의 백슬래시 유무도 무시한다. 빈 항목(연속된 `;`에서
/// 생기는 것)은 비교 대상에서 뺀다.
fn contains_dir(path_value: &str, dir: &str) -> bool {
    let target = normalize(dir);
    path_value.split(';').map(str::trim).filter(|s| !s.is_empty()).any(|entry| normalize(entry) == target)
}

fn normalize(entry: &str) -> String {
    entry.trim().trim_end_matches('\\').to_lowercase()
}

/// `dir`을 PATH 끝에 추가한다.
///
/// 값이 비어 있으면 `dir` 하나만 남기고, 이미 `;`로 끝나 있으면 세미콜론을 더
/// 붙이지 않는다 — 그러지 않으면 빈 항목(`;;`)이 새로 생긴다.
fn append_dir(path_value: &str, dir: &str) -> String {
    if path_value.is_empty() {
        dir.to_string()
    } else if path_value.ends_with(';') {
        format!("{path_value}{dir}")
    } else {
        format!("{path_value};{dir}")
    }
}

/// `path_value`에서 `dir`과 일치하는 항목만 제거한다.
///
/// 나머지 항목은 순서와 내용을 그대로 두므로 `%VAR%` 같은 항목도 건드리지 않는다.
/// 빈 항목은 결과에 남기지 않으므로 선행/후행/중복 `;`가 생기지 않는다.
fn remove_dir_entry(path_value: &str, dir: &str) -> String {
    let target = normalize(dir);
    path_value
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .filter(|entry| normalize(entry) != target)
        .collect::<Vec<_>>()
        .join(";")
}

// -------------------------------------------------------------- 레지스트리 I/O

/// `HKCU\Environment`를 열거나(없으면) 만든다. 스코프를 벗어나면 자동으로 닫는다.
struct EnvKey(HKEY);

impl EnvKey {
    fn open() -> Result<Self> {
        let subkey = wide(ENV_KEY);
        let mut hkey = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
        }
        .ok()
        .context("Could not open HKCU\\Environment")?;
        Ok(Self(hkey))
    }
}

impl Drop for EnvKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// `Path` 값을 (타입, 문자열)로 읽는다. 값이 아예 없으면 `None`.
fn query_path(key: &EnvKey) -> Result<Option<(REG_VALUE_TYPE, String)>> {
    let name = wide(PATH_VALUE);
    let mut value_type = REG_VALUE_TYPE::default();
    let mut size: u32 = 0;

    let err =
        unsafe { RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, Some(&mut value_type), None, Some(&mut size)) };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    err.ok().context("Could not query the size of the Path value")?;

    if size == 0 {
        return Ok(Some((value_type, String::new())));
    }

    let mut buf = vec![0u8; size as usize];
    unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
    }
    .ok()
    .context("Could not read the Path value")?;

    // UTF-16LE 바이트를 코드 유닛으로 재조립한 뒤, 저장되어 있을 수도 없을 수도 있는
    // 끝의 NUL을 from_wide가 알아서 잘라낸다.
    let units: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Ok(Some((value_type, from_wide(&units))))
}

/// `Path` 값을 쓴다. `value_type`으로 원래 타입(또는 새로 만드는 경우 `REG_EXPAND_SZ`)을 유지한다.
fn write_path(key: &EnvKey, value_type: REG_VALUE_TYPE, value: &str) -> Result<()> {
    let name = wide(PATH_VALUE);
    // REG_SZ/REG_EXPAND_SZ는 끝에 NUL이 있는 것이 관례이므로 wide()가 붙인 것을 그대로 쓴다.
    let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();

    unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, value_type, Some(&data)) }
        .ok()
        .context("Could not write the Path value")
}

/// 새 터미널이 아니라도 탐색기가 띄우는 새 프로세스가 바뀐 환경 변수를 보도록 알린다.
///
/// 실패해도 무시한다 — PATH 자체는 이미 레지스트리에 반영되었고, 이 알림은 편의일 뿐이다.
fn broadcast_env_change() {
    let env = wide(ENV_KEY);
    let mut result: usize = 0;
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(env.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            Some(&mut result),
        );
    }
}

/// 사용자 PATH에 `dir`을 추가한다. 이미 있으면 손대지 않고 `false`를 돌려준다.
pub fn add(dir: &Path) -> Result<bool> {
    let dir = dir.display().to_string();
    let key = EnvKey::open()?;

    let (value_type, current) = query_path(&key)?.unwrap_or((REG_EXPAND_SZ, String::new()));
    if contains_dir(&current, &dir) {
        return Ok(false);
    }

    write_path(&key, value_type, &append_dir(&current, &dir))?;
    broadcast_env_change();
    Ok(true)
}

/// 사용자 PATH에서 `dir`을 제거한다. 없었으면 손대지 않고 `false`를 돌려준다.
pub fn remove(dir: &Path) -> Result<bool> {
    let dir = dir.display().to_string();
    let key = EnvKey::open()?;

    let Some((value_type, current)) = query_path(&key)? else {
        return Ok(false);
    };
    if !contains_dir(&current, &dir) {
        return Ok(false);
    }

    write_path(&key, value_type, &remove_dir_entry(&current, &dir))?;
    broadcast_env_change();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 대소문자와_트레일링_백슬래시를_무시하고_존재를_확인한다() {
        let path = r"C:\Tools\drive-archive;C:\Windows\System32";
        assert!(contains_dir(path, r"c:\tools\drive-archive"));
        assert!(contains_dir(path, r"C:\Tools\drive-archive\"));
        assert!(!contains_dir(path, r"C:\Tools\other"));
    }

    #[test]
    fn 빈_path에_추가하면_그_경로만_남는다() {
        assert_eq!(append_dir("", r"C:\Tools\drive-archive"), r"C:\Tools\drive-archive");
    }

    #[test]
    fn 트레일링_세미콜론_뒤에_추가해도_이중_세미콜론이_생기지_않는다() {
        assert_eq!(append_dir(r"C:\a;", r"C:\b"), r"C:\a;C:\b");
        assert_eq!(append_dir(r"C:\a", r"C:\b"), r"C:\a;C:\b");
    }

    #[test]
    fn 중간_항목을_제거하면_나머지가_남는다() {
        let path = r"C:\a;C:\Tools\drive-archive;C:\b";
        assert_eq!(remove_dir_entry(path, r"C:\Tools\drive-archive"), r"C:\a;C:\b");
    }

    #[test]
    fn var_항목은_건드리지_않는다() {
        // %VAR% 형태는 값이 아니라 문자 그대로 비교되어야 하고, 지우는 대상이
        // 아니면 손대지 않아야 한다.
        let path = r"%SystemRoot%\system32;C:\Tools\drive-archive;%SystemRoot%";
        assert_eq!(remove_dir_entry(path, r"C:\Tools\drive-archive"), r"%SystemRoot%\system32;%SystemRoot%");
    }

    #[test]
    fn 없는_항목을_제거하면_원본_그대로다() {
        let path = r"C:\a;C:\b";
        assert_eq!(remove_dir_entry(path, r"C:\Tools\drive-archive"), path);
    }

    #[test]
    fn 빈_항목이_섞여_있어도_제거_후_정리된다() {
        // 사람이 손으로 PATH를 고치면 ;;가 남기도 한다. 제거하는 김에 정리된다.
        let path = r";C:\a;;C:\Tools\drive-archive;";
        assert_eq!(remove_dir_entry(path, r"C:\Tools\drive-archive"), r"C:\a");
    }
}
