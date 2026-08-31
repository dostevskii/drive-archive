//! 웹 화면의 비밀번호와 세션.
//!
//! 서버는 127.0.0.1에만 뜨지만, 로컬에서 볼 때도 터널을 거치므로 서버 입장에서
//! 외부 접속과 로컬 접속은 구별되지 않는다. 그래서 접속 주소로 신뢰를 판단하지
//! 않고 모든 요청에 인증을 요구한다.
//!
//! 비밀번호는 인덱스 DB가 아니라 `auth.json`에 따로 둔다. 인덱스는 스캔이 통째로
//! 갈아엎고 `forget`으로 지워지는 데이터라 인증 정보와 수명이 다르다.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

/// 비밀번호 해시가 놓이는 파일 이름.
const AUTH_FILE: &str = "auth.json";

/// 암호학적 난수를 얻는다. salt와 세션 토큰에 쓴다.
///
/// `BCryptGenRandom`을 쓰는 것은 크레이트를 늘리지 않기 위해서다. `windows`
/// 크레이트는 이미 들어와 있어 feature 하나만 켜면 된다.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut buf = vec![0u8; n];
    unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .context("난수를 얻지 못했습니다")?;
    Ok(buf)
}

fn auth_path(dir: &Path) -> PathBuf {
    dir.join(AUTH_FILE)
}

/// 비밀번호를 argon2id로 해시해 저장한다. 이미 있으면 덮어쓴다.
pub fn set_password_at(dir: &Path, password: &str) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("폴더를 만들 수 없습니다: {}", dir.display()))?;

    let salt_bytes = random_bytes(16)?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| anyhow::anyhow!("salt를 만들 수 없습니다: {e}"))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("비밀번호를 해시하지 못했습니다: {e}"))?
        .to_string();

    let body = serde_json::json!({ "hash": hash }).to_string();
    std::fs::write(auth_path(dir), body)
        .with_context(|| format!("비밀번호를 저장할 수 없습니다: {}", dir.display()))?;
    Ok(())
}

/// 비밀번호가 맞는지 본다. 설정되어 있지 않으면 거짓이다.
pub fn verify_at(dir: &Path, password: &str) -> Result<bool> {
    let Ok(body) = std::fs::read_to_string(auth_path(dir)) else {
        return Ok(false);
    };
    let stored: serde_json::Value =
        serde_json::from_str(&body).context("auth.json을 읽을 수 없습니다")?;
    let Some(hash) = stored.get("hash").and_then(|h| h.as_str()) else {
        return Ok(false);
    };
    let parsed = PasswordHash::new(hash)
        .map_err(|e| anyhow::anyhow!("저장된 해시가 깨졌습니다: {e}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// 비밀번호가 설정되어 있는가.
pub fn is_configured_at(dir: &Path) -> bool {
    std::fs::read_to_string(auth_path(dir))
        .ok()
        .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("hash").and_then(|h| h.as_str()).map(str::to_string))
        .is_some_and(|h| !h.is_empty())
}

pub fn set_password(password: &str) -> Result<()> {
    set_password_at(&crate::db::data_dir()?, password)
}

pub fn verify(password: &str) -> Result<bool> {
    verify_at(&crate::db::data_dir()?, password)
}

pub fn is_configured() -> bool {
    crate::db::data_dir().is_ok_and(|d| is_configured_at(&d))
}

/// 세션이 살아 있는 시간. 쓰는 동안에는 계속 이만큼씩 밀린다.
pub const SESSION_SECS: u64 = 86_400;

/// 발급한 세션과 각각의 만료 시각.
///
/// 메모리에만 둔다. 서버를 다시 띄우면 전부 무효가 되는데, 단순하고 오히려 안전하다.
#[derive(Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, SystemTime>>,
}

impl Sessions {
    /// 새 토큰을 발급한다. 32바이트 난수를 16진수로 적는다.
    pub fn issue(&self, now: SystemTime) -> Result<String> {
        let token = random_bytes(32)?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut live = self.live.lock().unwrap();
        live.insert(token.clone(), now + Duration::from_secs(SESSION_SECS));
        Ok(token)
    }

    /// 유효한 토큰인가. 유효하면 만료를 24시간 뒤로 다시 잡는다.
    ///
    /// 만료된 것은 이 자리에서 지운다. 따로 청소하는 사람이 없으면 오래 켜 둔
    /// 컴퓨터에서 죽은 세션이 쌓이기만 한다.
    pub fn check(&self, token: &str, now: SystemTime) -> bool {
        if token.is_empty() {
            return false;
        }
        let mut live = self.live.lock().unwrap();
        live.retain(|_, expires| *expires > now);
        match live.get_mut(token) {
            Some(expires) => {
                *expires = now + Duration::from_secs(SESSION_SECS);
                true
            }
            None => false,
        }
    }

    /// 살아 있는 세션 수. 테스트에서 쓴다.
    pub fn len(&self) -> usize {
        self.live.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 난수는_매번_다르다() {
        let a = random_bytes(32).unwrap();
        let b = random_bytes(32).unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "같은 값이 두 번 나왔다면 난수원이 고장 난 것이다");
    }

    #[test]
    fn 저장한_비밀번호로_통과한다() {
        let dir = tempfile::tempdir().unwrap();
        set_password_at(dir.path(), "정확한비밀번호").unwrap();
        assert!(verify_at(dir.path(), "정확한비밀번호").unwrap());
    }

    #[test]
    fn 다른_비밀번호는_막힌다() {
        let dir = tempfile::tempdir().unwrap();
        set_password_at(dir.path(), "정확한비밀번호").unwrap();
        assert!(!verify_at(dir.path(), "틀린비밀번호").unwrap());
        assert!(!verify_at(dir.path(), "").unwrap());
    }

    #[test]
    fn 설정하지_않았으면_통과시키지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_configured_at(dir.path()));
        // 파일이 없다고 해서 아무나 통과시키면 안 된다.
        assert!(!verify_at(dir.path(), "아무거나").unwrap());
    }

    #[test]
    fn 다시_설정하면_옛_비밀번호는_막힌다() {
        let dir = tempfile::tempdir().unwrap();
        set_password_at(dir.path(), "첫번째비밀번호").unwrap();
        set_password_at(dir.path(), "두번째비밀번호").unwrap();
        assert!(!verify_at(dir.path(), "첫번째비밀번호").unwrap());
        assert!(verify_at(dir.path(), "두번째비밀번호").unwrap());
    }

    #[test]
    fn 같은_비밀번호도_해시가_다르다() {
        // salt가 매번 새로 나와야 한다. 같으면 salt를 안 쓰고 있는 것이다.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        set_password_at(a.path(), "같은값입니다").unwrap();
        set_password_at(b.path(), "같은값입니다").unwrap();
        let ha = std::fs::read_to_string(a.path().join("auth.json")).unwrap();
        let hb = std::fs::read_to_string(b.path().join("auth.json")).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn 발급한_토큰은_통과한다() {
        let s = Sessions::default();
        let now = SystemTime::UNIX_EPOCH;
        let t = s.issue(now).unwrap();
        assert!(s.check(&t, now));
    }

    #[test]
    fn 모르는_토큰은_막는다() {
        let s = Sessions::default();
        let now = SystemTime::UNIX_EPOCH;
        s.issue(now).unwrap();
        assert!(!s.check("아무거나", now));
        assert!(!s.check("", now));
    }

    #[test]
    fn 하루가_지나면_만료된다() {
        let s = Sessions::default();
        let now = SystemTime::UNIX_EPOCH;
        let t = s.issue(now).unwrap();
        let 하루뒤 = now + Duration::from_secs(SESSION_SECS + 1);
        assert!(!s.check(&t, 하루뒤));
    }

    #[test]
    fn 쓰는_동안_만료가_밀린다() {
        // 23시간마다 한 번씩 들어오면 사흘이 지나도 살아 있어야 한다.
        let s = Sessions::default();
        let mut now = SystemTime::UNIX_EPOCH;
        let t = s.issue(now).unwrap();
        for _ in 0..3 {
            now += Duration::from_secs(23 * 3600);
            assert!(s.check(&t, now), "23시간 간격이면 연장되어야 한다");
        }
        // 그러다 하루를 통째로 쉬면 끊긴다.
        now += Duration::from_secs(SESSION_SECS + 1);
        assert!(!s.check(&t, now));
    }

    #[test]
    fn 토큰은_매번_다르다() {
        let s = Sessions::default();
        let now = SystemTime::UNIX_EPOCH;
        let a = s.issue(now).unwrap();
        let b = s.issue(now).unwrap();
        assert_ne!(a, b);
        assert!(a.len() >= 32, "토큰이 짧으면 찍어 맞힐 수 있다");
    }

    #[test]
    fn 만료된_토큰은_보관하지_않는다() {
        // 세션이 쌓이기만 하면 오래 켜 둔 컴퓨터에서 메모리가 는다.
        let s = Sessions::default();
        let now = SystemTime::UNIX_EPOCH;
        let t = s.issue(now).unwrap();
        let 하루뒤 = now + Duration::from_secs(SESSION_SECS + 1);
        s.check(&t, 하루뒤);
        assert_eq!(s.len(), 0);
    }
}
