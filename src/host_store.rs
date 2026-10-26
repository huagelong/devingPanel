use crate::database::connect;
use crate::env_store;
use crate::preferences;
use keyring::{Entry, Error as KeyringError};
use rusqlite::{Connection, params};
use std::fs::{File, OpenOptions};
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CREDENTIAL_SERVICE: &str = "devingPanel:ssh";
const KEY_CHUNK_SERVICE: &str = "devingPanel:ssh-key-chunks";
const KEY_CHUNK_UTF16_UNITS: usize = 1000;
const MAX_KEY_CHUNKS: usize = 128;
static CREDENTIAL_LIFECYCLE: Mutex<()> = Mutex::new(());

fn credential_lock() -> MutexGuard<'static, ()> {
    CREDENTIAL_LIFECYCLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct LifecycleGuard {
    _local: MutexGuard<'static, ()>,
    _file: File,
}

fn lifecycle_lock() -> Result<LifecycleGuard, String> {
    let local = credential_lock();
    let path = preferences::data_dir()
        .ok_or("无法确定本机配置目录。")?
        .join("credential-lifecycle.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "无法打开凭据操作锁。")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err("另一客户端正在处理凭据，请稍后重试。".into());
            }
            Err(_) => return Err("无法锁定凭据操作。".into()),
        }
    }
    Ok(LifecycleGuard {
        _local: local,
        _file: file,
    })
}

pub(crate) fn with_credential_lock<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _guard = lifecycle_lock()?;
    operation()
}

#[derive(Clone)]
pub struct Host {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub username: String,
    pub group: String,
    pub auth: String,
    pub trusted_fingerprint: Option<String>,
}

pub struct ConnectionMaterial {
    pub host: Host,
    pub credential: String,
    pub trusted_fingerprint: Option<String>,
}

fn read_credential(id: &str) -> Result<String, String> {
    let manifest = Entry::new(KEY_CHUNK_SERVICE, id).map_err(|_| "系统凭据管理器不可用。")?;
    match manifest.get_password() {
        Ok(count) => {
            let count = count
                .parse::<usize>()
                .ok()
                .filter(|count| (1..=MAX_KEY_CHUNKS).contains(count))
                .ok_or("私钥凭据清单无效，请重新配置私钥。")?;
            let mut value = String::new();
            for index in 0..count {
                let chunk = Entry::new(KEY_CHUNK_SERVICE, &format!("{id}:{index}"))
                    .map_err(|_| "系统凭据管理器不可用。")?
                    .get_password()
                    .map_err(|_| "私钥凭据不完整，请重新配置私钥。")?;
                value.push_str(&chunk);
            }
            Ok(value)
        }
        Err(KeyringError::NoEntry) => Entry::new(CREDENTIAL_SERVICE, id)
            .map_err(|_| "系统凭据管理器不可用。")?
            .get_password()
            .map_err(|_| "主机凭据不可读取，请重新配置。".into()),
        Err(_) => Err("系统凭据管理器不可用。".into()),
    }
}

pub fn connection_material(id: i64) -> Result<ConnectionMaterial, String> {
    let conn = connect()?;
    let _guard = lifecycle_lock()?;
    let (host, credential_id): (Host, String) = conn
        .query_row(
            "SELECT id, name, address, port, username, group_name, auth, trusted_fingerprint, credential_id
             FROM hosts WHERE id = ?1 AND deleting = 0",
            [id],
            |row| Ok((Host {
                id: row.get(0)?, name: row.get(1)?, address: row.get(2)?, port: row.get(3)?,
                username: row.get(4)?, group: row.get(5)?, auth: row.get(6)?,
                trusted_fingerprint: row.get(7)?,
            }, row.get(8)?)),
        )
        .map_err(|_| "主机不存在。")?;
    Ok(ConnectionMaterial {
        trusted_fingerprint: host.trusted_fingerprint.clone(),
        host,
        credential: read_credential(&credential_id)?,
    })
}

pub fn trust_fingerprint(
    id: i64,
    address: &str,
    port: u16,
    previous: Option<&str>,
    fingerprint: &str,
) -> Result<(), String> {
    let conn = connect()?;
    let _guard = lifecycle_lock()?;
    trust_fingerprint_in(&conn, id, address, port, previous, fingerprint)
}

fn trust_fingerprint_in(
    conn: &Connection,
    id: i64,
    address: &str,
    port: u16,
    previous: Option<&str>,
    fingerprint: &str,
) -> Result<(), String> {
    if !fingerprint.starts_with("SHA256:") || fingerprint.len() > 128 {
        return Err("SSH 指纹格式无效。".into());
    }
    if conn
        .execute(
            "UPDATE hosts SET trusted_fingerprint = ?1 WHERE id = ?2 AND address = ?3
         AND port = ?4 AND trusted_fingerprint IS ?5 AND deleting = 0",
            params![fingerprint, id, address, port, previous],
        )
        .map_err(|err| err.to_string())?
        == 0
    {
        return Err("主机配置或可信指纹已变化，请重新测试连接。".into());
    }
    Ok(())
}

pub struct NewHost {
    pub name: String,
    pub address: String,
    pub port: String,
    pub username: String,
    pub group: String,
    pub auth: String,
    pub credential: String,
}

pub struct HostUpdate {
    pub name: String,
    pub address: String,
    pub port: String,
    pub username: String,
    pub group: String,
    pub auth: String,
    pub credential: Option<String>,
}

impl NewHost {
    fn normalize(mut self) -> Result<(Self, u16), String> {
        let port = normalize_fields(
            &mut self.name,
            &mut self.address,
            &self.port,
            &mut self.username,
            &mut self.group,
        )?;
        if !matches!(self.auth.as_str(), "password" | "key") || self.credential.is_empty() {
            return Err("请选择认证方式并填写密码或私钥。".into());
        }
        Ok((self, port))
    }
}

impl HostUpdate {
    fn normalize(mut self) -> Result<(Self, u16), String> {
        let port = normalize_fields(
            &mut self.name,
            &mut self.address,
            &self.port,
            &mut self.username,
            &mut self.group,
        )?;
        if !matches!(self.auth.as_str(), "password" | "key")
            || self.credential.as_deref() == Some("")
        {
            return Err("认证方式或新凭据无效。".into());
        }
        Ok((self, port))
    }
}

fn normalize_fields(
    name: &mut String,
    address: &mut String,
    port: &str,
    username: &mut String,
    group: &mut String,
) -> Result<u16, String> {
    *name = name.trim().into();
    *address = address.trim().into();
    *username = username.trim().into();
    *group = group.trim().into();
    let port = port
        .trim()
        .parse::<u16>()
        .map_err(|_| "SSH 端口必须为 1～65535。")?;
    if name.is_empty() || username.is_empty() || port == 0 {
        return Err("请填写别名、用户名及有效的 SSH 端口。".into());
    }
    if !valid_address(address) {
        return Err("请输入有效的 IP 或域名。".into());
    }
    Ok(port)
}

fn valid_address(address: &str) -> bool {
    if address.parse::<IpAddr>().is_ok() {
        return true;
    }
    address.len() <= 253
        && address.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        })
}

fn list_in(conn: &Connection) -> rusqlite::Result<Vec<Host>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, address, port, username, group_name, auth, trusted_fingerprint FROM hosts WHERE deleting = 0 ORDER BY id DESC",
    )?;
    stmt.query_map([], |row| {
        Ok(Host {
            id: row.get(0)?,
            name: row.get(1)?,
            address: row.get(2)?,
            port: row.get(3)?,
            username: row.get(4)?,
            group: row.get(5)?,
            auth: row.get(6)?,
            trusted_fingerprint: row.get(7)?,
        })
    })?
    .collect()
}

pub fn list() -> Result<(Vec<Host>, Option<String>), String> {
    let conn = connect()?;
    let warning = match lifecycle_lock() {
        Ok(_guard) => {
            let warnings = [
                cleanup_pending_with(&conn, delete_credential),
                cleanup_deleting_with(&conn, delete_credential),
                env_store::cleanup_pending_in(&conn, env_store::delete_pending_secret),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            (!warnings.is_empty()).then(|| warnings.join(" "))
        }
        Err(error) => Some(error),
    };
    let hosts = list_in(&conn).map_err(|err| err.to_string())?;
    Ok((hosts, warning))
}

fn delete_credential(id: &str) -> Result<(), String> {
    let manifest = Entry::new(KEY_CHUNK_SERVICE, id).map_err(|_| "系统凭据管理器不可用。")?;
    match manifest.get_password() {
        Ok(count) => {
            let count = count
                .parse::<usize>()
                .ok()
                .filter(|count| (1..=MAX_KEY_CHUNKS).contains(count))
                .ok_or("私钥凭据清单无效，无法安全清理。")?;
            for index in 0..count {
                delete_entry(KEY_CHUNK_SERVICE, &format!("{id}:{index}"))?;
            }
            delete_entry(KEY_CHUNK_SERVICE, id)?;
        }
        Err(KeyringError::NoEntry) => {}
        Err(_) => return Err("系统凭据清理失败。".into()),
    }
    delete_entry(CREDENTIAL_SERVICE, id)
}

fn delete_entry(service: &str, id: &str) -> Result<(), String> {
    let entry = Entry::new(service, id).map_err(|_| "系统凭据管理器不可用。")?;
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(_) => Err("系统凭据清理失败。".into()),
    }
}

fn write_credential(id: &str, value: &str, auth: &str) -> Result<(), String> {
    if auth != "key" || value.encode_utf16().count() <= KEY_CHUNK_UTF16_UNITS {
        return Entry::new(CREDENTIAL_SERVICE, id)
            .map_err(|_| "系统凭据管理器不可用。")?
            .set_password(value)
            .map_err(|_| "系统凭据写入失败。".into());
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for ch in value.chars() {
        if units + ch.len_utf16() > KEY_CHUNK_UTF16_UNITS {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += ch.len_utf16();
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.len() > MAX_KEY_CHUNKS {
        return Err("私钥内容过大，无法安全保存。".into());
    }
    // The manifest is written first so pending-reference cleanup can find partial writes.
    Entry::new(KEY_CHUNK_SERVICE, id)
        .map_err(|_| "系统凭据管理器不可用。")?
        .set_password(&chunks.len().to_string())
        .map_err(|_| "系统凭据写入失败。")?;
    for (index, chunk) in chunks.iter().enumerate() {
        Entry::new(KEY_CHUNK_SERVICE, &format!("{id}:{index}"))
            .map_err(|_| "系统凭据管理器不可用。")?
            .set_password(chunk)
            .map_err(|_| "系统凭据写入失败。")?;
    }
    Ok(())
}

fn cleanup_pending_with(
    conn: &Connection,
    delete: impl Fn(&str) -> Result<(), String>,
) -> Option<String> {
    let pending = (|| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT credential_id FROM pending_credentials")?;
        stmt.query_map([], |row| row.get(0))?.collect()
    })();
    let Ok(pending) = pending else {
        return Some("无法检查待清理的凭据引用。".into());
    };
    let mut failed = 0;
    for id in pending {
        let linked = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM hosts WHERE credential_id = ?1)",
            [&id],
            |row| row.get::<_, bool>(0),
        );
        if linked.is_err()
            || (!linked.unwrap_or(false) && delete(&id).is_err())
            || conn
                .execute(
                    "DELETE FROM pending_credentials WHERE credential_id = ?1",
                    [&id],
                )
                .is_err()
        {
            failed += 1;
        }
    }
    (failed > 0).then(|| format!("仍有 {failed} 个凭据引用待清理；下次启动将重试。"))
}

fn cleanup_deleting_with(
    conn: &Connection,
    delete: impl Fn(&str) -> Result<(), String>,
) -> Option<String> {
    let deleting = (|| -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = conn.prepare("SELECT id, credential_id FROM hosts WHERE deleting = 1")?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    })();
    let Ok(deleting) = deleting else {
        return Some("无法检查待删除的主机。".into());
    };
    let mut failed = 0;
    for (id, credential_id) in deleting {
        if delete(&credential_id).is_err() || finalize_host_deletion(conn, id).is_err() {
            failed += 1;
        }
    }
    (failed > 0).then(|| format!("仍有 {failed} 台主机的凭据/配置待删除；下次启动将重试。"))
}

fn finalize_host_deletion(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    let transaction = conn.unchecked_transaction()?;
    env_store::queue_for_host(&transaction, id)?;
    transaction.execute("DELETE FROM hosts WHERE id = ?1 AND deleting = 1", [id])?;
    transaction.commit()
}

fn insert_in(
    conn: &Connection,
    host: &NewHost,
    port: u16,
    credential_id: &str,
) -> Result<Host, String> {
    conn.execute(
        "INSERT INTO hosts (name, address, port, username, auth, group_name, credential_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            host.name,
            host.address,
            port,
            host.username,
            host.auth,
            host.group,
            credential_id
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(Host {
        id: conn.last_insert_rowid(),
        name: host.name.clone(),
        address: host.address.clone(),
        port,
        username: host.username.clone(),
        group: host.group.clone(),
        auth: host.auth.clone(),
        trusted_fingerprint: None,
    })
}

fn commit_host(
    conn: &mut Connection,
    host: &NewHost,
    port: u16,
    credential_id: &str,
) -> Result<Host, String> {
    let transaction = conn.transaction().map_err(|err| err.to_string())?;
    let saved = insert_in(&transaction, host, port, credential_id)?;
    transaction
        .execute(
            "DELETE FROM pending_credentials WHERE credential_id = ?1",
            [credential_id],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(saved)
}

pub fn create(input: NewHost) -> Result<Host, String> {
    let (input, port) = input.normalize()?;
    let credential_id = Uuid::new_v4().to_string();
    let mut conn = connect()?;
    let _guard = lifecycle_lock()?;
    conn.execute(
        "INSERT INTO pending_credentials (credential_id) VALUES (?1)",
        [&credential_id],
    )
    .map_err(|err| err.to_string())?;
    if let Err(error) = write_credential(&credential_id, &input.credential, &input.auth) {
        if delete_credential(&credential_id).is_ok() {
            let _ = conn.execute(
                "DELETE FROM pending_credentials WHERE credential_id = ?1",
                [&credential_id],
            );
            return Err(format!("{error} 主机未保存。"));
        }
        return Err(format!("凭据写入失败且清理待重试；引用 {credential_id}。"));
    }
    let result = commit_host(&mut conn, &input, port, &credential_id);
    if result.is_err() {
        if delete_credential(&credential_id).is_ok() {
            let _ = conn.execute(
                "DELETE FROM pending_credentials WHERE credential_id = ?1",
                [&credential_id],
            );
        } else {
            return Err(format!(
                "主机未保存，凭据清理待重试；引用 {credential_id}。"
            ));
        }
    }
    result
}

fn update_in(
    conn: &Connection,
    id: i64,
    input: HostUpdate,
    new_credential_id: Option<&str>,
) -> Result<Host, String> {
    let (input, port) = input.normalize()?;
    let transaction = conn
        .unchecked_transaction()
        .map_err(|err| err.to_string())?;
    let conflicts: bool = transaction
        .query_row(
            "SELECT EXISTS (
                SELECT 1 FROM project_hosts own
                JOIN project_hosts other ON other.project_id = own.project_id AND other.host_id <> own.host_id
                JOIN hosts peer ON peer.id = other.host_id
                WHERE own.host_id = ?1 AND peer.group_name <> ?2
            )",
            params![id, input.group],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if conflicts {
        return Err("该主机与其他同组主机绑定了项目，请先调整项目的目标主机。".into());
    }
    let old_auth: String = transaction
        .query_row(
            "SELECT auth FROM hosts WHERE id = ?1 AND deleting = 0",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "主机不存在。")?;
    if old_auth != input.auth && new_credential_id.is_none() {
        return Err("切换认证方式时必须填写新密码或私钥。".into());
    }
    if new_credential_id.is_some() {
        let old_id: String = transaction
            .query_row(
                "SELECT credential_id FROM hosts WHERE id = ?1 AND deleting = 0",
                [id],
                |row| row.get(0),
            )
            .map_err(|_| "主机不存在。")?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO pending_credentials VALUES (?1)",
                [old_id],
            )
            .map_err(|err| err.to_string())?;
    }
    let changed = transaction.execute(
        "UPDATE hosts SET name = ?1, address = ?2, port = ?3, username = ?4, group_name = ?5,
         trusted_fingerprint = CASE WHEN address <> ?2 OR port <> ?3 THEN NULL ELSE trusted_fingerprint END,
         credential_id = COALESCE(?7, credential_id), auth = ?8
         WHERE id = ?6 AND deleting = 0",
        params![input.name, input.address, port, input.username, input.group, id, new_credential_id, input.auth],
    ).map_err(|err| err.to_string())?;
    if changed == 0 {
        return Err("主机不存在。".into());
    }
    if let Some(reference) = new_credential_id {
        transaction
            .execute(
                "DELETE FROM pending_credentials WHERE credential_id = ?1",
                [reference],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|host| host.id == id)
        .ok_or_else(|| "主机不存在。".into())
}

pub fn update(id: i64, mut input: HostUpdate) -> Result<(Host, Option<String>), String> {
    let conn = connect()?;
    let _guard = lifecycle_lock()?;
    let credential = input.credential.take();
    if !matches!(input.auth.as_str(), "password" | "key") || credential.as_deref() == Some("") {
        return Err("认证方式或新凭据无效。".into());
    }
    let reference = credential.as_ref().map(|_| Uuid::new_v4().to_string());
    if let (Some(credential), Some(reference)) = (&credential, &reference) {
        conn.execute("INSERT INTO pending_credentials VALUES (?1)", [reference])
            .map_err(|err| err.to_string())?;
        if let Err(error) = write_credential(reference, credential, &input.auth) {
            let warning = cleanup_pending_with(&conn, delete_credential);
            return Err(match warning {
                Some(warning) => format!("{error} 凭据未更换；{warning}"),
                None => format!("{error} 凭据未更换。"),
            });
        }
    }
    let result = update_in(&conn, id, input, reference.as_deref());
    let warning = cleanup_pending_with(&conn, delete_credential);
    match result {
        Ok(host) => Ok((host, warning)),
        Err(error) => Err(match warning {
            Some(warning) => format!("{error} {warning}"),
            None => error,
        }),
    }
}

fn delete_in(
    conn: &Connection,
    id: i64,
    delete: impl Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    let credential_id: String = conn
        .query_row(
            "SELECT credential_id FROM hosts WHERE id = ?1 AND deleting = 0",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "主机不存在。")?;
    conn.execute("UPDATE hosts SET deleting = 1 WHERE id = ?1", [id])
        .map_err(|err| err.to_string())?;
    if delete(&credential_id).is_err() {
        return Err("凭据清理未完成；主机已标记待删除，下次启动重试。".into());
    }
    finalize_host_deletion(conn, id).map_err(|_| "凭据已清理，但主机配置待删除；下次启动重试。")?;
    Ok(())
}

pub fn delete(id: i64) -> Result<Option<String>, String> {
    let conn = connect()?;
    let _guard = lifecycle_lock()?;
    delete_in(&conn, id, delete_credential)?;
    Ok(env_store::cleanup_pending_in(
        &conn,
        env_store::delete_pending_secret,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        HostUpdate, NewHost, cleanup_deleting_with, cleanup_pending_with, commit_host,
        credential_lock, delete_in, insert_in, list_in, trust_fingerprint_in, update_in,
    };
    use crate::database::initialize;
    use rusqlite::Connection;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    #[cfg(windows)]
    #[test]
    #[ignore = "writes and removes isolated Windows Credential Manager entries"]
    fn windows_password_rotation_round_trip() {
        let host = super::create(NewHost {
            name: format!("rotation-{}", uuid::Uuid::new_v4()),
            address: "203.0.113.10".into(),
            port: "22".into(),
            username: "root".into(),
            group: String::new(),
            auth: "password".into(),
            credential: "old-secret".into(),
        })
        .unwrap();
        let conn = crate::database::connect().unwrap();
        let old_id: String = conn
            .query_row(
                "SELECT credential_id FROM hosts WHERE id = ?1",
                [host.id],
                |row| row.get(0),
            )
            .unwrap();
        let (updated, warning) = super::update(
            host.id,
            HostUpdate {
                name: host.name.clone(),
                address: host.address.clone(),
                port: host.port.to_string(),
                username: host.username.clone(),
                group: host.group.clone(),
                auth: "password".into(),
                credential: Some("new-secret".into()),
            },
        )
        .unwrap();
        assert_eq!(updated.id, host.id);
        assert!(warning.is_none());
        let new_id: String = conn
            .query_row(
                "SELECT credential_id FROM hosts WHERE id = ?1",
                [host.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(old_id, new_id);
        assert_eq!(
            keyring::Entry::new(super::CREDENTIAL_SERVICE, &new_id)
                .unwrap()
                .get_password()
                .unwrap(),
            "new-secret"
        );
        assert!(matches!(
            keyring::Entry::new(super::CREDENTIAL_SERVICE, &old_id)
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
        let key_content = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}🔑{}\n-----END OPENSSH PRIVATE KEY-----",
            "A".repeat(1100),
            "B".repeat(1100)
        );
        let (switched, warning) = super::update(
            host.id,
            HostUpdate {
                name: host.name.clone(),
                address: host.address.clone(),
                port: host.port.to_string(),
                username: host.username.clone(),
                group: host.group.clone(),
                auth: "key".into(),
                credential: Some(key_content.clone()),
            },
        )
        .unwrap();
        assert_eq!(switched.auth, "key");
        assert!(warning.is_none());
        let key_id: String = conn
            .query_row(
                "SELECT credential_id FROM hosts WHERE id = ?1",
                [host.id],
                |row| row.get(0),
            )
            .unwrap();
        let count: usize = keyring::Entry::new(super::KEY_CHUNK_SERVICE, &key_id)
            .unwrap()
            .get_password()
            .unwrap()
            .parse()
            .unwrap();
        assert!(count >= 2);
        let chunks = (0..count)
            .map(|index| {
                keyring::Entry::new(super::KEY_CHUNK_SERVICE, &format!("{key_id}:{index}"))
                    .unwrap()
                    .get_password()
                    .unwrap()
            })
            .collect::<String>();
        assert_eq!(chunks, key_content);
        assert!(matches!(
            keyring::Entry::new(super::CREDENTIAL_SERVICE, &new_id)
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
        assert!(super::delete(host.id).unwrap().is_none());
        assert!(matches!(
            keyring::Entry::new(super::CREDENTIAL_SERVICE, &key_id)
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
        assert!(matches!(
            keyring::Entry::new(super::KEY_CHUNK_SERVICE, &key_id)
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
        for index in 0..count {
            assert!(matches!(
                keyring::Entry::new(super::KEY_CHUNK_SERVICE, &format!("{key_id}:{index}"))
                    .unwrap()
                    .get_password(),
                Err(keyring::Error::NoEntry)
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes and removes isolated Windows Credential Manager entries"]
    fn windows_partial_key_write_is_cleaned_on_restart() {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = crate::database::connect().unwrap();
        conn.execute("INSERT INTO pending_credentials VALUES (?1)", [&id])
            .unwrap();
        keyring::Entry::new(super::KEY_CHUNK_SERVICE, &id)
            .unwrap()
            .set_password("3")
            .unwrap();
        keyring::Entry::new(super::KEY_CHUNK_SERVICE, &format!("{id}:0"))
            .unwrap()
            .set_password("partial-key")
            .unwrap();
        let (_, warning) = super::list().unwrap();
        assert!(warning.is_none());
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_credentials WHERE credential_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
        assert!(matches!(
            keyring::Entry::new(super::KEY_CHUNK_SERVICE, &id)
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
        assert!(matches!(
            keyring::Entry::new(super::KEY_CHUNK_SERVICE, &format!("{id}:0"))
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
    }

    #[test]
    fn bound_host_cannot_leave_its_project_group_alone() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'app', '.', 'Dockerfile', 'app');
             INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id)
             VALUES (1, 'one', 'one.example', 22, 'root', 'password', 'prod', 'one'),
                    (2, 'two', 'two.example', 22, 'root', 'password', 'prod', 'two');
             INSERT INTO project_hosts VALUES (1, 1, 0), (1, 2, 1);",
        ).unwrap();
        let edit = HostUpdate {
            name: "one".into(),
            address: "one.example".into(),
            port: "22".into(),
            username: "root".into(),
            group: "stage".into(),
            auth: "password".into(),
            credential: None,
        };
        assert!(update_in(&conn, 1, edit, None).is_err());
        assert_eq!(list_in(&conn).unwrap()[0].group, "prod");
        conn.execute("DELETE FROM project_hosts WHERE host_id = 2", [])
            .unwrap();
        let edit = HostUpdate {
            name: "one".into(),
            address: "one.example".into(),
            port: "22".into(),
            username: "root".into(),
            group: "stage".into(),
            auth: "password".into(),
            credential: None,
        };
        assert_eq!(update_in(&conn, 1, edit, None).unwrap().group, "stage");
    }

    #[test]
    fn key_auth_requires_new_credential_when_switching_modes() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let input = NewHost {
            name: "key-host".into(),
            address: "key.example".into(),
            port: "22".into(),
            username: "root".into(),
            group: String::new(),
            auth: "key".into(),
            credential: "synthetic-private-key".into(),
        };
        let host = insert_in(&conn, &input, 22, "key-ref").unwrap();
        assert_eq!(host.auth, "key");
        let update = || HostUpdate {
            name: host.name.clone(),
            address: host.address.clone(),
            port: "22".into(),
            username: host.username.clone(),
            group: String::new(),
            auth: "password".into(),
            credential: None,
        };
        assert!(update_in(&conn, host.id, update(), None).is_err());
        assert_eq!(list_in(&conn).unwrap()[0].auth, "key");
        conn.execute(
            "INSERT INTO pending_credentials VALUES ('password-ref')",
            [],
        )
        .unwrap();
        assert_eq!(
            update_in(&conn, host.id, update(), Some("password-ref"))
                .unwrap()
                .auth,
            "password"
        );
        let old: String = conn
            .query_row("SELECT credential_id FROM pending_credentials", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(old, "key-ref");
        let stored: String = conn
            .query_row(
                "SELECT credential_id FROM hosts WHERE id = ?1",
                [host.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "password-ref");
        assert_ne!(stored, input.credential);
    }

    #[test]
    fn fingerprint_trust_requires_same_endpoint_and_previous_value() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO hosts (id, name, address, port, username, auth, credential_id)
            VALUES (1, 'web', 'web.example', 22, 'root', 'password', 'random-ref');",
        )
        .unwrap();
        assert!(trust_fingerprint_in(&conn, 1, "other.example", 22, None, "SHA256:first").is_err());
        trust_fingerprint_in(&conn, 1, "web.example", 22, None, "SHA256:first").unwrap();
        assert!(trust_fingerprint_in(&conn, 1, "web.example", 22, None, "SHA256:second").is_err());
        assert!(
            trust_fingerprint_in(
                &conn,
                1,
                "web.example",
                2222,
                Some("SHA256:first"),
                "SHA256:second"
            )
            .is_err()
        );
        trust_fingerprint_in(
            &conn,
            1,
            "web.example",
            22,
            Some("SHA256:first"),
            "SHA256:second",
        )
        .unwrap();
        assert_eq!(
            list_in(&conn).unwrap()[0].trusted_fingerprint.as_deref(),
            Some("SHA256:second")
        );
    }

    #[test]
    fn password_rotation_commits_new_reference_before_old_cleanup() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id)
             VALUES (1, 'web', 'web.example', 22, 'root', 'password', 'prod', 'old-ref');
             INSERT INTO pending_credentials VALUES ('new-ref');",
        )
        .unwrap();
        let update = || HostUpdate {
            name: "web".into(),
            address: "web.example".into(),
            port: "22".into(),
            username: "root".into(),
            group: "prod".into(),
            auth: "password".into(),
            credential: None,
        };
        assert_eq!(
            update_in(&conn, 1, update(), Some("new-ref")).unwrap().name,
            "web"
        );
        let current: String = conn
            .query_row("SELECT credential_id FROM hosts WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(current, "new-ref");
        let pending: String = conn
            .query_row("SELECT credential_id FROM pending_credentials", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, "old-ref");
        assert!(cleanup_pending_with(&conn, |_| Err("vault unavailable".into())).is_some());
        assert!(
            cleanup_pending_with(&conn, |id| {
                assert_eq!(id, "old-ref");
                Ok(())
            })
            .is_none()
        );
        conn.execute("INSERT INTO pending_credentials VALUES ('another-ref')", [])
            .unwrap();
        assert!(update_in(&conn, 999, update(), Some("another-ref")).is_err());
        assert_eq!(
            conn.query_row("SELECT credential_id FROM hosts WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "new-ref"
        );
        assert!(
            cleanup_pending_with(&conn, |id| {
                assert_eq!(id, "another-ref");
                Ok(())
            })
            .is_none()
        );
        conn.execute("INSERT INTO pending_credentials VALUES ('failed-ref')", [])
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_rotation BEFORE UPDATE ON hosts BEGIN SELECT RAISE(ABORT, 'blocked'); END;").unwrap();
        assert!(update_in(&conn, 1, update(), Some("failed-ref")).is_err());
        assert_eq!(
            conn.query_row("SELECT credential_id FROM hosts WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "new-ref"
        );
        assert_eq!(
            conn.query_row("SELECT credential_id FROM pending_credentials", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "failed-ref"
        );
        conn.execute_batch("DROP TRIGGER fail_rotation;").unwrap();
        assert!(
            cleanup_pending_with(&conn, |id| {
                assert_eq!(id, "failed-ref");
                Ok(())
            })
            .is_none()
        );
    }

    #[test]
    fn host_table_stores_reference_not_password() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let (input, port) = NewHost {
            name: " web-1 ".into(),
            address: "203.0.113.10".into(),
            port: "22".into(),
            username: "deploy".into(),
            group: "prod".into(),
            auth: "password".into(),
            credential: "test-secret".into(),
        }
        .normalize()
        .unwrap();
        let host = insert_in(&conn, &input, port, "random-reference").unwrap();
        assert_eq!(host.name, "web-1");
        assert_eq!(list_in(&conn).unwrap()[0].address, "203.0.113.10");
        let stored: String = conn
            .query_row("SELECT credential_id FROM hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, "random-reference");
        assert_ne!(stored, input.credential);
        assert!(
            NewHost {
                port: "0".into(),
                ..input
            }
            .normalize()
            .is_err()
        );
        for address in [
            "user@host",
            "foo:22",
            "?",
            "bad host",
            "bad_domain",
            &"a".repeat(64),
        ] {
            assert!(
                NewHost {
                    name: "web".into(),
                    address: address.into(),
                    port: "22".into(),
                    username: "root".into(),
                    group: String::new(),
                    auth: "password".into(),
                    credential: "dummy".into(),
                }
                .normalize()
                .is_err()
            );
        }
        assert!(
            NewHost {
                name: "web".into(),
                address: "::1".into(),
                port: "22".into(),
                username: "root".into(),
                group: String::new(),
                auth: "password".into(),
                credential: "dummy".into(),
            }
            .normalize()
            .is_ok()
        );
    }

    #[test]
    fn failed_host_commit_keeps_cleanup_reference() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute("INSERT INTO pending_credentials VALUES ('orphan-id')", [])
            .unwrap();
        let host = NewHost {
            name: "web".into(),
            address: "203.0.113.10".into(),
            port: "22".into(),
            username: "root".into(),
            group: String::new(),
            auth: "password".into(),
            credential: "dummy".into(),
        };
        assert!(commit_host(&mut conn, &host, 0, "orphan-id").is_err());
        let warning = cleanup_pending_with(&conn, |_| Err("vault unavailable".into()));
        assert!(warning.is_some());
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_credentials", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 1);
        assert!(cleanup_pending_with(&conn, |_| Ok(())).is_none());
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_credentials", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn cleanup_waits_for_host_creation_to_commit() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        initialize(&conn.lock().unwrap()).unwrap();
        conn.lock()
            .unwrap()
            .execute("INSERT INTO pending_credentials VALUES ('active-id')", [])
            .unwrap();
        let guard = credential_lock();
        let (started, ready) = mpsc::channel();
        let removed = Arc::new(AtomicBool::new(false));
        let worker_conn = conn.clone();
        let worker_removed = removed.clone();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            let _guard = credential_lock();
            cleanup_pending_with(&worker_conn.lock().unwrap(), |_| {
                worker_removed.store(true, Ordering::SeqCst);
                Ok(())
            })
        });
        ready.recv().unwrap();
        let host = NewHost {
            name: "web".into(),
            address: "203.0.113.10".into(),
            port: "22".into(),
            username: "root".into(),
            group: String::new(),
            auth: "password".into(),
            credential: "dummy".into(),
        };
        commit_host(&mut conn.lock().unwrap(), &host, 22, "active-id").unwrap();
        drop(guard);
        assert!(worker.join().unwrap().is_none());
        assert!(!removed.load(Ordering::SeqCst));
        assert_eq!(list_in(&conn.lock().unwrap()).unwrap().len(), 1);
    }

    #[test]
    fn edits_invalidate_trust_and_failed_delete_is_retryable() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let input = NewHost {
            name: "web".into(),
            address: "203.0.113.10".into(),
            port: "22".into(),
            username: "root".into(),
            group: "prod".into(),
            auth: "password".into(),
            credential: "dummy".into(),
        };
        let host = insert_in(&conn, &input, 22, "credential-id").unwrap();
        conn.execute(
            "UPDATE hosts SET trusted_fingerprint = 'SHA256:old' WHERE id = ?1",
            [host.id],
        )
        .unwrap();
        let renamed = update_in(
            &conn,
            host.id,
            HostUpdate {
                name: "web-renamed".into(),
                address: "203.0.113.10".into(),
                port: "22".into(),
                username: "root".into(),
                group: "prod".into(),
                auth: "password".into(),
                credential: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(renamed.name, "web-renamed");
        let fingerprint: Option<String> = conn
            .query_row(
                "SELECT trusted_fingerprint FROM hosts WHERE id = ?1",
                [host.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fingerprint.as_deref(), Some("SHA256:old"));
        update_in(
            &conn,
            host.id,
            HostUpdate {
                name: "web-renamed".into(),
                address: "203.0.113.11".into(),
                port: "22".into(),
                username: "root".into(),
                group: "prod".into(),
                auth: "password".into(),
                credential: None,
            },
            None,
        )
        .unwrap();
        let fingerprint: Option<String> = conn
            .query_row(
                "SELECT trusted_fingerprint FROM hosts WHERE id = ?1",
                [host.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fingerprint.is_none());
        assert!(delete_in(&conn, host.id, |_| Err("vault unavailable".into())).is_err());
        assert!(list_in(&conn).unwrap().is_empty());
        assert!(cleanup_deleting_with(&conn, |_| Err("vault unavailable".into())).is_some());
        assert!(cleanup_deleting_with(&conn, |_| Ok(())).is_none());
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn deleted_vault_entry_can_retry_failed_database_delete() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let input = NewHost {
            name: "web".into(),
            address: "203.0.113.10".into(),
            port: "22".into(),
            username: "root".into(),
            group: String::new(),
            auth: "password".into(),
            credential: "dummy".into(),
        };
        let host = insert_in(&conn, &input, 22, "credential-id").unwrap();
        conn.execute_batch("CREATE TRIGGER fail_host_delete BEFORE DELETE ON hosts BEGIN SELECT RAISE(ABORT, 'blocked'); END;").unwrap();
        assert!(delete_in(&conn, host.id, |_| Ok(())).is_err());
        assert!(list_in(&conn).unwrap().is_empty());
        assert!(cleanup_deleting_with(&conn, |_| Ok(())).is_some());
        conn.execute_batch("DROP TRIGGER fail_host_delete;")
            .unwrap();
        assert!(cleanup_deleting_with(&conn, |_| Ok(())).is_none());
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
