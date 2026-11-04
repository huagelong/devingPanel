use crate::database::connect;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Prepared,
    Restoring,
    Restored,
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
            Self::Restoring => "restoring",
            Self::Restored => "restored",
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

#[derive(Clone)]
pub struct Intent {
    pub id: String,
    pub project_id: i64,
    pub source_task_id: String,
    pub target_task_id: String,
}

#[derive(Clone)]
pub struct TaskSnapshot {
    pub id: String,
    pub project_id: i64,
    pub source_task_id: String,
    pub target_task_id: String,
    pub phase: String,
    pub outcome: String,
    pub detail: String,
    pub started_at: String,
    pub observed_containers: Option<i64>,
    pub pending_directories: Option<String>,
}

const TASK_QUERY: &str = "SELECT id, project_id, source_task_id, target_task_id, phase, outcome,
    detail, started_at, observed_containers,
    (SELECT GROUP_CONCAT(directory, ', ') FROM rollback_uploaded_archives a
     WHERE a.rollback_id = rollback_tasks.id AND a.state != 'cleaned')
    FROM rollback_tasks";

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSnapshot> {
    Ok(TaskSnapshot {
        id: row.get(0)?,
        project_id: row.get(1)?,
        source_task_id: row.get(2)?,
        target_task_id: row.get(3)?,
        phase: row.get(4)?,
        outcome: row.get(5)?,
        detail: row.get(6)?,
        started_at: row.get(7)?,
        observed_containers: row.get(8)?,
        pending_directories: row.get(9)?,
    })
}

pub fn recent(project_id: Option<i64>) -> Result<Vec<TaskSnapshot>, String> {
    let conn = connect()?;
    let mut query = conn
        .prepare(&format!(
            "{TASK_QUERY} WHERE (?1 IS NULL OR project_id = ?1)
         ORDER BY CASE WHEN outcome IN ('running', 'unknown', 'blocked') THEN 0 ELSE 1 END,
             rowid DESC LIMIT 200"
        ))
        .map_err(|err| err.to_string())?;
    query
        .query_map([project_id], task_from_row)
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

pub fn get(id: &str) -> Result<TaskSnapshot, String> {
    connect()?
        .query_row(&format!("{TASK_QUERY} WHERE id = ?1"), [id], task_from_row)
        .map_err(|_| "回滚任务不存在。".into())
}

pub fn candidate(source_id: &str) -> Result<Intent, String> {
    candidate_in(&connect()?, source_id)
}

fn candidate_in(conn: &Connection, source_id: &str) -> Result<Intent, String> {
    let (project_id, daemon, outcome, health): (i64, String, String, Option<String>) = conn
        .query_row(
            "SELECT project_id, docker_daemon_id, outcome, health_result FROM release_tasks WHERE id = ?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|_| "当前任务没有可信 Docker daemon 身份。")?;
    let current: String = conn.query_row(
        "SELECT task_id FROM project_current_version WHERE project_id = ?1 AND docker_daemon_id = ?2",
        params![project_id, daemon], |row| row.get(0),
    ).map_err(|_| "项目没有已确认的当前版本；不能推断回滚目标。")?;
    let target: String = if outcome == "complete" && health.as_deref() == Some("healthy") {
        if current != source_id {
            return Err("当前发布记录并非已确认运行版本。".into());
        }
        conn.query_row(
            "SELECT predecessor_task_id FROM release_predecessors WHERE task_id = ?1",
            [source_id],
            |row| row.get(0),
        )
        .map_err(|_| "当前版本缺少经发布确认的前驱记录，不能回滚。")?
    } else {
        current
    };
    let intent = Intent {
        id: uuid::Uuid::new_v4().to_string(),
        project_id,
        source_task_id: source_id.to_owned(),
        target_task_id: target,
    };
    validate_in(conn, &intent)?;
    Ok(intent)
}

fn create_in(conn: &Connection, intent: &Intent) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    validate_in(&tx, intent)?;
    let changed = tx
        .execute(
            "INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id,
             docker_daemon_id, phase, outcome)
         SELECT ?1, ?2, ?3, ?4, docker_daemon_id, 'prepared', 'running'
         FROM release_tasks WHERE id = ?3 AND project_id = ?2 AND docker_daemon_id IS NOT NULL",
            params![
                intent.id,
                intent.project_id,
                intent.source_task_id,
                intent.target_task_id
            ],
        )
        .map_err(|_| "已有待核实回滚任务，不能重复开始。")?;
    if changed != 1 {
        return Err("回滚源任务缺少可信 Docker daemon 身份。".into());
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

fn validate_in(conn: &Connection, intent: &Intent) -> Result<(), String> {
    if uuid::Uuid::parse_str(&intent.id).is_err()
        || intent.project_id <= 0
        || intent.source_task_id == intent.target_task_id
    {
        return Err("回滚任务标识无效。".into());
    }
    let batch_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM release_batches WHERE project_id = ?1
         AND state IN ('prepared', 'building', 'ready', 'running', 'paused', 'unknown')",
            [intent.project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if batch_active != 0 {
        let failed_source: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_batches b JOIN release_batch_targets t
             ON t.batch_id = b.id JOIN release_tasks r ON r.id = t.release_task_id
             WHERE b.project_id = ?1 AND b.state = 'paused'
             AND t.release_task_id = ?2 AND t.state = 'failed'
             AND r.phase = 'checked' AND r.outcome = 'blocked'
             AND r.health_result = 'failed'
             AND NOT EXISTS (SELECT 1 FROM release_batch_targets other
                 WHERE other.batch_id = b.id AND other.state IN ('running', 'unknown'))",
                params![intent.project_id, intent.source_task_id],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())?;
        if failed_source != 1 {
            return Err("活跃批次只允许当前已核实健康失败的目标受控回滚。".into());
        }
    }
    let source: (i64, i64, String, String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT rowid, project_id, phase, outcome, health_result, docker_daemon_id
         FROM release_tasks WHERE id = ?1",
            [&intent.source_task_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .map_err(|_| "当前发布任务不存在。")?;
    if source.1 != intent.project_id
        || source.2 != "checked"
        || !matches!(
            (source.3.as_str(), source.4.as_deref()),
            ("complete", Some("healthy")) | ("blocked", Some("failed"))
        )
        || source.5.is_none()
    {
        return Err("当前发布任务未达到可回滚阶段。".into());
    }
    let newer: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1
             AND docker_daemon_id = ?2 AND rowid > ?3",
            params![intent.project_id, source.5, source.0],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    let other_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1 AND id <> ?2
         AND (docker_daemon_id = ?3 OR docker_daemon_id IS NULL)
         AND outcome IN ('running', 'unknown', 'blocked')",
            params![intent.project_id, intent.source_task_id, source.5],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if newer != 0 || other_active != 0 {
        return Err("项目有更新或待核实发布任务，不能回滚。".into());
    }
    let already_rolled_back: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rollback_tasks WHERE source_task_id = ?1 AND outcome = 'complete'",
        [&intent.source_task_id], |row| row.get(0),
    ).map_err(|err| err.to_string())?;
    if already_rolled_back != 0 {
        return Err("该源任务已成功回滚，不能再次执行。".into());
    }
    let rollback_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rollback_tasks WHERE project_id = ?1
         AND (docker_daemon_id = ?2 OR docker_daemon_id IS NULL)
         AND outcome IN ('running', 'unknown', 'blocked')",
            params![intent.project_id, source.5],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if rollback_active != 0 {
        return Err("已有待核实回滚任务，不能重复开始。".into());
    }
    let target: (i64, i64, String, String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT rowid, project_id, phase, outcome, health_result, docker_daemon_id
         FROM release_tasks WHERE id = ?1",
            [&intent.target_task_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .map_err(|_| "目标历史版本不存在。")?;
    if target.0 >= source.0
        || target.1 != intent.project_id
        || target.2 != "checked"
        || target.3 != "complete"
        || target.4.as_deref() != Some("healthy")
        || target.5 != source.5
    {
        return Err("目标不是同一 Docker daemon 的较早健康完成版本。".into());
    }
    let current: String = conn.query_row(
        "SELECT task_id FROM project_current_version WHERE project_id = ?1 AND docker_daemon_id = ?2",
        params![intent.project_id, source.5], |row| row.get(0),
    ).map_err(|_| "已确认的当前版本不存在。")?;
    let expected: String = if source.3 == "complete" {
        if current != intent.source_task_id {
            return Err("当前发布记录不是已确认运行版本。".into());
        }
        conn.query_row(
            "SELECT predecessor_task_id FROM release_predecessors WHERE task_id = ?1",
            [&intent.source_task_id],
            |row| row.get(0),
        )
        .map_err(|_| "当前版本缺少前驱记录。")?
    } else {
        current
    };
    if expected != intent.target_task_id {
        return Err("目标不是最近的上一健康完成版本。".into());
    }
    let material: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM release_materials WHERE task_id = ?1 AND state = 'ready'
         AND chunks BETWEEN 1 AND 128 AND length(material_sha256) = 64
         AND material_sha256 NOT GLOB '*[^0-9a-fA-F]*'",
            [&intent.target_task_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    let (images, incomplete): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN image_ids IS NULL OR archive_bytes IS NULL
            OR archive_bytes <= 0 OR archive_sha256 IS NULL OR length(archive_sha256) != 64
            THEN 1 ELSE 0 END), 0) FROM release_task_images WHERE task_id = ?1",
            [&intent.target_task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|err| err.to_string())?;
    let (services, missing_ids): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(confirmed_image_id IS NULL), 0)
         FROM release_task_services WHERE task_id = ?1",
            [&intent.target_task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|err| err.to_string())?;
    if material != 1
        || !(1..=16).contains(&images)
        || incomplete != 0
        || services < images
        || services > 100
        || missing_ids != 0
    {
        return Err("目标版本材料记录不完整，禁止回滚。".into());
    }
    Ok(())
}

pub fn create(intent: &Intent) -> Result<(), String> {
    let result = create_in(&connect()?, intent);
    crate::audit::record(
        "rollback",
        "project",
        intent.project_id,
        "prepared",
        result.is_ok(),
    );
    result
}

fn advance_in(conn: &Connection, id: &str, from: Phase, to: Phase) -> Result<(), String> {
    if !matches!(
        (from, to),
        (Phase::Prepared, Phase::Restoring)
            | (Phase::Restoring, Phase::Restored)
            | (Phase::Restored, Phase::Staging)
            | (Phase::Staging, Phase::Staged)
            | (Phase::Staged, Phase::Validating)
            | (Phase::Validating, Phase::Validated)
            | (Phase::Validated, Phase::Deploying)
            | (Phase::Deploying, Phase::Checking)
    ) {
        return Err("回滚阶段转换无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE rollback_tasks SET phase = ?3, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND phase = ?2 AND outcome = 'running'",
            params![id, from.saved(), to.saved()],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("回滚阶段已变化。".into());
    }
    Ok(())
}

pub fn advance(id: &str, from: Phase, to: Phase) -> Result<(), String> {
    advance_in(&connect()?, id, from, to)
}

pub fn reserve_uploaded_archive(
    rollback_id: &str,
    service: &str,
    directory: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    reserve_uploaded_archive_in(&connect()?, rollback_id, service, directory, bytes, sha256)
}

fn reserve_uploaded_archive_in(
    conn: &Connection,
    rollback_id: &str,
    service: &str,
    directory: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    let suffix = directory
        .strip_prefix("/tmp/devingpanel.")
        .ok_or("回滚临时目录无效。")?;
    if suffix.len() != 32
        || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        || bytes == 0
        || bytes > i64::MAX as u64
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("回滚归档记录无效。".into());
    }
    let changed = conn.execute(
        "INSERT INTO rollback_uploaded_archives (rollback_id, service, directory, bytes, sha256, state)
         SELECT r.id, ?2, ?3, ?4, ?5, 'reserved' FROM rollback_tasks r
         JOIN release_task_images i ON i.task_id = r.target_task_id AND i.service = ?2
         WHERE r.id = ?1 AND r.phase = 'restoring' AND r.outcome = 'running'
         AND i.archive_bytes = ?4 AND i.archive_sha256 = ?5",
        params![rollback_id, service, directory, bytes as i64, sha256],
    ).map_err(|_| "无法记录远端临时归档，禁止加载。")?;
    if changed != 1 {
        return Err("远端临时归档记录未完成。".into());
    }
    Ok(())
}

pub fn confirm_uploaded_archive(
    rollback_id: &str,
    service: &str,
    uploaded: &crate::ssh::UploadedArchive,
) -> Result<(), String> {
    confirm_uploaded_archive_in(&connect()?, rollback_id, service, uploaded)
}

fn confirm_uploaded_archive_in(
    conn: &Connection,
    rollback_id: &str,
    service: &str,
    uploaded: &crate::ssh::UploadedArchive,
) -> Result<(), String> {
    if uploaded.path != format!("{}/image.tar", uploaded.directory) {
        return Err("远端归档路径与保留意图不符。".into());
    }
    let changed = conn
        .execute(
            "UPDATE rollback_uploaded_archives SET state = 'uploaded'
         WHERE rollback_id = ?1 AND service = ?2 AND state = 'reserved'
         AND directory = ?3 AND bytes = ?4 AND sha256 = ?5",
            params![
                rollback_id,
                service,
                uploaded.directory,
                uploaded.bytes as i64,
                uploaded.sha256
            ],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("远端归档与预留记录不一致。".into());
    }
    Ok(())
}

pub fn advance_uploaded_archive(
    rollback_id: &str,
    service: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    advance_uploaded_archive_in(&connect()?, rollback_id, service, from, to)
}

fn advance_uploaded_archive_in(
    conn: &Connection,
    rollback_id: &str,
    service: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    if !matches!(
        (from, to),
        ("uploaded", "loaded") | ("loaded", "cleaned") | ("reserved", "cleaned")
    ) {
        return Err("远端归档状态转换无效。".into());
    }
    let changed = conn.execute(
        "UPDATE rollback_uploaded_archives SET state = ?4 WHERE rollback_id = ?1 AND service = ?2 AND state = ?3",
        params![rollback_id, service, from, to],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("远端归档状态已变化。".into());
    }
    Ok(())
}

fn settle_in(
    conn: &Connection,
    id: &str,
    phase: Phase,
    uncertain: bool,
    detail: &str,
) -> Result<(), String> {
    if phase == Phase::Checked || detail.len() > 512 || detail.chars().any(char::is_control) {
        return Err("回滚结果说明无效。".into());
    }
    let outcome = if uncertain { "unknown" } else { "blocked" };
    let changed = conn
        .execute(
            "UPDATE rollback_tasks SET outcome = ?3, detail = ?4, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND phase = ?2 AND outcome = 'running'",
            params![id, phase.saved(), outcome, detail],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("回滚结果已变化。".into());
    }
    Ok(())
}

pub fn settle(id: &str, phase: Phase, uncertain: bool, detail: &str) -> Result<(), String> {
    settle_in(&connect()?, id, phase, uncertain, detail)
}

fn complete_in(
    conn: &Connection,
    id: &str,
    healthy: bool,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    if healthy {
        verify_target_observed(&tx, id, observed)?;
    }
    let outcome = if healthy { "complete" } else { "blocked" };
    let detail = if healthy {
        "全部服务健康，已恢复目标版本"
    } else {
        "回滚服务未全部健康，需人工核实"
    };
    let changed = tx.execute(
        "UPDATE rollback_tasks SET phase = 'checked', outcome = ?2, detail = ?3,
         observed_containers = ?4,
         updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND phase = 'checking' AND outcome = 'running'",
        params![id, outcome, detail, observed.len() as i64],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("回滚状态已变化。".into());
    }
    if healthy {
        move_current_pointer(&tx, id)?;
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

fn verify_target_observed(
    conn: &Connection,
    id: &str,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    let target: String = conn
        .query_row(
            "SELECT target_task_id FROM rollback_tasks WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "回滚目标已变化。")?;
    let expected: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT service, reference, confirmed_image_id FROM release_task_services
         WHERE task_id = ?1 ORDER BY service",
        )
        .map_err(|err| err.to_string())?
        .query_map([&target], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|err| err.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|_| "回滚目标服务 ID 不完整。")?;
    if expected.is_empty()
        || expected.len() != observed.len()
        || expected.len() > 100
        || expected.iter().any(|(service, reference, image_id)| {
            observed
                .iter()
                .filter(|container| {
                    container.service == *service
                        && container.config_image == *reference
                        && container.image_id == *image_id
                        && container.status == "running"
                        && container.health.as_deref() == Some("healthy")
                })
                .count()
                != 1
        })
    {
        return Err("目标版本容器镜像或健康状态不匹配，回滚未完成。".into());
    }
    Ok(())
}

fn move_current_pointer(conn: &Connection, id: &str) -> Result<(), String> {
    let (project_id, source, target, daemon): (i64, String, String, String) = conn
        .query_row(
            "SELECT r.project_id, r.source_task_id, r.target_task_id, t.docker_daemon_id
             FROM rollback_tasks r JOIN release_tasks t ON t.id = r.source_task_id
             WHERE r.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|err| err.to_string())?;
    let pointer: String = conn
        .query_row(
            "SELECT task_id FROM project_current_version WHERE project_id = ?1 AND docker_daemon_id = ?2",
            params![project_id, daemon],
            |row| row.get(0),
        )
        .map_err(|_| "已确认版本指针缺失；结果待核实。")?;
    if pointer != source && pointer != target {
        return Err("回滚期间已确认版本发生变化；结果待核实。".into());
    }
    let moved = conn
        .execute(
            "UPDATE project_current_version SET task_id = ?3, updated_at = CURRENT_TIMESTAMP
         WHERE project_id = ?1 AND docker_daemon_id = ?2 AND task_id = ?4",
            params![project_id, daemon, target, pointer],
        )
        .map_err(|err| err.to_string())?;
    if moved != 1 {
        return Err("回滚版本指针已变化；结果待核实。".into());
    }
    conn.execute(
        "UPDATE release_tasks SET outcome = 'failed',
        detail = '已通过受控回滚恢复上一版本' WHERE id = ?1
        AND phase = 'checked' AND outcome = 'blocked' AND health_result = 'failed'",
        [&source],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn complete(
    id: &str,
    healthy: bool,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    let result = complete_in(&connect()?, id, healthy, observed);
    crate::audit::record("rollback", "task", id, "checked", result.is_ok() && healthy);
    result
}

fn reconcile_in(
    conn: &Connection,
    task: &TaskSnapshot,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    if !matches!(
        (task.phase.as_str(), task.outcome.as_str()),
        ("checking", "unknown" | "running") | ("checked", "blocked")
    ) {
        return Err("该回滚阶段可能仍有远端副作用，不能只读确认完成。".into());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    verify_target_observed(&tx, &task.id, observed)?;
    let changed = tx
        .execute(
            "UPDATE rollback_tasks SET phase = 'checked', outcome = 'complete',
         detail = '已人工只读复核回滚目标全部服务健康', observed_containers = ?4,
         updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND phase = ?2 AND outcome = ?3",
            params![task.id, task.phase, task.outcome, observed.len() as i64],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("回滚任务阶段已变化。".into());
    }
    move_current_pointer(&tx, &task.id)?;
    tx.execute(
        "INSERT INTO rollback_reconciliations (rollback_id, observed_containers)
        VALUES (?1, ?2)",
        params![task.id, observed.len() as i64],
    )
    .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn reconcile(
    task: &TaskSnapshot,
    observed: &[crate::ssh::ObservedContainer],
) -> Result<(), String> {
    reconcile_in(&connect()?, task, observed)
}

#[cfg(test)]
mod tests {
    use super::{
        Intent, Phase, advance_in, advance_uploaded_archive_in, complete_in,
        confirm_uploaded_archive_in, create_in, move_current_pointer, reconcile_in,
        reserve_uploaded_archive_in, settle_in,
    };
    use crate::database::initialize;
    use rusqlite::Connection;

    fn version_materials(conn: &Connection, task_id: &str) {
        let image_id = format!("sha256:{}", "a".repeat(64));
        conn.execute(
            "INSERT INTO release_materials (task_id, state, chunks, material_sha256)
            VALUES (?1, 'ready', 1, ?2)",
            rusqlite::params![task_id, "b".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_task_images (task_id, ordinal, service, reference, platform,
            image_ids, archive_sha256, archive_bytes)
            VALUES (?1, 0, 'web', 'app:v1', 'linux/amd64', ?2, ?3, 123)",
            rusqlite::params![
                task_id,
                serde_json::to_string(std::slice::from_ref(&image_id)).unwrap(),
                "c".repeat(64)
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_task_services (task_id, service, reference, confirmed_image_id)
            VALUES (?1, 'web', 'app:v1', ?2)",
            rusqlite::params![task_id, image_id],
        )
        .unwrap();
    }

    #[test]
    fn rollback_moves_only_the_source_daemon_pointer() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        for (id, daemon) in [
            ("first", "DAEMON:ONE"),
            ("second", "DAEMON:ONE"),
            ("other", "DAEMON:TWO"),
        ] {
            conn.execute(
                "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome, health_result) VALUES (?1, 1, 2, 'app', 'host', '203.0.113.10',
                22, 'user', 'password', 'SHA256:test', 'unix:///var/run/docker.sock', ?2,
                'app:v1', 'linux/amd64', ?3, 'checked', 'complete', 'healthy')",
                rusqlite::params![id, daemon, "a".repeat(64)],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
            VALUES (1, 'second', 'DAEMON:ONE'), (1, 'other', 'DAEMON:TWO')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id, phase, outcome)
            VALUES ('rollback', 1, 'second', 'first', 'checking', 'running')", []).unwrap();
        move_current_pointer(&conn, "rollback").unwrap();
        let pointers = conn.prepare("SELECT docker_daemon_id, task_id FROM project_current_version ORDER BY docker_daemon_id")
            .unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(
            pointers,
            [
                ("DAEMON:ONE".into(), "first".into()),
                ("DAEMON:TWO".into(), "other".into())
            ]
        );
    }

    #[test]
    fn active_rollback_on_another_daemon_does_not_block_candidate() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        for (id, daemon) in [
            ("a-old", "DAEMON:ONE"),
            ("a-new", "DAEMON:ONE"),
            ("b-old", "DAEMON:TWO"),
            ("b-new", "DAEMON:TWO"),
        ] {
            conn.execute(
                "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome, health_result) VALUES (?1, 1, 2, 'app', 'host', '203.0.113.10',
                22, 'user', 'password', 'SHA256:test', 'unix:///var/run/docker.sock', ?2,
                'app:v1', 'linux/amd64', ?3, 'checked', 'complete', 'healthy')",
                rusqlite::params![id, daemon, "a".repeat(64)],
            )
            .unwrap();
        }
        version_materials(&conn, "b-old");
        conn.execute(
            "INSERT INTO release_predecessors (task_id, predecessor_task_id)
            VALUES ('b-new', 'b-old')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
            VALUES (1, 'a-new', 'DAEMON:ONE'), (1, 'b-new', 'DAEMON:TWO')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id,
            docker_daemon_id, phase, outcome) VALUES ('a-rollback', 1, 'a-new', 'a-old',
            'DAEMON:ONE', 'checking', 'unknown')",
            [],
        )
        .unwrap();
        let candidate = super::candidate_in(&conn, "b-new").unwrap();
        assert_eq!(candidate.target_task_id, "b-old");
        create_in(&conn, &candidate).unwrap();
        let daemon: String = conn
            .query_row(
                "SELECT docker_daemon_id FROM rollback_tasks WHERE id = ?1",
                [&candidate.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(daemon, "DAEMON:TWO");
        assert!(super::candidate_in(&conn, "b-new").is_err());
    }

    #[test]
    fn rollback_intent_requires_latest_safe_source_and_previous_completed_target() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let insert = |id: &str, outcome: &str, health: &str| {
            conn.execute(
                "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome, health_result) VALUES (?1, 1, 2, 'app', 'host', '203.0.113.10',
                22, 'user', 'password', 'SHA256:test', 'unix:///var/run/docker.sock', 'DAEMON:ID',
                'app:v1', 'linux/amd64', ?2, 'checked', ?3, ?4)",
                rusqlite::params![id, "a".repeat(64), outcome, health],
            )
            .unwrap();
        };
        insert("first", "complete", "healthy");
        insert("second", "complete", "healthy");
        insert("third", "blocked", "failed");
        version_materials(&conn, "second");
        conn.execute("INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES ('second', 'first')", []).unwrap();
        conn.execute(
            "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
            VALUES (1, 'second', 'DAEMON:ID')",
            [],
        )
        .unwrap();
        let intent = Intent {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: 1,
            source_task_id: "third".into(),
            target_task_id: "second".into(),
        };
        let mut invalid = Intent {
            target_task_id: "first".into(),
            ..Intent {
                id: uuid::Uuid::new_v4().to_string(),
                project_id: 1,
                source_task_id: "third".into(),
                target_task_id: "second".into(),
            }
        };
        assert!(create_in(&conn, &invalid).is_err());
        invalid.target_task_id = "third".into();
        assert!(create_in(&conn, &invalid).is_err());
        conn.execute("INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
            host_address, host_port, host_username, host_auth, host_fingerprint,
            docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
            phase, outcome, health_result)
            SELECT 'other-daemon', project_id, host_id, project_name, host_name, host_address, host_port,
                host_username, host_auth, host_fingerprint, docker_endpoint, 'DAEMON:OTHER',
                image_reference, platform, config_sha256, 'checked', 'complete', 'healthy'
            FROM release_tasks WHERE id = 'third'", []).unwrap();
        assert_eq!(
            super::candidate_in(&conn, "third").unwrap().target_task_id,
            "second"
        );
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES ('batch', 1, ?1, 'prepared')",
            ["a".repeat(64)],
        )
        .unwrap();
        assert!(create_in(&conn, &intent).is_err());
        conn.execute(
            "UPDATE release_batches SET state = 'paused' WHERE id = 'batch'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
            host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
            platform, docker_endpoint, docker_daemon_id, state, release_task_id)
            VALUES ('batch', 0, 2, 'host', 'test', '203.0.113.10', 22, 'user', 'password',
            'SHA256:test', 'linux/amd64', 'unix:///var/run/docker.sock', 'DAEMON:ID',
            'failed', 'third')",
            [],
        )
        .unwrap();
        create_in(&conn, &intent).unwrap();
        assert!(super::candidate_in(&conn, "third").is_err());
        assert!(
            create_in(
                &conn,
                &Intent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ..intent.clone()
                }
            )
            .is_err()
        );
        advance_in(&conn, &intent.id, Phase::Prepared, Phase::Restoring).unwrap();
        assert!(advance_in(&conn, &intent.id, Phase::Prepared, Phase::Restoring).is_err());
        let uploaded = crate::ssh::UploadedArchive {
            directory: format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple()),
            path: String::new(),
            bytes: 123,
            sha256: "c".repeat(64),
            image_ids: vec![],
        };
        let uploaded = crate::ssh::UploadedArchive {
            path: format!("{}/image.tar", uploaded.directory),
            ..uploaded
        };
        reserve_uploaded_archive_in(
            &conn,
            &intent.id,
            "web",
            &uploaded.directory,
            uploaded.bytes,
            &uploaded.sha256,
        )
        .unwrap();
        assert!(
            reserve_uploaded_archive_in(
                &conn,
                &intent.id,
                "web",
                &uploaded.directory,
                uploaded.bytes,
                &uploaded.sha256
            )
            .is_err()
        );
        confirm_uploaded_archive_in(&conn, &intent.id, "web", &uploaded).unwrap();
        assert!(confirm_uploaded_archive_in(&conn, &intent.id, "web", &uploaded).is_err());
        advance_uploaded_archive_in(&conn, &intent.id, "web", "uploaded", "loaded").unwrap();
        advance_uploaded_archive_in(&conn, &intent.id, "web", "loaded", "cleaned").unwrap();
        assert!(
            advance_uploaded_archive_in(&conn, &intent.id, "web", "loaded", "cleaned").is_err()
        );
        settle_in(&conn, &intent.id, Phase::Restoring, true, "result unknown").unwrap();
        assert!(complete_in(&conn, &intent.id, true, &[]).is_err());
    }

    #[test]
    fn successful_rollback_moves_current_pointer_and_failed_next_release_chooses_restored_version()
    {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let insert = |id: &str, outcome: &str, health: &str| {
            conn.execute(
                "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome, health_result) VALUES (?1, 1, 2, 'app', 'host', '203.0.113.10',
                22, 'user', 'password', 'SHA256:test', 'unix:///var/run/docker.sock', 'DAEMON:ID',
                'app:v1', 'linux/amd64', ?2, 'checked', ?3, ?4)",
                rusqlite::params![id, "a".repeat(64), outcome, health],
            )
            .unwrap();
        };
        insert("first", "complete", "healthy");
        insert("second", "complete", "healthy");
        version_materials(&conn, "first");
        conn.execute("INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES ('second', 'first')", []).unwrap();
        conn.execute(
            "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
            VALUES (1, 'second', 'DAEMON:ID')",
            [],
        )
        .unwrap();
        let rollback = Intent {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: 1,
            source_task_id: "second".into(),
            target_task_id: "first".into(),
        };
        create_in(&conn, &rollback).unwrap();
        let phases = [
            Phase::Prepared,
            Phase::Restoring,
            Phase::Restored,
            Phase::Staging,
            Phase::Staged,
            Phase::Validating,
            Phase::Validated,
            Phase::Deploying,
            Phase::Checking,
        ];
        for pair in phases.windows(2) {
            advance_in(&conn, &rollback.id, pair[0], pair[1]).unwrap();
        }
        let observed = [crate::ssh::ObservedContainer {
            service: "web".into(),
            status: "running".into(),
            health: Some("healthy".into()),
            config_image: "app:v1".into(),
            image_id: format!("sha256:{}", "a".repeat(64)),
        }];
        complete_in(&conn, &rollback.id, true, &observed).unwrap();
        let current: String = conn
            .query_row(
                "SELECT task_id FROM project_current_version WHERE project_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(current, "first");
        insert("new-complete", "complete", "healthy");
        conn.execute("INSERT INTO release_predecessors (task_id, predecessor_task_id) VALUES ('new-complete', 'first')", []).unwrap();
        conn.execute(
            "UPDATE project_current_version SET task_id = 'new-complete' WHERE project_id = 1",
            [],
        )
        .unwrap();
        let next = super::candidate_in(&conn, "new-complete").unwrap();
        assert_eq!(next.target_task_id, "first");
        conn.execute(
            "UPDATE release_tasks SET outcome = 'failed' WHERE id = 'new-complete'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE project_current_version SET task_id = 'first' WHERE project_id = 1",
            [],
        )
        .unwrap();
        insert("third", "blocked", "failed");
        let candidate = super::candidate_in(&conn, "third").unwrap();
        assert_eq!(candidate.target_task_id, "first");
        assert!(
            create_in(
                &conn,
                &Intent {
                    target_task_id: "second".into(),
                    ..candidate.clone()
                }
            )
            .is_err()
        );
        create_in(&conn, &candidate).unwrap();
        for pair in phases.windows(2) {
            advance_in(&conn, &candidate.id, pair[0], pair[1]).unwrap();
        }
        settle_in(
            &conn,
            &candidate.id,
            Phase::Checking,
            true,
            "SSH disconnected",
        )
        .unwrap();
        let task = super::TaskSnapshot {
            id: candidate.id.clone(),
            project_id: 1,
            source_task_id: "third".into(),
            target_task_id: "first".into(),
            phase: "checking".into(),
            outcome: "unknown".into(),
            detail: String::new(),
            started_at: String::new(),
            observed_containers: None,
            pending_directories: None,
        };
        assert!(reconcile_in(&conn, &task, &[]).is_err());
        reconcile_in(&conn, &task, &observed).unwrap();
        assert!(reconcile_in(&conn, &task, &observed).is_err());
        let count: i64 = conn
            .query_row(
                "SELECT observed_containers FROM rollback_reconciliations WHERE rollback_id = ?1",
                [&candidate.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let outcome: String = conn
            .query_row(
                "SELECT outcome FROM release_tasks WHERE id = 'third'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "failed");
    }
}
