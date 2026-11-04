use crate::database::connect;
use crate::{project_store, release_archive, release_materials, release_store, ssh};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub keep_count: u8,
    pub auto_enabled: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            keep_count: 3,
            auto_enabled: false,
        }
    }
}

fn load_in(conn: &Connection) -> Result<Policy, String> {
    conn.query_row(
        "SELECT keep_count, auto_enabled FROM retention_policy WHERE id = 1",
        [],
        |row| {
            Ok(Policy {
                keep_count: row.get(0)?,
                auto_enabled: row.get(1)?,
            })
        },
    )
    .map_err(|err| format!("无法读取发布保留策略：{err}"))
}

pub fn load() -> Result<Policy, String> {
    load_in(&connect()?)
}

pub fn check_schedule() -> Result<Option<(String, String)>, String> {
    let checked: Option<String> = connect()?
        .query_row(
            "SELECT last_checked_at FROM retention_policy WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|err| format!("无法读取自动检查时间：{err}"))?;
    checked
        .map(|value| {
            let utc = chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S")
                .map_err(|_| "自动检查时间记录无效。".to_string())?
                .and_utc();
            let local = utc.with_timezone(&chrono::Local);
            Ok((
                local.format("%Y-%m-%d %H:%M").to_string(),
                (local + chrono::Duration::days(1))
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
            ))
        })
        .transpose()
}

fn save_in(conn: &Connection, policy: Policy) -> Result<(), String> {
    if !(1..=100).contains(&policy.keep_count) {
        return Err("已完成版本保留数必须为 1～100。".into());
    }
    conn.execute(
        "UPDATE retention_policy SET keep_count = ?1, auto_enabled = ?2,
         last_checked_at = CASE WHEN auto_enabled = 0 AND ?2 = 1 THEN NULL ELSE last_checked_at END
         WHERE id = 1",
        params![policy.keep_count, policy.auto_enabled],
    )
    .map_err(|err| format!("无法保存发布保留策略：{err}"))?;
    Ok(())
}

pub fn save(policy: Policy) -> Result<(), String> {
    save_in(&connect()?, policy)
}

fn claim_daily_check_in(conn: &Connection) -> Result<bool, String> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let changed = tx
        .execute(
            "UPDATE retention_policy SET last_checked_at = CURRENT_TIMESTAMP
             WHERE id = 1 AND auto_enabled = 1 AND
             (last_checked_at IS NULL OR last_checked_at <= datetime('now', '-1 day'))",
            [],
        )
        .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())?;
    Ok(changed == 1)
}

pub fn claim_daily_check() -> Result<bool, String> {
    claim_daily_check_in(&connect()?)
}

pub fn due_daily_check() -> Result<bool, String> {
    connect()?
        .query_row(
            "SELECT auto_enabled = 1 AND
             (last_checked_at IS NULL OR last_checked_at <= datetime('now', '-1 day'))
             FROM retention_policy WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())
}

#[derive(Clone)]
pub struct Candidate {
    pub task_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub host_name: String,
    pub image_count: usize,
    pub local_bytes: u64,
    pub needs_review: bool,
}

pub struct Preview {
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<String>,
    pub keep_count: u8,
    pub protected: usize,
}

fn cleanup_state(conn: &Connection, task_id: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT state FROM retention_cleanup WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(|err| err.to_string())
}

pub fn preview() -> Result<Preview, String> {
    let policy = load()?;
    let conn = connect()?;
    let mut preview = Preview {
        candidates: Vec::new(),
        skipped: Vec::new(),
        keep_count: policy.keep_count,
        protected: 0,
    };
    for project in project_store::list()? {
        let plan = release_store::retention_plan(project.id)?;
        if plan.unresolved {
            preview
                .skipped
                .push(format!("{}：存在未结束或待核实的任务", project.name));
            continue;
        }
        for entry in plan.entries {
            if entry.class != release_store::RetentionClass::HistoricalReview {
                preview.protected += 1;
                continue;
            }
            if preview.candidates.len() >= 100 {
                return Err("清理候选超过 100 个，请先处理部分历史版本。".into());
            }
            if cleanup_state(&conn, &entry.task_id)?.as_deref() == Some("complete") {
                continue;
            }
            let task = release_store::get_task(&entry.task_id)?;
            let images = match release_store::historical_images(&entry.task_id) {
                Ok(images) if !images.is_empty() => images,
                _ => {
                    preview
                        .skipped
                        .push(format!("{}：镜像记录不完整", &entry.task_id[..8]));
                    continue;
                }
            };
            let local_bytes = release_store::planned_archive_services(&entry.task_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(service, recorded)| {
                    recorded
                        .then(|| release_store::archive_record(&entry.task_id, &service).ok())
                        .flatten()
                        .map(|record| record.0)
                })
                .sum();
            let state = cleanup_state(&conn, &entry.task_id)?;
            preview.candidates.push(Candidate {
                task_id: entry.task_id,
                project_id: project.id,
                project_name: project.name.clone(),
                host_name: task.host.name,
                image_count: images.len(),
                local_bytes,
                needs_review: matches!(state.as_deref(), Some("running" | "review")),
            });
        }
    }
    Ok(preview)
}

fn shared_references(task: &release_store::TaskSnapshot) -> Result<HashSet<String>, String> {
    let conn = connect()?;
    let daemon = task
        .docker_daemon_id
        .as_deref()
        .ok_or("任务缺少 Docker daemon 身份。")?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT own.reference FROM release_task_images own
             WHERE own.task_id = ?1 AND EXISTS (
               SELECT 1 FROM release_task_images other
               JOIN release_tasks t ON t.id = other.task_id
               WHERE other.task_id <> ?1 AND other.reference = own.reference
               AND t.docker_daemon_id = ?2)",
        )
        .map_err(|err| err.to_string())?;
    stmt.query_map(params![task.id, daemon], |row| row.get(0))
        .map_err(|err| err.to_string())?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|err| err.to_string())
}

fn set_cleanup_state(task_id: &str, state: &str, detail: &str) -> Result<(), String> {
    connect()?
        .execute(
            "INSERT INTO retention_cleanup (task_id, state, detail) VALUES (?1, ?2, ?3)
             ON CONFLICT(task_id) DO UPDATE SET state = excluded.state,
             detail = excluded.detail, updated_at = CURRENT_TIMESTAMP",
            params![task_id, state, detail],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub struct Summary {
    pub completed: usize,
    pub skipped: usize,
    pub skipped_shared_tags: usize,
    pub failed: Vec<String>,
    pub retryable_failures: usize,
    pub removed_tags: usize,
    pub reclaimed_local_bytes: u64,
}

pub fn execute(task_ids: &[String], automatic: bool) -> Result<Summary, String> {
    let current = preview()?;
    let selected: HashSet<&str> = task_ids.iter().map(String::as_str).collect();
    let mut summary = Summary {
        completed: 0,
        skipped: 0,
        skipped_shared_tags: 0,
        failed: Vec::new(),
        retryable_failures: 0,
        removed_tags: 0,
        reclaimed_local_bytes: 0,
    };
    for candidate in current.candidates {
        if !selected.contains(candidate.task_id.as_str()) {
            continue;
        }
        if automatic && candidate.needs_review {
            summary.skipped += 1;
            continue;
        }
        let mut needs_review = false;
        let mut state_claimed = false;
        let result = (|| {
            let _guard = release_materials::acquire_guard(candidate.project_id)?;
            let fresh = release_store::retention_plan(candidate.project_id)?;
            if fresh.unresolved
                || !fresh.entries.iter().any(|entry| {
                    entry.task_id == candidate.task_id
                        && entry.class == release_store::RetentionClass::HistoricalReview
                })
            {
                return Err("版本保留状态已变化，请重新预览。".into());
            }
            let task = release_store::get_task(&candidate.task_id)?;
            let images = release_store::historical_images(&candidate.task_id)?;
            let shared = shared_references(&task)?;
            set_cleanup_state(&candidate.task_id, "running", "远端清理中")?;
            state_claimed = true;
            let remote =
                ssh::cleanup_historical_release(&task, &images, &shared, &mut needs_review)?;
            needs_review = true;
            let bytes = release_archive::remove_historical(&candidate.task_id)?;
            release_materials::remove_historical(&candidate.task_id)?;
            set_cleanup_state(&candidate.task_id, "complete", "清理完成")?;
            Ok::<_, String>((remote.removed_tags, remote.skipped_shared_tags, bytes))
        })();
        match result {
            Ok((tags, shared_tags, bytes)) => {
                summary.completed += 1;
                summary.removed_tags += tags;
                summary.skipped_shared_tags += shared_tags;
                summary.reclaimed_local_bytes += bytes;
            }
            Err(error) => {
                if needs_review {
                    let _ = set_cleanup_state(&candidate.task_id, "review", &error);
                } else if state_claimed {
                    summary.retryable_failures += 1;
                    if let Ok(conn) = connect() {
                        let _ = conn.execute(
                            "DELETE FROM retention_cleanup WHERE task_id = ?1 AND state = 'running'",
                            [&candidate.task_id],
                        );
                    }
                }
                summary
                    .failed
                    .push(format!("{}：{error}", &candidate.task_id[..8]));
            }
        }
    }
    if automatic && summary.retryable_failures > 0 {
        connect()?
            .execute(
                "UPDATE retention_policy SET last_checked_at = datetime('now', '-23 hours', '-45 minutes') WHERE id = 1 AND auto_enabled = 1",
                [],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::{Policy, claim_daily_check_in, load_in, save_in};
    use crate::database::initialize;
    use rusqlite::Connection;

    #[test]
    fn policy_defaults_to_three_and_requires_explicit_auto_enable() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        assert_eq!(load_in(&conn).unwrap(), Policy::default());
        assert!(!claim_daily_check_in(&conn).unwrap());
        save_in(
            &conn,
            Policy {
                keep_count: 1,
                auto_enabled: true,
            },
        )
        .unwrap();
        assert!(claim_daily_check_in(&conn).unwrap());
        assert!(!claim_daily_check_in(&conn).unwrap());
        assert!(
            save_in(
                &conn,
                Policy {
                    keep_count: 0,
                    auto_enabled: true
                }
            )
            .is_err()
        );
    }
}
