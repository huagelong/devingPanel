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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Failed,
    Unknown,
    Blocked,
}

impl Outcome {
    fn saved(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Blocked => "blocked",
        }
    }
}

pub struct Intent {
    pub id: String,
    pub project_id: i64,
    pub project_name: String,
    pub host: Host,
    pub image_reference: String,
    pub platform: String,
    pub config_sha256: String,
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
         image_reference, platform, config_sha256, phase, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'prepared', 'running')",
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
                intent.image_reference,
                intent.platform,
                intent.config_sha256
            ],
        )
        .map_err(|_| "该项目已有未核实的发布任务，不能重复开始。".to_owned())?;
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

fn settle_in(
    conn: &Connection,
    id: &str,
    phase: Phase,
    outcome: Outcome,
    detail: &str,
) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::{Intent, Outcome, Phase, advance_in, create_in, settle_in};
    use crate::database::initialize;
    use crate::host_store::Host;
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
            image_reference: "test:v1-amd64".into(),
            platform: "linux/amd64".into(),
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
}
