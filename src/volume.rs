//! 연결된 외장 NTFS 볼륨을 찾아 식별한다.
//!
//! 드라이브 문자는 연결 순서에 따라 바뀌므로 식별자로 쓸 수 없다.
//! NTFS 볼륨 시리얼 번호를 안정적인 식별자로 사용하고,
//! 볼륨 라벨은 사용자가 물리 라벨과 대조하는 이름으로만 쓴다.

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BusTypeUsb, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows::core::PCWSTR;

/// `GetDriveTypeW` 반환값: 이동식 저장소(USB 메모리, 일부 외장하드).
const DRIVE_REMOVABLE: u32 = 2;
/// `GetDriveTypeW` 반환값: 고정 디스크. USB 외장하드 대부분이 여기에 해당한다.
const DRIVE_FIXED: u32 = 3;

/// 연결되어 있는 외장 NTFS 볼륨 하나.
#[derive(Debug, Clone, Serialize)]
pub struct Volume {
    /// 드라이브 문자 하나 (예: `'E'`). 연결 때마다 바뀔 수 있다.
    pub letter: char,
    /// NTFS 볼륨 시리얼 번호를 8자리 대문자 16진수로 표기한 것. 하드의 고유 식별자.
    pub serial: String,
    /// 볼륨 라벨. 비어 있으면 `(라벨 없음)`으로 채운다.
    pub label: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl Volume {
    /// 스캔 시작점이 되는 루트 경로 (예: `E:\`).
    pub fn root_path(&self) -> String {
        format!("{}:\\", self.letter)
    }
}

/// 널 종료 UTF-16 문자열로 변환한다. Win32 문자열 인자에 쓴다.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 널 종료 UTF-16 버퍼에서 문자열을 읽어낸다.
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 현재 시스템에 존재하는 드라이브 문자를 모두 반환한다.
fn logical_drive_letters() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| (b'A' + i as u8) as char)
        .collect()
}

/// 볼륨의 라벨·시리얼·파일시스템 이름을 읽는다.
///
/// 미디어가 없는 드라이브(빈 카드리더 등)에서는 실패하므로 `None`을 돌려준다.
fn volume_info(letter: char) -> Option<(String, String, String)> {
    let root = wide(&format!("{letter}:\\"));
    let mut name_buf = [0u16; 256];
    let mut fs_buf = [0u16; 64];
    let mut serial: u32 = 0;

    unsafe {
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            Some(&mut name_buf),
            Some(&mut serial),
            None,
            None,
            Some(&mut fs_buf),
        )
        .ok()?;
    }

    let label = from_wide(&name_buf);
    let fs = from_wide(&fs_buf);
    Some((format!("{serial:08X}"), label, fs))
}

/// 볼륨의 전체 용량과 여유 공간을 바이트 단위로 읽는다.
fn disk_space(letter: char) -> (u64, u64) {
    let root = wide(&format!("{letter}:\\"));
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    unsafe {
        let _ = GetDiskFreeSpaceExW(PCWSTR(root.as_ptr()), None, Some(&mut total), Some(&mut free));
    }
    (total, free)
}

/// 볼륨이 USB로 연결되어 있는지 확인한다.
///
/// `IOCTL_STORAGE_QUERY_PROPERTY`는 접근 권한 0으로 연 핸들에서도 동작하므로
/// 관리자 권한이 필요 없다.
fn is_usb(letter: char) -> bool {
    let path = wide(&format!("\\\\.\\{letter}:"));

    let handle: HANDLE = match unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0, // 접근 권한 없음 - 메타데이터 조회만 하므로 충분하다
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return false,
    };

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // 서술자 뒤에 가변 길이 문자열이 붙으므로 넉넉한 버퍼를 쓴다.
    let mut buf = [0u8; 1024];
    let mut returned: u32 = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
        .is_ok()
    };

    unsafe {
        let _ = CloseHandle(handle);
    }

    if !ok || (returned as usize) < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return false;
    }

    let desc = unsafe { &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    desc.BusType == BusTypeUsb
}

/// 지금 연결된 외장 NTFS 볼륨을 모두 찾는다.
///
/// USB로 연결된 NTFS 볼륨만 반환한다. 내장 디스크, 네트워크 드라이브,
/// NTFS가 아닌 볼륨(exFAT, FAT32 등)은 제외된다.
pub fn list_external_volumes() -> Vec<Volume> {
    let mut found = Vec::new();

    for letter in logical_drive_letters() {
        let root = wide(&format!("{letter}:\\"));
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
        if drive_type != DRIVE_FIXED && drive_type != DRIVE_REMOVABLE {
            continue;
        }

        let Some((serial, label, fs)) = volume_info(letter) else {
            continue;
        };
        if !fs.eq_ignore_ascii_case("NTFS") {
            continue;
        }
        if !is_usb(letter) {
            continue;
        }

        let (total_bytes, free_bytes) = disk_space(letter);
        found.push(Volume {
            letter,
            serial,
            label: if label.is_empty() {
                "(라벨 없음)".to_string()
            } else {
                label
            },
            total_bytes,
            free_bytes,
        });
    }

    found
}

/// 특정 드라이브 문자를 외장 NTFS 볼륨으로 확인하고 정보를 읽는다.
///
/// `scan E:` 처럼 사용자가 드라이브를 직접 지정했을 때 쓴다.
pub fn volume_at(letter: char) -> Result<Volume> {
    let letter = letter.to_ascii_uppercase();
    list_external_volumes()
        .into_iter()
        .find(|v| v.letter == letter)
        .with_context(|| {
            format!("{letter}: 드라이브는 연결된 외장 NTFS 볼륨이 아닙니다. `drive-archive drives`로 목록을 확인하세요.")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_는_널_종료_문자열을_만든다() {
        assert_eq!(wide("E:"), vec![69, 58, 0]);
    }

    #[test]
    fn from_wide_는_널_앞까지만_읽는다() {
        let buf = [0xD55Cu16, 0xAE00, 0, 0x41, 0x42];
        assert_eq!(from_wide(&buf), "한글");
    }

    #[test]
    fn from_wide_는_널이_없어도_전체를_읽는다() {
        assert_eq!(from_wide(&[0x41u16, 0x42]), "AB");
    }

    #[test]
    fn 논리_드라이브에는_시스템_드라이브가_포함된다() {
        // 어떤 Windows 시스템에도 부팅 드라이브는 존재한다.
        assert!(!logical_drive_letters().is_empty());
    }
}
