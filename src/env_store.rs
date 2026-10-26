use crate::database::connect;
use crate::host_store::with_credential_lock;
use keyring::{Entry, Error as KeyringError};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

const CREDENTIAL_SERVICE: &str = "devingPanel:env";

pub(crate) fn queue_for_project(conn: &Connection, project_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO pending_env_credentials (credential_id)
         SELECT credential_id FROM project_env WHERE project_id = ?1 AND credential_id IS NOT NULL",
        [project_id],
    )?;
    Ok(())
}

pub(crate) fn queue_for_host(conn: &Connection, host_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO pending_env_credentials (credential_id)
         SELECT credential_id FROM project_env WHERE host_id = ?1 AND credential_id IS NOT NULL",
        [host_id],
    )?;
    Ok(())
}

#[derive(Clone)]
pub struct EnvRow {
    pub name: String,
    pub value: Option<String>,
    pub credential_id: Option<String>,
}

pub struct EnvInput {
    pub name: String,
    pub value: Option<String>,
    pub credential_id: Option<String>,
    pub sensitive: bool,
}

fn delete_secret(id: &str) -> Result<(), String> {
    let entry = Entry::new(CREDENTIAL_SERVICE, id).map_err(|_| "系统凭据管理器不可用。")?;
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(_) => Err("环境变量凭据清理失败。".into()),
    }
}

pub(crate) fn delete_pending_secret(id: &str) -> Result<(), String> {
    delete_secret(id)
}

fn write_secret(id: &str, value: &str) -> Result<(), String> {
    Entry::new(CREDENTIAL_SERVICE, id)
        .map_err(|_| "系统凭据管理器不可用，不能保存敏感变量。")?
        .set_password(value)
        .map_err(|_| "写入系统凭据管理器失败，未保存环境变量。".into())
}

fn read_secret(id: &str) -> Result<String, String> {
    let entry = Entry::new(CREDENTIAL_SERVICE, id)
        .map_err(|_| "系统凭据管理器不可用，无法读取环境变量。")?;
    match entry.get_password() {
        Ok(value) => Ok(value),
        Err(KeyringError::NoEntry) => Err("敏感环境变量凭据已丢失，请重新填写。".into()),
        Err(_) => Err("系统凭据管理器无法读取环境变量。".into()),
    }
}

fn render_env_file(values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut content = String::new();
    for (name, value) in values {
        if !valid_name(name) {
            return Err("环境变量名无效或使用了保留键。".into());
        }
        content.push_str(name);
        content.push_str("=\"");
        for ch in value.chars() {
            match ch {
                '\\' => content.push_str("\\\\"),
                '"' => content.push_str("\\\""),
                '$' => content.push_str("$$"),
                '\n' => content.push_str("\\n"),
                '\r' => content.push_str("\\r"),
                '\t' => content.push_str("\\t"),
                ch if ch.is_control() => {
                    return Err("环境变量包含无法写入 .env 的控制字符。".into());
                }
                ch => content.push(ch),
            }
        }
        content.push_str("\"\n");
    }
    Ok(content)
}

fn render_for_host_in(
    conn: &Connection,
    project_id: i64,
    host_id: i64,
    read: impl Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    if !scope_bound(conn, project_id, host_id)? {
        return Err("目标主机绑定已变化，请重新选择。".into());
    }
    let mut values = BTreeMap::new();
    for scope in [None, Some(host_id)] {
        for row in list_in(conn, project_id, scope)? {
            values.insert(row.name, (row.value, row.credential_id));
        }
    }
    let mut resolved = BTreeMap::new();
    for (name, (value, reference)) in values {
        let value = match (value, reference) {
            (Some(value), None) => value,
            (None, Some(reference)) => read(&reference)?,
            _ => return Err("环境变量配置不完整。".into()),
        };
        resolved.insert(name, value);
    }
    render_env_file(&resolved)
}

pub fn check_deployment_environment(project_id: i64, host_ids: &[i64]) -> Result<(), String> {
    let conn = connect()?;
    with_credential_lock(|| {
        for host_id in host_ids {
            let env_file = render_for_host_in(&conn, project_id, *host_id, read_secret)?;
            drop(env_file);
        }
        Ok(())
    })
}

pub fn deployment_env_file(project_id: i64, host_id: i64) -> Result<Vec<u8>, String> {
    let conn = connect()?;
    with_credential_lock(|| {
        render_for_host_in(&conn, project_id, host_id, read_secret).map(String::into_bytes)
    })
}

pub(crate) fn cleanup_pending_in(
    conn: &Connection,
    delete: impl Fn(&str) -> Result<(), String>,
) -> Option<String> {
    let pending = (|| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT credential_id FROM pending_env_credentials")?;
        stmt.query_map([], |row| row.get(0))?.collect()
    })();
    let Ok(pending) = pending else {
        return Some("无法检查待清理的环境变量凭据。".into());
    };
    let mut failed = 0;
    for id in pending {
        let linked = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM project_env WHERE credential_id = ?1)",
            [&id],
            |row| row.get::<_, bool>(0),
        );
        if linked.is_err()
            || (!linked.unwrap_or(false) && delete(&id).is_err())
            || conn
                .execute(
                    "DELETE FROM pending_env_credentials WHERE credential_id = ?1",
                    [&id],
                )
                .is_err()
        {
            failed += 1;
        }
    }
    (failed > 0).then(|| format!("仍有 {failed} 个环境变量凭据待清理；下次启动将重试。"))
}

pub fn cleanup_pending() -> Result<Option<String>, String> {
    let conn = connect()?;
    with_credential_lock(|| Ok(cleanup_pending_in(&conn, delete_secret)))
}

fn list_in(
    conn: &Connection,
    project_id: i64,
    host_id: Option<i64>,
) -> Result<Vec<EnvRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name, value, credential_id FROM project_env
         WHERE project_id = ?1 AND host_id IS ?2 ORDER BY ordinal",
        )
        .map_err(|err| err.to_string())?;
    stmt.query_map(params![project_id, host_id], |row| {
        Ok(EnvRow {
            name: row.get(0)?,
            value: row.get(1)?,
            credential_id: row.get(2)?,
        })
    })
    .map_err(|err| err.to_string())?
    .collect::<rusqlite::Result<_>>()
    .map_err(|err| err.to_string())
}

pub fn list(
    project_id: i64,
    host_id: Option<i64>,
) -> Result<(Vec<EnvRow>, Option<String>), String> {
    let conn = connect()?;
    with_credential_lock(|| {
        let warning = cleanup_pending_in(&conn, delete_secret);
        Ok((list_in(&conn, project_id, host_id)?, warning))
    })
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && name != "DEVINGPANEL_IMAGE"
}

fn scope_bound(conn: &Connection, project_id: i64, host_id: i64) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_hosts binding
         JOIN hosts host ON host.id = binding.host_id
         WHERE binding.project_id = ?1 AND binding.host_id = ?2 AND host.deleting = 0)",
        params![project_id, host_id],
        |row| row.get(0),
    )
    .map_err(|err| err.to_string())
}

fn reference_exists(
    conn: &Connection,
    project_id: i64,
    host_id: Option<i64>,
    name: &str,
    reference: &str,
) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_env
         WHERE project_id = ?1 AND host_id IS ?2 AND name = ?3 AND credential_id = ?4)",
        params![project_id, host_id, name, reference],
        |row| row.get(0),
    )
    .map_err(|err| err.to_string())
}

fn save_in(
    conn: &mut Connection,
    project_id: i64,
    host_id: Option<i64>,
    inputs: Vec<EnvInput>,
    mut write: impl FnMut(&str, &str) -> Result<(), String>,
) -> Result<Vec<EnvRow>, String> {
    let project_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if !project_exists {
        return Err("项目不存在。".into());
    }
    if let Some(host_id) = host_id
        && !scope_bound(conn, project_id, host_id)?
    {
        return Err("只能为已绑定的目标主机配置专属变量。".into());
    }
    let mut seen = HashSet::new();
    let mut retained = Vec::new();
    for input in &inputs {
        if !valid_name(&input.name) || !seen.insert(&input.name) {
            return Err("环境变量名无效、重复或使用了保留键。".into());
        }
        if input.sensitive && input.value.is_none() {
            let reference = input
                .credential_id
                .as_deref()
                .ok_or("敏感变量需要填写新值。")?;
            if !reference_exists(conn, project_id, host_id, &input.name, reference)? {
                return Err("敏感变量引用已失效，请重新输入。".into());
            }
            retained.push((input.name.clone(), reference.to_owned()));
        } else if !input.sensitive && input.value.is_none() {
            return Err("请填写非敏感变量的值。".into());
        }
    }
    let mut prepared = Vec::new();
    for input in inputs {
        if input.sensitive {
            let id = if let Some(value) = input.value {
                let id = Uuid::new_v4().to_string();
                conn.execute("INSERT INTO pending_env_credentials VALUES (?1)", [&id])
                    .map_err(|err| err.to_string())?;
                write(&id, &value)?;
                id
            } else {
                input.credential_id.expect("validated sensitive reference")
            };
            prepared.push((input.name, None, Some(id)));
        } else {
            prepared.push((input.name, input.value, None));
        }
    }
    let transaction = conn.transaction().map_err(|err| err.to_string())?;
    if let Some(host_id) = host_id
        && !scope_bound(&transaction, project_id, host_id)?
    {
        return Err("目标主机绑定已变化，请重新打开环境变量。".into());
    }
    for (name, reference) in &retained {
        if !reference_exists(&transaction, project_id, host_id, name, reference)? {
            return Err("敏感变量引用已变化，请重新打开环境变量。".into());
        }
    }
    transaction.execute(
        "INSERT OR IGNORE INTO pending_env_credentials (credential_id)
         SELECT credential_id FROM project_env WHERE project_id = ?1 AND host_id IS ?2 AND credential_id IS NOT NULL",
        params![project_id, host_id]
    ).map_err(|err| err.to_string())?;
    transaction
        .execute(
            "DELETE FROM project_env WHERE project_id = ?1 AND host_id IS ?2",
            params![project_id, host_id],
        )
        .map_err(|err| err.to_string())?;
    for (ordinal, (name, value, credential_id)) in prepared.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO project_env (project_id, host_id, ordinal, name, value, credential_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    project_id,
                    host_id,
                    ordinal as i64,
                    name,
                    value,
                    credential_id
                ],
            )
            .map_err(|err| err.to_string())?;
        if let Some(id) = credential_id {
            transaction
                .execute(
                    "DELETE FROM pending_env_credentials WHERE credential_id = ?1",
                    [id],
                )
                .map_err(|err| err.to_string())?;
        }
    }
    transaction.commit().map_err(|err| err.to_string())?;
    list_in(conn, project_id, host_id)
}

pub fn save(
    project_id: i64,
    host_id: Option<i64>,
    inputs: Vec<EnvInput>,
) -> Result<(Vec<EnvRow>, Option<String>), String> {
    let mut conn = connect()?;
    with_credential_lock(|| {
        let result = save_in(&mut conn, project_id, host_id, inputs, write_secret);
        let warning = cleanup_pending_in(&conn, delete_secret);
        result.map(|rows| (rows, warning))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        EnvInput, cleanup_pending_in, list_in, queue_for_host, render_env_file, render_for_host_in,
        save_in,
    };
    use crate::database::initialize;
    use rusqlite::Connection;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};

    #[test]
    fn host_override_is_resolved_before_reading_secrets() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'app', '.', 'Dockerfile', 'app');
             INSERT INTO hosts (id, name, address, port, username, auth, credential_id)
             VALUES (2, 'host', 'host.example', 22, 'root', 'password', 'host-ref');
             INSERT INTO project_hosts VALUES (1, 2, 0);
             INSERT INTO project_env (project_id, ordinal, name, credential_id) VALUES (1, 0, 'TOKEN', 'common-ref');
             INSERT INTO project_env (project_id, ordinal, name, value) VALUES (1, 1, 'PORT', '8080');
             INSERT INTO project_env (project_id, host_id, ordinal, name, credential_id)
             VALUES (1, 2, 0, 'TOKEN', 'host-ref');"
        ).unwrap();
        let content = render_for_host_in(&conn, 1, 2, |id| {
            assert_eq!(id, "host-ref");
            Ok("host-secret".into())
        })
        .unwrap();
        assert!(content.contains("TOKEN=\"host-secret\""));
        assert!(content.contains("PORT=\"8080\""));
        assert!(!content.contains("common-ref"));
        assert!(render_for_host_in(&conn, 1, 999, |_| Ok(String::new())).is_err());
    }

    #[test]
    fn dotenv_quoting_preserves_special_values() {
        let values = BTreeMap::from([
            ("A".into(), "a # $HOME ' quote \\ trail\\".into()),
            ("B".into(), "line\nnext\t\"quoted\"".into()),
        ]);
        let encoded = render_env_file(&values).unwrap();
        assert!(encoded.contains("A=\"a # $$HOME ' quote \\\\ trail\\\\\""));
        assert!(encoded.contains("B=\"line\\nnext\\t\\\"quoted\\\"\""));
        assert!(render_env_file(&BTreeMap::from([("BAD".into(), "has\0nul".into())])).is_err());
    }

    #[test]
    #[ignore = "requires local Docker Compose CLI"]
    fn generated_dotenv_is_read_by_compose() {
        let dir =
            std::env::temp_dir().join(format!("deving-panel-dotenv-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let a = "a # $HOME ' quote \\ trail\\";
        let b = "line\nnext\t\"quoted\"";
        let values = BTreeMap::from([("A".into(), a.into()), ("B".into(), b.into())]);
        std::fs::write(dir.join(".env"), render_env_file(&values).unwrap()).unwrap();
        std::fs::write(dir.join("compose.yml"),
            "services:\n  test:\n    image: busybox\n    environment:\n      A: ${A}\n      B: ${B}\n").unwrap();
        let base = || {
            let mut command = std::process::Command::new("docker");
            command
                .args(["compose", "--project-directory"])
                .arg(&dir)
                .arg("--env-file")
                .arg(dir.join(".env"))
                .arg("-f")
                .arg(dir.join("compose.yml"))
                .env_remove("A")
                .env_remove("B")
                .stderr(std::process::Stdio::null());
            command
        };
        let output = base().args(["config", "--environment"]).output().unwrap();
        assert!(output.status.success());
        let vars = String::from_utf8(output.stdout).unwrap();
        assert!(vars.lines().any(|line| line == format!("A={a}")));
        let output = base()
            .args(["config", "--format", "json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let config: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(config["services"]["test"]["environment"]["B"], b);
        std::fs::remove_file(dir.join(".env")).unwrap();
        std::fs::remove_file(dir.join("compose.yml")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    #[ignore = "requires Docker daemon and a local redis:7-alpine image"]
    fn generated_dotenv_reaches_container_unchanged() {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join(format!("deving-panel-env-container-{id}"));
        std::fs::create_dir(&dir).unwrap();
        let expected = "a # $HOME ' quote \\ trail\\";
        let values = BTreeMap::from([("A".into(), expected.into())]);
        std::fs::write(dir.join(".env"), render_env_file(&values).unwrap()).unwrap();
        std::fs::write(
            dir.join("compose.yml"),
            "services:\n  direct:\n    image: redis:7-alpine\n    env_file: .env\n",
        )
        .unwrap();
        let project = format!("envprobe{id}");
        let base = || {
            let mut command = std::process::Command::new("docker");
            command
                .args(["compose", "-p", &project, "--project-directory"])
                .arg(&dir)
                .arg("--env-file")
                .arg(dir.join(".env"))
                .arg("-f")
                .arg(dir.join("compose.yml"))
                .env_remove("A")
                .stderr(std::process::Stdio::null());
            command
        };
        let output = base()
            .args([
                "run",
                "--rm",
                "--no-deps",
                "--pull",
                "never",
                "--entrypoint",
                "env",
                "direct",
            ])
            .output()
            .unwrap();
        let _ = base().arg("down").output();
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .any(|line| line == format!("A={expected}"))
        );
        std::fs::remove_file(dir.join(".env")).unwrap();
        std::fs::remove_file(dir.join("compose.yml")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes and removes an isolated Windows Credential Manager entry"]
    fn windows_vault_round_trip() {
        let conn = crate::database::connect().unwrap();
        conn.execute("INSERT INTO projects (name, path, dockerfile, image) VALUES ('env-smoke', '.', 'Dockerfile', 'env-smoke')", []).unwrap();
        let id = conn.last_insert_rowid();
        let value = format!("smoke-{}", uuid::Uuid::new_v4());
        let (rows, warning) = super::save(
            id,
            None,
            vec![EnvInput {
                name: "TOKEN".into(),
                value: Some(value.clone()),
                credential_id: None,
                sensitive: true,
            }],
        )
        .unwrap();
        assert!(warning.is_none());
        let reference = rows[0].credential_id.clone().unwrap();
        assert_eq!(
            keyring::Entry::new(super::CREDENTIAL_SERVICE, &reference)
                .unwrap()
                .get_password()
                .unwrap(),
            value
        );
        assert_eq!(super::list(id, None).unwrap().0[0].value, None);
        assert!(crate::project_store::delete(id).unwrap().is_none());
        assert!(matches!(
            keyring::Entry::new(super::CREDENTIAL_SERVICE, &reference)
                .unwrap()
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
    }

    #[test]
    fn secrets_stay_out_of_sqlite_and_orphans_are_retryable() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'app', '.', 'Dockerfile', 'app');
             INSERT INTO hosts (id, name, address, port, username, auth, credential_id)
             VALUES (2, 'host', 'host.example', 22, 'root', 'password', 'host-credential');
             INSERT INTO project_hosts VALUES (1, 2, 0);",
        ).unwrap();
        let vault = RefCell::new(HashMap::<String, String>::new());
        let initial = save_in(
            &mut conn,
            1,
            None,
            vec![
                EnvInput {
                    name: "TOKEN".into(),
                    value: Some("secret-value".into()),
                    credential_id: None,
                    sensitive: true,
                },
                EnvInput {
                    name: "MODE".into(),
                    value: Some("prod".into()),
                    credential_id: None,
                    sensitive: false,
                },
            ],
            |id, value| {
                vault.borrow_mut().insert(id.into(), value.into());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(initial[0].value, None);
        let reference = initial[0].credential_id.clone().unwrap();
        assert_eq!(vault.borrow()[&reference], "secret-value");
        assert_eq!(
            list_in(&conn, 1, None).unwrap()[1].value.as_deref(),
            Some("prod")
        );
        assert!(save_in(&mut conn, 1, Some(3), vec![], |_, _| Ok(())).is_err());
        for invalid in ["DEVINGPANEL_IMAGE", "BAD-NAME", "1START"] {
            assert!(
                save_in(
                    &mut conn,
                    1,
                    None,
                    vec![EnvInput {
                        name: invalid.into(),
                        value: Some("value".into()),
                        credential_id: None,
                        sensitive: false,
                    }],
                    |_, _| Ok(())
                )
                .is_err()
            );
        }
        assert!(
            save_in(
                &mut conn,
                1,
                Some(2),
                vec![EnvInput {
                    name: "TOKEN".into(),
                    value: None,
                    credential_id: Some(reference.clone()),
                    sensitive: true,
                }],
                |_, _| Ok(())
            )
            .is_err()
        );
        let retained = save_in(
            &mut conn,
            1,
            None,
            vec![EnvInput {
                name: "TOKEN".into(),
                value: None,
                credential_id: Some(reference.clone()),
                sensitive: true,
            }],
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(
            retained[0].credential_id.as_deref(),
            Some(reference.as_str())
        );
        assert!(cleanup_pending_in(&conn, |_| Ok(())).is_none());
        let removed = save_in(&mut conn, 1, None, vec![], |_, _| Ok(())).unwrap();
        assert!(removed.is_empty());
        assert!(cleanup_pending_in(&conn, |_| Err("vault offline".into())).is_some());
        assert!(
            cleanup_pending_in(&conn, |id| {
                vault.borrow_mut().remove(id);
                Ok(())
            })
            .is_none()
        );
        assert!(vault.borrow().is_empty());
        let host_rows = save_in(
            &mut conn,
            1,
            Some(2),
            vec![EnvInput {
                name: "HOST_TOKEN".into(),
                value: Some("host-secret".into()),
                credential_id: None,
                sensitive: true,
            }],
            |id, value| {
                vault.borrow_mut().insert(id.into(), value.into());
                Ok(())
            },
        )
        .unwrap();
        queue_for_host(&conn, 2).unwrap();
        conn.execute("DELETE FROM hosts WHERE id = 2", []).unwrap();
        assert!(list_in(&conn, 1, Some(2)).unwrap().is_empty());
        assert!(
            cleanup_pending_in(&conn, |id| {
                vault.borrow_mut().remove(id);
                Ok(())
            })
            .is_none()
        );
        assert!(
            !vault
                .borrow()
                .contains_key(host_rows[0].credential_id.as_ref().unwrap())
        );
    }

    #[test]
    fn binding_change_during_secret_write_cannot_restore_host_scope() {
        let path = std::env::temp_dir().join(format!(
            "deving-panel-env-test-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'app', '.', 'Dockerfile', 'app');
             INSERT INTO hosts (id, name, address, port, username, auth, credential_id)
             VALUES (2, 'host', 'host.example', 22, 'root', 'password', 'host-ref');
             INSERT INTO project_hosts VALUES (1, 2, 0);"
        ).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let result = save_in(
            &mut conn,
            1,
            Some(2),
            vec![EnvInput {
                name: "TOKEN".into(),
                value: Some("secret".into()),
                credential_id: None,
                sensitive: true,
            }],
            |_, _| {
                other
                    .execute("DELETE FROM project_hosts WHERE project_id = 1", [])
                    .unwrap();
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(list_in(&conn, 1, Some(2)).unwrap().is_empty());
        assert!(cleanup_pending_in(&conn, |_| Ok(())).is_none());
        drop(other);
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }
}
