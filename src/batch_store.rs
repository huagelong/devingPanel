use crate::database::connect;
use crate::stack_release::BatchPreflight;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[allow(dead_code)] // Batch history UI is not enabled yet.
pub struct BatchTargetRecord {
    pub ordinal: usize,
    pub host: crate::host_store::Host,
    pub platform: String,
    pub docker_endpoint: String,
    pub docker_daemon_id: String,
    pub env_sha256: Option<String>,
    pub state: String,
    pub release_task_id: Option<String>,
    pub task_outcome: Option<String>,
}

#[allow(dead_code)] // Batch history UI is not enabled yet.
pub struct BatchImageRecord {
    pub service: String,
    pub platform: String,
    pub reference: String,
    pub archive_bytes: Option<i64>,
    pub archive_sha256: Option<String>,
    pub image_ids: Option<Vec<String>>,
}

#[allow(dead_code)] // Batch history UI is not enabled yet.
pub struct BatchRecord {
    pub id: String,
    pub project_id: i64,
    pub config_sha256: String,
    pub state: String,
    pub created_at: String,
    pub images: Vec<BatchImageRecord>,
    pub targets: Vec<BatchTargetRecord>,
}

fn create_in(conn: &Connection, project_id: i64, plan: &BatchPreflight) -> Result<String, String> {
    if project_id <= 0
        || plan.config_sha256.len() != 64
        || !plan.config_sha256.bytes().all(|ch| ch.is_ascii_hexdigit())
        || plan.plan.hosts.len() < 2
        || plan.plan.hosts.len() != plan.targets.len()
        || plan.plan.hosts.len() != plan.env_files.len()
        || plan.plan.builds.is_empty()
        || plan.plan.builds.len() > 32
    {
        return Err("批次意图或目标快照无效。".into());
    }
    let mut daemons = HashSet::new();
    for (host, identity) in plan.plan.hosts.iter().zip(&plan.targets) {
        let mut services = HashSet::new();
        if host.host_id != identity.host_id
            || host.platform != identity.platform
            || identity.host.id != host.host_id
            || identity.host.trusted_fingerprint.is_none()
            || !daemons.insert(&identity.docker_daemon_id)
            || identity.docker_daemon_id.is_empty()
            || identity.docker_endpoint.is_empty()
            || host.images.len() != plan.builds.len()
            || host.images.len()
                != plan
                    .plan
                    .builds
                    .iter()
                    .filter(|build| build.platform == host.platform)
                    .count()
            || host.images.iter().any(|(service, reference)| {
                if !services.insert(service) {
                    return true;
                }
                plan.plan
                    .builds
                    .iter()
                    .filter(|build| {
                        build.service == *service
                            && build.platform == host.platform
                            && build.reference == *reference
                    })
                    .count()
                    != 1
            })
        {
            return Err("批次目标与镜像矩阵不一致。".into());
        }
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let manifest: i64 = tx
        .query_row(
            "SELECT manifest_mode FROM projects WHERE id = ?1",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|_| "批次项目不存在。")?;
    if manifest != 1 {
        return Err("批次只支持 YAML 项目。".into());
    }
    let unresolved: i64 = tx
        .query_row(
            "SELECT (SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1
            AND outcome IN ('running', 'unknown', 'blocked'))
          + (SELECT COUNT(*) FROM rollback_tasks WHERE project_id = ?1
            AND outcome IN ('running', 'unknown', 'blocked'))",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if unresolved != 0 {
        return Err("项目有待核实的发布或回滚任务，不能创建批次。".into());
    }
    for (ordinal, target) in plan.targets.iter().enumerate() {
        let host = &target.host;
        let bound: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM project_hosts p JOIN hosts h ON h.id = p.host_id
             WHERE p.project_id = ?1 AND p.ordinal = ?2 AND p.host_id = ?3
             AND h.deleting = 0 AND h.name = ?4 AND h.group_name = ?5
             AND h.address = ?6 AND h.port = ?7 AND h.username = ?8
             AND h.auth = ?9 AND h.trusted_fingerprint = ?10",
                params![
                    project_id,
                    ordinal as i64,
                    host.id,
                    host.name,
                    host.group,
                    host.address,
                    host.port,
                    host.username,
                    host.auth,
                    host.trusted_fingerprint
                ],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())?;
        if bound != 1 {
            return Err("批次目标绑定或可信主机资料已变化。".into());
        }
    }
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM project_hosts WHERE project_id = ?1",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if count as usize != plan.targets.len() {
        return Err("批次目标主机数量已变化。".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO release_batches (id, project_id, config_sha256, state)
         VALUES (?1, ?2, ?3, 'prepared')",
        params![id, project_id, plan.config_sha256],
    )
    .map_err(|_| "项目已有未完成或待核实的批次。".to_owned())?;
    for build in &plan.plan.builds {
        tx.execute(
            "INSERT INTO release_batch_images (batch_id, service, platform, reference)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, build.service, build.platform, build.reference],
        )
        .map_err(|_| "批次镜像矩阵重复。".to_owned())?;
    }
    for (ordinal, target) in plan.targets.iter().enumerate() {
        let host = &target.host;
        tx.execute(
            "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
             host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
             platform, docker_endpoint, docker_daemon_id, env_sha256, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'waiting')",
            params![
                id,
                ordinal as i64,
                host.id,
                host.name,
                host.group,
                host.address,
                host.port,
                host.username,
                host.auth,
                host.trusted_fingerprint,
                target.platform,
                target.docker_endpoint,
                target.docker_daemon_id,
                format!("{:x}", Sha256::digest(&plan.env_files[ordinal]))
            ],
        )
        .map_err(|_| "批次目标身份重复。".to_owned())?;
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(id)
}

#[allow(dead_code)] // Batch execution remains disabled until per-host release intents are atomic.
pub fn create(project_id: i64, plan: &BatchPreflight) -> Result<String, String> {
    crate::stack_release::revalidate_batch(
        project_id,
        plan,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    create_in(&connect()?, project_id, plan)
}

fn begin_build_in(conn: &Connection, id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE release_batches SET state = 'building', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND state = 'prepared'",
            [id],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("批次构建阶段已变化。".into());
    }
    Ok(())
}

#[allow(dead_code)] // The coordinator must call this before the first local build.
pub fn begin_build(id: &str) -> Result<(), String> {
    begin_build_in(&connect()?, id)
}

fn terminate_unstarted_in(conn: &Connection, id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE release_batches SET state = 'terminated', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND state IN ('prepared', 'building', 'ready')
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets WHERE batch_id = ?1
             AND (state != 'waiting' OR release_task_id IS NOT NULL))",
            [id],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("批次已有目标任务或阶段待核实，不能按未启动终止。".into());
    }
    Ok(())
}

#[allow(dead_code)] // Manual resolution; never deletes images or archives automatically.
pub fn terminate_unstarted(id: &str) -> Result<(), String> {
    let conn = connect()?;
    let project_id: i64 = conn
        .query_row(
            "SELECT project_id FROM release_batches WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "批次不存在。")?;
    let _guard = crate::release_materials::acquire_guard(project_id)?;
    terminate_unstarted_in(&conn, id)
}

pub(crate) fn terminate_resolved_in(conn: &Connection, id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE release_batches SET state = 'terminated', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND state IN ('paused', 'running')
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets t WHERE t.batch_id = ?1
             AND t.state IN ('running', 'unknown'))
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets t JOIN release_tasks r
             ON r.id = t.release_task_id WHERE t.batch_id = ?1
             AND r.outcome IN ('running', 'unknown', 'blocked'))
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets t JOIN release_materials m
             ON m.task_id = t.release_task_id WHERE t.batch_id = ?1 AND m.state = 'writing')
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets t
             LEFT JOIN release_tasks r ON r.id = t.release_task_id
             WHERE t.batch_id = ?1 AND (
                (t.state = 'waiting' AND t.release_task_id IS NOT NULL)
                OR (t.state = 'healthy' AND r.outcome IS NOT 'complete')
                OR (t.state IN ('failed', 'skipped') AND r.outcome IS NOT 'failed')
                OR t.state NOT IN ('waiting', 'healthy', 'failed', 'skipped'))) ",
            [id],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("批次仍有待核实的目标，不能终止。".into());
    }
    Ok(())
}

#[allow(dead_code)] // Explicit terminal decision after all started tasks are resolved.
pub fn terminate_resolved(id: &str) -> Result<(), String> {
    let conn = connect()?;
    let project_id: i64 = conn
        .query_row(
            "SELECT project_id FROM release_batches WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "批次不存在。")?;
    let _guard = crate::release_materials::acquire_guard(project_id)?;
    terminate_resolved_in(&conn, id)
}

pub(crate) fn skip_failed_in(conn: &Connection, id: &str, host_id: i64) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE release_batch_targets SET state = 'skipped', updated_at = CURRENT_TIMESTAMP
         WHERE batch_id = ?1 AND host_id = ?2 AND state = 'failed'
         AND EXISTS (SELECT 1 FROM release_batches b WHERE b.id = ?1 AND b.state = 'paused')
         AND EXISTS (SELECT 1 FROM release_tasks r WHERE r.id = release_task_id
             AND r.outcome = 'failed')
         AND NOT EXISTS (SELECT 1 FROM release_materials m
             WHERE m.task_id = release_task_id AND m.state = 'writing')
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets other
             WHERE other.batch_id = ?1 AND (
                 other.state IN ('running', 'unknown')
                 OR EXISTS (SELECT 1 FROM release_tasks r
                     WHERE r.id = other.release_task_id
                     AND r.outcome IN ('running', 'unknown', 'blocked'))
                 OR EXISTS (SELECT 1 FROM release_materials m
                     WHERE m.task_id = other.release_task_id AND m.state = 'writing')))
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets other
             LEFT JOIN release_tasks r ON r.id = other.release_task_id
             WHERE other.batch_id = ?1 AND (
                 (other.state = 'waiting' AND other.release_task_id IS NOT NULL)
                 OR (other.state = 'healthy' AND r.outcome IS NOT 'complete')
                 OR (other.state IN ('failed', 'skipped') AND r.outcome IS NOT 'failed')))
         AND NOT EXISTS (SELECT 1 FROM release_batch_targets earlier
             WHERE earlier.batch_id = ?1 AND earlier.ordinal < release_batch_targets.ordinal
             AND earlier.state NOT IN ('healthy', 'skipped'))",
            params![id, host_id],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("该主机仍待核实或不在当前批次顺序，不能跳过。".into());
    }
    let pending: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM release_batch_targets WHERE batch_id = ?1 AND state = 'waiting'",
            [id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    let state = if pending == 0 { "complete" } else { "running" };
    let parent = tx
        .execute(
            "UPDATE release_batches SET state = ?2, updated_at = CURRENT_TIMESTAMP
        WHERE id = ?1 AND state = 'paused'",
            params![id, state],
        )
        .map_err(|err| err.to_string())?;
    if parent != 1 {
        return Err("批次结果已变化。".into());
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

#[allow(dead_code)] // Exposed only after failed-task reconciliation and user confirmation.
pub fn skip_failed(id: &str, host_id: i64) -> Result<(), String> {
    let conn = connect()?;
    let project_id: i64 = conn
        .query_row(
            "SELECT project_id FROM release_batches WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "批次不存在。")?;
    let _guard = crate::release_materials::acquire_guard(project_id)?;
    skip_failed_in(&conn, id, host_id)
}

pub(crate) fn pause_waiting(id: &str, ordinal: usize) -> Result<(), String> {
    let conn = connect()?;
    let changed = conn
        .execute(
            "UPDATE release_batches SET state = 'paused', updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND state IN ('ready', 'running')
         AND EXISTS (SELECT 1 FROM release_batch_targets WHERE batch_id = ?1
             AND ordinal = ?2 AND state = 'waiting' AND release_task_id IS NULL)",
            params![id, ordinal as i64],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("批次目标已有任务或阶段已变化。".into());
    }
    Ok(())
}

pub(crate) fn close_pretransfer_in(conn: &Connection, task_id: &str) -> Result<(), String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx.execute(
        "UPDATE release_tasks SET outcome = 'failed', detail = '批次本机准备中断；确认尚未开始传输',
         updated_at = CURRENT_TIMESTAMP WHERE id = ?1
         AND ((phase IN ('building', 'built') AND outcome = 'running')
             OR (phase = 'prepared' AND outcome = 'failed'))
         AND NOT EXISTS (SELECT 1 FROM release_materials m
             WHERE m.task_id = release_tasks.id AND m.state = 'writing')
         AND EXISTS (SELECT 1 FROM release_batch_targets t JOIN release_batches b
             ON b.id = t.batch_id WHERE t.release_task_id = ?1
             AND t.state = 'running' AND b.state = 'running')",
        [task_id],
    ).map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("任务可能已开始远端传输，不能按本机准备中断关闭。".into());
    }
    let target = tx
        .execute(
            "UPDATE release_batch_targets SET state = 'failed',
        updated_at = CURRENT_TIMESTAMP WHERE release_task_id = ?1 AND state = 'running'",
            [task_id],
        )
        .map_err(|err| err.to_string())?;
    let parent = tx
        .execute(
            "UPDATE release_batches SET state = 'paused',
        updated_at = CURRENT_TIMESTAMP WHERE state = 'running'
        AND id = (SELECT batch_id FROM release_batch_targets WHERE release_task_id = ?1)",
            [task_id],
        )
        .map_err(|err| err.to_string())?;
    if target != 1 || parent != 1 {
        return Err("批次目标状态已变化。".into());
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

#[allow(dead_code)] // Manual local-only recovery after the project process guard is released.
pub fn close_pretransfer(task_id: &str) -> Result<(), String> {
    let conn = connect()?;
    let project_id: i64 = conn
        .query_row(
            "SELECT project_id FROM release_tasks WHERE id = ?1",
            [task_id],
            |row| row.get(0),
        )
        .map_err(|_| "任务不存在。")?;
    let _guard = crate::release_materials::acquire_guard(project_id)?;
    close_pretransfer_in(&conn, task_id)
}

pub(crate) fn require_building(id: &str, project_id: i64, digest: &str) -> Result<(), String> {
    let valid: i64 = connect()?
        .query_row(
            "SELECT COUNT(*) FROM release_batches WHERE id = ?1 AND project_id = ?2
         AND config_sha256 = ?3 AND state = 'building'",
            params![id, project_id, digest],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if valid != 1 {
        return Err("批次构建意图不存在或状态已变化。".into());
    }
    Ok(())
}

fn record_archive_in(
    conn: &Connection,
    id: &str,
    service: &str,
    platform: &str,
    bytes: u64,
    sha256: &str,
    image_ids: &[String],
) -> Result<(), String> {
    if bytes == 0
        || bytes > i64::MAX as u64
        || sha256.len() != 64
        || !sha256.bytes().all(|ch| ch.is_ascii_hexdigit())
        || !valid_image_ids(image_ids)
    {
        return Err("批次共享归档摘要无效。".into());
    }
    let changed = conn
        .execute(
            "UPDATE release_batch_images SET archive_bytes = ?4, archive_sha256 = ?5,
             image_ids = ?6
         WHERE batch_id = ?1 AND service = ?2 AND platform = ?3
         AND archive_bytes IS NULL AND archive_sha256 IS NULL AND image_ids IS NULL
         AND EXISTS (SELECT 1 FROM release_batches WHERE id = ?1 AND state = 'building')",
            params![
                id,
                service,
                platform,
                bytes as i64,
                sha256,
                serde_json::to_string(image_ids).map_err(|_| "无法保存共享镜像 ID。")?
            ],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("批次共享归档记录已变化。".into());
    }
    Ok(())
}

pub(crate) fn record_archive(
    id: &str,
    service: &str,
    platform: &str,
    bytes: u64,
    sha256: &str,
    image_ids: &[String],
) -> Result<(), String> {
    record_archive_in(&connect()?, id, service, platform, bytes, sha256, image_ids)
}

fn valid_image_ids(ids: &[String]) -> bool {
    !ids.is_empty()
        && ids.len() <= 4
        && ids.iter().all(|id| {
            id.len() == 71
                && id.starts_with("sha256:")
                && id[7..].bytes().all(|ch| ch.is_ascii_hexdigit())
        })
}

pub(crate) fn archive_record(
    id: &str,
    service: &str,
    platform: &str,
) -> Result<(u64, String, Vec<String>, String), String> {
    let (bytes, sha256, encoded, reference): (i64, String, String, String) = connect()?
        .query_row(
            "SELECT archive_bytes, archive_sha256, image_ids, reference FROM release_batch_images
         WHERE batch_id = ?1 AND service = ?2 AND platform = ?3",
            params![id, service, platform],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|_| "批次共享归档记录缺失。")?;
    let ids: Vec<String> = serde_json::from_str(&encoded).map_err(|_| "共享镜像 ID 无效。")?;
    if bytes <= 0 || sha256.len() != 64 || !valid_image_ids(&ids) {
        return Err("批次共享归档记录无效。".into());
    }
    Ok((bytes as u64, sha256, ids, reference))
}

fn ready_in(
    conn: &Connection,
    id: &str,
    mut verify: impl FnMut(&str, &str, &str, u64, &str, &[String]) -> Result<(), String>,
) -> Result<(), String> {
    let phase: String = conn
        .query_row(
            "SELECT state FROM release_batches WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "批次不存在。")?;
    if phase != "building" {
        return Err("批次未处于构建阶段。".into());
    }
    let images = conn
        .prepare(
            "SELECT service, platform, reference, archive_bytes, archive_sha256, image_ids
        FROM release_batch_images WHERE batch_id = ?1 ORDER BY service, platform",
        )
        .map_err(|err| err.to_string())?
        .query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    if images.is_empty() {
        return Err("批次没有共享镜像计划。".into());
    }
    let count = images.len();
    for (service, platform, reference, bytes, sha256, encoded_ids) in images {
        let bytes = bytes
            .filter(|value| *value > 0)
            .ok_or("批次共享归档记录不完整。")?;
        let sha256 = sha256
            .filter(|value| value.len() == 64 && value.bytes().all(|ch| ch.is_ascii_hexdigit()))
            .ok_or("批次共享归档记录不完整。")?;
        let ids: Vec<String> = serde_json::from_str(&encoded_ids.ok_or("批次共享镜像 ID 缺失。")?)
            .map_err(|_| "批次共享镜像 ID 无效。")?;
        if !valid_image_ids(&ids) {
            return Err("批次共享镜像 ID 无效。".into());
        }
        verify(&service, &platform, &reference, bytes as u64, &sha256, &ids)?;
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE release_batches SET state = 'ready',
        updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND state = 'building'
        AND (SELECT COUNT(*) FROM release_batch_images WHERE batch_id = ?1
             AND archive_bytes > 0 AND image_ids IS NOT NULL
             AND length(archive_sha256) = 64
             AND archive_sha256 NOT GLOB '*[^0-9a-fA-F]*') = ?2
        AND NOT EXISTS (SELECT 1 FROM release_batch_targets WHERE batch_id = ?1
            AND state != 'waiting')",
            params![id, count as i64],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 {
        return Err("批次构建状态已变化。".into());
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(())
}

#[allow(dead_code)] // This gate runs only after all shared archives are retained and verified.
pub fn ready(id: &str) -> Result<(), String> {
    ready_in(
        &connect()?,
        id,
        |service, platform, reference, bytes, sha256, ids| {
            crate::release_archive::verify_shared_identity_recorded(
                id, service, platform, reference, bytes, sha256, ids,
            )
        },
    )
}

#[allow(dead_code)] // History UI will consume this when batch execution is enabled.
pub fn get(id: &str) -> Result<BatchRecord, String> {
    let conn = connect()?;
    get_in(&conn, id)
}

pub fn recent(project_id: Option<i64>) -> Result<Vec<BatchRecord>, String> {
    let conn = connect()?;
    recent_in(&conn, project_id)
}

pub(crate) fn interrupted_projects_in(conn: &Connection) -> Result<Vec<i64>, String> {
    conn.prepare(
        "SELECT DISTINCT b.project_id FROM release_batches b
        JOIN release_batch_targets bt ON bt.batch_id = b.id
        JOIN release_tasks t ON t.id = bt.release_task_id
        WHERE b.state = 'running' AND bt.state = 'running' AND t.outcome = 'running'
        AND t.phase IN ('uploading', 'uploaded', 'loading', 'loaded',
            'awaiting_deployment', 'staging', 'staged', 'validating',
            'validated', 'deploying', 'checking')",
    )
    .map_err(|err| err.to_string())?
    .query_map([], |row| row.get::<_, i64>(0))
    .map_err(|err| err.to_string())?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|err| err.to_string())
}

pub(crate) fn mark_interrupted_in(conn: &Connection, project_id: i64) -> Result<bool, String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let task: Option<(String, String)> = tx.query_row(
        "SELECT t.id, b.id FROM release_batches b
         JOIN release_batch_targets bt ON bt.batch_id = b.id
         JOIN release_tasks t ON t.id = bt.release_task_id
         WHERE b.project_id = ?1 AND b.state = 'running' AND bt.state = 'running'
         AND t.outcome = 'running' AND t.phase IN ('uploading', 'uploaded', 'loading', 'loaded',
             'awaiting_deployment', 'staging', 'staged', 'validating', 'validated', 'deploying', 'checking')",
        [project_id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(|err| err.to_string())?;
    let Some((task_id, batch_id)) = task else {
        return Ok(false);
    };
    let active: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM release_batch_targets WHERE batch_id = ?1 AND state = 'running'",
            [&batch_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if active != 1 {
        return Err("批次存在多个进行中目标，未自动核实。".into());
    }
    let changed = tx.execute("UPDATE release_tasks SET outcome = 'unknown',
        detail = '客户端执行中断；远端结果待人工核实，未自动重试', updated_at = CURRENT_TIMESTAMP
        WHERE id = ?1 AND outcome = 'running' AND phase IN ('uploading', 'uploaded', 'loading', 'loaded',
            'awaiting_deployment', 'staging', 'staged', 'validating', 'validated', 'deploying', 'checking')",
        [&task_id]).map_err(|err| err.to_string())?;
    let target = tx
        .execute(
            "UPDATE release_batch_targets SET state = 'unknown', updated_at = CURRENT_TIMESTAMP
        WHERE release_task_id = ?1 AND state = 'running'",
            [&task_id],
        )
        .map_err(|err| err.to_string())?;
    let batch = tx
        .execute(
            "UPDATE release_batches SET state = 'unknown', updated_at = CURRENT_TIMESTAMP
        WHERE id = ?1 AND state = 'running'",
            [&batch_id],
        )
        .map_err(|err| err.to_string())?;
    if changed != 1 || target != 1 || batch != 1 {
        return Err("中断批次状态已变化，未更新。".into());
    }
    tx.commit().map_err(|err| err.to_string())?;
    Ok(true)
}

fn recent_in(conn: &Connection, project_id: Option<i64>) -> Result<Vec<BatchRecord>, String> {
    let ids = conn.prepare("SELECT id FROM release_batches WHERE (?1 IS NULL OR project_id = ?1)
        ORDER BY CASE WHEN state IN ('prepared', 'building', 'ready', 'running', 'paused', 'unknown')
            THEN 0 ELSE 1 END, created_at DESC, rowid DESC LIMIT 100")
        .map_err(|err| err.to_string())?
        .query_map([project_id], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>().map_err(|err| err.to_string())?;
    ids.iter().map(|id| get_in(conn, id)).collect()
}

fn get_in(conn: &Connection, id: &str) -> Result<BatchRecord, String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)
        .map_err(|err| err.to_string())?;
    let (project_id, config_sha256, state, created_at) = tx
        .query_row(
            "SELECT project_id, config_sha256, state, created_at
             FROM release_batches WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|_| "批次记录不存在。")?;
    let images = tx
        .prepare(
            "SELECT service, platform, reference, archive_bytes, archive_sha256, image_ids
         FROM release_batch_images WHERE batch_id = ?1 ORDER BY service, platform",
        )
        .map_err(|err| err.to_string())?
        .query_map([id], |row| {
            Ok(BatchImageRecord {
                service: row.get(0)?,
                platform: row.get(1)?,
                reference: row.get(2)?,
                archive_bytes: row.get(3)?,
                archive_sha256: row.get(4)?,
                image_ids: row
                    .get::<_, Option<String>>(5)?
                    .map(|text| serde_json::from_str(&text))
                    .transpose()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let targets = tx
        .prepare(
            "SELECT t.ordinal, t.host_id, t.host_name, t.host_group, t.host_address, t.host_port,
                t.host_username, t.host_auth, t.host_fingerprint, t.platform, t.docker_endpoint,
                t.docker_daemon_id, t.env_sha256, t.state, t.release_task_id, r.outcome
                FROM release_batch_targets t LEFT JOIN release_tasks r ON r.id = t.release_task_id
         WHERE t.batch_id = ?1 ORDER BY t.ordinal",
        )
        .map_err(|err| err.to_string())?
        .query_map([id], |row| {
            Ok(BatchTargetRecord {
                ordinal: row.get::<_, i64>(0)? as usize,
                host: crate::host_store::Host {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    group: row.get(3)?,
                    address: row.get(4)?,
                    port: row.get(5)?,
                    username: row.get(6)?,
                    auth: row.get(7)?,
                    trusted_fingerprint: Some(row.get(8)?),
                },
                platform: row.get(9)?,
                docker_endpoint: row.get(10)?,
                docker_daemon_id: row.get(11)?,
                env_sha256: row.get(12)?,
                state: row.get(13)?,
                release_task_id: row.get(14)?,
                task_outcome: row.get(15)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(BatchRecord {
        id: id.into(),
        project_id,
        config_sha256,
        state,
        created_at,
        images,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        begin_build_in, create_in, get_in, interrupted_projects_in, mark_interrupted_in, ready_in,
        recent_in, record_archive_in, skip_failed_in, terminate_resolved_in,
        terminate_unstarted_in,
    };
    use crate::database::initialize;
    use crate::host_store::Host;
    use crate::project_config::BuildSpec;
    use crate::stack_release::{
        BatchBuild, BatchHost, BatchPlan, BatchPreflight, BatchTargetIdentity,
    };
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};

    #[test]
    fn interrupted_remote_batch_becomes_unknown_without_touching_waiting_targets() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image)
            VALUES (7, 'smoke', '.', 'Dockerfile', 'smoke');
            INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES ('interrupted', 7, 'digest', 'running');
            INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome, health_result) VALUES ('healthy-task', 7, 10, 'smoke', 'previous',
                'previous.invalid', 22, 'root', 'password', 'SHA256:verified',
                'unix:///var/run/docker.sock', 'DAEMON:PREVIOUS', 'smoke:v1', 'linux/amd64',
                'digest', 'checked', 'complete', 'healthy');
            INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome) VALUES ('remote-task', 7, 11, 'smoke', 'one',
                'one.invalid', 22, 'root', 'password', 'SHA256:verified',
                'unix:///var/run/docker.sock', 'DAEMON:ONE', 'smoke:v1', 'linux/amd64',
                'digest', 'uploading', 'running');
            INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
                host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
                platform, docker_endpoint, docker_daemon_id, state, release_task_id)
             VALUES ('interrupted', 0, 10, 'previous', 'test', 'previous.invalid', 22, 'root',
                'password', 'SHA256:verified', 'linux/amd64', 'unix:///var/run/docker.sock',
                'DAEMON:PREVIOUS', 'healthy', 'healthy-task'),
                ('interrupted', 1, 11, 'one', 'test', 'one.invalid', 22, 'root',
                'password', 'SHA256:verified', 'linux/amd64', 'unix:///var/run/docker.sock',
                'DAEMON:ONE', 'running', 'remote-task'),
                ('interrupted', 2, 12, 'two', 'test', 'two.invalid', 22, 'root',
                'password', 'SHA256:verified', 'linux/amd64', 'unix:///var/run/docker.sock',
                'DAEMON:TWO', 'waiting', NULL);",
        )
        .unwrap();
        assert_eq!(interrupted_projects_in(&conn).unwrap(), [7]);
        conn.execute(
            "UPDATE release_batch_targets SET state = 'running'
            WHERE batch_id = 'interrupted' AND ordinal = 2",
            [],
        )
        .unwrap();
        assert!(mark_interrupted_in(&conn, 7).is_err());
        let state: String = conn
            .query_row(
                "SELECT outcome FROM release_tasks WHERE id = 'remote-task'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "running");
        conn.execute(
            "UPDATE release_batch_targets SET state = 'waiting'
            WHERE batch_id = 'interrupted' AND ordinal = 2",
            [],
        )
        .unwrap();
        assert!(mark_interrupted_in(&conn, 7).unwrap());
        assert!(!mark_interrupted_in(&conn, 7).unwrap());
        let batch = get_in(&conn, "interrupted").unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[0].task_outcome.as_deref(), Some("complete"));
        assert_eq!(batch.targets[1].state, "unknown");
        assert_eq!(batch.targets[1].task_outcome.as_deref(), Some("unknown"));
        assert_eq!(batch.targets[2].state, "waiting");
        assert!(skip_failed_in(&conn, "interrupted", 11).is_err());
        assert!(terminate_resolved_in(&conn, "interrupted").is_err());
        assert!(interrupted_projects_in(&conn).unwrap().is_empty());
    }

    #[test]
    fn batch_intent_and_ordered_waiting_targets_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("batch-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("state.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, path, dockerfile, image, manifest_mode)
            VALUES (1, 'app', '.', 'Dockerfile', 'app', 1)",
            [],
        )
        .unwrap();
        let hosts = ["one", "two"].map(|name| Host {
            id: if name == "one" { 11 } else { 12 },
            name: name.into(),
            address: format!("{name}.invalid"),
            port: 22,
            username: "root".into(),
            group: "test".into(),
            auth: "password".into(),
            trusted_fingerprint: Some("SHA256:verified".into()),
        });
        for (ordinal, host) in hosts.iter().enumerate() {
            conn.execute(
                "INSERT INTO hosts (id, name, address, port, username, auth, group_name,
                credential_id, trusted_fingerprint) VALUES (?1, ?2, ?3, 22, 'root', 'password',
                'test', ?4, 'SHA256:verified')",
                rusqlite::params![
                    host.id,
                    host.name,
                    host.address,
                    format!("credential-{ordinal}")
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO project_hosts (project_id, host_id, ordinal)
                VALUES (1, ?1, ?2)",
                rusqlite::params![host.id, ordinal as i64],
            )
            .unwrap();
        }
        let plan = BatchPreflight {
            plan: BatchPlan {
                builds: vec![BatchBuild {
                    service: "web".into(),
                    platform: "linux/amd64".into(),
                    reference: "app:v1-amd64".into(),
                }],
                hosts: hosts
                    .iter()
                    .map(|host| BatchHost {
                        host_id: host.id,
                        platform: "linux/amd64".into(),
                        images: vec![("web".into(), "app:v1-amd64".into())],
                    })
                    .collect(),
            },
            config_sha256: "a".repeat(64),
            builds: vec![BuildSpec {
                service: "web".into(),
                dockerfile: "Dockerfile".into(),
                image: "app".into(),
                platform: "auto".into(),
                args: vec![],
            }],
            compose_files: vec![],
            env_files: vec![vec![], vec![]],
            targets: hosts
                .iter()
                .enumerate()
                .map(|(index, host)| BatchTargetIdentity {
                    host_id: host.id,
                    host: host.clone(),
                    platform: "linux/amd64".into(),
                    docker_endpoint: "unix:///var/run/docker.sock".into(),
                    docker_daemon_id: format!("DAEMON:{index}"),
                })
                .collect(),
        };
        let mut reordered = plan;
        reordered.targets.swap(0, 1);
        assert!(create_in(&conn, 1, &reordered).is_err());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM release_batches", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        reordered.targets.swap(0, 1);
        reordered.builds.push(BuildSpec {
            service: "api".into(),
            dockerfile: "Dockerfile.api".into(),
            image: "api".into(),
            platform: "auto".into(),
            args: vec![],
        });
        reordered.plan.builds.push(BatchBuild {
            service: "api".into(),
            platform: "linux/amd64".into(),
            reference: "api:v1-amd64".into(),
        });
        for host in &mut reordered.plan.hosts {
            host.images.push(("web".into(), "app:v1-amd64".into()));
        }
        assert!(create_in(&conn, 1, &reordered).is_err());
        reordered.builds.pop();
        reordered.plan.builds.pop();
        for host in &mut reordered.plan.hosts {
            host.images.pop();
        }
        let id = create_in(&conn, 1, &reordered).unwrap();
        assert!(create_in(&conn, 1, &reordered).is_err());
        terminate_unstarted_in(&conn, &id).unwrap();
        assert!(terminate_unstarted_in(&conn, &id).is_err());
        let building = create_in(&conn, 1, &reordered).unwrap();
        begin_build_in(&conn, &building).unwrap();
        conn.execute(
            "UPDATE release_batch_targets SET state = 'running'
             WHERE batch_id = ?1 AND ordinal = 0",
            [&building],
        )
        .unwrap();
        assert!(terminate_unstarted_in(&conn, &building).is_err());
        conn.execute(
            "UPDATE release_batch_targets SET state = 'waiting'
             WHERE batch_id = ?1 AND ordinal = 0",
            [&building],
        )
        .unwrap();
        terminate_unstarted_in(&conn, &building).unwrap();
        reordered.plan.builds.push(BatchBuild {
            service: "web".into(),
            platform: "linux/arm64".into(),
            reference: "app:v1-arm64".into(),
        });
        reordered.plan.hosts[1].platform = "linux/arm64".into();
        reordered.plan.hosts[1].images[0].1 = "app:v1-arm64".into();
        reordered.targets[1].platform = "linux/arm64".into();
        let mixed = create_in(&conn, 1, &reordered).unwrap();
        assert_ne!(mixed, id);
        begin_build_in(&conn, &mixed).unwrap();
        assert!(ready_in(&conn, &mixed, |_, _, _, _, _, _| Ok(())).is_err());
        let ids = vec![format!("sha256:{}", "d".repeat(64))];
        record_archive_in(
            &conn,
            &mixed,
            "web",
            "linux/amd64",
            123,
            &"b".repeat(64),
            &ids,
        )
        .unwrap();
        assert!(ready_in(&conn, &mixed, |_, _, _, _, _, _| Ok(())).is_err());
        record_archive_in(
            &conn,
            &mixed,
            "web",
            "linux/arm64",
            124,
            &"c".repeat(64),
            &ids,
        )
        .unwrap();
        assert!(
            ready_in(&conn, &mixed, |_, _, _, _, _, _| Err(
                "missing archive".into()
            ))
            .is_err()
        );
        ready_in(&conn, &mixed, |_, _, _, _, _, ids| {
            assert_eq!(ids.len(), 1);
            Ok(())
        })
        .unwrap();
        assert!(ready_in(&conn, &mixed, |_, _, _, _, _, _| Ok(())).is_err());
        drop(conn);
        let reopened = Connection::open(&path).unwrap();
        let batch = get_in(&reopened, &mixed).unwrap();
        assert_eq!(recent_in(&reopened, Some(1)).unwrap().len(), 3);
        assert!(recent_in(&reopened, Some(99)).unwrap().is_empty());
        assert_eq!(batch.state, "ready");
        assert!(terminate_resolved_in(&reopened, &mixed).is_err());
        assert_eq!(
            batch
                .targets
                .iter()
                .map(|target| (target.host.id, target.state.as_str()))
                .collect::<Vec<_>>(),
            [(11, "waiting"), (12, "waiting")]
        );
        assert_eq!(batch.config_sha256, "a".repeat(64));
        assert_eq!(batch.images.len(), 2);
        assert_eq!(batch.images[0].image_ids.as_ref(), Some(&ids));
        assert_eq!(batch.targets[1].platform, "linux/arm64");
        assert_eq!(batch.targets[1].docker_daemon_id, "DAEMON:1");
        let expected_env = format!("{:x}", Sha256::digest([]));
        assert_eq!(
            batch.targets[1].env_sha256.as_deref(),
            Some(expected_env.as_str())
        );
        reopened
            .execute_batch(
                "INSERT INTO release_tasks
            (id, project_id, host_id, project_name, host_name, host_address, host_port,
             host_username, host_auth, host_fingerprint, docker_daemon_id, image_reference,
             platform, config_sha256, phase, outcome)
            VALUES ('failed-host', 1, 11, 'app', 'one', 'one.invalid', 22,
             'root', 'password', 'SHA256:verified', 'DAEMON:0', 'app:v1-amd64',
             'linux/amd64', 'digest', 'checked', 'unknown')",
            )
            .unwrap();
        reopened
            .execute(
                "UPDATE release_batches SET state = 'paused' WHERE id = ?1",
                [&mixed],
            )
            .unwrap();
        reopened
            .execute(
                "UPDATE release_batch_targets SET state = 'failed', release_task_id = 'failed-host'
            WHERE batch_id = ?1 AND ordinal = 0",
                [&mixed],
            )
            .unwrap();
        assert!(skip_failed_in(&reopened, &mixed, 11).is_err());
        assert!(terminate_resolved_in(&reopened, &mixed).is_err());
        reopened
            .execute(
                "UPDATE release_tasks SET outcome = 'failed' WHERE id = 'failed-host'",
                [],
            )
            .unwrap();
        reopened
            .execute(
                "UPDATE release_batch_targets SET state = 'unknown'
            WHERE batch_id = ?1 AND ordinal = 1",
                [&mixed],
            )
            .unwrap();
        assert!(skip_failed_in(&reopened, &mixed, 11).is_err());
        assert!(terminate_resolved_in(&reopened, &mixed).is_err());
        reopened
            .execute(
                "UPDATE release_batch_targets SET state = 'waiting'
            WHERE batch_id = ?1 AND ordinal = 1",
                [&mixed],
            )
            .unwrap();
        reopened
            .execute(
                "UPDATE release_batch_targets SET state = 'failed'
            WHERE batch_id = ?1 AND ordinal = 1",
                [&mixed],
            )
            .unwrap();
        assert!(skip_failed_in(&reopened, &mixed, 11).is_err());
        assert!(terminate_resolved_in(&reopened, &mixed).is_err());
        reopened
            .execute(
                "UPDATE release_batch_targets SET state = 'waiting'
            WHERE batch_id = ?1 AND ordinal = 1",
                [&mixed],
            )
            .unwrap();
        reopened
            .execute(
                "INSERT INTO release_materials (task_id, state, chunks)
            VALUES ('failed-host', 'writing', 0)",
                [],
            )
            .unwrap();
        assert!(skip_failed_in(&reopened, &mixed, 11).is_err());
        assert!(terminate_resolved_in(&reopened, &mixed).is_err());
        reopened
            .execute(
                "DELETE FROM release_materials WHERE task_id = 'failed-host'",
                [],
            )
            .unwrap();
        skip_failed_in(&reopened, &mixed, 11).unwrap();
        let skipped = get_in(&reopened, &mixed).unwrap();
        assert_eq!(skipped.state, "running");
        assert_eq!(skipped.targets[0].state, "skipped");
        assert_eq!(skipped.targets[0].task_outcome.as_deref(), Some("failed"));
        assert_eq!(skipped.targets[1].state, "waiting");
        terminate_resolved_in(&reopened, &mixed).unwrap();
        assert_eq!(get_in(&reopened, &mixed).unwrap().state, "terminated");
        drop(reopened);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
