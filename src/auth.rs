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

/// 한 주소가 이만큼 틀리면 잠근다.
const IP_LIMIT: u32 = 5;
/// 주소를 가리지 않고 이만큼 쌓이면 전부 잠근다.
const GLOBAL_LIMIT: u32 = 20;
/// 잠가 두는 시간.
const LOCK_SECS: u64 = 60;

/// 전역 카운터가 기억을 지우기까지의 조용한 시간.
///
/// 잠금 시간(`LOCK_SECS`)과 같게 두면 "20회 몰아치기 → 잠금 해제까지 대기 → 반복"으로
/// 카운터가 매 사이클 새것이 되어, 주소를 지어내는 공격자가 전역 잠금을 사실상
/// 우회한다. 리셋 창을 잠금보다 훨씬 길게 두면 기록이 남아 있는 동안에는 실패
/// 한 번에 곧바로 다시 잠기므로, 어떤 속도로 두드리든 분당 한 번 수준으로 눌린다.
const GLOBAL_RESET_SECS: u64 = 600;

/// 로그인 실패를 세어 무차별 대입을 막는다.
///
/// argon2 검증 자체가 느려 초당 수십 회가 한계지만, 밖에 열리는 이상 잠금이 있어야 한다.
#[derive(Default)]
pub struct Gate {
    /// 주소별 (실패 횟수, 마지막 실패 시각)
    per_ip: Mutex<HashMap<String, (u32, SystemTime)>>,
    /// 주소를 가리지 않은 (실패 횟수, 마지막 실패 시각).
    /// `X-Forwarded-For`를 지어내 주소별 카운터를 피해 가는 경우를 막는다.
    global: Mutex<Option<(u32, SystemTime)>>,
}

/// 마지막 실패로부터 얼마 지났는지 보고, 잠금 시간이 남았으면 그 초를 준다.
fn lock_remaining(at: SystemTime, now: SystemTime) -> Option<u64> {
    let passed = now.duration_since(at).unwrap_or_default().as_secs();
    (passed <= LOCK_SECS).then(|| LOCK_SECS - passed)
}

impl Gate {
    pub fn note_failure(&self, ip: &str, now: SystemTime) {
        {
            let mut per_ip = self.per_ip.lock().unwrap();
            let e = per_ip.entry(ip.to_string()).or_insert((0, now));
            // 잠금 시간이 지났으면 처음부터 다시 센다.
            if lock_remaining(e.1, now).is_none() {
                *e = (0, now);
            }
            e.0 += 1;
            e.1 = now;
        }

        let mut g = self.global.lock().unwrap();
        let cur = match *g {
            Some((n, at))
                if now.duration_since(at).unwrap_or_default().as_secs() <= GLOBAL_RESET_SECS =>
            {
                n
            }
            _ => 0,
        };
        *g = Some((cur + 1, now));
    }

    pub fn note_success(&self, ip: &str) {
        self.per_ip.lock().unwrap().remove(ip);
    }

    /// 잠겨 있으면 남은 초를 준다.
    pub fn locked_for(&self, ip: &str, now: SystemTime) -> Option<u64> {
        if let Some((n, at)) = *self.global.lock().unwrap() {
            if n >= GLOBAL_LIMIT {
                if let Some(left) = lock_remaining(at, now) {
                    return Some(left);
                }
            }
        }

        let per_ip = self.per_ip.lock().unwrap();
        let &(n, at) = per_ip.get(ip)?;
        if n < IP_LIMIT {
            return None;
        }
        lock_remaining(at, now)
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

    #[test]
    fn 다섯_번_틀리면_그_주소가_잠긴다() {
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for _ in 0..4 {
            g.note_failure("1.1.1.1", now);
        }
        assert_eq!(g.locked_for("1.1.1.1", now), None, "4회까지는 열려 있다");
        g.note_failure("1.1.1.1", now);
        assert_eq!(g.locked_for("1.1.1.1", now), Some(LOCK_SECS));
    }

    #[test]
    fn 잠금은_시간이_지나면_풀린다() {
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for _ in 0..5 {
            g.note_failure("1.1.1.1", now);
        }
        let 나중 = now + Duration::from_secs(LOCK_SECS + 1);
        assert_eq!(g.locked_for("1.1.1.1", 나중), None);
    }

    #[test]
    fn 성공하면_카운터가_지워진다() {
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for _ in 0..4 {
            g.note_failure("1.1.1.1", now);
        }
        g.note_success("1.1.1.1");
        for _ in 0..4 {
            g.note_failure("1.1.1.1", now);
        }
        assert_eq!(g.locked_for("1.1.1.1", now), None, "성공이 카운터를 지웠어야 한다");
    }

    #[test]
    fn 다른_주소는_말려들지_않는다() {
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for _ in 0..5 {
            g.note_failure("1.1.1.1", now);
        }
        assert!(g.locked_for("1.1.1.1", now).is_some());
        assert_eq!(g.locked_for("2.2.2.2", now), None);
    }

    #[test]
    fn 주소를_매번_바꿔도_전역_카운터에_걸린다() {
        // X-Forwarded-For는 보내는 쪽이 지어낼 수 있다. 주소별 카운터만 두면
        // 매 요청마다 다른 값을 적어 잠금을 통째로 피해 간다.
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for i in 0..GLOBAL_LIMIT {
            g.note_failure(&format!("10.0.0.{i}"), now);
        }
        assert_eq!(
            g.locked_for("처음보는주소", now),
            Some(LOCK_SECS),
            "전역 카운터가 한도에 닿으면 처음 보는 주소도 막아야 한다"
        );
    }

    #[test]
    fn 전역_잠금도_시간이_지나면_풀린다() {
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for i in 0..GLOBAL_LIMIT {
            g.note_failure(&format!("10.0.0.{i}"), now);
        }
        let 나중 = now + Duration::from_secs(LOCK_SECS + 1);
        assert_eq!(g.locked_for("처음보는주소", 나중), None);
    }

    #[test]
    fn 잠금이_풀려도_전역_기록은_바로_잊히지_않는다() {
        // "20회 몰아치기 → 잠금이 풀릴 때까지 대기 → 반복"을 하면, 리셋 창이 잠금
        // 시간과 같을 때 카운터가 매 사이클 새것이 된다. 잠금이 풀린 뒤의 실패
        // 한 번이 곧바로 다시 잠가야 이 우회가 막힌다.
        let g = Gate::default();
        let now = SystemTime::UNIX_EPOCH;
        for i in 0..GLOBAL_LIMIT {
            g.note_failure(&format!("10.0.0.{i}"), now);
        }
        let 잠금해제후 = now + Duration::from_secs(LOCK_SECS + 1);
        assert_eq!(g.locked_for("새주소", 잠금해제후), None, "잠금 자체는 풀린다");
        g.note_failure("또다른주소", 잠금해제후);
        assert_eq!(
            g.locked_for("새주소", 잠금해제후),
            Some(LOCK_SECS),
            "기록이 남아 있으므로 실패 한 번에 다시 잠긴다"
        );
    }
}
