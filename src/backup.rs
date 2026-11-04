use crate::database;
use crate::host_store::with_credential_lock;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Utc;
use keyring::{Entry, Error as KeyringError};
use pbkdf2::pbkdf2_hmac;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use s3::{Bucket, Region, creds::Credentials};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashSet;
use uuid::Uuid;

const FORMAT: &str = "devingpanel-config-backup";
const KDF_ITERATIONS: u32 = 600_000;
const MAX_PLAIN: usize = 5 * 1024 * 1024;
const MAX_FILE: usize = 8 * 1024 * 1024;
const S3_SERVICE: &str = "devingPanel:s3-backup";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupHost {
    id: i64,
    name: String,
    address: String,
    port: u16,
    username: String,
    auth: String,
    group: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupEnv {
    host_id: Option<i64>,
    name: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupProject {
    id: i64,
    name: String,
    path: String,
    dockerfile: String,
    image: String,
    tag_rule: String,
    platform: String,
    service: String,
    manifest_mode: bool,
    compose_files: Vec<String>,
    build_args: Vec<String>,
    host_ids: Vec<i64>,
    env: Vec<BackupEnv>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupData {
    version: u32,
    hosts: Vec<BackupHost>,
    projects: Vec<BackupProject>,
    retention_keep_count: u8,
    retention_auto_enabled: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format: String,
    version: u32,
    created_at: String,
    cipher: String,
    kdf: String,
    iterations: u32,
    salt: String,
    nonce: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    header: Header,
    ciphertext: String,
}

pub struct Preview {
    pub created_at: String,
    pub source: String,
    pub hosts: usize,
    pub projects: usize,
    data: BackupData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub path_style: bool,
    pub access_key: String,
}

#[derive(Clone)]
pub struct S3Object {
    pub key: String,
    pub modified: String,
    pub bytes: i64,
}

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|error| error.to_string())
}

fn snapshot(conn: &Connection) -> Result<BackupData, String> {
    let mut hosts_stmt = sql(conn.prepare(
        "SELECT id, name, address, port, username, auth, group_name FROM hosts WHERE deleting = 0 ORDER BY id",
    ))?;
    let hosts = sql(hosts_stmt.query_map([], |row| {
        Ok(BackupHost {
            id: row.get(0)?,
            name: row.get(1)?,
            address: row.get(2)?,
            port: row.get(3)?,
            username: row.get(4)?,
            auth: row.get(5)?,
            group: row.get(6)?,
        })
    }))?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(|error| error.to_string())?;
    let mut projects_stmt = sql(conn.prepare(
        "SELECT id, name, path, dockerfile, image, tag_rule, platform, service, manifest_mode FROM projects ORDER BY id",
    ))?;
    let mut projects = sql(projects_stmt.query_map([], |row| {
        Ok(BackupProject {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            dockerfile: row.get(3)?,
            image: row.get(4)?,
            tag_rule: row.get(5)?,
            platform: row.get(6)?,
            service: row.get(7)?,
            manifest_mode: row.get(8)?,
            compose_files: Vec::new(),
            build_args: Vec::new(),
            host_ids: Vec::new(),
            env: Vec::new(),
        })
    }))?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(|error| error.to_string())?;
    for project in &mut projects {
        let mut files = sql(conn.prepare(
            "SELECT path FROM project_compose_files WHERE project_id = ?1 ORDER BY ordinal",
        ))?;
        project.compose_files = sql(files.query_map([project.id], |row| row.get(0)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|error| error.to_string())?;
        let mut args = sql(conn.prepare(
            "SELECT name FROM project_build_args WHERE project_id = ?1 ORDER BY ordinal",
        ))?;
        project.build_args = sql(args.query_map([project.id], |row| row.get(0)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|error| error.to_string())?;
        let mut targets = sql(conn.prepare(
            "SELECT ph.host_id FROM project_hosts ph
            JOIN hosts h ON h.id = ph.host_id AND h.deleting = 0
            WHERE ph.project_id = ?1 ORDER BY ph.ordinal",
        ))?;
        project.host_ids = sql(targets.query_map([project.id], |row| row.get(0)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|error| error.to_string())?;
        let mut env = sql(conn.prepare(
            "SELECT e.host_id, e.name FROM project_env e
            WHERE e.project_id = ?1 AND (e.host_id IS NULL OR EXISTS (
                SELECT 1 FROM project_hosts ph JOIN hosts h ON h.id = ph.host_id AND h.deleting = 0
                WHERE ph.project_id = e.project_id AND ph.host_id = e.host_id))
            ORDER BY e.host_id, e.ordinal",
        ))?;
        project.env = sql(env.query_map([project.id], |row| {
            Ok(BackupEnv {
                host_id: row.get(0)?,
                name: row.get(1)?,
            })
        }))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|error| error.to_string())?;
    }
    let (keep, auto): (u8, bool) = sql(conn.query_row(
        "SELECT keep_count, auto_enabled FROM retention_policy WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ))?;
    Ok(BackupData {
        version: 1,
        hosts,
        projects,
        retention_keep_count: keep,
        retention_auto_enabled: auto,
    })
}

fn validate(data: &BackupData) -> Result<(), String> {
    if data.version != 1
        || !(1..=100).contains(&data.retention_keep_count)
        || data.hosts.len() > 10_000
        || data.projects.len() > 10_000
    {
        return Err("备份配置版本或数量无效。".into());
    }
    let mut hosts = HashSet::new();
    for host in &data.hosts {
        if host.id <= 0
            || !hosts.insert(host.id)
            || host.name.trim().is_empty()
            || host.address.trim().is_empty()
            || host.username.trim().is_empty()
            || host.port == 0
            || !matches!(host.auth.as_str(), "password" | "key")
            || host.name.len() > 512
            || host.address.len() > 512
            || host.group.len() > 512
        {
            return Err("备份中的主机配置无效。".into());
        }
    }
    let groups: std::collections::HashMap<_, _> = data
        .hosts
        .iter()
        .map(|host| (host.id, host.group.as_str()))
        .collect();
    let mut projects = HashSet::new();
    for project in &data.projects {
        if project.id <= 0
            || !projects.insert(project.id)
            || project.name.trim().is_empty()
            || project.path.trim().is_empty()
            || project.name.len() > 512
            || project.path.len() > 4096
            || project.compose_files.len() > 256
            || project.build_args.len() > 256
            || project.env.len() > 2048
        {
            return Err("备份中的项目配置无效。".into());
        }
        let mut targets = HashSet::new();
        let mut group = None;
        for id in &project.host_ids {
            let current = groups.get(id).ok_or("项目引用了不存在的主机。")?;
            if !targets.insert(id) || group.is_some_and(|group| group != *current) {
                return Err("项目主机分组或引用无效。".into());
            }
            group = Some(*current);
        }
        let mut arg_names = HashSet::new();
        if project
            .build_args
            .iter()
            .any(|name| name.is_empty() || name.len() > 256 || !arg_names.insert(name))
        {
            return Err("项目构建参数名称无效。".into());
        }
        let mut env_names = HashSet::new();
        for env in &project.env {
            if env.name.is_empty()
                || env.name.len() > 256
                || env.host_id.is_some_and(|id| !targets.contains(&id))
                || !env_names.insert((env.host_id, env.name.as_str()))
            {
                return Err("项目环境变量引用或名称无效。".into());
            }
        }
    }
    Ok(())
}

pub fn encrypt_current(password: &str) -> Result<Vec<u8>, String> {
    let mut conn = database::connect()?;
    let tx = sql(conn.transaction())?;
    let data = snapshot(&tx)?;
    sql(tx.commit())?;
    encrypt(&data, password)
}

fn encrypt(data: &BackupData, password: &str) -> Result<Vec<u8>, String> {
    if !(8..=1024).contains(&password.chars().count()) {
        return Err("备份密码须为 8～1024 个字符。".into());
    }
    validate(data)?;
    let plain = serde_json::to_vec(data).map_err(|_| "无法编码备份配置。")?;
    if plain.len() > MAX_PLAIN {
        return Err("配置超过 5 MiB 备份上限。".into());
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt).map_err(|_| "无法生成备份随机盐。")?;
    getrandom::fill(&mut nonce).map_err(|_| "无法生成备份随机数。")?;
    let header = Header {
        format: FORMAT.into(),
        version: 1,
        created_at: Utc::now().to_rfc3339(),
        cipher: "AES-256-GCM".into(),
        kdf: "PBKDF2-SHA-256".into(),
        iterations: KDF_ITERATIONS,
        salt: STANDARD.encode(salt),
        nonce: STANDARD.encode(nonce),
    };
    let aad = serde_json::to_vec(&header).map_err(|_| "无法编码备份头。")?;
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, KDF_ITERATIONS, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "无法初始化备份加密。")?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plain,
                aad: &aad,
            },
        )
        .map_err(|_| "备份加密失败。")?;
    let output = serde_json::to_vec(&Envelope {
        header,
        ciphertext: STANDARD.encode(ciphertext),
    })
    .map_err(|_| "无法编码加密文件。")?;
    if output.len() > MAX_FILE {
        return Err("加密文件超过大小上限。".into());
    }
    Ok(output)
}

pub fn preview(bytes: &[u8], password: &str, source: &str) -> Result<Preview, String> {
    if password.chars().count() > 1024 {
        return Err("备份密码须为 8～1024 个字符。".into());
    }
    if bytes.len() > MAX_FILE {
        return Err("备份文件超过 8 MiB 上限。".into());
    }
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| "备份文件格式无效。")?;
    let header = &envelope.header;
    if header.format != FORMAT
        || header.version != 1
        || header.cipher != "AES-256-GCM"
        || header.kdf != "PBKDF2-SHA-256"
        || header.iterations != KDF_ITERATIONS
        || chrono::DateTime::parse_from_rfc3339(&header.created_at).is_err()
    {
        return Err("备份文件版本或加密参数不受支持。".into());
    }
    let salt = STANDARD
        .decode(&header.salt)
        .map_err(|_| "备份随机盐无效。")?;
    let nonce = STANDARD
        .decode(&header.nonce)
        .map_err(|_| "备份随机数无效。")?;
    if salt.len() != 16 || nonce.len() != 12 {
        return Err("备份加密参数长度无效。".into());
    }
    let ciphertext = STANDARD
        .decode(&envelope.ciphertext)
        .map_err(|_| "备份密文无效。")?;
    if ciphertext.len() < 16 || ciphertext.len() > MAX_PLAIN + 16 {
        return Err("备份密文大小无效。".into());
    }
    let aad = serde_json::to_vec(header).map_err(|_| "无法验证备份头。")?;
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, KDF_ITERATIONS, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "无法初始化备份解密。")?;
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| "密码错误或备份文件已被修改。")?;
    let data: BackupData = serde_json::from_slice(&plain).map_err(|_| "备份配置结构无效。")?;
    validate(&data)?;
    Ok(Preview {
        created_at: header.created_at.clone(),
        source: source.into(),
        hosts: data.hosts.len(),
        projects: data.projects.len(),
        data,
    })
}

fn ensure_idle(conn: &Connection) -> Result<(), String> {
    let busy: bool = sql(conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM release_tasks WHERE outcome IN ('running','unknown','blocked')
            UNION ALL SELECT 1 FROM rollback_tasks WHERE outcome IN ('running','unknown','blocked')
            UNION ALL SELECT 1 FROM release_batches WHERE state IN ('prepared','building','ready','running','paused','unknown')
        )", [], |row| row.get(0)))?;
    if busy {
        Err("有发布、回滚或待核实任务，暂不能恢复配置。".into())
    } else {
        Ok(())
    }
}

pub fn restore(preview: &Preview) -> Result<Vec<String>, String> {
    validate(&preview.data)?;
    with_credential_lock(|| {
        let mut conn = database::connect()?;
        restore_in(&mut conn, &preview.data)
    })?;
    let mut warnings = Vec::new();
    match crate::host_store::cleanup_orphaned_credentials() {
        Ok(Some(warning)) | Err(warning) => warnings.push(warning),
        Ok(None) => {}
    }
    match crate::env_store::cleanup_pending() {
        Ok(Some(warning)) | Err(warning) => warnings.push(warning),
        Ok(None) => {}
    }
    if let Err(warning) = crate::registry::cleanup_pending_credentials() {
        warnings.push(warning);
    }
    if let Err(warning) = cleanup_artifacts() {
        warnings.push(warning);
    }
    Ok(warnings)
}

fn restore_in(conn: &mut Connection, data: &BackupData) -> Result<(), String> {
    validate(data)?;
    let tx = sql(conn.transaction_with_behavior(TransactionBehavior::Immediate))?;
    ensure_idle(&tx)?;
    sql(tx.execute_batch("PRAGMA defer_foreign_keys = ON"))?;
    sql(tx.execute(
        "INSERT OR IGNORE INTO pending_credentials SELECT credential_id FROM hosts",
        [],
    ))?;
    sql(tx.execute("INSERT OR IGNORE INTO pending_env_credentials SELECT credential_id FROM project_env WHERE credential_id IS NOT NULL", []))?;
    sql(tx.execute("INSERT OR IGNORE INTO pending_registry_credentials SELECT credential_id FROM registry_settings", []))?;
    sql(tx.execute("INSERT OR IGNORE INTO pending_restore_artifacts (kind,id,chunks)
            SELECT 'task', t.id, CASE WHEN m.state = 'writing' THEN 128 ELSE COALESCE(m.chunks, 0) END
            FROM release_tasks t LEFT JOIN release_materials m ON m.task_id = t.id", []))?;
    sql(tx.execute(
        "INSERT OR IGNORE INTO pending_restore_artifacts (kind,id,chunks)
            SELECT 'batch', id, 0 FROM release_batches",
        [],
    ))?;
    for table in [
        "release_batch_attempts",
        "release_batch_targets",
        "release_batch_images",
        "release_batches",
        "rollback_reconciliations",
        "rollback_uploaded_archives",
        "rollback_tasks",
        "project_current_version",
        "release_predecessors",
        "release_task_health_reconciliations",
        "release_task_resolutions",
        "release_uploaded_archives",
        "release_task_services",
        "release_task_images",
        "retention_cleanup",
        "release_materials",
        "release_tasks",
        "project_env",
        "project_hosts",
        "project_build_args",
        "project_compose_files",
        "restore_review",
        "projects",
        "hosts",
        "registry_settings",
    ] {
        sql(tx.execute(&format!("DELETE FROM {table}"), []))?;
    }
    for host in &data.hosts {
        sql(tx.execute(
            "INSERT INTO hosts (id,name,address,port,username,auth,group_name,credential_id)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                host.id,
                host.name,
                host.address,
                host.port,
                host.username,
                host.auth,
                host.group,
                Uuid::new_v4().to_string()
            ],
        ))?;
    }
    for project in &data.projects {
        sql(tx.execute("INSERT INTO projects (id,name,path,dockerfile,image,tag_rule,platform,service,manifest_mode)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![project.id, project.name, project.path, project.dockerfile,
                    project.image, project.tag_rule, project.platform, project.service, project.manifest_mode]))?;
        sql(tx.execute(
            "INSERT INTO restore_review (project_id) VALUES (?1)",
            [project.id],
        ))?;
        for (ordinal, file) in project.compose_files.iter().enumerate() {
            sql(tx.execute(
                "INSERT INTO project_compose_files VALUES (?1,?2,?3)",
                params![project.id, ordinal as i64, file],
            ))?;
        }
        for (ordinal, name) in project.build_args.iter().enumerate() {
            sql(tx.execute(
                "INSERT INTO project_build_args VALUES (?1,?2,?3,'')",
                params![project.id, ordinal as i64, name],
            ))?;
        }
        for (ordinal, id) in project.host_ids.iter().enumerate() {
            sql(tx.execute(
                "INSERT INTO project_hosts VALUES (?1,?2,?3)",
                params![project.id, id, ordinal as i64],
            ))?;
        }
        for (ordinal, env) in project.env.iter().enumerate() {
            sql(tx.execute(
                "INSERT INTO project_env (project_id,host_id,ordinal,name,value)
                    VALUES (?1,?2,?3,?4,'')",
                params![project.id, env.host_id, ordinal as i64, env.name],
            ))?;
        }
    }
    sql(tx.execute(
        "UPDATE retention_policy SET keep_count = ?1, auto_enabled = ?2,
            last_checked_at = NULL WHERE id = 1",
        params![data.retention_keep_count, data.retention_auto_enabled],
    ))?;
    sql(tx.commit())?;
    Ok(())
}

pub fn cleanup_artifacts() -> Result<(), String> {
    let conn = database::connect()?;
    let mut stmt = sql(conn.prepare("SELECT kind,id,chunks FROM pending_restore_artifacts"))?;
    let pending = sql(stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }))?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(|error| error.to_string())?;
    let mut failed = false;
    for (kind, id, chunks) in pending {
        let result = if kind == "task" {
            crate::release_materials::remove_orphaned(&id, chunks)
                .and_then(|_| crate::release_archive::remove_orphaned(&id, false))
        } else {
            crate::release_archive::remove_orphaned(&id, true)
        };
        if result.is_ok() {
            sql(conn.execute(
                "DELETE FROM pending_restore_artifacts WHERE kind = ?1 AND id = ?2",
                params![kind, id],
            ))?;
        } else {
            failed = true;
        }
    }
    if failed {
        Err("旧版发布材料清理待重试，请检查本机数据目录。".into())
    } else {
        Ok(())
    }
}

pub fn needs_review(project_id: i64) -> Result<bool, String> {
    let conn = database::connect()?;
    sql(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM restore_review WHERE project_id = ?1)",
        [project_id],
        |row| row.get(0),
    ))
}

pub fn confirm_project(project_id: i64) -> Result<(), String> {
    let conn = database::connect()?;
    sql(conn.execute(
        "DELETE FROM restore_review WHERE project_id = ?1",
        [project_id],
    ))?;
    Ok(())
}

fn s3_entry(id: &str) -> Result<Entry, String> {
    Entry::new(S3_SERVICE, id).map_err(|_| "系统凭据管理器不可用。".into())
}

pub fn load_s3() -> Result<Option<S3Config>, String> {
    let conn = database::connect()?;
    sql(conn
        .query_row(
            "SELECT endpoint,region,bucket,prefix,path_style,access_key
        FROM s3_backup_settings WHERE id = 1",
            [],
            |row| {
                Ok(S3Config {
                    endpoint: row.get(0)?,
                    region: row.get(1)?,
                    bucket: row.get(2)?,
                    prefix: row.get(3)?,
                    path_style: row.get(4)?,
                    access_key: row.get(5)?,
                })
            },
        )
        .optional())
}

fn validate_s3(config: &S3Config) -> Result<(), String> {
    let endpoint = url::Url::parse(&config.endpoint).map_err(|_| "S3 Endpoint 无效。")?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || config.region.trim().is_empty()
        || config.region.len() > 128
        || !config
            .region
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        || !(3..=63).contains(&config.bucket.len())
        || !config
            .bucket
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-' || ch == b'.')
        || config.bucket.starts_with('-')
        || config.bucket.starts_with('.')
        || config.bucket.ends_with('-')
        || config.bucket.ends_with('.')
        || config.access_key.trim().is_empty()
        || config.access_key.len() > 256
        || config.access_key.chars().any(char::is_control)
        || config.prefix.len() > 512
        || config.prefix.contains("..")
        || config.prefix.chars().any(char::is_control)
    {
        return Err(
            "S3 配置无效：须使用 HTTPS Endpoint 并填写 Region、Bucket 和 Access Key。".into(),
        );
    }
    Ok(())
}

fn s3_credentials() -> Result<(S3Config, String), String> {
    let conn = database::connect()?;
    let (config, id): (S3Config, String) = conn
        .query_row(
            "SELECT endpoint,region,bucket,prefix,path_style,access_key,credential_id
         FROM s3_backup_settings WHERE id = 1",
            [],
            |row| {
                Ok((
                    S3Config {
                        endpoint: row.get(0)?,
                        region: row.get(1)?,
                        bucket: row.get(2)?,
                        prefix: row.get(3)?,
                        path_style: row.get(4)?,
                        access_key: row.get(5)?,
                    },
                    row.get(6)?,
                ))
            },
        )
        .map_err(|_| "请先保存 S3 配置。".to_string())?;
    let secret = s3_entry(&id)?
        .get_password()
        .map_err(|_| "S3 密钥不可读取，请重新配置。")?;
    Ok((config, secret))
}

pub fn save_s3(config: S3Config, secret: &str) -> Result<(), String> {
    validate_s3(&config)?;
    if secret.len() > 4096 || secret.chars().any(char::is_control) {
        return Err("S3 Secret Key 无效。".into());
    }
    with_credential_lock(|| {
        let conn = database::connect()?;
        let old: Option<(S3Config, String)> = sql(conn.query_row(
            "SELECT endpoint,region,bucket,prefix,path_style,access_key,credential_id FROM s3_backup_settings WHERE id = 1",
            [], |row| Ok((S3Config { endpoint: row.get(0)?, region: row.get(1)?, bucket: row.get(2)?,
                prefix: row.get(3)?, path_style: row.get(4)?, access_key: row.get(5)? }, row.get(6)?)),
        ).optional())?;
        if secret.is_empty()
            && old
                .as_ref()
                .is_none_or(|(saved, _)| saved.access_key != config.access_key)
        {
            return Err("首次保存或更换 Access Key 时须填写 Secret Key。".into());
        }
        let id = if secret.is_empty() {
            old.as_ref().unwrap().1.clone()
        } else {
            Uuid::new_v4().to_string()
        };
        if !secret.is_empty() {
            sql(conn.execute("INSERT INTO pending_s3_credentials VALUES (?1)", [&id]))?;
            s3_entry(&id)?
                .set_password(secret)
                .map_err(|_| "S3 密钥写入系统凭据管理器失败。")?;
        }
        let tx = sql(conn.unchecked_transaction())?;
        if let Some((_, old_id)) = &old
            && old_id != &id
        {
            sql(tx.execute(
                "INSERT OR IGNORE INTO pending_s3_credentials VALUES (?1)",
                [old_id],
            ))?;
        }
        sql(tx.execute(
            "INSERT INTO s3_backup_settings VALUES (1,?1,?2,?3,?4,?5,?6,?7)
            ON CONFLICT(id) DO UPDATE SET endpoint=excluded.endpoint,region=excluded.region,
            bucket=excluded.bucket,prefix=excluded.prefix,path_style=excluded.path_style,
            access_key=excluded.access_key,credential_id=excluded.credential_id",
            params![
                config.endpoint,
                config.region,
                config.bucket,
                config.prefix,
                config.path_style,
                config.access_key,
                id
            ],
        ))?;
        sql(tx.execute(
            "DELETE FROM pending_s3_credentials WHERE credential_id = ?1",
            [&id],
        ))?;
        sql(tx.commit())?;
        cleanup_s3_credentials()
    })
}

pub fn remove_s3() -> Result<(), String> {
    with_credential_lock(|| {
        let conn = database::connect()?;
        let tx = sql(conn.unchecked_transaction())?;
        sql(tx.execute(
            "INSERT OR IGNORE INTO pending_s3_credentials
            SELECT credential_id FROM s3_backup_settings",
            [],
        ))?;
        sql(tx.execute("DELETE FROM s3_backup_settings", []))?;
        sql(tx.commit())?;
        cleanup_s3_credentials()
    })
}

fn cleanup_s3_credentials() -> Result<(), String> {
    let conn = database::connect()?;
    let mut stmt = sql(conn.prepare("SELECT credential_id FROM pending_s3_credentials"))?;
    let ids = sql(stmt.query_map([], |row| row.get::<_, String>(0)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    for id in ids {
        let active: bool = sql(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM s3_backup_settings WHERE credential_id = ?1)",
            [&id],
            |row| row.get(0),
        ))?;
        if !active {
            match s3_entry(&id)?.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => {}
                Err(_) => return Err("旧 S3 密钥清理失败，稍后重试。".into()),
            }
        }
        sql(conn.execute(
            "DELETE FROM pending_s3_credentials WHERE credential_id = ?1",
            [&id],
        ))?;
    }
    Ok(())
}

pub fn cleanup_pending_s3_credentials() -> Result<(), String> {
    with_credential_lock(cleanup_s3_credentials)
}

fn bucket(config: &S3Config, secret: &str) -> Result<Box<Bucket>, String> {
    validate_s3(config)?;
    let region = Region::Custom {
        region: config.region.clone(),
        endpoint: config.endpoint.clone(),
    };
    let credentials = Credentials::new(Some(&config.access_key), Some(secret), None, None, None)
        .map_err(|_| "S3 凭据无效。")?;
    let bucket =
        Bucket::new(&config.bucket, region, credentials).map_err(|_| "无法创建 S3 客户端。")?;
    Ok(if config.path_style {
        bucket.with_path_style()
    } else {
        bucket
    })
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法启动 S3 网络操作。".into())
}

fn object_prefix(config: &S3Config) -> String {
    let trimmed = config.prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

pub fn test_s3() -> Result<(), String> {
    let (config, secret) = s3_credentials()?;
    let bucket = bucket(&config, &secret)?;
    runtime()?.block_on(async {
        bucket
            .list_page(object_prefix(&config), None, None, None, Some(1))
            .await
            .map(|_| ())
            .map_err(|_| "S3 连接或权限检查失败。".into())
    })
}

pub fn list_s3() -> Result<Vec<S3Object>, String> {
    let (config, secret) = s3_credentials()?;
    let bucket = bucket(&config, &secret)?;
    let prefix = object_prefix(&config);
    runtime()?.block_on(async {
        let pages = bucket
            .list(prefix.clone(), None)
            .await
            .map_err(|_| "无法列出 S3 备份。")?;
        let mut objects: Vec<_> = pages
            .into_iter()
            .flat_map(|page| page.contents)
            .filter(|object| object.key.starts_with(&prefix) && object.key.ends_with(".dpbackup"))
            .map(|object| S3Object {
                key: object.key,
                modified: object.last_modified,
                bytes: object.size as i64,
            })
            .collect();
        objects.sort_by(|a, b| b.key.cmp(&a.key));
        objects.truncate(200);
        Ok(objects)
    })
}

pub fn upload_s3(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_FILE {
        return Err("备份文件超过大小上限。".into());
    }
    let (config, secret) = s3_credentials()?;
    let bucket = bucket(&config, &secret)?;
    let key = format!(
        "{}{}-{}.dpbackup",
        object_prefix(&config),
        Utc::now().format("%Y%m%d-%H%M%S"),
        Uuid::new_v4()
    );
    runtime()?.block_on(async {
        bucket
            .put_object(&key, bytes)
            .await
            .map_err(|_| "上传 S3 备份失败。")?;
        Ok(key)
    })
}

fn checked_key(config: &S3Config, key: &str) -> Result<(), String> {
    if !key.starts_with(&object_prefix(config))
        || !key.ends_with(".dpbackup")
        || key.contains("..")
        || key.chars().any(char::is_control)
    {
        return Err("S3 备份对象路径无效。".into());
    }
    Ok(())
}

pub fn download_s3(key: &str) -> Result<Vec<u8>, String> {
    let (config, secret) = s3_credentials()?;
    checked_key(&config, key)?;
    let bucket = bucket(&config, &secret)?;
    runtime()?.block_on(async {
        let response = bucket
            .get_object(key)
            .await
            .map_err(|_| "下载 S3 备份失败。")?;
        if response.as_slice().len() > MAX_FILE {
            return Err("S3 备份超过大小上限。".into());
        }
        Ok(response.as_slice().to_vec())
    })
}

pub fn delete_s3(key: &str) -> Result<(), String> {
    let (config, secret) = s3_credentials()?;
    checked_key(&config, key)?;
    let bucket = bucket(&config, &secret)?;
    runtime()?.block_on(async {
        bucket
            .delete_object(key)
            .await
            .map(|_| ())
            .map_err(|_| "删除 S3 备份失败。".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_references_and_duplicate_names() {
        let mut data = BackupData {
            version: 1,
            hosts: vec![],
            projects: vec![BackupProject {
                id: 1,
                name: "p".into(),
                path: "/tmp/p".into(),
                dockerfile: "Dockerfile".into(),
                image: "p".into(),
                tag_rule: "{timestamp}".into(),
                platform: "auto".into(),
                service: "".into(),
                manifest_mode: false,
                compose_files: vec![],
                build_args: vec![],
                host_ids: vec![7],
                env: vec![],
            }],
            retention_keep_count: 3,
            retention_auto_enabled: false,
        };
        assert!(validate(&data).is_err());
        data.projects[0].host_ids.clear();
        data.projects[0].build_args = vec!["A".into(), "A".into()];
        assert!(validate(&data).is_err());
    }

    #[test]
    fn rejects_non_https_s3_endpoint() {
        let config = S3Config {
            endpoint: "http://example.com".into(),
            region: "x".into(),
            bucket: "b".into(),
            prefix: "".into(),
            path_style: true,
            access_key: "a".into(),
        };
        assert!(validate_s3(&config).is_err());
    }

    #[test]
    fn encrypted_backup_rejects_wrong_password_and_tampering() {
        let data = BackupData {
            version: 1,
            hosts: vec![],
            projects: vec![],
            retention_keep_count: 3,
            retention_auto_enabled: false,
        };
        let bytes = encrypt(&data, "long password 123").unwrap();
        assert_eq!(
            preview(&bytes, "long password 123", "test").unwrap().hosts,
            0
        );
        assert!(preview(&bytes, "wrong password", "test").is_err());
        let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        envelope["header"]["created_at"] = serde_json::json!("2020-01-01T00:00:00Z");
        assert!(
            preview(
                &serde_json::to_vec(&envelope).unwrap(),
                "long password 123",
                "test"
            )
            .is_err()
        );
        let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        envelope["ciphertext"] = serde_json::json!("AAAA");
        assert!(
            preview(
                &serde_json::to_vec(&envelope).unwrap(),
                "long password 123",
                "test"
            )
            .is_err()
        );
    }

    #[test]
    fn backup_excludes_secrets_and_restore_requires_review() {
        let conn = Connection::open_in_memory().unwrap();
        database::initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO hosts
            (id,name,address,port,username,auth,credential_id,trusted_fingerprint)
            VALUES (1,'server','example.com',22,'deploy','password','ssh-secret-ref','SHA256:old');
            INSERT INTO projects (id,name,path,dockerfile,image)
            VALUES (1,'app','/source','Dockerfile','app');
            INSERT INTO project_hosts VALUES (1,1,0);
            INSERT INTO project_build_args VALUES (1,0,'TOKEN','build-secret');
            INSERT INTO project_env (project_id,ordinal,name,value)
            VALUES (1,0,'API_TOKEN','env-secret');",
        )
        .unwrap();
        let data = snapshot(&conn).unwrap();
        let bytes = encrypt(&data, "long password 123").unwrap();
        let encrypted_text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!encrypted_text.contains("server"));
        assert!(!encrypted_text.contains("build-secret"));
        let preview = preview(&bytes, "long password 123", "test").unwrap();
        let plain = serde_json::to_string(&preview.data).unwrap();
        for excluded in ["ssh-secret-ref", "SHA256:old", "build-secret", "env-secret"] {
            assert!(!plain.contains(excluded));
        }

        let mut restored = Connection::open_in_memory().unwrap();
        database::initialize(&restored).unwrap();
        restore_in(&mut restored, &preview.data).unwrap();
        let (credential, fingerprint): (String, Option<String>) = restored
            .query_row(
                "SELECT credential_id,trusted_fingerprint FROM hosts WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_ne!(credential, "ssh-secret-ref");
        assert!(fingerprint.is_none());
        let arg: String = restored
            .query_row("SELECT value FROM project_build_args", [], |row| row.get(0))
            .unwrap();
        let env: String = restored
            .query_row("SELECT value FROM project_env", [], |row| row.get(0))
            .unwrap();
        assert!(arg.is_empty() && env.is_empty());
        let review: i64 = restored
            .query_row("SELECT COUNT(*) FROM restore_review", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review, 1);
    }

    #[test]
    fn restore_rolls_back_after_a_database_write_fails() {
        let mut conn = Connection::open_in_memory().unwrap();
        database::initialize(&conn).unwrap();
        conn.execute("INSERT INTO projects (id,name,path,dockerfile,image) VALUES (1,'existing','/a','Dockerfile','a')", []).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_restored_project BEFORE INSERT ON projects
            WHEN NEW.id = 2 BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
        )
        .unwrap();
        let mut data = BackupData {
            version: 1,
            hosts: vec![],
            projects: vec![],
            retention_keep_count: 3,
            retention_auto_enabled: false,
        };
        data.projects.push(BackupProject {
            id: 2,
            name: "new".into(),
            path: "/b".into(),
            dockerfile: "Dockerfile".into(),
            image: "b".into(),
            tag_rule: "{timestamp}".into(),
            platform: "auto".into(),
            service: "".into(),
            manifest_mode: false,
            compose_files: vec![],
            build_args: vec![],
            host_ids: vec![],
            env: vec![],
        });
        assert!(restore_in(&mut conn, &data).is_err());
        let name: String = conn
            .query_row("SELECT name FROM projects WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "existing");
    }
}
