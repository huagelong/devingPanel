use crate::database;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use pbkdf2::pbkdf2_hmac;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const ITERATIONS: u32 = 600_000;

#[derive(Clone, PartialEq, Eq)]
pub struct LockSettings {
    pub idle_minutes: u16,
    salt: [u8; 16],
    verifier: [u8; 32],
}

fn read(conn: &Connection) -> Result<Option<LockSettings>, String> {
    let row: Option<(String, String, u16)> = conn
        .query_row(
            "SELECT salt, verifier, idle_minutes FROM screen_lock_settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    row.map(|(salt, verifier, idle_minutes)| {
        let salt: [u8; 16] = STANDARD
            .decode(salt)
            .map_err(|_| "锁屏配置损坏。")?
            .try_into()
            .map_err(|_| "锁屏配置损坏。")?;
        let verifier: [u8; 32] = STANDARD
            .decode(verifier)
            .map_err(|_| "锁屏配置损坏。")?
            .try_into()
            .map_err(|_| "锁屏配置损坏。")?;
        if !(1..=1440).contains(&idle_minutes) {
            return Err("锁屏间隔无效。".into());
        }
        Ok(LockSettings {
            idle_minutes,
            salt,
            verifier,
        })
    })
    .transpose()
}

pub fn load() -> Result<Option<LockSettings>, String> {
    read(&database::connect()?)
}

fn check_password(password: &str) -> Result<(), String> {
    if !(8..=1024).contains(&password.chars().count()) {
        Err("锁屏密码须为 8～1024 个字符。".into())
    } else {
        Ok(())
    }
}

fn check_interval(minutes: u16) -> Result<(), String> {
    if (1..=1440).contains(&minutes) {
        Ok(())
    } else {
        Err("自动锁屏间隔须为 1～1440 分钟。".into())
    }
}

fn derive(password: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut verifier = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, ITERATIONS, &mut verifier);
    verifier
}

fn matches(settings: &LockSettings, password: &str) -> bool {
    derive(password, &settings.salt)
        .ct_eq(&settings.verifier)
        .into()
}

pub fn enable(
    password: &str,
    confirmation: &str,
    idle_minutes: u16,
) -> Result<LockSettings, String> {
    check_password(password)?;
    check_interval(idle_minutes)?;
    if password != confirmation {
        return Err("两次锁屏密码不一致。".into());
    }
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| "无法生成锁屏随机盐。")?;
    let verifier = derive(password, &salt);
    let mut conn = database::connect()?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    if read(&tx)?.is_some() {
        return Err("锁屏已启用，请先验证当前密码。".into());
    }
    tx.execute(
        "INSERT INTO screen_lock_settings VALUES (1, ?1, ?2, ?3)",
        params![
            STANDARD.encode(salt),
            STANDARD.encode(verifier),
            idle_minutes
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(LockSettings {
        idle_minutes,
        salt,
        verifier,
    })
}

pub fn verify(password: &str) -> Result<bool, String> {
    Ok(load()?.is_some_and(|settings| matches(&settings, password)))
}

pub fn change(
    current: &str,
    password: Option<(&str, &str)>,
    idle_minutes: u16,
) -> Result<LockSettings, String> {
    check_interval(idle_minutes)?;
    if let Some((new, confirmation)) = password {
        check_password(new)?;
        if new != confirmation {
            return Err("两次锁屏密码不一致。".into());
        }
    }
    let mut conn = database::connect()?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let old = read(&tx)?.ok_or("锁屏未启用。")?;
    if !matches(&old, current) {
        return Err("当前锁屏密码错误。".into());
    }
    let mut salt = old.salt;
    let mut verifier = old.verifier;
    if let Some((new, _)) = password {
        getrandom::fill(&mut salt).map_err(|_| "无法生成锁屏随机盐。")?;
        verifier = derive(new, &salt);
    }
    tx.execute(
        "UPDATE screen_lock_settings SET salt = ?1, verifier = ?2, idle_minutes = ?3 WHERE id = 1",
        params![
            STANDARD.encode(salt),
            STANDARD.encode(verifier),
            idle_minutes
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(LockSettings {
        idle_minutes,
        salt,
        verifier,
    })
}

pub fn disable(current: &str) -> Result<(), String> {
    let mut conn = database::connect()?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let old = read(&tx)?.ok_or("锁屏未启用。")?;
    if !matches(&old, current) {
        return Err("当前锁屏密码错误。".into());
    }
    tx.execute("DELETE FROM screen_lock_settings WHERE id = 1", [])
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_verification_and_limits() {
        let salt = [7; 16];
        let settings = LockSettings {
            idle_minutes: 5,
            salt,
            verifier: derive("strong password", &salt),
        };
        assert!(matches(&settings, "strong password"));
        assert!(!matches(&settings, "wrong password"));
        assert!(check_password("short").is_err());
        assert!(check_interval(0).is_err());
        assert!(check_interval(1441).is_err());
    }
}
