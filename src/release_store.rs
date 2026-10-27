use crate::database::connect;
use crate::host_store::Host;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Prepared,
    Building,
    Built,
    Uploading,
    Uploaded,
    Loading,
    Loaded,
    AwaitingDeployment,
    Staging,
    Staged,
    Validating,
    Validated,
    Deploying,
    Checking,
    Checked,
}

impl Phase {
    fn saved(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Building => "building",
            Self::Built => "built",
            Self::Uploading => "uploading",
            Self::Uploaded => "uploaded",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::AwaitingDeployment => "awaiting_deployment",
            Self::Staging => "staging",
            Self::Staged => "staged",
            Self::Validating => "validating",
            Self::Validated => "validated",
            Self::Deploying => "deploying",
            Self::Checking => "checking",
            Self::Checked => "checked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthResult {
    Healthy,
    RunningUnverified,
    Failed,
}

impl HealthResult {
    fn saved(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::RunningUnverified => "running_unverified",
            Self::Failed => "failed",
        }
    }
}

impl Outcome {
    fn saved(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

pub struct Intent {
    pub id: String,
    pub project_id: i64,
    pub project_name: String,
    pub host: Host,
    pub docker_endpoint: String,
    pub docker_daemon_id: String,
    pub image_reference: String,
    pub platform: String,
    pub images: Vec<(String, String, String)>,
    pub config_sha256: String,
}

#[derive(Clone)]
pub struct TaskSnapshot {
    pub id: String,
    pub project_id: i64,
    pub project_name: String,
    pub host: Host,
    pub docker_endpoint: Option<String>,
    pub docker_daemon_id: Option<String>,
    pub phase: String,
    pub outcome: String,
    pub health_result: Option<String>,
    pub detail: String,
    pub started_at: String,
}

#[derive(Clone)]
pub struct HistoricalImage {
    pub service: String,
    pub reference: String,
    pub platform: String,
    pub image_ids: Vec<String>,
}

pub struct PlannedService {
    pub service: String,
    pub reference: String,
    pub image_ids: Vec<String>,
}

struct BuiltArchiveRecord {
    service: String,
    reference: String,
    image_ids: Option<String>,
    bytes: Option<i64>,
    sha256: Option<String>,
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSnapshot> {
    Ok(TaskSnapshot {
        id: row.get(0)?,
        project_id: row.get(1)?,
        project_name: row.get(2)?,
        host: Host {
            id: row.get(3)?,
            name: row.get(4)?,
            address: row.get(5)?,
            port: row.get(6)?,
            username: row.get(7)?,
            auth: row.get(8)?,
            trusted_fingerprint: Some(row.get(9)?),
            group: String::new(),
        },
        docker_endpoint: row.get(10)?,
        docker_daemon_id: row.get(11)?,
        phase: row.get(12)?,
        outcome: row.get(13)?,
        health_result: row.get(14)?,
        detail: row.get(15)?,
        started_at: row.get(16)?,
    })
}

const TASK_QUERY: &str = "SELECT id, project_id, project_name, host_id, host_name, host_address,
    host_port, host_username, host_auth, host_fingerprint, docker_endpoint, docker_daemon_id, phase, outcome, health_result, detail, started_at
    FROM release_tasks";

#[cfg(test)]
fn unresolved_in(conn: &Connection, project_id: Option<i64>) -> Result<Vec<TaskSnapshot>, String> {
    let sql = format!(
        "{TASK_QUERY} WHERE outcome IN ('running', 'unknown', 'blocked')
        AND (?1 IS NULL OR project_id = ?1) ORDER BY started_at DESC, id DESC LIMIT 200"
    );
    conn.prepare(&sql)
        .map_err(|err| err.to_string())?
        .query_map([project_id], task_from_row)
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

pub fn recent(project_id: Option<i64>) -> Result<Vec<TaskSnapshot>, String> {
    let conn = connect()?;
    let sql = format!(
        "{TASK_QUERY} WHERE (?1 IS NULL OR project_id = ?1)
        ORDER BY CASE WHEN outcome IN ('running', 'unknown', 'blocked') THEN 0 ELSE 1 END,
                 started_at DESC, id DESC LIMIT 200"
    );
    conn.prepare(&sql)
        .map_err(|err| err.to_string())?
        .query_map([project_id], task_from_row)
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

pub fn get_unresolved(id: &str) -> Result<TaskSnapshot, String> {
    let conn = connect()?;
    let sql =
        format!("{TASK_QUERY} WHERE id = ?1 AND outcome IN ('running', 'unknown', 'blocked')");
    conn.query_row(&sql, [id], task_from_row)
        .map_err(|_| "待核实任务不存在。".into())
}

pub fn get_task(id: &str) -> Result<TaskSnapshot, String> {
    let conn = connect()?;
    conn.query_row(&format!("{TASK_QUERY} WHERE id = ?1"), [id], task_from_row)
        .map_err(|_| "历史任务不存在。".into())
}

fn previous_healthy_in(conn: &Connection, current_id: &str) -> Result<TaskSnapshot, String> {
    let current = conn
        .query_row(
            "SELECT rowid, project_id, host_id, host_address, host_port, host_username,
                host_auth, host_fingerprint, docker_endpoint, docker_daemon_id,
                phase, outcome, health_result FROM release_tasks WHERE id = ?1",
            [current_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            },
        )
        .map_err(|_| "当前发布任务不存在。")?;
    let (
        rowid,
        project_id,
        host_id,
        address,
        port,
        username,
        auth,
        fingerprint,
        endpoint,
        daemon,
        phase,
        outcome,
        health,
    ) = current;
    let endpoint = endpoint.ok_or("当前任务缺少 Docker endpoint 快照。")?;
    let daemon = daemon.ok_or("当前任务缺少 Docker daemon 身份快照。")?;
    if phase != "checked" || outcome != "blocked" || health.as_deref() != Some("healthy") {
        return Err("当前任务没有结构化的全部健康结果，不能选择上一版本。".into());
    }
    let newer: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1 AND rowid > ?2",
            params![project_id, rowid],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if newer != 0 {
        return Err("该任务之后已有其他发布记录，不能把它当作当前版本。".into());
    }
    conn.query_row(
        &format!(
            "{TASK_QUERY} WHERE rowid < ?1 AND project_id = ?2 AND host_id = ?3
            AND host_address = ?4 AND host_port = ?5 AND host_username = ?6
            AND host_auth = ?7 AND host_fingerprint = ?8 AND docker_endpoint = ?9
            AND docker_daemon_id = ?10 AND phase = 'checked' AND health_result = 'healthy'
            AND outcome = 'complete' ORDER BY rowid DESC LIMIT 1"
        ),
        params![
            rowid,
            project_id,
            host_id,
            address,
            port,
            username,
            auth,
            fingerprint,
            endpoint,
            daemon
        ],
        task_from_row,
    )
    .map_err(|_| "同一主机与 Docker daemon 没有上一条已完成且确认健康的版本。".into())
}

pub fn previous_healthy(current_id: &str) -> Result<TaskSnapshot, String> {
    previous_healthy_in(&connect()?, current_id)
}

fn resolve_absent_in(conn: &Connection, task: &TaskSnapshot) -> Result<(), String> {
    if task.docker_endpoint.is_none() || task.docker_daemon_id.is_none() {
        return Err("旧任务缺少 Docker daemon 身份快照，不能自动解除阻断。".into());
    }
    if !matches!(
        (task.phase.as_str(), task.outcome.as_str()),
        ("checked", "blocked") | ("checking", "unknown")
    ) {
        return Err("当前阶段可能仍有远端副作用，不能解除阻断。".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE release_tasks SET phase = 'checked', outcome = 'failed',
         detail = '已人工确认该 Compose 项目无容器；历史任务关闭', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND project_id = ?2 AND phase = ?3 AND outcome = ?4",
            params![task.id, task.project_id, task.phase, task.outcome],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("核实期间发布任务状态已变化。".into());
    }
    tx.execute(
        "INSERT INTO release_task_resolutions (task_id, decision, observed_containers)
         VALUES (?1, 'confirmed_absent', 0)",
        [&task.id],
    )
    .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn resolve_absent(task: &TaskSnapshot) -> Result<(), String> {
    resolve_absent_in(&connect()?, task)
}

fn create_in(conn: &Connection, intent: &Intent) -> Result<(), String> {
    let fingerprint = intent
        .host
        .trusted_fingerprint
        .as_deref()
        .ok_or("目标主机指纹尚未确认。")?;
    if uuid::Uuid::parse_str(&intent.id).is_err()
        || intent.config_sha256.len() != 64
        || !intent
            .config_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || intent.project_id <= 0
        || intent.host.id <= 0
        || !intent.docker_endpoint.starts_with("unix:///")
        || intent.docker_endpoint.len() > 512
        || intent.docker_endpoint.chars().any(char::is_control)
        || intent.docker_daemon_id.is_empty()
        || intent.docker_daemon_id.len() > 128
        || !intent
            .docker_daemon_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_'))
        || intent.images.is_empty()
        || intent.images.len() > 16
        || intent.images[0].1 != intent.image_reference
        || intent.images[0].2 != intent.platform
        || intent.images.iter().any(|(service, reference, platform)| {
            service.is_empty()
                || reference.is_empty()
                || !matches!(platform.as_str(), "linux/amd64" | "linux/arm64")
                || service.chars().any(char::is_control)
                || reference.chars().any(char::is_control)
        })
    {
        return Err("发布任务标识或配置摘要无效。".into());
    }
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let name: String = transaction
        .query_row(
            "SELECT name FROM projects WHERE id = ?1",
            [intent.project_id],
            |row| row.get(0),
        )
        .map_err(|_| "项目已被删除，不能开始发布。")?;
    if name != intent.project_name {
        return Err("项目配置在发布前变化。".into());
    }
    let target: (String, u16, String, String, Option<String>) = transaction
        .query_row(
            "SELECT address, port, username, auth, trusted_fingerprint FROM hosts WHERE id = ?1 AND deleting = 0",
            [intent.host.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .map_err(|_| "目标主机已删除或正在删除。")?;
    if target.0 != intent.host.address
        || target.1 != intent.host.port
        || target.2 != intent.host.username
        || target.3 != intent.host.auth
        || target.4.as_deref() != Some(fingerprint)
    {
        return Err("目标主机连接信息或可信指纹已变化。".into());
    }
    transaction
        .execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
         host_address, host_port, host_username, host_auth, host_fingerprint,
         docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256, phase, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 'prepared', 'running')",
            params![
                intent.id,
                intent.project_id,
                intent.host.id,
                intent.project_name,
                intent.host.name,
                intent.host.address,
                intent.host.port,
                intent.host.username,
                intent.host.auth,
                fingerprint,
                intent.docker_endpoint,
                intent.docker_daemon_id,
                intent.image_reference,
                intent.platform,
                intent.config_sha256
            ],
        )
        .map_err(|_| "该项目已有未核实的发布任务，不能重复开始。".to_owned())?;
    for (ordinal, (service, reference, platform)) in intent.images.iter().enumerate() {
        transaction.execute(
            "INSERT INTO release_task_images (task_id, ordinal, service, reference, platform) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![intent.id, ordinal as i64, service, reference, platform],
        ).map_err(|_| "发布镜像服务重复或不合法。".to_owned())?;
    }
    transaction
        .execute(
            "INSERT INTO release_materials (task_id, state, chunks) VALUES (?1, 'writing', 0)",
            [&intent.id],
        )
        .map_err(|_| "无法记录版本材料写入意图。".to_owned())?;
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn create(intent: &Intent) -> Result<(), String> {
    create_in(&connect()?, intent)
}

fn advance_in(conn: &Connection, id: &str, from: Phase, to: Phase) -> Result<(), String> {
    if !matches!(
        (from, to),
        (Phase::Prepared, Phase::Building)
            | (Phase::Building, Phase::Built)
            | (Phase::Built, Phase::Uploading)
            | (Phase::Uploading, Phase::Uploaded)
            | (Phase::Uploaded, Phase::Loading)
            | (Phase::Loading, Phase::Loaded)
            | (Phase::Loaded, Phase::AwaitingDeployment)
            | (Phase::AwaitingDeployment, Phase::Staging)
            | (Phase::Staging, Phase::Staged)
            | (Phase::Staged, Phase::Validating)
            | (Phase::Validating, Phase::Validated)
            | (Phase::Validated, Phase::Deploying)
            | (Phase::Deploying, Phase::Checking)
    ) {
        return Err("发布阶段转换无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE release_tasks SET phase = ?3, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND phase = ?2 AND outcome = 'running'",
            params![id, from.saved(), to.saved()],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("发布阶段已变化，不能重复执行。".into());
    }
    Ok(())
}

pub fn advance(id: &str, from: Phase, to: Phase) -> Result<(), String> {
    advance_in(&connect()?, id, from, to)
}

fn record_image_ids_in(
    conn: &Connection,
    task_id: &str,
    service: &str,
    ids: &[String],
) -> Result<(), String> {
    if ids.is_empty()
        || ids.len() > 4
        || ids.iter().any(|id| {
            id.len() != 71
                || !id.starts_with("sha256:")
                || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("镜像归档 ID 无效。".into());
    }
    let encoded = serde_json::to_string(ids).map_err(|_| "无法编码镜像 ID。")?;
    let changed = conn.execute(
        "UPDATE release_task_images SET image_ids = ?3
         WHERE task_id = ?1 AND service = ?2 AND image_ids IS NULL
         AND EXISTS (SELECT 1 FROM release_tasks WHERE id = ?1 AND phase = 'loading' AND outcome = 'running')",
        params![task_id, service, encoded],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("镜像加载阶段或目标服务已变化。".into());
    }
    Ok(())
}

pub fn record_image_ids(task_id: &str, service: &str, ids: &[String]) -> Result<(), String> {
    record_image_ids_in(&connect()?, task_id, service, ids)
}

fn record_archive_in(
    conn: &Connection,
    task_id: &str,
    service: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    if bytes == 0
        || bytes > i64::MAX as u64
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("本机镜像归档校验信息无效。".into());
    }
    let changed = conn.execute(
        "UPDATE release_task_images SET archive_bytes = ?3, archive_sha256 = ?4
         WHERE task_id = ?1 AND service = ?2 AND archive_bytes IS NULL
         AND EXISTS (SELECT 1 FROM release_tasks WHERE id = ?1 AND phase = 'uploading' AND outcome = 'running')",
        params![task_id, service, bytes as i64, sha256],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("镜像传输阶段或本机归档状态已变化。".into());
    }
    Ok(())
}

pub fn record_archive(
    task_id: &str,
    service: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    record_archive_in(&connect()?, task_id, service, bytes, sha256)
}

pub fn archive_record(task_id: &str, service: &str) -> Result<(u64, String), String> {
    let (bytes, sha256): (i64, String) = connect()?
        .query_row(
            "SELECT archive_bytes, archive_sha256 FROM release_task_images
             WHERE task_id = ?1 AND service = ?2 AND archive_bytes IS NOT NULL",
            params![task_id, service],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "该服务没有完整的本机镜像归档记录。")?;
    if bytes <= 0 || sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("本机镜像归档记录无效。".into());
    }
    Ok((bytes as u64, sha256))
}

pub fn planned_archive_services(task_id: &str) -> Result<Vec<(String, bool)>, String> {
    let conn = connect()?;
    let mut query = conn
        .prepare(
            "SELECT service, archive_bytes IS NOT NULL FROM release_task_images
         WHERE task_id = ?1 ORDER BY ordinal",
        )
        .map_err(|err| err.to_string())?;
    let services = query
        .query_map([task_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<(String, bool)>, _>>()
        .map_err(|err| err.to_string())?;
    if services.is_empty() {
        return Err("该任务没有镜像构建计划。".into());
    }
    Ok(services)
}

fn historical_images_in(conn: &Connection, task_id: &str) -> Result<Vec<HistoricalImage>, String> {
    let mut query = conn
        .prepare(
            "SELECT service, reference, platform, image_ids FROM release_task_images
         WHERE task_id = ?1 ORDER BY ordinal",
        )
        .map_err(|err| err.to_string())?;
    let images = query
        .query_map([task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    if images.is_empty() || images.len() > 16 {
        return Err("历史镜像计划不完整。".into());
    }
    images
        .into_iter()
        .map(|(service, reference, platform, ids)| {
            let ids = ids.ok_or("历史镜像尚未完成加载前记录。")?;
            let image_ids: Vec<String> =
                serde_json::from_str(&ids).map_err(|_| "历史镜像 ID 记录无效。")?;
            if image_ids.is_empty()
                || image_ids.len() > 4
                || image_ids.iter().any(|id| {
                    id.len() != 71
                        || !id.starts_with("sha256:")
                        || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Err("历史镜像 ID 记录无效。".into());
            }
            Ok(HistoricalImage {
                service,
                reference,
                platform,
                image_ids,
            })
        })
        .collect()
}

pub fn historical_images(task_id: &str) -> Result<Vec<HistoricalImage>, String> {
    historical_images_in(&connect()?, task_id)
}

fn record_services_in(
    conn: &Connection,
    task_id: &str,
    services: &[crate::ssh::StackService],
) -> Result<(), String> {
    if services.is_empty() || services.len() > 100 {
        return Err("整栈服务计划无效。".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let phase: String = tx
        .query_row(
            "SELECT phase FROM release_tasks WHERE id = ?1 AND outcome = 'running'",
            [task_id],
            |row| row.get(0),
        )
        .map_err(|_| "发布任务已变化，不能保存服务计划。")?;
    if phase != "validating" {
        return Err("服务计划只能在 Compose 校验阶段保存。".into());
    }
    let prior: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM release_task_services WHERE task_id = ?1",
            [task_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if prior != 0 {
        return Err("服务计划已保存，不能覆盖。".into());
    }
    let built: Vec<(String, String, String)> = tx.prepare(
        "SELECT service, reference, image_ids FROM release_task_images WHERE task_id = ?1 ORDER BY ordinal"
    ).map_err(|err| err.to_string())?
        .query_map([task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|err| err.to_string())?
        .collect::<Result<_, _>>().map_err(|err| err.to_string())?;
    if built.is_empty()
        || built.iter().any(|(name, reference, ids)| {
            !services.iter().any(|service| {
                service.service == *name
                    && service.reference == *reference
                    && serde_json::to_string(&service.image_ids)
                        .is_ok_and(|encoded| encoded == *ids)
            })
        })
    {
        return Err("整栈服务计划与已加载镜像不一致。".into());
    }
    for service in services {
        if service.service.is_empty()
            || service.reference.is_empty()
            || service.service.chars().any(char::is_control)
            || service.reference.chars().any(char::is_control)
            || (!service.image_ids.is_empty()
                && !built.iter().any(|(name, _, _)| *name == service.service))
        {
            return Err("整栈服务计划无效。".into());
        }
        let encoded = if service.image_ids.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&service.image_ids).map_err(|_| "镜像 ID 编码失败。")?)
        };
        tx.execute("INSERT INTO release_task_services (task_id, service, reference, image_ids) VALUES (?1, ?2, ?3, ?4)",
            params![task_id, service.service, service.reference, encoded])
            .map_err(|_| "整栈服务名重复，未保存服务计划。")?;
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn record_services(task_id: &str, services: &[crate::ssh::StackService]) -> Result<(), String> {
    record_services_in(&connect()?, task_id, services)
}

pub fn planned_services(task_id: &str) -> Result<Vec<PlannedService>, String> {
    let conn = connect()?;
    let mut query = conn.prepare("SELECT service, reference, image_ids FROM release_task_services WHERE task_id = ?1 ORDER BY service")
        .map_err(|err| err.to_string())?;
    let rows = query
        .query_map([task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    if rows.is_empty() || rows.len() > 100 {
        return Err("该任务没有完整的整栈服务计划，不能确认完成。".into());
    }
    rows.into_iter()
        .map(|(service, reference, encoded)| {
            let image_ids = encoded
                .map(|value| serde_json::from_str(&value).map_err(|_| "整栈镜像 ID 无效。"))
                .transpose()?
                .unwrap_or_default();
            Ok(PlannedService {
                service,
                reference,
                image_ids,
            })
        })
        .collect()
}

fn complete_checked_in(
    conn: &Connection,
    task: &TaskSnapshot,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    if task.phase != "checked"
        || task.outcome != "blocked"
        || task.health_result.as_deref() != Some("healthy")
        || task.docker_endpoint.is_none()
        || task.docker_daemon_id.is_none()
        || observed.is_empty()
        || observed.len() > 100
    {
        return Err("该任务不能确认健康完成。".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let material_ready: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM release_materials WHERE task_id = ?1 AND state = 'ready'
         AND chunks BETWEEN 1 AND 128 AND length(material_sha256) = 64
         AND material_sha256 NOT GLOB '*[^0-9a-fA-F]*'",
            [&task.id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    let built: Vec<BuiltArchiveRecord> = tx
        .prepare(
            "SELECT service, reference, image_ids, archive_bytes, archive_sha256
         FROM release_task_images WHERE task_id = ?1 ORDER BY ordinal",
        )
        .map_err(|err| err.to_string())?
        .query_map([&task.id], |row| {
            Ok(BuiltArchiveRecord {
                service: row.get(0)?,
                reference: row.get(1)?,
                image_ids: row.get(2)?,
                bytes: row.get(3)?,
                sha256: row.get(4)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|err| err.to_string())?;
    if material_ready != 1
        || built.is_empty()
        || built.len() > 16
        || built.iter().any(|image| {
            image.bytes.is_none_or(|value| value <= 0)
                || image.sha256.as_deref().is_none_or(|value| {
                    value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                || image.image_ids.as_deref().is_none_or(|value| {
                    serde_json::from_str::<Vec<String>>(value).map_or(true, |ids| ids.is_empty())
                })
        })
    {
        return Err("发布材料或自有镜像归档记录不完整。".into());
    }
    let services = tx
        .prepare(
            "SELECT service, reference, image_ids FROM release_task_services WHERE task_id = ?1",
        )
        .map_err(|err| err.to_string())?
        .query_map([&task.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    if services.len() != observed.len() {
        return Err("远端容器与已保存整栈服务计划数量不同。".into());
    }
    if built.iter().any(|built_image| {
        !services.iter().any(|(service, reference, encoded)| {
            service == &built_image.service
                && reference == &built_image.reference
                && encoded == &built_image.image_ids
        })
    }) {
        return Err("整栈服务计划与镜像归档不一致。".into());
    }
    for (service, reference, ids) in services {
        let matches: Vec<_> = observed
            .iter()
            .filter(|item| item.service == service)
            .collect();
        if matches.len() != 1 {
            return Err("远端容器服务缺失或重复。".into());
        }
        let container = matches[0];
        let built_ids: Vec<String> = ids
            .map(|value| serde_json::from_str(&value).map_err(|_| "自有镜像计划无效。"))
            .transpose()?
            .unwrap_or_default();
        if container.status != "running"
            || container.health.as_deref() != Some("healthy")
            || container.config_image != reference
            || container.image_id.len() != 71
            || !container.image_id.starts_with("sha256:")
            || !container.image_id[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || (!built_ids.is_empty() && !built_ids.contains(&container.image_id))
        {
            return Err("远端服务未健康或使用的镜像与发布计划不同。".into());
        }
        let changed = tx
            .execute(
                "UPDATE release_task_services SET confirmed_image_id = ?3
             WHERE task_id = ?1 AND service = ?2 AND confirmed_image_id IS NULL",
                params![task.id, service, container.image_id],
            )
            .map_err(|err| err.to_string())?;
        if changed != 1 {
            return Err("服务镜像确认状态已变化。".into());
        }
    }
    let changed = tx
        .execute(
            "UPDATE release_tasks SET outcome = 'complete',
         detail = '已人工复核整栈健康与版本材料；回滚执行仍未开放', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND project_id = ?2 AND host_id = ?3
           AND host_address = ?4 AND host_port = ?5 AND host_username = ?6 AND host_auth = ?7
           AND host_fingerprint = ?8 AND docker_endpoint = ?9 AND docker_daemon_id = ?10
           AND phase = 'checked' AND outcome = 'blocked' AND health_result = 'healthy'
           AND NOT EXISTS (SELECT 1 FROM release_tasks AS newer
               WHERE newer.project_id = ?2 AND newer.rowid > release_tasks.rowid)
           AND EXISTS (SELECT 1 FROM hosts WHERE id = ?3 AND deleting = 0
               AND address = ?4 AND port = ?5 AND username = ?6 AND auth = ?7
               AND trusted_fingerprint = ?8)",
            params![
                task.id,
                task.project_id,
                task.host.id,
                task.host.address,
                task.host.port,
                task.host.username,
                task.host.auth,
                task.host.trusted_fingerprint,
                task.docker_endpoint,
                task.docker_daemon_id
            ],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("任务或主机连接信息已变化，未解除阻断。".into());
    }
    tx.execute(
        "INSERT INTO release_task_resolutions (task_id, decision, observed_containers)
         VALUES (?1, 'confirmed_healthy', ?2)",
        params![task.id, observed.len() as i64],
    )
    .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn complete_checked(
    task: &TaskSnapshot,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    complete_checked_in(&connect()?, task, observed)
}

fn settle_in(
    conn: &Connection,
    id: &str,
    phase: Phase,
    outcome: Outcome,
    detail: &str,
) -> Result<(), String> {
    if phase == Phase::Checked {
        return Err("已核实任务必须记录结构化健康结果。".into());
    }
    if detail.len() > 512 || detail.chars().any(char::is_control) {
        return Err("发布结果说明无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE release_tasks SET outcome = ?3, detail = ?4, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND phase = ?2 AND outcome = 'running'",
            params![id, phase.saved(), outcome.saved(), detail],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("发布结果已变化，不能重复记录。".into());
    }
    Ok(())
}

pub fn settle(id: &str, phase: Phase, outcome: Outcome, detail: &str) -> Result<(), String> {
    settle_in(&connect()?, id, phase, outcome, detail)
}

fn settle_checked_in(
    conn: &Connection,
    id: &str,
    health: HealthResult,
    detail: &str,
) -> Result<(), String> {
    if detail.len() > 512 || detail.chars().any(char::is_control) {
        return Err("健康核实说明无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE release_tasks SET phase = 'checked', outcome = 'blocked', health_result = ?2, detail = ?3,
         updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND phase = 'checking' AND outcome = 'running' AND health_result IS NULL",
            params![id, health.saved(), detail],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("健康核实结果已变化，不能重复记录。".into());
    }
    Ok(())
}

pub fn settle_checked(id: &str, health: HealthResult, detail: &str) -> Result<(), String> {
    settle_checked_in(&connect()?, id, health, detail)
}

#[cfg(test)]
mod tests {
    use super::{
        HealthResult, Intent, Outcome, Phase, TASK_QUERY, TaskSnapshot, advance_in,
        complete_checked_in, create_in, historical_images_in, previous_healthy_in,
        record_archive_in, record_image_ids_in, record_services_in, resolve_absent_in,
        settle_checked_in, settle_in, task_from_row, unresolved_in,
    };
    use crate::database::initialize;
    use crate::host_store::Host;
    use crate::ssh::{ObservedContainer, StackService};
    use rusqlite::Connection;

    fn intent() -> Intent {
        Intent {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: 1,
            project_name: "test".into(),
            host: Host {
                id: 2,
                name: "web".into(),
                address: "203.0.113.10".into(),
                port: 22,
                username: "deploy".into(),
                group: "prod".into(),
                auth: "password".into(),
                trusted_fingerprint: Some("SHA256:verified".into()),
            },
            docker_endpoint: "unix:///var/run/docker.sock".into(),
            docker_daemon_id: "ABC:123".into(),
            image_reference: "test:v1-amd64".into(),
            platform: "linux/amd64".into(),
            images: vec![("web".into(), "test:v1-amd64".into(), "linux/amd64".into())],
            config_sha256: "a".repeat(64),
        }
    }

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES
                (1, 'test', 'path', 'Dockerfile', 'test'),
                (3, 'test', 'path2', 'Dockerfile', 'test');
             INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id, trusted_fingerprint)
             VALUES (2, 'web', '203.0.113.10', 22, 'deploy', 'password', 'prod', 'credential-ref', 'SHA256:verified');",
        ).unwrap();
    }

    #[test]
    fn intent_precedes_side_effects_and_blocks_duplicate_active_tasks() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let first = intent();
        create_in(&conn, &first).unwrap();
        assert!(create_in(&conn, &intent()).is_err());
        assert!(advance_in(&conn, &first.id, Phase::Prepared, Phase::Uploading).is_err());
        advance_in(&conn, &first.id, Phase::Prepared, Phase::Building).unwrap();
        assert!(advance_in(&conn, &first.id, Phase::Prepared, Phase::Building).is_err());
        settle_in(
            &conn,
            &first.id,
            Phase::Building,
            Outcome::Unknown,
            "结果待核实",
        )
        .unwrap();
        assert!(advance_in(&conn, &first.id, Phase::Building, Phase::Built).is_err());
        assert!(create_in(&conn, &intent()).is_err());
        let mut other = intent();
        other.project_id = 3;
        create_in(&conn, &other).unwrap();
        let state: (String, String) = conn
            .query_row(
                "SELECT phase, outcome FROM release_tasks WHERE id = ?1",
                [&first.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, ("building".into(), "unknown".into()));
    }

    #[test]
    fn rejects_untrusted_host_and_invalid_details() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let mut draft = intent();
        draft.host.trusted_fingerprint = None;
        assert!(create_in(&conn, &draft).is_err());
        draft.host.trusted_fingerprint = Some("SHA256:verified".into());
        create_in(&conn, &draft).unwrap();
        assert!(
            settle_in(
                &conn,
                &draft.id,
                Phase::Prepared,
                Outcome::Failed,
                "secret\nvalue"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_deleted_or_changed_connection_targets() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        conn.execute("UPDATE hosts SET deleting = 1 WHERE id = 2", [])
            .unwrap();
        assert!(create_in(&conn, &draft).is_err());
        conn.execute(
            "UPDATE hosts SET deleting = 0, address = '203.0.113.11' WHERE id = 2",
            [],
        )
        .unwrap();
        assert!(create_in(&conn, &draft).is_err());
        conn.execute("UPDATE hosts SET address = '203.0.113.10' WHERE id = 2", [])
            .unwrap();
        conn.execute("DELETE FROM projects WHERE id = 1", [])
            .unwrap();
        assert!(create_in(&conn, &draft).is_err());
    }

    #[test]
    fn persists_all_planned_images_before_any_release_phase() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let mut draft = intent();
        draft.images.push((
            "worker".into(),
            "test-worker:v1-amd64".into(),
            "linux/amd64".into(),
        ));
        create_in(&conn, &draft).unwrap();
        let writing: String = conn
            .query_row(
                "SELECT state FROM release_materials WHERE task_id = ?1",
                [&draft.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(writing, "writing");
        let saved = conn.prepare("SELECT service, reference FROM release_task_images WHERE task_id = ?1 ORDER BY ordinal").unwrap()
            .query_map([&draft.id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(
            saved,
            [
                ("web".into(), "test:v1-amd64".into()),
                ("worker".into(), "test-worker:v1-amd64".into())
            ]
        );
        let mut invalid = intent();
        invalid.project_id = 3;
        invalid.images.push(invalid.images[0].clone());
        assert!(create_in(&conn, &invalid).is_err());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE id = ?1",
                [&invalid.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn image_archive_ids_are_saved_once_before_remote_load() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let ids = vec![
            format!("sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64)),
        ];
        assert!(record_image_ids_in(&conn, &draft.id, "web", &ids).is_err());
        let phases = [
            Phase::Prepared,
            Phase::Building,
            Phase::Built,
            Phase::Uploading,
            Phase::Uploaded,
            Phase::Loading,
        ];
        for pair in phases.windows(2) {
            advance_in(&conn, &draft.id, pair[0], pair[1]).unwrap();
        }
        record_image_ids_in(&conn, &draft.id, "web", &ids).unwrap();
        assert!(record_image_ids_in(&conn, &draft.id, "web", &ids).is_err());
        let saved: String = conn
            .query_row(
                "SELECT image_ids FROM release_task_images WHERE task_id = ?1",
                [&draft.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Vec<String>>(&saved).unwrap(), ids);
        let historical = historical_images_in(&conn, &draft.id).unwrap();
        assert_eq!(historical.len(), 1);
        assert_eq!(historical[0].service, "web");
        assert_eq!(historical[0].image_ids, ids);
    }

    #[test]
    fn historical_images_reject_missing_or_invalid_ids() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        assert!(historical_images_in(&conn, &draft.id).is_err());
        conn.execute(
            "UPDATE release_task_images SET image_ids = '[\"bad\"]' WHERE task_id = ?1",
            [&draft.id],
        )
        .unwrap();
        assert!(historical_images_in(&conn, &draft.id).is_err());
    }

    #[test]
    fn local_archive_metadata_is_saved_once_before_upload() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let hash = "a".repeat(64);
        assert!(record_archive_in(&conn, &draft.id, "web", 5, &hash).is_err());
        advance_in(&conn, &draft.id, Phase::Prepared, Phase::Building).unwrap();
        advance_in(&conn, &draft.id, Phase::Building, Phase::Built).unwrap();
        advance_in(&conn, &draft.id, Phase::Built, Phase::Uploading).unwrap();
        assert!(record_archive_in(&conn, &draft.id, "web", 0, &hash).is_err());
        record_archive_in(&conn, &draft.id, "web", 5, &hash).unwrap();
        assert!(record_archive_in(&conn, &draft.id, "web", 5, &hash).is_err());
        let saved: (i64, String) = conn
            .query_row(
                "SELECT archive_bytes, archive_sha256 FROM release_task_images WHERE task_id = ?1",
                [&draft.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(saved, (5, hash));
    }

    #[test]
    fn only_confirmed_absence_closes_safe_terminal_phases_with_audit() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let mut task = TaskSnapshot {
            id: draft.id.clone(),
            project_id: draft.project_id,
            project_name: draft.project_name.clone(),
            host: draft.host.clone(),
            docker_endpoint: Some(draft.docker_endpoint.clone()),
            docker_daemon_id: Some(draft.docker_daemon_id.clone()),
            phase: "deploying".into(),
            outcome: "unknown".into(),
            health_result: None,
            detail: String::new(),
            started_at: String::new(),
        };
        assert!(resolve_absent_in(&conn, &task).is_err());
        let phases = [
            Phase::Prepared,
            Phase::Building,
            Phase::Built,
            Phase::Uploading,
            Phase::Uploaded,
            Phase::Loading,
            Phase::Loaded,
            Phase::AwaitingDeployment,
            Phase::Staging,
            Phase::Staged,
            Phase::Validating,
            Phase::Validated,
            Phase::Deploying,
            Phase::Checking,
        ];
        for pair in phases.windows(2) {
            advance_in(&conn, &draft.id, pair[0], pair[1]).unwrap();
        }
        settle_checked_in(&conn, &draft.id, HealthResult::Healthy, "待人工确认").unwrap();
        task.phase = "checked".into();
        task.outcome = "blocked".into();
        assert_eq!(unresolved_in(&conn, Some(1)).unwrap().len(), 1);
        resolve_absent_in(&conn, &task).unwrap();
        assert!(unresolved_in(&conn, Some(1)).unwrap().is_empty());
        assert!(resolve_absent_in(&conn, &task).is_err());
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM release_task_resolutions WHERE task_id = ?1 AND decision = 'confirmed_absent' AND observed_containers = 0", [&draft.id], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
        create_in(&conn, &intent()).unwrap();
    }

    #[test]
    fn compose_validation_requires_staged_files_and_stops_before_deployment() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let phases = [
            Phase::Prepared,
            Phase::Building,
            Phase::Built,
            Phase::Uploading,
            Phase::Uploaded,
            Phase::Loading,
            Phase::Loaded,
            Phase::AwaitingDeployment,
            Phase::Staging,
            Phase::Staged,
            Phase::Validating,
            Phase::Validated,
            Phase::Deploying,
            Phase::Checking,
        ];
        assert!(advance_in(&conn, &draft.id, Phase::Prepared, Phase::Validating).is_err());
        for pair in phases.windows(2) {
            advance_in(&conn, &draft.id, pair[0], pair[1]).unwrap();
        }
        settle_checked_in(&conn, &draft.id, HealthResult::Failed, "启动尚未实现").unwrap();
        assert!(create_in(&conn, &intent()).is_err());
    }

    #[test]
    fn checked_health_is_atomic_and_cannot_be_invented_for_old_or_failed_tasks() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        assert!(settle_checked_in(&conn, &draft.id, HealthResult::Healthy, "healthy").is_err());
        let phases = [
            Phase::Prepared,
            Phase::Building,
            Phase::Built,
            Phase::Uploading,
            Phase::Uploaded,
            Phase::Loading,
            Phase::Loaded,
            Phase::AwaitingDeployment,
            Phase::Staging,
            Phase::Staged,
            Phase::Validating,
            Phase::Validated,
            Phase::Deploying,
            Phase::Checking,
        ];
        for pair in phases.windows(2) {
            advance_in(&conn, &draft.id, pair[0], pair[1]).unwrap();
        }
        assert!(
            settle_in(
                &conn,
                &draft.id,
                Phase::Checked,
                Outcome::Failed,
                "skip result"
            )
            .is_err()
        );
        settle_checked_in(
            &conn,
            &draft.id,
            HealthResult::Healthy,
            "all services healthy",
        )
        .unwrap();
        assert!(settle_checked_in(&conn, &draft.id, HealthResult::Failed, "rewrite").is_err());
        let saved: (String, Option<String>) = conn
            .query_row(
                "SELECT outcome, health_result FROM release_tasks WHERE id = ?1",
                [&draft.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(saved, ("blocked".into(), Some("healthy".into())));
    }

    #[test]
    fn previous_version_requires_completed_health_and_same_daemon() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let first = intent();
        create_in(&conn, &first).unwrap();
        let phases = [
            Phase::Prepared,
            Phase::Building,
            Phase::Built,
            Phase::Uploading,
            Phase::Uploaded,
            Phase::Loading,
            Phase::Loaded,
            Phase::AwaitingDeployment,
            Phase::Staging,
            Phase::Staged,
            Phase::Validating,
            Phase::Validated,
            Phase::Deploying,
            Phase::Checking,
        ];
        for pair in phases.windows(2) {
            advance_in(&conn, &first.id, pair[0], pair[1]).unwrap();
        }
        settle_checked_in(&conn, &first.id, HealthResult::Healthy, "healthy").unwrap();
        assert!(create_in(&conn, &intent()).is_err());
        conn.execute(
            "UPDATE release_tasks SET outcome = 'complete' WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
        let second = intent();
        create_in(&conn, &second).unwrap();
        assert!(previous_healthy_in(&conn, &second.id).is_err());
        for pair in phases.windows(2) {
            advance_in(&conn, &second.id, pair[0], pair[1]).unwrap();
        }
        settle_checked_in(&conn, &second.id, HealthResult::Healthy, "healthy").unwrap();
        assert_eq!(previous_healthy_in(&conn, &second.id).unwrap().id, first.id);
        conn.execute(
            "UPDATE release_tasks SET docker_daemon_id = 'changed' WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
        assert!(previous_healthy_in(&conn, &second.id).is_err());
        conn.execute("UPDATE release_tasks SET docker_daemon_id = 'ABC:123', health_result = NULL WHERE id = ?1", [&first.id]).unwrap();
        assert!(previous_healthy_in(&conn, &second.id).is_err());
    }

    #[test]
    fn healthy_stack_confirmation_is_atomic_and_unlocks_first_release() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let ids = vec![format!("sha256:{}", "a".repeat(64))];
        let archive = "b".repeat(64);
        let phases = [
            Phase::Prepared,
            Phase::Building,
            Phase::Built,
            Phase::Uploading,
            Phase::Uploaded,
            Phase::Loading,
            Phase::Loaded,
            Phase::AwaitingDeployment,
            Phase::Staging,
            Phase::Staged,
            Phase::Validating,
            Phase::Validated,
            Phase::Deploying,
            Phase::Checking,
        ];
        for pair in phases.windows(2) {
            if pair[0] == Phase::Uploading {
                record_archive_in(&conn, &draft.id, "web", 123, &archive).unwrap();
            }
            if pair[0] == Phase::Loading {
                record_image_ids_in(&conn, &draft.id, "web", &ids).unwrap();
            }
            if pair[0] == Phase::Validating {
                let services = vec![
                    StackService {
                        service: "web".into(),
                        reference: draft.image_reference.clone(),
                        image_ids: ids.clone(),
                    },
                    StackService {
                        service: "db".into(),
                        reference: "postgres:16".into(),
                        image_ids: vec![],
                    },
                ];
                record_services_in(&conn, &draft.id, &services).unwrap();
                assert!(record_services_in(&conn, &draft.id, &services).is_err());
            }
            advance_in(&conn, &draft.id, pair[0], pair[1]).unwrap();
        }
        settle_checked_in(&conn, &draft.id, HealthResult::Healthy, "all healthy").unwrap();
        let task = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id = ?1"),
                [&draft.id],
                task_from_row,
            )
            .unwrap();
        let observed = vec![
            ObservedContainer {
                service: "web".into(),
                status: "running".into(),
                health: Some("healthy".into()),
                image_id: ids[0].clone(),
                config_image: draft.image_reference.clone(),
            },
            ObservedContainer {
                service: "db".into(),
                status: "running".into(),
                health: Some("healthy".into()),
                image_id: format!("sha256:{}", "c".repeat(64)),
                config_image: "postgres:16".into(),
            },
        ];
        assert!(complete_checked_in(&conn, &task, &observed[..1]).is_err());
        let mut wrong = observed.clone();
        wrong[0].image_id = format!("sha256:{}", "d".repeat(64));
        assert!(complete_checked_in(&conn, &task, &wrong).is_err());
        let mut no_health = observed.clone();
        no_health[1].health = None;
        assert!(complete_checked_in(&conn, &task, &no_health).is_err());
        assert!(complete_checked_in(&conn, &task, &observed).is_err());
        conn.execute(
            "UPDATE release_materials SET state = 'ready', chunks = 1, material_sha256 = ?2 WHERE task_id = ?1",
            rusqlite::params![draft.id, "e".repeat(64)],
        ).unwrap();
        assert!(create_in(&conn, &intent()).is_err());
        complete_checked_in(&conn, &task, &observed).unwrap();
        assert!(complete_checked_in(&conn, &task, &observed).is_err());
        let audit: (String, i64) = conn.query_row("SELECT decision, observed_containers FROM release_task_resolutions WHERE task_id = ?1", [&draft.id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(audit, ("confirmed_healthy".into(), 2));
        let saved: String = conn.query_row("SELECT confirmed_image_id FROM release_task_services WHERE task_id = ?1 AND service = 'db'", [&draft.id], |row| row.get(0)).unwrap();
        assert_eq!(saved, observed[1].image_id);
        create_in(&conn, &intent()).unwrap();
    }
}
