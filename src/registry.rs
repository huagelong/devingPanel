use crate::database;
use crate::host_store::with_credential_lock;
use keyring::{Entry, Error as KeyringError};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const SERVICE: &str = "devingPanel:registry";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub address: String,
    pub username: String,
    pub enabled: bool,
}

#[derive(PartialEq, Eq)]
pub struct Credentials {
    pub config: Config,
    pub token: String,
}

fn entry(id: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, id).map_err(|_| "系统凭据管理器不可用。".into())
}

fn delete_secret(id: &str) -> Result<(), String> {
    match entry(id)?.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(_) => Err("镜像仓库旧凭据清理失败。".into()),
    }
}

fn cleanup_pending(conn: &Connection) -> Result<usize, String> {
    let mut stmt = conn
        .prepare("SELECT credential_id FROM pending_registry_credentials")
        .map_err(|error| error.to_string())?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut failed = 0;
    for id in ids {
        let used: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM registry_settings WHERE credential_id = ?1)",
                [&id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !used && delete_secret(&id).is_err() {
            failed += 1;
            continue;
        }
        conn.execute(
            "DELETE FROM pending_registry_credentials WHERE credential_id = ?1",
            [&id],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(failed)
}

fn ensure_no_active_release(conn: &Connection) -> Result<(), String> {
    let active: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM release_tasks
          WHERE registry_mode = 1 AND outcome IN ('running', 'unknown', 'blocked')
          UNION ALL SELECT 1 FROM release_batches
          WHERE registry_mode = 1 AND state IN ('prepared', 'building', 'ready', 'running', 'paused', 'unknown'))",
        [], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if active {
        Err("有未结束的发布任务，暂不能修改镜像仓库配置。".into())
    } else {
        Ok(())
    }
}

pub fn load() -> Result<Option<Config>, String> {
    let conn = database::connect()?;
    conn.query_row(
        "SELECT address, username, enabled FROM registry_settings WHERE id = 1",
        [],
        |row| {
            Ok(Config {
                address: row.get(0)?,
                username: row.get(1)?,
                enabled: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|error| error.to_string())
}

pub fn cleanup_pending_credentials() -> Result<(), String> {
    let conn = database::connect()?;
    with_credential_lock(|| {
        cleanup_local_auth_dirs()?;
        let failed = cleanup_pending(&conn)?;
        if failed > 0 {
            Err("镜像仓库旧令牌清理待重试。".into())
        } else {
            Ok(())
        }
    })
}

fn cleanup_local_auth_dirs() -> Result<(), String> {
    let root = std::env::temp_dir();
    let root = root.canonicalize().map_err(|_| "无法核对本机临时目录。")?;
    let entries = fs::read_dir(&root).map_err(|_| "无法列出本机临时目录。")?;
    for entry in entries {
        let entry = entry.map_err(|_| "无法核对本机临时认证目录。")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(id) = name.strip_prefix("devingpanel-registry-") else {
            continue;
        };
        if Uuid::parse_str(id).is_err() {
            continue;
        }
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|_| "无法核对本机临时认证目录。")?;
        if !metadata.file_type().is_dir() {
            continue;
        }
        let age = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok());
        if age.is_none_or(|age| age < Duration::from_secs(24 * 60 * 60)) {
            continue;
        }
        let path = entry
            .path()
            .canonicalize()
            .map_err(|_| "无法核对本机临时认证目录路径。")?;
        if path.parent() != Some(root.as_path()) {
            continue;
        }
        fs::remove_dir_all(path).map_err(|_| "过期的本机 Docker 认证目录清理失败，请轮换令牌。")?;
    }
    Ok(())
}

pub fn credentials() -> Result<Option<Credentials>, String> {
    let conn = database::connect()?;
    with_credential_lock(|| credentials_in(&conn))
}

fn credentials_in(conn: &Connection) -> Result<Option<Credentials>, String> {
    let row: Option<(String, String, String, bool)> = conn
        .query_row(
            "SELECT address, username, credential_id, enabled FROM registry_settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    row.map(|(address, username, id, enabled)| {
        let token = entry(&id)?
            .get_password()
            .map_err(|_| "镜像仓库令牌无法读取，请重新配置。")?;
        Ok(Credentials {
            config: Config {
                address,
                username,
                enabled,
            },
            token,
        })
    })
    .transpose()
}

pub fn enabled_credentials() -> Result<Option<Credentials>, String> {
    let conn = database::connect()?;
    with_credential_lock(|| enabled_credentials_in(&conn))
}

fn enabled_credentials_in(conn: &Connection) -> Result<Option<Credentials>, String> {
    let enabled: Option<bool> = conn
        .query_row(
            "SELECT enabled FROM registry_settings WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if enabled != Some(true) {
        return Ok(None);
    }
    credentials_in(conn)
}

pub fn create_release_intent(
    intent: &crate::release_store::Intent,
    snapshot: &Option<Credentials>,
) -> Result<(), String> {
    let conn = database::connect()?;
    with_credential_lock(|| {
        if &enabled_credentials_in(&conn)? != snapshot {
            return Err("镜像仓库配置在发布预检期间变化，请重新开始。".into());
        }
        crate::release_store::create(intent)?;
        if snapshot.is_some() {
            let saved = conn.execute(
                "UPDATE release_tasks SET registry_mode = 1 WHERE id = ?1 AND phase = 'prepared' AND outcome = 'running'",
                [&intent.id],
            );
            if !matches!(saved, Ok(1)) {
                let _ = crate::release_store::settle(
                    &intent.id,
                    crate::release_store::Phase::Prepared,
                    crate::release_store::Outcome::Failed,
                    "镜像仓库发布意图保存失败",
                );
                return Err("镜像仓库发布意图保存失败。".into());
            }
        }
        Ok(())
    })
}

pub fn normalize(address: &str, username: &str, token: &str) -> Result<Credentials, String> {
    let address = address.trim().trim_end_matches('/');
    let address = address.strip_prefix("https://").unwrap_or(address);
    let valid_host = address.len() <= 253
        && !address.is_empty()
        && address.split(':').count() <= 2
        && address.split(':').next().is_some_and(|host| {
            host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && label
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
            })
        })
        && address
            .split(':')
            .nth(1)
            .is_none_or(|port| port.parse::<u16>().is_ok_and(|port| port > 0));
    if !valid_host {
        return Err("请输入 HTTPS 镜像仓库域名及可选端口。".into());
    }
    let username = username.trim();
    if username.is_empty() || username.len() > 256 || username.chars().any(char::is_control) {
        return Err("请输入有效的镜像仓库用户名。".into());
    }
    if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control) {
        return Err("请输入有效的镜像仓库令牌。".into());
    }
    Ok(Credentials {
        config: Config {
            address: address.to_ascii_lowercase(),
            username: username.into(),
            enabled: false,
        },
        token: token.into(),
    })
}

pub fn save(address: &str, username: &str, token: &str) -> Result<Config, String> {
    let config = normalize(address, username, token)?;
    let conn = database::connect()?;
    with_credential_lock(|| {
        ensure_no_active_release(&conn)?;
        let pending = cleanup_pending(&conn)?;
        if pending > 0 {
            return Err("仍有镜像仓库凭据待清理，请稍后重试。".into());
        }
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO pending_registry_credentials VALUES (?1)",
            [&id],
        )
        .map_err(|error| error.to_string())?;
        entry(&id)?
            .set_password(&config.token)
            .map_err(|_| "镜像仓库令牌保存失败。")?;
        let old: Option<String> = conn
            .query_row(
                "SELECT credential_id FROM registry_settings WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let result = (|| -> Result<(), String> {
            conn.execute_batch("BEGIN IMMEDIATE")
                .map_err(|error| error.to_string())?;
            ensure_no_active_release(&conn)?;
            if let Some(old) = &old {
                conn.execute(
                    "INSERT OR IGNORE INTO pending_registry_credentials VALUES (?1)",
                    [old],
                )
                .map_err(|error| error.to_string())?;
            }
            conn.execute(
                "INSERT INTO registry_settings (id, address, username, credential_id) VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET address = excluded.address, username = excluded.username,
                 credential_id = excluded.credential_id",
                params![config.config.address, config.config.username, id],
            ).map_err(|error| error.to_string())?;
            conn.execute(
                "DELETE FROM pending_registry_credentials WHERE credential_id = ?1",
                [&id],
            )
            .map_err(|error| error.to_string())?;
            conn.execute_batch("COMMIT")
                .map_err(|error| error.to_string())
        })();
        if let Err(error) = result {
            let _ = conn.execute_batch("ROLLBACK");
            let _ = cleanup_pending(&conn);
            return Err(error);
        }
        let failed = cleanup_pending(&conn)?;
        if failed > 0 {
            return Err("镜像仓库配置已保存，但旧令牌清理待重试。".into());
        }
        load()?.ok_or("镜像仓库配置保存后无法读取。".into())
    })
}

pub fn save_retaining(address: &str, username: &str, token: &str) -> Result<Config, String> {
    if !token.is_empty() {
        return save(address, username, token);
    }
    let saved = credentials()?.ok_or("请填写镜像仓库令牌。")?;
    let config = normalize(address, username, &saved.token)?;
    if config.config.address != saved.config.address
        || config.config.username != saved.config.username
    {
        return Err("更改地址或用户名时必须重新填写令牌。".into());
    }
    Ok(saved.config)
}

struct DockerAuthDir(std::path::PathBuf);

impl DockerAuthDir {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("devingpanel-registry-{}", Uuid::new_v4()));
        fs::create_dir(&path).map_err(|_| "无法创建临时 Docker 认证目录。")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|_| "无法保护临时 Docker 认证目录。")?;
        }
        Ok(Self(path))
    }
}

impl Drop for DockerAuthDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn finish_local_auth<T>(result: Result<T, String>, directory: DockerAuthDir) -> Result<T, String> {
    let cleanup = fs::remove_dir_all(&directory.0)
        .map_err(|_| "本机临时 Docker 认证目录清理失败，请核对并轮换令牌。".to_owned());
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}；{cleanup}")),
    }
}

fn local_login(credentials: &Credentials, directory: &DockerAuthDir) -> Result<(), String> {
    let mut child = crate::local_process::command("docker")
        .args([
            "login",
            &credentials.config.address,
            "--username",
            &credentials.config.username,
            "--password-stdin",
        ])
        .env("DOCKER_CONFIG", &directory.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法启动本机 Docker 登录。")?;
    child
        .stdin
        .take()
        .ok_or("无法输入镜像仓库令牌。")?
        .write_all(credentials.token.as_bytes())
        .map_err(|_| "无法输入镜像仓库令牌。")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err("镜像仓库登录失败，请检查地址、用户名和令牌。".into()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("镜像仓库登录超时或状态未知。".into());
            }
        }
    }
}

fn manifest_is_missing(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    (error.contains("no such manifest") || error.contains("manifest unknown"))
        && !error.contains("unauthorized")
        && !error.contains("denied")
}

fn ensure_remote_tag_absent(reference: &str, directory: &DockerAuthDir) -> Result<(), String> {
    let error_file = fs::File::create(directory.0.join("manifest-inspect.stderr"))
        .map_err(|_| "无法检查远端镜像 tag。")?;
    let mut child = crate::local_process::command("docker")
        .args(["manifest", "inspect", reference])
        .env("DOCKER_CONFIG", &directory.0)
        .stdout(Stdio::null())
        .stderr(error_file)
        .spawn()
        .map_err(|_| "无法启动远端镜像 tag 检查。")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Err("远端镜像 tag 已存在，禁止覆盖。".into());
            }
            Ok(Some(_)) => {
                let error = fs::read_to_string(directory.0.join("manifest-inspect.stderr"))
                    .map_err(|_| "无法核对远端镜像 tag 检查结果。")?;
                return if manifest_is_missing(&error) {
                    Ok(())
                } else {
                    Err("远端镜像 tag 状态未知，已停止推送。".into())
                };
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("远端镜像 tag 检查超时或状态未知，已停止推送。".into());
            }
        }
    }
}

pub fn push(reference: &str, cancel: &AtomicBool) -> Result<(), String> {
    let credentials = enabled_credentials()?.ok_or("镜像仓库发布模式未启用。")?;
    push_with_credentials(&credentials, reference, cancel)
}

pub fn push_with_credentials(
    credentials: &Credentials,
    reference: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if !credentials.config.enabled {
        return Err("镜像仓库发布模式未启用。".into());
    }
    if !reference.starts_with(&format!("{}/", credentials.config.address)) {
        return Err("项目镜像名须以已配置的 Registry 地址开头。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像推送已取消。".into());
    }
    let directory = DockerAuthDir::new()?;
    let result = (|| {
        local_login(credentials, &directory)?;
        ensure_remote_tag_absent(reference, &directory)?;
        let endpoint = crate::docker::local_engine_endpoint()?;
        let mut child = crate::docker::command_for_endpoint(&endpoint)
            .args(["image", "push", reference])
            .env("DOCKER_CONFIG", &directory.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "无法启动镜像推送。")?;
        let deadline = Instant::now() + Duration::from_secs(60 * 60);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => return Err("镜像推送失败；远端 tag 状态待核对。".into()),
                Ok(None) if !cancel.load(Ordering::Relaxed) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("镜像推送中断或超时；远端 tag 状态待核对。".into());
                }
            }
        }
    })();
    finish_local_auth(result, directory)
}

pub fn set_enabled(enabled: bool) -> Result<Config, String> {
    let conn = database::connect()?;
    with_credential_lock(|| {
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| error.to_string())?;
        let result = (|| {
            ensure_no_active_release(&conn)?;
            let count = conn
                .execute(
                    "UPDATE registry_settings SET enabled = ?1 WHERE id = 1",
                    [enabled],
                )
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("请先保存镜像仓库配置。".into());
            }
            conn.execute_batch("COMMIT")
                .map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        result?;
        load()?.ok_or("镜像仓库配置无法读取。".into())
    })
}

pub fn active_task_ids() -> Result<std::collections::HashSet<String>, String> {
    let conn = database::connect()?;
    let mut stmt = conn
        .prepare("SELECT id FROM release_tasks WHERE registry_mode = 1 AND outcome IN ('running', 'unknown', 'blocked')")
        .map_err(|error| error.to_string())?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(ids
        .into_iter()
        .filter_map(|id| Uuid::parse_str(&id).ok())
        .map(|id| id.simple().to_string())
        .collect())
}

pub fn test_login(address: &str, username: &str, token: &str) -> Result<(), String> {
    let candidate = if token.is_empty() {
        let saved = credentials()?.ok_or("请填写镜像仓库令牌。")?;
        let normalized = normalize(address, username, &saved.token)?;
        if normalized.config.address != saved.config.address
            || normalized.config.username != saved.config.username
        {
            return Err("更改地址或用户名时必须重新填写令牌。".into());
        }
        saved
    } else {
        normalize(address, username, token)?
    };
    let directory = DockerAuthDir::new()?;
    let result = local_login(&candidate, &directory);
    finish_local_auth(result, directory)
}

pub fn remove() -> Result<(), String> {
    let conn = database::connect()?;
    with_credential_lock(|| {
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| error.to_string())?;
        let result = (|| -> Result<(), String> {
            ensure_no_active_release(&conn)?;
            conn.execute(
                "INSERT OR IGNORE INTO pending_registry_credentials SELECT credential_id FROM registry_settings",
                [],
            ).map_err(|error| error.to_string())?;
            conn.execute("DELETE FROM registry_settings", [])
                .map_err(|error| error.to_string())?;
            conn.execute_batch("COMMIT")
                .map_err(|error| error.to_string())
        })();
        if let Err(error) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error);
        }
        let failed = cleanup_pending(&conn)?;
        if failed > 0 {
            Err("镜像仓库配置已删除，但令牌清理待重试。".into())
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{manifest_is_missing, normalize};

    #[test]
    fn only_missing_manifest_errors_allow_a_new_tag() {
        assert!(manifest_is_missing(
            "no such manifest: registry.example.com/app:v1"
        ));
        assert!(manifest_is_missing("manifest unknown: manifest unknown"));
        assert!(!manifest_is_missing(
            "unauthorized: authentication required"
        ));
        assert!(!manifest_is_missing("denied: manifest unknown"));
        assert!(!manifest_is_missing("connection refused"));
    }

    #[test]
    fn accepts_https_registry_with_optional_port() {
        let result = normalize("https://registry.example.com:5000/", " deploy ", "token").unwrap();
        assert_eq!(result.config.address, "registry.example.com:5000");
        assert_eq!(result.config.username, "deploy");
        assert!(!result.config.enabled);
    }

    #[test]
    fn rejects_paths_insecure_schemes_and_control_characters() {
        for address in [
            "http://registry.example.com",
            "registry.example.com/team",
            "registry.example.com:0",
            "registry.example.com:bad",
        ] {
            assert!(normalize(address, "deploy", "token").is_err(), "{address}");
        }
        assert!(normalize("registry.example.com", "deploy\nadmin", "token").is_err());
        assert!(normalize("registry.example.com", "deploy", "token\nsecret").is_err());
    }
}
