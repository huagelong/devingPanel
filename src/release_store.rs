use crate::database::connect;
use crate::host_store::Host;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::HashMap;

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
    pub execution_finished_at: Option<String>,
    pub image_reference: String,
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

pub struct ConfirmedService {
    pub service: String,
    pub reference: String,
    pub image_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionClass {
    UnresolvedHold,
    LatestCompleted,
    PreviousCompleted,
    HistoricalReview,
    UncertainHistory,
}

pub struct RetentionEntry {
    pub task_id: String,
    pub class: RetentionClass,
}

pub struct RetentionPlan {
    pub unresolved: bool,
    pub entries: Vec<RetentionEntry>,
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
        execution_finished_at: row.get(17)?,
        image_reference: row.get(18)?,
    })
}

const TASK_QUERY: &str = "SELECT id, project_id, project_name, host_id, host_name, host_address,
    host_port, host_username, host_auth, host_fingerprint, docker_endpoint, docker_daemon_id, phase, outcome, health_result, detail, started_at, execution_finished_at, image_reference
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

fn retention_plan_in(conn: &Connection, project_id: i64) -> Result<RetentionPlan, String> {
    if project_id <= 0 {
        return Err("项目 ID 无效。".into());
    }
    let rows = conn
        .prepare(
            "SELECT id, docker_endpoint, docker_daemon_id, phase, outcome, health_result
         FROM release_tasks WHERE project_id = ?1 ORDER BY rowid DESC",
        )
        .map_err(|err| err.to_string())?
        .query_map([project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let rollback_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rollback_tasks WHERE project_id = ?1
         AND outcome IN ('running', 'unknown', 'blocked')",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    let unresolved = rollback_active != 0
        || rows
            .iter()
            .any(|row| matches!(row.4.as_str(), "running" | "unknown" | "blocked"));
    let pointers = conn
        .prepare(
            "SELECT docker_daemon_id, task_id FROM project_current_version WHERE project_id = ?1",
        )
        .map_err(|err| err.to_string())?
        .query_map([project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(|err| err.to_string())?;
    let mut current = HashMap::new();
    for (daemon, id) in pointers {
        if !rows.iter().any(|row| {
            row.0 == id
                && row.1.is_some()
                && row.2.as_deref() == Some(daemon.as_str())
                && row.3 == "checked"
                && row.4 == "complete"
                && row.5.as_deref() == Some("healthy")
        }) {
            continue;
        }
        let predecessor: Option<String> = conn
            .query_row(
                "SELECT predecessor_task_id FROM release_predecessors WHERE task_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| err.to_string())?
            .flatten();
        current.insert(daemon, (id, predecessor));
    }
    let entries = rows
        .into_iter()
        .map(|(task_id, endpoint, daemon, phase, outcome, health)| {
            let class = if unresolved {
                RetentionClass::UnresolvedHold
            } else if phase == "checked"
                && outcome == "complete"
                && health.as_deref() == Some("healthy")
                && endpoint.is_some()
                && daemon.is_some()
            {
                match daemon.as_ref().and_then(|daemon| current.get(daemon)) {
                    Some((current_id, predecessor)) => {
                        if task_id == *current_id {
                            RetentionClass::LatestCompleted
                        } else if predecessor.as_deref() == Some(task_id.as_str()) {
                            RetentionClass::PreviousCompleted
                        } else {
                            RetentionClass::HistoricalReview
                        }
                    }
                    _ => RetentionClass::UncertainHistory,
                }
            } else {
                RetentionClass::UncertainHistory
            };
            RetentionEntry { task_id, class }
        })
        .collect();
    Ok(RetentionPlan {
        unresolved,
        entries,
    })
}

pub fn retention_plan(project_id: i64) -> Result<RetentionPlan, String> {
    retention_plan_in(&connect()?, project_id)
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

pub fn completed_versions_for(task: &TaskSnapshot) -> Result<Vec<TaskSnapshot>, String> {
    completed_versions_for_in(&connect()?, task)
}

pub fn current_completed_for(task: &TaskSnapshot) -> Result<TaskSnapshot, String> {
    let conn = connect()?;
    let daemon = task
        .docker_daemon_id
        .as_deref()
        .ok_or("任务缺少 Docker daemon 身份快照。")?;
    conn.query_row(
        &format!(
            "{TASK_QUERY} WHERE id = (SELECT task_id FROM project_current_version
            WHERE project_id = ?1 AND docker_daemon_id = ?2)
            AND project_id = ?1 AND docker_daemon_id = ?2 AND phase = 'checked'
            AND outcome = 'complete' AND health_result = 'healthy'"
        ),
        params![task.project_id, daemon],
        task_from_row,
    )
    .map_err(|_| "项目没有同一 Docker daemon 的已确认健康当前版本。".into())
}

fn completed_versions_for_in(
    conn: &Connection,
    task: &TaskSnapshot,
) -> Result<Vec<TaskSnapshot>, String> {
    let _endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("任务缺少 Docker endpoint 快照。")?;
    let daemon = task
        .docker_daemon_id
        .as_deref()
        .ok_or("任务缺少 Docker daemon 身份快照。")?;
    let _fingerprint = task
        .host
        .trusted_fingerprint
        .as_deref()
        .ok_or("任务缺少 SSH 指纹快照。")?;
    let query = format!(
        "{TASK_QUERY} WHERE project_id = ?1 AND docker_daemon_id = ?2
        AND phase = 'checked' AND outcome = 'complete'
        AND health_result = 'healthy' ORDER BY rowid DESC"
    );
    conn.prepare(&query)
        .map_err(|err| err.to_string())?
        .query_map(params![task.project_id, daemon], task_from_row)
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

pub fn confirmed_services(task_id: &str) -> Result<Vec<ConfirmedService>, String> {
    confirmed_services_in(&connect()?, task_id)
}

fn confirmed_services_in(
    conn: &Connection,
    task_id: &str,
) -> Result<Vec<ConfirmedService>, String> {
    let rows = conn
        .prepare(
            "SELECT service, reference, confirmed_image_id FROM release_task_services
         WHERE task_id = ?1 ORDER BY service",
        )
        .map_err(|err| err.to_string())?
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
        return Err("已完成版本的服务快照缺失。".into());
    }
    rows.into_iter()
        .map(|(service, reference, image_id)| {
            let image_id = image_id.ok_or("已完成版本的服务镜像 ID 缺失。")?;
            if image_id.len() != 71
                || !image_id.starts_with("sha256:")
                || !image_id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("已完成版本的服务镜像 ID 无效。".into());
            }
            Ok(ConfirmedService {
                service,
                reference,
                image_id,
            })
        })
        .collect()
}

fn previous_healthy_in(conn: &Connection, current_id: &str) -> Result<TaskSnapshot, String> {
    let current = conn
        .query_row(
            "SELECT rowid, project_id, docker_endpoint, docker_daemon_id,
                phase, outcome, health_result FROM release_tasks WHERE id = ?1",
            [current_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .map_err(|_| "当前发布任务不存在。")?;
    let (rowid, project_id, endpoint, daemon, phase, outcome, health) = current;
    endpoint.ok_or("当前任务缺少 Docker endpoint 快照。")?;
    let daemon = daemon.ok_or("当前任务缺少 Docker daemon 身份快照。")?;
    if phase != "checked" || outcome != "blocked" || health.as_deref() != Some("healthy") {
        return Err("当前任务没有结构化的全部健康结果，不能选择上一版本。".into());
    }
    let newer: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1
             AND docker_daemon_id = ?2 AND rowid > ?3",
            params![project_id, daemon, rowid],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if newer != 0 {
        return Err("该任务之后已有其他发布记录，不能把它当作当前版本。".into());
    }
    conn.query_row(
        &format!(
            "{TASK_QUERY} WHERE rowid < ?1 AND project_id = ?2 AND docker_daemon_id = ?3
            AND phase = 'checked' AND health_result = 'healthy'
            AND outcome = 'complete' ORDER BY rowid DESC LIMIT 1"
        ),
        params![rowid, project_id, daemon],
        task_from_row,
    )
    .map_err(|_| "同一 Docker daemon 没有上一条已完成且确认健康的版本。".into())
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
    resolve_batch_failure_in(&tx, &task.id)?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn resolve_absent(task: &TaskSnapshot) -> Result<(), String> {
    resolve_absent_in(&connect()?, task)
}

fn resolve_predeploy_unchanged_in(
    conn: &Connection,
    task: &TaskSnapshot,
    previous_id: &str,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    if task.phase != "uploading"
        || task.outcome != "unknown"
        || task.docker_endpoint.is_none()
        || task.docker_daemon_id.is_none()
    {
        return Err("仅镜像传输结果不明的任务可按旧版本核实关闭。".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let source_rowid: i64 = tx
        .query_row(
            "SELECT rowid FROM release_tasks WHERE id = ?1 AND project_id = ?2
         AND phase = 'uploading' AND outcome = 'unknown' AND docker_endpoint = ?3
         AND docker_daemon_id = ?4 AND host_id = ?5 AND host_address = ?6
         AND host_port = ?7 AND host_username = ?8 AND host_auth = ?9 AND host_fingerprint = ?10",
            params![
                task.id,
                task.project_id,
                task.docker_endpoint,
                task.docker_daemon_id,
                task.host.id,
                task.host.address,
                task.host.port,
                task.host.username,
                task.host.auth,
                task.host.trusted_fingerprint
            ],
            |row| row.get(0),
        )
        .map_err(|_| "发布任务或可信主机快照已变化。")?;
    let (current_id, previous_rowid): (String, i64) = tx
        .query_row(
            "SELECT v.task_id, t.rowid FROM project_current_version v
         JOIN release_tasks t ON t.id = v.task_id
         WHERE v.project_id = ?1 AND v.docker_daemon_id = ?2
         AND t.phase = 'checked' AND t.outcome = 'complete' AND t.health_result = 'healthy'",
            params![task.project_id, task.docker_daemon_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "已确认的旧健康版本已变化。")?;
    if current_id != previous_id || previous_rowid >= source_rowid {
        return Err("当前版本指针或发布顺序已变化。".into());
    }
    let expected = confirmed_services_in(&tx, previous_id)?;
    if expected.len() != observed.len()
        || expected.iter().any(|service| {
            observed
                .iter()
                .filter(|item| {
                    item.service == service.service
                        && item.config_image == service.reference
                        && item.image_id == service.image_id
                        && item.status == "running"
                        && item.health.as_deref() == Some("healthy")
                })
                .count()
                != 1
        })
    {
        return Err("远端容器并非完整健康的旧版本，未关闭任务。".into());
    }
    let changed = tx.execute(
        "UPDATE release_tasks SET phase = 'checked', outcome = 'failed',
         detail = '已人工只读核实旧版本全部健康；上传中断任务关闭，远端临时归档仍需人工检查',
         updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND phase = 'uploading' AND outcome = 'unknown'",
        [&task.id],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("核实期间发布任务状态已变化。".into());
    }
    tx.execute(
        "INSERT INTO release_task_resolutions (task_id, decision, observed_containers)
         VALUES (?1, 'confirmed_previous_running_predeploy', ?2)",
        params![task.id, observed.len() as i64],
    )
    .map_err(|err| err.to_string())?;
    transition_batch_target_in(&tx, &task.id, "unknown", "failed", "unknown", "paused")?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn resolve_predeploy_unchanged(
    task: &TaskSnapshot,
    previous_id: &str,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    resolve_predeploy_unchanged_in(&connect()?, task, previous_id, observed)
}

fn create_in(conn: &Connection, intent: &Intent) -> Result<(), String> {
    create_with_batch_in(conn, intent, None, None)
}

fn create_with_batch_in(
    conn: &Connection,
    intent: &Intent,
    batch_id: Option<&str>,
    env_sha256: Option<&str>,
) -> Result<(), String> {
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
    let rollback_active: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM rollback_tasks WHERE project_id = ?1
         AND (docker_daemon_id = ?2 OR docker_daemon_id IS NULL)
         AND outcome IN ('running', 'unknown', 'blocked')",
            params![intent.project_id, intent.docker_daemon_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if rollback_active != 0 {
        return Err("该项目有待核实的回滚任务，不能开始新发布。".into());
    }
    let active_batch: Option<(String, String)> = transaction
        .query_row(
            "SELECT id, state FROM release_batches WHERE project_id = ?1
             AND state IN ('prepared', 'building', 'ready', 'running', 'paused', 'unknown')",
            [intent.project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    match (&active_batch, batch_id, env_sha256) {
        (None, None, None) => {}
        (Some((active, phase)), Some(requested), Some(_))
            if active == requested && matches!(phase.as_str(), "ready" | "running") => {}
        _ => return Err("批次意图不存在、阶段不允许或单主机发布与批次冲突。".into()),
    }
    if let Some(batch_id) = batch_id {
        let digest = env_sha256.ok_or("批次环境摘要缺失。")?;
        if digest.len() != 64 || !digest.bytes().all(|ch| ch.is_ascii_hexdigit()) {
            return Err("批次环境摘要无效。".into());
        }
        let eligible: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM release_batch_targets t JOIN release_batches b
             ON b.id = t.batch_id JOIN project_hosts p ON p.project_id = b.project_id
             AND p.ordinal = t.ordinal AND p.host_id = t.host_id
             WHERE t.batch_id = ?1 AND b.project_id = ?2
             AND b.config_sha256 = ?3 AND t.host_id = ?4 AND t.host_name = ?5
             AND t.host_group = ?6 AND t.host_address = ?7 AND t.host_port = ?8
             AND t.host_username = ?9 AND t.host_auth = ?10 AND t.host_fingerprint = ?11
             AND t.platform = ?12 AND t.docker_endpoint = ?13
             AND t.docker_daemon_id = ?14 AND t.state = 'waiting'
             AND t.env_sha256 = ?15
             AND NOT EXISTS (SELECT 1 FROM release_batch_targets earlier
               LEFT JOIN release_tasks prior ON prior.id = earlier.release_task_id
               WHERE earlier.batch_id = t.batch_id AND earlier.ordinal < t.ordinal
               AND (earlier.state NOT IN ('healthy', 'skipped')
                 OR (earlier.state = 'healthy' AND (prior.phase IS NOT 'checked'
                     OR prior.outcome IS NOT 'complete' OR prior.health_result IS NOT 'healthy'))
                 OR (earlier.state = 'skipped' AND prior.outcome IS NOT 'failed')))
             AND NOT EXISTS (SELECT 1 FROM release_batch_targets later
               WHERE later.batch_id = t.batch_id AND later.ordinal > t.ordinal
               AND (later.state != 'waiting' OR later.release_task_id IS NOT NULL))",
                params![
                    batch_id,
                    intent.project_id,
                    intent.config_sha256,
                    intent.host.id,
                    intent.host.name,
                    intent.host.group,
                    intent.host.address,
                    intent.host.port,
                    intent.host.username,
                    intent.host.auth,
                    fingerprint,
                    intent.platform,
                    intent.docker_endpoint,
                    intent.docker_daemon_id,
                    digest
                ],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())?;
        let images: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM release_batch_images WHERE batch_id = ?1 AND platform = ?2",
                params![batch_id, intent.platform],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())?;
        let incomplete: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM release_batch_images WHERE batch_id = ?1 AND platform = ?2
             AND (archive_bytes IS NULL OR archive_bytes <= 0 OR image_ids IS NULL
                 OR archive_sha256 IS NULL
                 OR length(archive_sha256) != 64 OR archive_sha256 GLOB '*[^0-9a-fA-F]*')",
                params![batch_id, intent.platform],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())?;
        if eligible != 1 || images as usize != intent.images.len() || incomplete != 0 {
            return Err("批次主机顺序、身份或镜像数量已变化。".into());
        }
        for (service, reference, platform) in &intent.images {
            let matches: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM release_batch_images WHERE batch_id = ?1
                 AND service = ?2 AND reference = ?3 AND platform = ?4",
                    params![batch_id, service, reference, platform],
                    |row| row.get(0),
                )
                .map_err(|err| err.to_string())?;
            if matches != 1 {
                return Err("批次自有镜像与主机发布意图不一致。".into());
            }
        }
    }
    let legacy_active: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1
             AND docker_daemon_id IS NULL AND outcome IN ('running', 'unknown', 'blocked')",
            [intent.project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if legacy_active != 0 {
        return Err("项目存在缺少 Docker 身份的待核实旧任务，禁止新发布。".into());
    }
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
    let target: (String, u16, String, String, Option<String>, String, String) = transaction
        .query_row(
            "SELECT address, port, username, auth, trusted_fingerprint, name, group_name
             FROM hosts WHERE id = ?1 AND deleting = 0",
            [intent.host.id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .map_err(|_| "目标主机已删除或正在删除。")?;
    if target.0 != intent.host.address
        || target.1 != intent.host.port
        || target.2 != intent.host.username
        || target.3 != intent.host.auth
        || target.4.as_deref() != Some(fingerprint)
        || (batch_id.is_some() && (target.5 != intent.host.name || target.6 != intent.host.group))
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
    if let Some(batch_id) = batch_id {
        let changed = transaction
            .execute(
                "UPDATE release_batch_targets SET state = 'running', release_task_id = ?3,
             updated_at = CURRENT_TIMESTAMP WHERE batch_id = ?1 AND host_id = ?2
             AND state = 'waiting' AND release_task_id IS NULL",
                params![batch_id, intent.host.id, intent.id],
            )
            .map_err(|err| err.to_string())?;
        if changed != 1 {
            return Err("批次主机发布意图已变化。".into());
        }
        transaction
            .execute(
                "UPDATE release_batches SET state = 'running',
            updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND state IN ('ready', 'running')",
                [batch_id],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn create(intent: &Intent) -> Result<(), String> {
    create_in(&connect()?, intent)
}

#[allow(dead_code)] // Batch coordinator will use this after shared builds are safely retained.
pub fn create_for_batch(intent: &Intent, batch_id: &str, env_sha256: &str) -> Result<(), String> {
    create_with_batch_in(&connect()?, intent, Some(batch_id), Some(env_sha256))
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
    let changed = conn
        .execute(
            "UPDATE release_task_images SET archive_bytes = ?3, archive_sha256 = ?4
         WHERE task_id = ?1 AND service = ?2 AND archive_bytes IS NULL
         AND EXISTS (SELECT 1 FROM release_tasks t WHERE t.id = ?1 AND t.outcome = 'running'
             AND (t.phase = 'uploading' OR (t.phase = 'built' AND EXISTS (
                 SELECT 1 FROM release_batch_targets bt WHERE bt.release_task_id = t.id
                 AND bt.state = 'running'))))",
            params![task_id, service, bytes as i64, sha256],
        )
        .map_err(|err| err.to_string())?;
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

fn reserve_uploaded_archive_in(
    conn: &Connection,
    task_id: &str,
    service: &str,
    directory: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    crate::ssh::validated_upload_directory(directory)?;
    if bytes == 0
        || bytes > i64::MAX as u64
        || sha256.len() != 64
        || !sha256.bytes().all(|ch| ch.is_ascii_hexdigit())
    {
        return Err("发布临时归档意图无效。".into());
    }
    let changed = conn.execute(
        "INSERT INTO release_uploaded_archives (task_id, service, directory, bytes, sha256, state)
         SELECT t.id, ?2, ?3, ?4, ?5, 'reserved' FROM release_tasks t
         JOIN release_batch_targets bt ON bt.release_task_id = t.id AND bt.state = 'running'
         JOIN release_task_images i ON i.task_id = t.id AND i.service = ?2
         WHERE t.id = ?1 AND t.phase = 'uploading' AND t.outcome = 'running'
         AND i.archive_bytes = ?4 AND i.archive_sha256 = ?5",
        params![task_id, service, directory, bytes as i64, sha256],
    ).map_err(|_| "无法预留远端归档路径，未开始传输。".to_owned())?;
    if changed != 1 {
        return Err("批次发布镜像或归档摘要已变化。".into());
    }
    Ok(())
}

#[allow(dead_code)] // Reserved before the first SFTP side effect.
pub fn reserve_uploaded_archive(
    task_id: &str,
    service: &str,
    directory: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    reserve_uploaded_archive_in(&connect()?, task_id, service, directory, bytes, sha256)
}

fn reserve_unknown_uploaded_archive_in(
    conn: &Connection,
    task_id: &str,
    service: &str,
    directory: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    crate::ssh::validated_upload_directory(directory)?;
    if bytes == 0
        || bytes > i64::MAX as u64
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("续传归档意图无效。".into());
    }
    let changed = conn.execute(
        "INSERT INTO release_uploaded_archives (task_id, service, directory, bytes, sha256, state)
         SELECT t.id, ?2, ?3, ?4, ?5, 'reserved' FROM release_tasks t
         JOIN release_batch_targets bt ON bt.release_task_id = t.id AND bt.state = 'unknown'
         JOIN release_batches b ON b.id = bt.batch_id AND b.state = 'unknown'
         JOIN release_task_images i ON i.task_id = t.id AND i.service = ?2
         WHERE t.id = ?1 AND t.phase = 'uploading' AND t.outcome = 'unknown'
         AND i.archive_bytes = ?4 AND i.archive_sha256 = ?5",
        params![task_id, service, directory, bytes as i64, sha256],
    ).map_err(|_| "无法预留待续传归档路径。".to_owned())?;
    if changed != 1 {
        return Err("续传任务或镜像材料已变化。".into());
    }
    Ok(())
}

pub fn reserve_unknown_uploaded_archive(
    task_id: &str,
    service: &str,
    directory: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    reserve_unknown_uploaded_archive_in(&connect()?, task_id, service, directory, bytes, sha256)
}

pub(crate) fn finish_unknown_upload_in(conn: &Connection, task_id: &str) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE release_tasks SET phase = 'uploaded', outcome = 'running',
         detail = '已人工核实续传归档完整，准备远端加载', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND phase = 'uploading' AND outcome = 'unknown'
         AND EXISTS (SELECT 1 FROM release_task_images WHERE task_id = ?1)
         AND NOT EXISTS (SELECT 1 FROM release_task_images i
             LEFT JOIN release_uploaded_archives u
             ON u.task_id = i.task_id AND u.service = i.service
             WHERE i.task_id = ?1 AND (u.state IS NOT 'uploaded'
                 OR u.bytes IS NOT i.archive_bytes OR u.sha256 IS NOT i.archive_sha256))
         AND EXISTS (SELECT 1 FROM release_batch_targets bt JOIN release_batches b
             ON b.id = bt.batch_id WHERE bt.release_task_id = ?1
             AND bt.state = 'unknown' AND b.state = 'unknown')",
            [task_id],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("续传归档尚未全部核实或任务状态已变化。".into());
    }
    transition_batch_target_in(&tx, task_id, "unknown", "running", "unknown", "running")?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn finish_unknown_upload(task_id: &str) -> Result<(), String> {
    finish_unknown_upload_in(&connect()?, task_id)
}

fn confirm_uploaded_archive_in(
    conn: &Connection,
    task_id: &str,
    service: &str,
    uploaded: &crate::ssh::UploadedArchive,
) -> Result<(), String> {
    if uploaded.path != format!("{}/image.tar", uploaded.directory)
        || uploaded.bytes > i64::MAX as u64
    {
        return Err("上传归档路径或大小无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE release_uploaded_archives SET state = 'uploaded'
         WHERE task_id = ?1 AND service = ?2 AND state = 'reserved'
         AND directory = ?3 AND bytes = ?4 AND sha256 = ?5",
            params![
                task_id,
                service,
                uploaded.directory,
                uploaded.bytes as i64,
                uploaded.sha256
            ],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("上传结果与远端路径预留记录不符。".into());
    }
    Ok(())
}

#[allow(dead_code)] // Only after confirmed SFTP size/hash verification.
pub fn confirm_uploaded_archive(
    task_id: &str,
    service: &str,
    uploaded: &crate::ssh::UploadedArchive,
) -> Result<(), String> {
    confirm_uploaded_archive_in(&connect()?, task_id, service, uploaded)
}

fn advance_uploaded_archive_in(
    conn: &Connection,
    task_id: &str,
    service: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    if !matches!((from, to), ("uploaded", "loaded") | ("loaded", "cleaned")) {
        return Err("远端归档状态转换无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE release_uploaded_archives SET state = ?4
         WHERE task_id = ?1 AND service = ?2 AND state = ?3",
            params![task_id, service, from, to],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("远端归档记录阶段已变化。".into());
    }
    Ok(())
}

#[allow(dead_code)] // The coordinator advances after Docker load and verified cleanup.
pub fn advance_uploaded_archive(
    task_id: &str,
    service: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    advance_uploaded_archive_in(&connect()?, task_id, service, from, to)
}

#[allow(dead_code)] // Restart-safe read of the exact reserved remote tar locations.
pub struct UploadedArchiveRecord {
    pub service: String,
    pub directory: String,
    pub bytes: u64,
    pub sha256: String,
    pub state: String,
}

#[allow(dead_code)]
pub fn uploaded_archives(task_id: &str) -> Result<Vec<UploadedArchiveRecord>, String> {
    let conn = connect()?;
    conn.prepare(
        "SELECT u.service, u.directory, u.bytes, u.sha256, u.state
        FROM release_uploaded_archives u JOIN release_task_images i
        ON i.task_id = u.task_id AND i.service = u.service
        WHERE u.task_id = ?1 ORDER BY i.ordinal",
    )
    .map_err(|err| err.to_string())?
    .query_map([task_id], |row| {
        let bytes = row.get::<_, i64>(2)?;
        if bytes <= 0 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(UploadedArchiveRecord {
            service: row.get(0)?,
            directory: row.get(1)?,
            bytes: bytes as u64,
            sha256: row.get(3)?,
            state: row.get(4)?,
        })
    })
    .map_err(|err| err.to_string())?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|err| err.to_string())
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
               WHERE newer.project_id = ?2 AND newer.docker_daemon_id = ?10
               AND newer.rowid > release_tasks.rowid)
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
    let previous: Option<String> = tx
        .query_row(
            "SELECT task_id FROM project_current_version WHERE project_id = ?1
         AND docker_daemon_id = ?2",
            params![task.project_id, task.docker_daemon_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    tx.execute(
        "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, ?2)",
        params![task.id, previous],
    )
    .map_err(|err| err.to_string())?;
    tx.execute(
        "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(project_id, docker_daemon_id) DO UPDATE SET task_id = excluded.task_id,
             updated_at = CURRENT_TIMESTAMP",
        params![task.project_id, task.id, task.docker_daemon_id],
    )
    .map_err(|err| err.to_string())?;
    update_batch_target_in(&tx, &task.id, "healthy", "running")?;
    tx.execute(
        "UPDATE release_batches SET state = 'complete', updated_at = CURRENT_TIMESTAMP
         WHERE id = (SELECT batch_id FROM release_batch_targets WHERE release_task_id = ?1)
         AND state = 'running' AND NOT EXISTS (
             SELECT 1 FROM release_batch_targets t WHERE t.batch_id = release_batches.id
             AND t.state NOT IN ('healthy', 'skipped'))",
        [&task.id],
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

fn reconcile_healthy_in(
    conn: &Connection,
    task: &TaskSnapshot,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    let eligible = (task.phase == "checking"
        && matches!(task.outcome.as_str(), "unknown" | "running"))
        || (task.phase == "deploying" && task.outcome == "unknown");
    if !eligible
        || task.health_result.is_some()
        || observed.is_empty()
        || observed.len() > 100
        || task.docker_endpoint.is_none()
        || task.docker_daemon_id.is_none()
    {
        return Err("仅启动已确认、健康核实结果未确认的任务可人工恢复。".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let ready: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM release_materials WHERE task_id = ?1 AND state = 'ready'
         AND chunks BETWEEN 1 AND 128 AND length(material_sha256) = 64
         AND material_sha256 NOT GLOB '*[^0-9a-fA-F]*'",
            [&task.id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    let built: Vec<(String, String, String, i64, String)> = tx
        .prepare(
            "SELECT service, reference, image_ids, archive_bytes, archive_sha256
         FROM release_task_images WHERE task_id = ?1",
        )
        .map_err(|err| err.to_string())?
        .query_map([&task.id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|_| "中断任务的镜像归档记录不完整。")?;
    let services: Vec<(String, String, Option<String>)> = tx
        .prepare(
            "SELECT service, reference, image_ids FROM release_task_services WHERE task_id = ?1",
        )
        .map_err(|err| err.to_string())?
        .query_map([&task.id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|err| err.to_string())?;
    if ready != 1
        || built.is_empty()
        || built.len() > 16
        || services.len() != observed.len()
        || built.iter().any(|(name, reference, ids, bytes, sha)| {
            *bytes <= 0
                || sha.len() != 64
                || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                || serde_json::from_str::<Vec<String>>(ids).map_or(true, |ids| ids.is_empty())
                || !services.iter().any(|(service, image, encoded)| {
                    service == name && image == reference && encoded.as_deref() == Some(ids)
                })
        })
    {
        return Err("中断任务的版本材料或服务计划不完整。".into());
    }
    for (service, reference, encoded) in services {
        let ids: Vec<String> = encoded
            .map(|value| serde_json::from_str(&value).map_err(|_| "中断任务的镜像计划无效。"))
            .transpose()?
            .unwrap_or_default();
        let matches: Vec<_> = observed
            .iter()
            .filter(|item| item.service == service)
            .collect();
        if matches.len() != 1
            || matches[0].status != "running"
            || matches[0].health.as_deref() != Some("healthy")
            || matches[0].config_image != reference
            || matches[0].image_id.len() != 71
            || !matches[0].image_id.starts_with("sha256:")
            || !matches[0].image_id[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || (!ids.is_empty() && !ids.contains(&matches[0].image_id))
        {
            return Err("中断任务远端服务没有全部健康或镜像不符。".into());
        }
    }
    let detail = if task.phase == "deploying" {
        "启动响应未确认后人工只读复核全部服务健康；仍需确认完成"
    } else {
        "健康核实未确认后人工只读复核全部服务健康；仍需确认完成"
    };
    let changed = tx.execute(
        "UPDATE release_tasks SET phase = 'checked', outcome = 'blocked', health_result = 'healthy',
         detail = ?13, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND project_id = ?2 AND host_id = ?3
           AND host_address = ?4 AND host_port = ?5 AND host_username = ?6 AND host_auth = ?7
           AND host_fingerprint = ?8 AND docker_endpoint = ?9 AND docker_daemon_id = ?10
            AND phase = ?12 AND outcome = ?11 AND health_result IS NULL
           AND NOT EXISTS (SELECT 1 FROM release_tasks AS newer
               WHERE newer.project_id = ?2 AND newer.docker_daemon_id = ?10
               AND newer.rowid > release_tasks.rowid)
           AND EXISTS (SELECT 1 FROM hosts WHERE id = ?3 AND deleting = 0
               AND address = ?4 AND port = ?5 AND username = ?6 AND auth = ?7
               AND trusted_fingerprint = ?8)",
        params![task.id, task.project_id, task.host.id, task.host.address, task.host.port,
            task.host.username, task.host.auth, task.host.trusted_fingerprint,
             task.docker_endpoint, task.docker_daemon_id, task.outcome, task.phase, detail],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("任务或主机身份已变化，未解除阻断。".into());
    }
    tx.execute(
        "INSERT INTO release_task_health_reconciliations
         (task_id, observed_containers, source_phase) VALUES (?1, ?2, ?3)",
        params![task.id, observed.len() as i64, task.phase],
    )
    .map_err(|err| err.to_string())?;
    if task.outcome == "unknown" {
        transition_batch_target_in(&tx, &task.id, "unknown", "running", "unknown", "running")?;
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn reconcile_healthy(
    task: &TaskSnapshot,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    reconcile_healthy_in(&connect()?, task, observed)
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
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE release_tasks SET outcome = ?3, detail = ?4, updated_at = CURRENT_TIMESTAMP,
             execution_finished_at = CASE WHEN ?3 = 'failed'
                 THEN COALESCE(execution_finished_at, CURRENT_TIMESTAMP)
                 ELSE execution_finished_at END
             WHERE id = ?1 AND phase = ?2 AND outcome = 'running'",
            params![id, phase.saved(), outcome.saved(), detail],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("发布结果已变化，不能重复记录。".into());
    }
    let (target_state, batch_state) = match outcome {
        Outcome::Failed => ("failed", "paused"),
        Outcome::Unknown => ("unknown", "unknown"),
    };
    update_batch_target_in(&tx, id, target_state, batch_state)?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

fn update_batch_target_in(
    conn: &Connection,
    task_id: &str,
    target_state: &str,
    batch_state: &str,
) -> Result<(), String> {
    transition_batch_target_in(
        conn,
        task_id,
        "running",
        target_state,
        "running",
        batch_state,
    )
}

fn resolve_batch_failure_in(conn: &Connection, task_id: &str) -> Result<(), String> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM release_batch_targets WHERE release_task_id = ?1",
            [task_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    match state.as_deref() {
        None | Some("failed") => Ok(()),
        Some("running") => {
            transition_batch_target_in(conn, task_id, "running", "failed", "running", "paused")
        }
        Some("unknown") => {
            transition_batch_target_in(conn, task_id, "unknown", "failed", "unknown", "paused")
        }
        _ => Err("批次目标阶段不能按远端空状态关闭。".into()),
    }
}

fn transition_batch_target_in(
    conn: &Connection,
    task_id: &str,
    from: &str,
    to: &str,
    batch_from: &str,
    batch_to: &str,
) -> Result<(), String> {
    let batch: Option<(String, String)> = conn
        .query_row(
            "SELECT batch_id, state FROM release_batch_targets WHERE release_task_id = ?1",
            [task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    let Some((batch_id, current_state)) = batch else {
        return Ok(());
    };
    if current_state != from {
        return Err("批次主机阶段已变化，任务结果未保存。".into());
    }
    let updated = conn
        .execute(
            "UPDATE release_batch_targets SET state = ?2, updated_at = CURRENT_TIMESTAMP
         WHERE release_task_id = ?1 AND state = ?3",
            params![task_id, to, from],
        )
        .map_err(|err| err.to_string())?;
    let parent = conn
        .execute(
            "UPDATE release_batches SET state = ?2, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND state = ?3",
            params![batch_id, batch_to, batch_from],
        )
        .map_err(|err| err.to_string())?;
    if updated != 1 || parent != 1 {
        return Err("批次状态已变化，任务结果未保存。".into());
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
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE release_tasks SET phase = 'checked', outcome = 'blocked', health_result = ?2, detail = ?3,
         updated_at = CURRENT_TIMESTAMP, execution_finished_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND phase = 'checking' AND outcome = 'running' AND health_result IS NULL",
            params![id, health.saved(), detail],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("健康核实结果已变化，不能重复记录。".into());
    }
    match health {
        HealthResult::Healthy => {}
        HealthResult::Failed => update_batch_target_in(&tx, id, "failed", "paused")?,
        HealthResult::RunningUnverified => update_batch_target_in(&tx, id, "unknown", "unknown")?,
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn settle_checked(id: &str, health: HealthResult, detail: &str) -> Result<(), String> {
    settle_checked_in(&connect()?, id, health, detail)
}

#[cfg(test)]
mod tests {
    use super::{
        HealthResult, Intent, Outcome, Phase, RetentionClass, TASK_QUERY, TaskSnapshot, advance_in,
        advance_uploaded_archive_in, complete_checked_in, completed_versions_for_in,
        confirm_uploaded_archive_in, confirmed_services_in, create_in, create_with_batch_in,
        finish_unknown_upload_in, historical_images_in, previous_healthy_in, reconcile_healthy_in,
        record_archive_in, record_image_ids_in, record_services_in,
        reserve_unknown_uploaded_archive_in, reserve_uploaded_archive_in, resolve_absent_in,
        resolve_predeploy_unchanged_in, retention_plan_in, settle_checked_in, settle_in,
        task_from_row, unresolved_in,
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
    fn unknown_upload_only_reopens_after_every_archive_is_confirmed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        conn.execute_batch("INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES ('batch-resume', 1, 'digest', 'unknown');
            INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome)
            VALUES ('task-resume', 1, 2, 'test', 'web', '203.0.113.10', 22,
                'deploy', 'password', 'SHA256:verified', 'unix:///var/run/docker.sock',
                'ABC:123', 'test:v1-amd64', 'linux/amd64', 'digest', 'uploading', 'unknown');
            INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
                host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
                platform, docker_endpoint, docker_daemon_id, state, release_task_id)
            VALUES ('batch-resume', 0, 2, 'web', 'prod', '203.0.113.10', 22,
                'deploy', 'password', 'SHA256:verified', 'linux/amd64',
                'unix:///var/run/docker.sock', 'ABC:123', 'unknown', 'task-resume');
            INSERT INTO release_task_images (task_id, ordinal, service, reference, platform,
                archive_bytes, archive_sha256) VALUES
                ('task-resume', 0, 'web', 'test:v1-amd64', 'linux/amd64', 123,
                 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'),
                ('task-resume', 1, 'worker', 'test-worker:v1-amd64', 'linux/amd64', 124,
                 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc');
            INSERT INTO release_uploaded_archives (task_id, service, directory, bytes, sha256, state)
            VALUES ('task-resume', 'web', '/tmp/devingpanel.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                123, 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'uploaded');").unwrap();
        assert!(finish_unknown_upload_in(&conn, "task-resume").is_err());
        reserve_unknown_uploaded_archive_in(
            &conn,
            "task-resume",
            "worker",
            "/tmp/devingpanel.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            124,
            &"c".repeat(64),
        )
        .unwrap();
        assert!(finish_unknown_upload_in(&conn, "task-resume").is_err());
        conn.execute(
            "UPDATE release_uploaded_archives SET state = 'uploaded'
            WHERE task_id = 'task-resume' AND service = 'worker'",
            [],
        )
        .unwrap();
        finish_unknown_upload_in(&conn, "task-resume").unwrap();
        let states: (String, String, String, String) = conn
            .query_row(
                "SELECT t.phase, t.outcome, bt.state, b.state FROM release_tasks t
             JOIN release_batch_targets bt ON bt.release_task_id = t.id
             JOIN release_batches b ON b.id = bt.batch_id WHERE t.id = 'task-resume'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            states,
            (
                "uploaded".into(),
                "running".into(),
                "running".into(),
                "running".into()
            )
        );
        assert!(finish_unknown_upload_in(&conn, "task-resume").is_err());
    }

    fn link_test_batch(conn: &Connection, draft: &Intent) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES (?1, ?2, ?3, 'running')",
            rusqlite::params![id, draft.project_id, draft.config_sha256],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
            host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
            platform, docker_endpoint, docker_daemon_id, state, release_task_id)
            VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'running', ?13)",
            rusqlite::params![
                id,
                draft.host.id,
                draft.host.name,
                draft.host.group,
                draft.host.address,
                draft.host.port,
                draft.host.username,
                draft.host.auth,
                draft.host.trusted_fingerprint,
                draft.platform,
                draft.docker_endpoint,
                draft.docker_daemon_id,
                draft.id
            ],
        )
        .unwrap();
        id
    }

    fn batch_states(conn: &Connection, id: &str) -> (String, String) {
        conn.query_row(
            "SELECT b.state, t.state FROM release_batches b JOIN release_batch_targets t
            ON t.batch_id = b.id WHERE b.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
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
        let finished: Option<String> = conn
            .query_row(
                "SELECT execution_finished_at FROM release_tasks WHERE id = ?1",
                [&first.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(finished.is_none());
    }

    #[test]
    fn active_batch_blocks_single_host_release_intent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES ('batch', 1, ?1, 'prepared')",
            ["a".repeat(64)],
        )
        .unwrap();
        assert!(create_in(&conn, &intent()).is_err());
        conn.execute(
            "UPDATE release_batches SET state = 'terminated' WHERE id = 'batch'",
            [],
        )
        .unwrap();
        create_in(&conn, &intent()).unwrap();
    }

    #[test]
    fn interrupted_batch_host_before_upload_can_close_without_remote_assumption() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let batch = link_test_batch(&conn, &draft);
        assert!(crate::batch_store::close_pretransfer_in(&conn, &draft.id).is_err());
        conn.execute(
            "UPDATE release_materials SET state = 'ready', chunks = 1,
            material_sha256 = ?2 WHERE task_id = ?1",
            rusqlite::params![draft.id, "b".repeat(64)],
        )
        .unwrap();
        advance_in(&conn, &draft.id, Phase::Prepared, Phase::Building).unwrap();
        advance_in(&conn, &draft.id, Phase::Building, Phase::Built).unwrap();
        crate::batch_store::close_pretransfer_in(&conn, &draft.id).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("paused".into(), "failed".into())
        );
        assert!(crate::batch_store::close_pretransfer_in(&conn, &draft.id).is_err());
        crate::batch_store::skip_failed_in(&conn, &batch, draft.host.id).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("complete".into(), "skipped".into())
        );
    }

    #[test]
    fn failed_prepared_batch_waits_for_credential_cleanup_before_closing() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let batch = link_test_batch(&conn, &draft);
        settle_in(
            &conn,
            &draft.id,
            Phase::Prepared,
            Outcome::Failed,
            "凭据保存失败",
        )
        .unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("paused".into(), "failed".into())
        );
        assert!(crate::batch_store::skip_failed_in(&conn, &batch, draft.host.id).is_err());
        conn.execute(
            "DELETE FROM release_materials WHERE task_id = ?1",
            [&draft.id],
        )
        .unwrap();
        crate::batch_store::skip_failed_in(&conn, &batch, draft.host.id).unwrap();
    }

    #[test]
    fn batch_host_release_intent_is_linked_in_one_transaction() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let batch_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES (?1, 1, ?2, 'ready')",
            rusqlite::params![batch_id, "a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_batch_images (batch_id, service, platform, reference)
            VALUES (?1, 'web', 'linux/amd64', 'test:v1-amd64')",
            [&batch_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_batch_images SET archive_bytes = 123,
            archive_sha256 = ?2, image_ids = ?3 WHERE batch_id = ?1",
            rusqlite::params![
                batch_id,
                "b".repeat(64),
                serde_json::to_string(&vec![format!("sha256:{}", "d".repeat(64))]).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
            host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
            platform, docker_endpoint, docker_daemon_id, state)
            VALUES (?1, 0, 2, 'web', 'prod', '203.0.113.10', 22, 'deploy', 'password',
            'SHA256:verified', 'linux/amd64', 'unix:///var/run/docker.sock', 'ABC:123', 'waiting')",
            [&batch_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hosts (id, name, address, port, username, auth,
            group_name, credential_id, trusted_fingerprint) VALUES
            (4, 'api', '203.0.113.11', 22, 'deploy', 'password',
             'prod', 'credential-other', 'SHA256:verified')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
            host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
            platform, docker_endpoint, docker_daemon_id, state)
            VALUES (?1, 1, 4, 'api', 'prod', '203.0.113.11', 22, 'deploy', 'password',
            'SHA256:verified', 'linux/amd64', 'unix:///var/run/docker.sock', 'OTHER:ID', 'waiting')",
            [&batch_id]).unwrap();
        conn.execute(
            "UPDATE release_batch_targets SET env_sha256 = ?2 WHERE batch_id = ?1",
            rusqlite::params![batch_id, "e".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_hosts (project_id, host_id, ordinal)
            VALUES (1, 2, 0), (1, 4, 1)",
            [],
        )
        .unwrap();
        let env_digest = "e".repeat(64);
        let draft = intent();
        let mut second = intent();
        second.host.id = 4;
        second.host.name = "api".into();
        second.host.address = "203.0.113.11".into();
        second.docker_daemon_id = "OTHER:ID".into();
        assert!(create_with_batch_in(&conn, &second, Some(&batch_id), Some(&env_digest)).is_err());
        assert!(create_in(&conn, &draft).is_err());
        assert!(create_with_batch_in(&conn, &draft, Some("wrong"), Some(&env_digest)).is_err());
        assert!(
            create_with_batch_in(&conn, &draft, Some(&batch_id), Some(&"f".repeat(64))).is_err()
        );
        conn.execute(
            "UPDATE project_hosts SET ordinal = 99 WHERE host_id = 2",
            [],
        )
        .unwrap();
        assert!(create_with_batch_in(&conn, &draft, Some(&batch_id), Some(&env_digest)).is_err());
        conn.execute("UPDATE project_hosts SET ordinal = 0 WHERE host_id = 2", [])
            .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM release_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
        create_with_batch_in(&conn, &draft, Some(&batch_id), Some(&env_digest)).unwrap();
        advance_in(&conn, &draft.id, Phase::Prepared, Phase::Building).unwrap();
        advance_in(&conn, &draft.id, Phase::Building, Phase::Built).unwrap();
        record_archive_in(&conn, &draft.id, "web", 123, &"b".repeat(64)).unwrap();
        advance_in(&conn, &draft.id, Phase::Built, Phase::Uploading).unwrap();
        let directory = format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple());
        reserve_uploaded_archive_in(&conn, &draft.id, "web", &directory, 123, &"b".repeat(64))
            .unwrap();
        assert!(
            reserve_uploaded_archive_in(&conn, &draft.id, "web", &directory, 123, &"b".repeat(64))
                .is_err()
        );
        let uploaded = crate::ssh::UploadedArchive {
            path: format!("{directory}/image.tar"),
            directory,
            bytes: 123,
            sha256: "b".repeat(64),
            image_ids: vec![],
        };
        confirm_uploaded_archive_in(&conn, &draft.id, "web", &uploaded).unwrap();
        advance_uploaded_archive_in(&conn, &draft.id, "web", "uploaded", "loaded").unwrap();
        advance_uploaded_archive_in(&conn, &draft.id, "web", "loaded", "cleaned").unwrap();
        let linked: (String, String) = conn
            .query_row(
                "SELECT state, release_task_id
            FROM release_batch_targets WHERE batch_id = ?1 AND ordinal = 0",
                [&batch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(linked, ("running".into(), draft.id.clone()));
        assert!(
            create_with_batch_in(&conn, &intent(), Some(&batch_id), Some(&env_digest)).is_err()
        );
        assert!(create_with_batch_in(&conn, &second, Some(&batch_id), Some(&env_digest)).is_err());
        conn.execute(
            "UPDATE release_tasks SET outcome = 'complete', phase = 'checked',
            health_result = 'healthy' WHERE id = ?1",
            [&draft.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_batch_targets SET state = 'healthy' WHERE batch_id = ?1
            AND ordinal = 0",
            [&batch_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_tasks SET outcome = 'blocked' WHERE id = ?1",
            [&draft.id],
        )
        .unwrap();
        assert!(create_with_batch_in(&conn, &second, Some(&batch_id), Some(&env_digest)).is_err());
        conn.execute(
            "UPDATE release_tasks SET outcome = 'complete' WHERE id = ?1",
            [&draft.id],
        )
        .unwrap();
        create_with_batch_in(&conn, &second, Some(&batch_id), Some(&env_digest)).unwrap();
        let next: String = conn
            .query_row(
                "SELECT release_task_id FROM release_batch_targets
            WHERE batch_id = ?1 AND ordinal = 1",
                [&batch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(next, second.id);
        settle_in(
            &conn,
            &second.id,
            Phase::Prepared,
            Outcome::Unknown,
            "待核实",
        )
        .unwrap();
        let states: (String, String) = conn
            .query_row(
                "SELECT b.state, t.state FROM release_batches b JOIN release_batch_targets t
             ON t.batch_id = b.id WHERE b.id = ?1 AND t.ordinal = 1",
                [&batch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(states, ("unknown".into(), "unknown".into()));
        assert!(settle_in(&conn, &second.id, Phase::Prepared, Outcome::Failed, "重写").is_err());
    }

    #[test]
    fn unresolved_release_blocks_its_daemon_and_unknown_legacy_blocks_all() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let first = intent();
        create_in(&conn, &first).unwrap();
        let mut other = intent();
        other.docker_daemon_id = "OTHER:ID".into();
        create_in(&conn, &other).unwrap();
        assert!(create_in(&conn, &intent()).is_err());
        let mut third = intent();
        third.docker_daemon_id = "THIRD:ID".into();
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
            host_address, host_port, host_username, host_auth, host_fingerprint,
            image_reference, platform, config_sha256, phase, outcome)
            SELECT 'legacy', project_id, host_id, project_name, host_name, host_address, host_port,
                host_username, host_auth, host_fingerprint, image_reference, platform,
                config_sha256, 'checking', 'unknown' FROM release_tasks WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
        assert!(create_in(&conn, &third).is_err());
    }

    #[test]
    fn unresolved_rollback_blocks_only_its_daemon_unless_legacy() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let old = intent();
        create_in(&conn, &old).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&old.id]).unwrap();
        conn.execute("INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id,
            docker_daemon_id, phase, outcome) VALUES ('active', 1, ?1, ?1, 'ABC:123', 'checking', 'unknown')",
            [&old.id]).unwrap();
        assert!(create_in(&conn, &intent()).is_err());
        let mut other = intent();
        other.docker_daemon_id = "OTHER:ID".into();
        create_in(&conn, &other).unwrap();
        conn.execute(
            "INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id,
            phase, outcome) VALUES ('legacy', 1, ?1, ?1, 'checking', 'unknown')",
            [&old.id],
        )
        .unwrap();
        let mut third = intent();
        third.docker_daemon_id = "THIRD:ID".into();
        assert!(create_in(&conn, &third).is_err());
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
        let batch = link_test_batch(&conn, &draft);
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
            execution_finished_at: None,
            image_reference: draft.image_reference.clone(),
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
        assert_eq!(
            batch_states(&conn, &batch),
            ("paused".into(), "failed".into())
        );
        conn.execute(
            "DELETE FROM release_materials WHERE task_id = ?1",
            [&draft.id],
        )
        .unwrap();
        crate::batch_store::terminate_resolved_in(&conn, &batch).unwrap();
        assert!(unresolved_in(&conn, Some(1)).unwrap().is_empty());
        assert!(resolve_absent_in(&conn, &task).is_err());
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM release_task_resolutions WHERE task_id = ?1 AND decision = 'confirmed_absent' AND observed_containers = 0", [&draft.id], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
        create_in(&conn, &intent()).unwrap();
    }

    #[test]
    fn predeploy_unknown_closes_only_with_exact_healthy_previous_version() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let old_image = format!("sha256:{}", "a".repeat(64));
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
            host_address, host_port, host_username, host_auth, host_fingerprint,
            docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
            phase, outcome, health_result) VALUES ('old', 1, 2, 'test', 'web', '203.0.113.10',
            22, 'deploy', 'password', 'SHA256:verified', 'unix:///var/run/docker.sock', 'ABC:123',
            'test:v1-amd64', 'linux/amd64', ?1, 'checked', 'complete', 'healthy')",
            ["a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_task_services (task_id, service, reference, confirmed_image_id)
            VALUES ('old', 'web', 'test:v1-amd64', ?1)",
            [&old_image],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
            VALUES (1, 'old', 'ABC:123')",
            [],
        )
        .unwrap();
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let batch = link_test_batch(&conn, &draft);
        advance_in(&conn, &draft.id, Phase::Prepared, Phase::Building).unwrap();
        advance_in(&conn, &draft.id, Phase::Building, Phase::Built).unwrap();
        advance_in(&conn, &draft.id, Phase::Built, Phase::Uploading).unwrap();
        settle_in(
            &conn,
            &draft.id,
            Phase::Uploading,
            Outcome::Unknown,
            "上传待核实",
        )
        .unwrap();
        assert!(crate::batch_store::skip_failed_in(&conn, &batch, draft.host.id).is_err());
        let task = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id = ?1"),
                [&draft.id],
                task_from_row,
            )
            .unwrap();
        let observed = [ObservedContainer {
            service: "web".into(),
            status: "running".into(),
            health: Some("healthy".into()),
            image_id: old_image,
            config_image: "test:v1-amd64".into(),
        }];
        assert!(resolve_predeploy_unchanged_in(&conn, &task, "wrong", &observed).is_err());
        let mut changed = observed.clone();
        changed[0].health = Some("unhealthy".into());
        assert!(resolve_predeploy_unchanged_in(&conn, &task, "old", &changed).is_err());
        resolve_predeploy_unchanged_in(&conn, &task, "old", &observed).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("paused".into(), "failed".into())
        );
        conn.execute(
            "DELETE FROM release_materials WHERE task_id = ?1",
            [&draft.id],
        )
        .unwrap();
        crate::batch_store::skip_failed_in(&conn, &batch, draft.host.id).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("complete".into(), "skipped".into())
        );
        assert!(resolve_predeploy_unchanged_in(&conn, &task, "old", &observed).is_err());
        let result: (String, String) = conn
            .query_row(
                "SELECT phase, outcome FROM release_tasks WHERE id = ?1",
                [&draft.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(result, ("checked".into(), "failed".into()));
        let audit: (String, i64) = conn.query_row("SELECT decision, observed_containers FROM release_task_resolutions WHERE task_id = ?1",
            [&draft.id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(audit, ("confirmed_previous_running_predeploy".into(), 1));
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
    fn newer_other_daemon_does_not_replace_previous_healthy_version() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let first = intent();
        create_in(&conn, &first).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&first.id]).unwrap();
        let second = intent();
        create_in(&conn, &second).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='blocked', health_result='healthy' WHERE id=?1", [&second.id]).unwrap();
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
            host_address, host_port, host_username, host_auth, host_fingerprint,
            docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
            phase, outcome, health_result)
            SELECT ?1, project_id, host_id, project_name, host_name, host_address, host_port,
                host_username, host_auth, host_fingerprint, docker_endpoint, 'OTHER:ID',
                image_reference, platform, config_sha256, 'checked', 'complete', 'healthy'
            FROM release_tasks WHERE id = ?2",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), second.id],
        )
        .unwrap();
        assert_eq!(previous_healthy_in(&conn, &second.id).unwrap().id, first.id);
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
        let batch_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES (?1, 1, ?2, 'running')",
            rusqlite::params![batch_id, draft.config_sha256],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
            host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
            platform, docker_endpoint, docker_daemon_id, state, release_task_id)
            VALUES (?1, 0, 2, 'web', 'prod', '203.0.113.10', 22, 'deploy', 'password',
            'SHA256:verified', 'linux/amd64', 'unix:///var/run/docker.sock', 'ABC:123',
            'running', ?2)",
            rusqlite::params![batch_id, draft.id],
        )
        .unwrap();
        let execution_finished_at: String = conn
            .query_row(
                "SELECT execution_finished_at FROM release_tasks WHERE id = ?1",
                [&draft.id],
                |row| row.get(0),
            )
            .unwrap();
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
        let states: (String, String) = conn
            .query_row(
                "SELECT b.state, t.state FROM release_batches b JOIN release_batch_targets t
             ON t.batch_id = b.id WHERE b.id = ?1",
                [&batch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(states, ("complete".into(), "healthy".into()));
        let after_confirmation: String = conn
            .query_row(
                "SELECT execution_finished_at FROM release_tasks WHERE id = ?1",
                [&draft.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after_confirmation, execution_finished_at);
        assert!(complete_checked_in(&conn, &task, &observed).is_err());
        let audit: (String, i64) = conn.query_row("SELECT decision, observed_containers FROM release_task_resolutions WHERE task_id = ?1", [&draft.id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(audit, ("confirmed_healthy".into(), 2));
        let saved: String = conn.query_row("SELECT confirmed_image_id FROM release_task_services WHERE task_id = ?1 AND service = 'db'", [&draft.id], |row| row.get(0)).unwrap();
        assert_eq!(saved, observed[1].image_id);
        create_in(&conn, &intent()).unwrap();
    }

    #[test]
    fn interrupted_health_reconciliation_remains_blocked_until_explicit_completion() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let batch = link_test_batch(&conn, &draft);
        let ids = vec![format!("sha256:{}", "a".repeat(64))];
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
                record_archive_in(&conn, &draft.id, "web", 123, &"b".repeat(64)).unwrap();
            }
            if pair[0] == Phase::Loading {
                record_image_ids_in(&conn, &draft.id, "web", &ids).unwrap();
            }
            if pair[0] == Phase::Validating {
                record_services_in(
                    &conn,
                    &draft.id,
                    &[
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
                    ],
                )
                .unwrap();
            }
            advance_in(&conn, &draft.id, pair[0], pair[1]).unwrap();
        }
        settle_in(
            &conn,
            &draft.id,
            Phase::Checking,
            Outcome::Unknown,
            "SSH interrupted",
        )
        .unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("unknown".into(), "unknown".into())
        );
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
        assert!(reconcile_healthy_in(&conn, &task, &observed).is_err());
        let mut deploying = task.clone();
        deploying.phase = "deploying".into();
        assert!(reconcile_healthy_in(&conn, &deploying, &observed).is_err());
        conn.execute("UPDATE release_materials SET state = 'ready', chunks = 1, material_sha256 = ?2 WHERE task_id = ?1",
            rusqlite::params![draft.id, "d".repeat(64)]).unwrap();
        let mut wrong = observed.clone();
        wrong[0].image_id = format!("sha256:{}", "f".repeat(64));
        assert!(reconcile_healthy_in(&conn, &task, &wrong).is_err());
        wrong = observed.clone();
        wrong[1].health = None;
        assert!(reconcile_healthy_in(&conn, &task, &wrong).is_err());
        assert!(create_in(&conn, &intent()).is_err());
        reconcile_healthy_in(&conn, &task, &observed).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("running".into(), "running".into())
        );
        assert!(reconcile_healthy_in(&conn, &task, &observed).is_err());
        let recovered = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id = ?1"),
                [&draft.id],
                task_from_row,
            )
            .unwrap();
        assert_eq!(
            (
                recovered.phase.as_str(),
                recovered.outcome.as_str(),
                recovered.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let count: i64 = conn.query_row("SELECT observed_containers FROM release_task_health_reconciliations WHERE task_id = ?1",
            [&draft.id], |row| row.get(0)).unwrap();
        assert_eq!(count, 2);
        assert!(create_in(&conn, &intent()).is_err());
        complete_checked_in(&conn, &recovered, &observed).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("complete".into(), "healthy".into())
        );
        assert_eq!(
            conn.query_row(
                "SELECT decision FROM release_task_resolutions WHERE task_id = ?1",
                [&draft.id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "confirmed_healthy"
        );
        create_in(&conn, &intent()).unwrap();
    }

    #[test]
    fn crashed_after_confirmed_start_can_reconcile_checking_running() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let image_id = format!("sha256:{}", "a".repeat(64));
        conn.execute(
            "UPDATE release_tasks SET phase = 'checking' WHERE id = ?1",
            [&draft.id],
        )
        .unwrap();
        conn.execute("UPDATE release_materials SET state = 'ready', chunks = 1, material_sha256 = ?2 WHERE task_id = ?1",
            rusqlite::params![draft.id, "b".repeat(64)]).unwrap();
        conn.execute("UPDATE release_task_images SET image_ids = ?2, archive_bytes = 123, archive_sha256 = ?3 WHERE task_id = ?1",
            rusqlite::params![draft.id, serde_json::to_string(std::slice::from_ref(&image_id)).unwrap(), "c".repeat(64)]).unwrap();
        conn.execute("INSERT INTO release_task_services (task_id, service, reference, image_ids) VALUES (?1, 'web', ?2, ?3)",
            rusqlite::params![draft.id, draft.image_reference, serde_json::to_string(std::slice::from_ref(&image_id)).unwrap()]).unwrap();
        let task = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id = ?1"),
                [&draft.id],
                task_from_row,
            )
            .unwrap();
        let observed = [ObservedContainer {
            service: "web".into(),
            status: "running".into(),
            health: Some("healthy".into()),
            image_id,
            config_image: draft.image_reference,
        }];
        reconcile_healthy_in(&conn, &task, &observed).unwrap();
        let state: (String, String, Option<String>) = conn
            .query_row(
                "SELECT phase, outcome, health_result FROM release_tasks WHERE id = ?1",
                [&draft.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            state,
            ("checked".into(), "blocked".into(), Some("healthy".into()))
        );
    }

    #[test]
    fn unknown_start_requires_verified_health_then_separate_completion() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let draft = intent();
        create_in(&conn, &draft).unwrap();
        let batch = link_test_batch(&conn, &draft);
        conn.execute(
            "UPDATE release_tasks SET phase = 'deploying', outcome = 'unknown' WHERE id = ?1",
            [&draft.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_batch_targets SET state = 'unknown' WHERE batch_id = ?1",
            [&batch],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_batches SET state = 'unknown' WHERE id = ?1",
            [&batch],
        )
        .unwrap();
        let image_id = format!("sha256:{}", "a".repeat(64));
        conn.execute(
            "UPDATE release_materials SET state = 'ready', chunks = 1,
             material_sha256 = ?2 WHERE task_id = ?1",
            rusqlite::params![draft.id, "b".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_task_images SET image_ids = ?2, archive_bytes = 123,
             archive_sha256 = ?3 WHERE task_id = ?1",
            rusqlite::params![
                draft.id,
                serde_json::to_string(std::slice::from_ref(&image_id)).unwrap(),
                "c".repeat(64)
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_task_services (task_id, service, reference, image_ids)
             VALUES (?1, 'web', ?2, ?3)",
            rusqlite::params![
                draft.id,
                draft.image_reference,
                serde_json::to_string(std::slice::from_ref(&image_id)).unwrap()
            ],
        )
        .unwrap();
        let task = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id = ?1"),
                [&draft.id],
                task_from_row,
            )
            .unwrap();
        let mut observed = [ObservedContainer {
            service: "web".into(),
            status: "running".into(),
            health: Some("unhealthy".into()),
            image_id,
            config_image: draft.image_reference,
        }];
        assert!(reconcile_healthy_in(&conn, &task, &observed).is_err());
        assert_eq!(
            batch_states(&conn, &batch),
            ("unknown".into(), "unknown".into())
        );
        observed[0].health = Some("healthy".into());
        reconcile_healthy_in(&conn, &task, &observed).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("running".into(), "running".into())
        );
        let recovered = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id = ?1"),
                [&draft.id],
                task_from_row,
            )
            .unwrap();
        assert_eq!(
            (
                recovered.phase.as_str(),
                recovered.outcome.as_str(),
                recovered.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let phase: String = conn
            .query_row(
                "SELECT source_phase FROM release_task_health_reconciliations
                 WHERE task_id = ?1",
                [&draft.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "deploying");
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [draft.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        complete_checked_in(&conn, &recovered, &observed).unwrap();
        assert_eq!(
            batch_states(&conn, &batch),
            ("complete".into(), "healthy".into())
        );
    }

    #[test]
    fn retention_holds_history_during_partial_failure_and_two_completed_versions_afterward() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let mut completed = Vec::new();
        for _ in 0..2 {
            let draft = intent();
            create_in(&conn, &draft).unwrap();
            conn.execute("UPDATE release_tasks SET phase = 'checked', outcome = 'complete', health_result = 'healthy' WHERE id = ?1",
                [&draft.id]).unwrap();
            completed.push(draft.id);
        }
        conn.execute("INSERT INTO project_current_version (project_id, task_id, docker_daemon_id) VALUES (1, ?1, 'ABC:123')",
            [&completed[1]]).unwrap();
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, ?2)",
            rusqlite::params![completed[1], completed[0]],
        )
        .unwrap();
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert!(!plan.unresolved);
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry.class)
                .collect::<Vec<_>>(),
            [
                RetentionClass::LatestCompleted,
                RetentionClass::PreviousCompleted
            ]
        );
        let failed = intent();
        create_in(&conn, &failed).unwrap();
        conn.execute("UPDATE release_tasks SET phase = 'checked', outcome = 'blocked', health_result = 'failed' WHERE id = ?1",
            [&failed.id]).unwrap();
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert!(plan.unresolved);
        assert_eq!(plan.entries.len(), 3);
        assert!(
            plan.entries
                .iter()
                .all(|entry| entry.class == RetentionClass::UnresolvedHold)
        );
        conn.execute(
            "UPDATE release_tasks SET outcome = 'failed' WHERE id = ?1",
            [&failed.id],
        )
        .unwrap();
        let third = intent();
        create_in(&conn, &third).unwrap();
        conn.execute("UPDATE release_tasks SET phase = 'checked', outcome = 'complete', health_result = 'healthy' WHERE id = ?1",
            [&third.id]).unwrap();
        conn.execute(
            "UPDATE project_current_version SET task_id = ?1 WHERE project_id = 1",
            [&third.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, ?2)",
            rusqlite::params![third.id, completed[1]],
        )
        .unwrap();
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert!(!plan.unresolved);
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry.class)
                .collect::<Vec<_>>(),
            [
                RetentionClass::LatestCompleted,
                RetentionClass::UncertainHistory,
                RetentionClass::PreviousCompleted,
                RetentionClass::HistoricalReview,
            ]
        );
        let mut another_daemon = intent();
        another_daemon.docker_daemon_id = "OTHER:ID".into();
        create_in(&conn, &another_daemon).unwrap();
        conn.execute("UPDATE release_tasks SET phase = 'checked', outcome = 'complete', health_result = 'healthy' WHERE id = ?1",
            [&another_daemon.id]).unwrap();
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert_eq!(plan.entries[0].class, RetentionClass::UncertainHistory);
        assert_eq!(plan.entries[1].class, RetentionClass::LatestCompleted);
        assert!(retention_plan_in(&conn, 0).is_err());
    }

    #[test]
    fn completed_version_inventory_requires_same_daemon_and_confirmed_service_ids() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let first = intent();
        create_in(&conn, &first).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&first.id]).unwrap();
        let image_id = format!("sha256:{}", "a".repeat(64));
        conn.execute("INSERT INTO release_task_services (task_id, service, reference, confirmed_image_id) VALUES (?1, 'web', ?2, ?3)",
            rusqlite::params![first.id, first.image_reference, image_id]).unwrap();
        let second = intent();
        create_in(&conn, &second).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&second.id]).unwrap();
        let task = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id=?1"),
                [&second.id],
                task_from_row,
            )
            .unwrap();
        assert_eq!(completed_versions_for_in(&conn, &task).unwrap().len(), 2);
        assert_eq!(
            confirmed_services_in(&conn, &first.id).unwrap()[0].image_id,
            image_id
        );
        assert!(confirmed_services_in(&conn, &second.id).is_err());
        let mut wrong_daemon = task.clone();
        wrong_daemon.docker_daemon_id = Some("OTHER:ID".into());
        assert!(
            completed_versions_for_in(&conn, &wrong_daemon)
                .unwrap()
                .is_empty()
        );
        conn.execute(
            "UPDATE release_task_services SET confirmed_image_id='invalid' WHERE task_id=?1",
            [&first.id],
        )
        .unwrap();
        assert!(confirmed_services_in(&conn, &first.id).is_err());
    }

    #[test]
    fn host_aliases_for_same_daemon_share_one_retention_history() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        conn.execute("INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id, trusted_fingerprint)
            VALUES (3, 'alias', '203.0.113.10', 22, 'deploy', 'password', 'prod', 'alias-ref', 'SHA256:verified')", []).unwrap();
        let first = intent();
        create_in(&conn, &first).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&first.id]).unwrap();
        let mut alias = intent();
        alias.host.id = 3;
        alias.host.name = "alias".into();
        create_in(&conn, &alias).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&alias.id]).unwrap();
        conn.execute("INSERT INTO project_current_version (project_id, task_id, docker_daemon_id) VALUES (1, ?1, 'ABC:123')",
            [&alias.id]).unwrap();
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, ?2)",
            rusqlite::params![alias.id, first.id],
        )
        .unwrap();
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry.class)
                .collect::<Vec<_>>(),
            [
                RetentionClass::LatestCompleted,
                RetentionClass::PreviousCompleted
            ]
        );
        let task = conn
            .query_row(
                &format!("{TASK_QUERY} WHERE id=?1"),
                [&alias.id],
                task_from_row,
            )
            .unwrap();
        assert_eq!(completed_versions_for_in(&conn, &task).unwrap().len(), 2);
        conn.execute(
            "UPDATE release_tasks SET outcome = 'blocked' WHERE id = ?1",
            [&alias.id],
        )
        .unwrap();
        assert_eq!(previous_healthy_in(&conn, &alias.id).unwrap().id, first.id);
    }

    #[test]
    fn retention_classifies_independent_daemon_pointers() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let first = intent();
        create_in(&conn, &first).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&first.id]).unwrap();
        let mut second = intent();
        second.docker_daemon_id = "OTHER:ID".into();
        create_in(&conn, &second).unwrap();
        conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1", [&second.id]).unwrap();
        for draft in [&first, &second] {
            conn.execute("INSERT INTO project_current_version (project_id, task_id, docker_daemon_id) VALUES (1, ?1, ?2)",
                rusqlite::params![draft.id, draft.docker_daemon_id]).unwrap();
        }
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(
            plan.entries
                .iter()
                .all(|entry| entry.class == RetentionClass::LatestCompleted)
        );
    }

    #[test]
    fn retention_follows_confirmed_pointer_after_rollback_not_newest_task() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed(&conn);
        let mut versions = Vec::new();
        for _ in 0..3 {
            let draft = intent();
            create_in(&conn, &draft).unwrap();
            conn.execute("UPDATE release_tasks SET phase='checked', outcome='complete', health_result='healthy' WHERE id=?1",
                [&draft.id]).unwrap();
            versions.push(draft.id);
        }
        let without_pointer = retention_plan_in(&conn, 1).unwrap();
        assert!(
            without_pointer
                .entries
                .iter()
                .all(|entry| entry.class == RetentionClass::UncertainHistory)
        );
        conn.execute("INSERT INTO project_current_version (project_id, task_id, docker_daemon_id) VALUES (1, ?1, 'ABC:123')",
            [&versions[0]]).unwrap();
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, NULL)",
            [&versions[0]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, ?2)",
            rusqlite::params![versions[1], versions[0]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES (?1, ?2)",
            rusqlite::params![versions[2], versions[1]],
        )
        .unwrap();
        let plan = retention_plan_in(&conn, 1).unwrap();
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry.class)
                .collect::<Vec<_>>(),
            [
                RetentionClass::HistoricalReview,
                RetentionClass::HistoricalReview,
                RetentionClass::LatestCompleted
            ]
        );
        assert_eq!(plan.entries[2].task_id, versions[0]);
        conn.execute(
            "UPDATE project_current_version SET task_id = ?1 WHERE project_id = 1",
            [&versions[1]],
        )
        .unwrap();
        let middle = retention_plan_in(&conn, 1).unwrap();
        assert_eq!(
            middle
                .entries
                .iter()
                .map(|entry| entry.class)
                .collect::<Vec<_>>(),
            [
                RetentionClass::HistoricalReview,
                RetentionClass::LatestCompleted,
                RetentionClass::PreviousCompleted
            ]
        );
        conn.execute(
            "UPDATE project_current_version SET docker_daemon_id = 'OTHER:ID' WHERE project_id = 1",
            [],
        )
        .unwrap();
        let invalid = retention_plan_in(&conn, 1).unwrap();
        assert!(
            invalid
                .entries
                .iter()
                .all(|entry| entry.class == RetentionClass::UncertainHistory)
        );
        conn.execute(
            "UPDATE project_current_version SET docker_daemon_id = 'ABC:123' WHERE project_id = 1",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id, phase, outcome)
            VALUES (?1, 1, ?2, ?3, 'checking', 'unknown')",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), versions[2], versions[1]]).unwrap();
        let held = retention_plan_in(&conn, 1).unwrap();
        assert!(held.unresolved);
        assert!(
            held.entries
                .iter()
                .all(|entry| entry.class == RetentionClass::UnresolvedHold)
        );
    }
}
