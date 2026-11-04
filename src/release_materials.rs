use crate::database::connect;
use crate::host_store::with_credential_lock;
use crate::ssh::{self, DeploymentFile};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use keyring::{Entry, Error as KeyringError};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

const SERVICE: &str = "devingPanel:release-materials";
const CHUNK_UTF16: usize = 1000;
const MAX_CHUNKS: usize = 128;
const MAX_MATERIAL_BYTES: usize = 64 * 1024;

pub struct PreparedMaterials {
    chunks: Vec<String>,
    sha256: String,
}

pub struct RestoredMaterials {
    pub compose_files: Vec<DeploymentFile>,
    pub env_file: Vec<u8>,
}

pub struct ReleaseGuard {
    _file: File,
}

fn try_project_guard(project_id: i64) -> Result<Option<ReleaseGuard>, String> {
    if project_id <= 0 {
        return Err("项目 ID 无效。".into());
    }
    let _ = connect()?;
    let root: PathBuf = crate::preferences::data_dir().ok_or("无法确定本机配置目录。")?;
    try_project_guard_at(&root, project_id)
}

fn try_project_guard_at(root: &Path, project_id: i64) -> Result<Option<ReleaseGuard>, String> {
    if project_id <= 0 {
        return Err("项目 ID 无效。".into());
    }
    let path = root.join(format!("release-project-{project_id}.lock"));
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| "无法打开项目发布锁。")?;
    match file.try_lock() {
        Ok(()) => Ok(Some(ReleaseGuard { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(_) => Err("无法锁定项目发布任务。".into()),
    }
}

pub fn acquire_guard(project_id: i64) -> Result<ReleaseGuard, String> {
    try_project_guard(project_id)?.ok_or("该项目已有正在执行的发布操作。".into())
}

fn recover_prepared_in<G>(
    conn: &rusqlite::Connection,
    mut guard: impl FnMut(i64) -> Result<Option<G>, String>,
    mut clear: impl FnMut(&str, usize) -> Result<(), String>,
) -> Result<usize, String> {
    let mut stmt = conn
        .prepare(
            "SELECT t.id, t.project_id, m.state, m.chunks
         FROM release_tasks AS t JOIN release_materials AS m ON m.task_id = t.id
         WHERE t.phase = 'prepared' AND t.outcome IN ('running', 'unknown', 'failed')",
        )
        .map_err(|err| err.to_string())?;
    let pending = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let mut recovered = 0;
    let mut failed = false;
    for (id, project_id, state, count) in pending {
        let Some(_guard) = (match guard(project_id) {
            Ok(value) => value,
            Err(_) => {
                failed = true;
                continue;
            }
        }) else {
            continue;
        };
        let count = if state == "ready" && (1..=MAX_CHUNKS as i64).contains(&count) {
            count as usize
        } else {
            MAX_CHUNKS
        };
        if clear(&id, count).is_err() {
            failed = true;
            continue;
        }
        let result = (|| {
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )
            .map_err(|err| err.to_string())?;
            tx.execute(
            "UPDATE release_tasks SET outcome = 'failed', detail = '构建前任务中断，已清理未完成的本机版本材料', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND phase = 'prepared' AND outcome IN ('running', 'unknown')",
            [&id],
            ).map_err(|err| err.to_string())?;
            let removed = tx.execute(
            "DELETE FROM release_materials WHERE task_id = ?1 AND EXISTS (
                SELECT 1 FROM release_tasks WHERE id = ?1 AND phase = 'prepared' AND outcome = 'failed')",
            [&id],
            ).map_err(|err| err.to_string())?;
            tx.commit().map_err(|err| err.to_string())?;
            Ok::<_, String>(removed == 1)
        })();
        match result {
            Ok(true) => recovered += 1,
            _ => failed = true,
        }
    }
    if failed {
        Err("部分构建前中断材料仍待清理；可在发布历史中重试。".into())
    } else {
        Ok(recovered)
    }
}

pub fn recover_prepared() -> Result<usize, String> {
    let conn = connect()?;
    recover_prepared_in(&conn, try_project_guard, |id, count| {
        with_credential_lock(|| {
            if cleanup(id, count) {
                Ok(())
            } else {
                Err("中断版本材料凭据清理失败，请稍后重试。".into())
            }
        })
    })
}

fn recover_interrupted_batches_in<G>(
    conn: &rusqlite::Connection,
    mut guard: impl FnMut(i64) -> Result<Option<G>, String>,
) -> Result<usize, String> {
    let projects = crate::batch_store::interrupted_projects_in(conn)?;
    let mut recovered = 0;
    let mut failed = false;
    for project_id in projects {
        let Some(_guard) = (match guard(project_id) {
            Ok(value) => value,
            Err(_) => {
                failed = true;
                continue;
            }
        }) else {
            continue;
        };
        match crate::batch_store::mark_interrupted_in(conn, project_id) {
            Ok(true) => recovered += 1,
            Ok(false) => {}
            Err(_) => failed = true,
        }
    }
    if failed {
        Err("部分中断批次未能标为待核实；请检查数据库并重试。".into())
    } else {
        Ok(recovered)
    }
}

pub fn recover_interrupted_batches() -> Result<usize, String> {
    recover_interrupted_batches_in(&connect()?, try_project_guard)
}

fn chunk_private_text(value: &str) -> Result<Vec<String>, String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for ch in value.chars() {
        if units + ch.len_utf16() > CHUNK_UTF16 {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += ch.len_utf16();
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() || chunks.len() > MAX_CHUNKS {
        return Err("本次 Compose/.env 版本材料超过本机凭据管理器保留上限，未开始发布。".into());
    }
    Ok(chunks)
}

pub fn prepare(files: &[DeploymentFile], env_file: &[u8]) -> Result<PreparedMaterials, String> {
    if files.is_empty() || files.len() > 16 || env_file.len() > 1024 * 1024 {
        return Err("待保留的部署材料无效。".into());
    }
    let mut paths = std::collections::HashSet::new();
    let mut encoded = Vec::new();
    let mut total = env_file.len();
    for file in files {
        ssh::validated_release_file(&file.path)?;
        if !paths.insert(&file.path) || file.bytes.len() > 16 * 1024 * 1024 {
            return Err("Compose 版本材料路径重复或过大。".into());
        }
        total = total.saturating_add(file.bytes.len());
        encoded.push(json!({"path": file.path, "bytes": STANDARD.encode(&file.bytes)}));
    }
    if total > MAX_MATERIAL_BYTES {
        return Err("当前可恢复发布的 Compose/.env 总量上限为 64 KiB，未开始构建。".into());
    }
    let value =
        json!({"version": 1, "compose": encoded, "env": STANDARD.encode(env_file)}).to_string();
    Ok(PreparedMaterials {
        chunks: chunk_private_text(&value)?,
        sha256: format!("{:x}", Sha256::digest(value.as_bytes())),
    })
}

fn entry(id: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, id).map_err(|_| "系统凭据管理器不可用。".into())
}

fn cleanup(id: &str, count: usize) -> bool {
    let mut complete = true;
    for key in (0..count)
        .map(|index| format!("{id}:{index}"))
        .chain(std::iter::once(id.to_owned()))
    {
        match entry(&key) {
            Ok(item) => match item.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => {}
                Err(_) => complete = false,
            },
            Err(_) => complete = false,
        }
    }
    complete
}

pub fn save(id: &str, materials: PreparedMaterials) -> Result<(), String> {
    uuid::Uuid::parse_str(id).map_err(|_| "发布任务 ID 无效。")?;
    with_credential_lock(|| {
        let conn = connect()?;
        let writing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM release_materials AS m JOIN release_tasks AS t ON t.id = m.task_id
             WHERE m.task_id = ?1 AND m.state = 'writing' AND t.phase = 'prepared' AND t.outcome = 'running'",
            [id], |row| row.get(0),
        ).map_err(|_| "无法核对版本材料写入意图。")?;
        if writing != 1 {
            return Err("任务尚未准备好保存版本材料。".into());
        }
        let mut written = 0;
        let result = (|| {
            for (index, chunk) in materials.chunks.iter().enumerate() {
                entry(&format!("{id}:{index}"))?
                    .set_password(chunk)
                    .map_err(|_| "无法写入版本材料到系统凭据管理器。")?;
                written += 1;
            }
            entry(id)?
                .set_password(&written.to_string())
                .map_err(|_| "无法写入版本材料清单。")?;
            let changed = conn
                .execute(
                    "UPDATE release_materials SET state = 'ready', chunks = ?2, material_sha256 = ?3
                 WHERE task_id = ?1 AND state = 'writing'",
                    params![id, written as i64, materials.sha256],
                )
                .map_err(|_| "无法确认版本材料已保存。")?;
            if changed != 1 {
                return Err("版本材料状态已变化。".into());
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = result {
            if cleanup(id, written + 1) {
                return Err(error);
            }
            return Err(format!("{error}；部分凭据可能残留，需人工核对任务 {id}。"));
        }
        Ok(())
    })
}

pub fn load(id: &str) -> Result<RestoredMaterials, String> {
    uuid::Uuid::parse_str(id).map_err(|_| "发布任务 ID 无效。")?;
    with_credential_lock(|| {
        let conn = connect()?;
        let (count, expected_sha256): (i64, Option<String>) = conn
            .query_row(
                "SELECT chunks, material_sha256 FROM release_materials WHERE task_id = ?1 AND state = 'ready'",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| "该任务没有完整的本机版本材料。")?;
        if !(1..=MAX_CHUNKS as i64).contains(&count) {
            return Err("版本材料清单无效。".into());
        }
        let manifest = entry(id)?
            .get_password()
            .map_err(|_| "版本材料凭据已丢失。")?;
        if manifest != count.to_string() {
            return Err("版本材料凭据清单不一致。".into());
        }
        let mut text = String::new();
        for index in 0..count {
            let chunk = entry(&format!("{id}:{index}"))?
                .get_password()
                .map_err(|_| "版本材料凭据不完整。")?;
            if chunk.encode_utf16().count() > CHUNK_UTF16 {
                return Err("版本材料块无效。".into());
            }
            text.push_str(&chunk);
        }
        let expected_sha256 =
            expected_sha256.ok_or("旧版本材料缺少原始摘要，不能证明快照完整。")?;
        if expected_sha256.len() != 64
            || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || format!("{:x}", Sha256::digest(text.as_bytes())) != expected_sha256
        {
            return Err("本机版本材料摘要校验失败。".into());
        }
        let document: Value = serde_json::from_str(&text).map_err(|_| "版本材料格式无效。")?;
        if document.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("版本材料版本不受支持。".into());
        }
        let files = document
            .get("compose")
            .and_then(Value::as_array)
            .ok_or("Compose 材料缺失。")?;
        if files.is_empty() || files.len() > 16 {
            return Err("Compose 材料数量无效。".into());
        }
        let mut paths = std::collections::HashSet::new();
        let mut compose_files = Vec::new();
        for file in files {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .ok_or("Compose 路径缺失。")?;
            ssh::validated_release_file(path)?;
            if !paths.insert(path) {
                return Err("Compose 材料路径重复。".into());
            }
            let bytes = file
                .get("bytes")
                .and_then(Value::as_str)
                .ok_or("Compose 材料缺失。")?;
            let bytes = STANDARD
                .decode(bytes)
                .map_err(|_| "Compose 材料编码无效。")?;
            compose_files.push(DeploymentFile {
                path: path.to_owned(),
                bytes,
            });
        }
        let env = document
            .get("env")
            .and_then(Value::as_str)
            .ok_or("环境材料缺失。")?;
        let env_file = STANDARD.decode(env).map_err(|_| "环境材料编码无效。")?;
        Ok(RestoredMaterials {
            compose_files,
            env_file,
        })
    })
}

pub fn remove_historical(id: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(id).map_err(|_| "发布任务 ID 无效。")?;
    with_credential_lock(|| {
        let conn = connect()?;
        let count: Option<i64> = conn
            .query_row(
                "SELECT chunks FROM release_materials WHERE task_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| err.to_string())?;
        let Some(count) = count else {
            return Ok(());
        };
        if !(0..=MAX_CHUNKS as i64).contains(&count) || !cleanup(id, count as usize) {
            return Err("历史版本凭据清理未完成，稍后可重试。".into());
        }
        conn.execute("DELETE FROM release_materials WHERE task_id = ?1", [id])
            .map_err(|err| err.to_string())?;
        Ok(())
    })
}

pub fn remove_orphaned(id: &str, count: i64) -> Result<(), String> {
    uuid::Uuid::parse_str(id).map_err(|_| "发布任务 ID 无效。")?;
    if !(0..=MAX_CHUNKS as i64).contains(&count) {
        return Err("版本材料块数无效。".into());
    }
    with_credential_lock(|| {
        if cleanup(id, count as usize) {
            Ok(())
        } else {
            Err("恢复前的版本材料清理待重试。".into())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CHUNK_UTF16, MAX_CHUNKS, MAX_MATERIAL_BYTES, prepare, recover_interrupted_batches_in,
        recover_prepared_in, try_project_guard_at,
    };
    use crate::database::initialize;
    use crate::ssh::DeploymentFile;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use rusqlite::{Connection, params};
    use serde_json::Value;
    use std::io::BufRead;

    #[test]
    #[ignore = "child process used by the file-backed project lock test"]
    fn project_lock_probe_child() {
        let root =
            std::path::PathBuf::from(std::env::var_os("DEVINGPANEL_LOCK_TEST_ROOT").unwrap());
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("devingpanel-batch-restart-")
        );
        let expected = std::env::var("DEVINGPANEL_LOCK_TEST_ACQUIRES").unwrap() == "yes";
        let guard = try_project_guard_at(&root, 91).unwrap();
        assert_eq!(guard.is_some(), expected);
    }

    #[test]
    #[ignore = "child process terminated by the interrupted batch recovery test"]
    fn project_lock_crash_child() {
        let root =
            std::path::PathBuf::from(std::env::var_os("DEVINGPANEL_LOCK_TEST_ROOT").unwrap());
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("devingpanel-batch-restart-")
        );
        let _guard = try_project_guard_at(&root, 91).unwrap().unwrap();
        println!("LOCK_HELD");
        std::thread::park();
    }

    #[test]
    fn startup_keeps_active_batch_running_until_its_project_lock_is_free() {
        let root = std::env::temp_dir().join(format!(
            "devingpanel-batch-restart-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES ('batch', 91, 'digest', 'running');
            INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
                host_address, host_port, host_username, host_auth, host_fingerprint,
                docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
                phase, outcome) VALUES ('task', 91, 11, 'test', 'one',
                'one.invalid', 22, 'root', 'password', 'SHA256:test',
                'unix:///var/run/docker.sock', 'DAEMON:ONE', 'test:v1', 'linux/amd64',
                'digest', 'loading', 'running');
            INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
                host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
                platform, docker_endpoint, docker_daemon_id, state, release_task_id)
            VALUES ('batch', 0, 11, 'one', 'test', 'one.invalid', 22, 'root',
                'password', 'SHA256:test', 'linux/amd64', 'unix:///var/run/docker.sock',
                'DAEMON:ONE', 'running', 'task'),
                ('batch', 1, 12, 'two', 'test', 'two.invalid', 22, 'root',
                 'password', 'SHA256:test', 'linux/amd64', 'unix:///var/run/docker.sock',
                 'DAEMON:TWO', 'waiting', NULL);",
        )
        .unwrap();
        let active = try_project_guard_at(&root, 91).unwrap().unwrap();
        let probe = |acquires: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "release_materials::tests::project_lock_probe_child",
                ])
                .env("DEVINGPANEL_LOCK_TEST_ROOT", &root)
                .env(
                    "DEVINGPANEL_LOCK_TEST_ACQUIRES",
                    if acquires { "yes" } else { "no" },
                )
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            assert!(
                command.status().unwrap().success(),
                "child lock probe failed"
            );
        };
        probe(false);
        assert_eq!(
            recover_interrupted_batches_in(&conn, |id| try_project_guard_at(&root, id)).unwrap(),
            0
        );
        let state: String = conn
            .query_row(
                "SELECT state FROM release_batches WHERE id='batch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "running");
        drop(active);
        probe(true);
        drop(conn);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "release_materials::tests::project_lock_crash_child",
                "--nocapture",
            ])
            .env("DEVINGPANEL_LOCK_TEST_ROOT", &root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "child did not acquire lock"
            );
            if line.contains("LOCK_HELD") {
                break;
            }
            line.clear();
        }
        probe(false);
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        probe(true);
        let reopened = Connection::open(&path).unwrap();
        reopened.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        assert_eq!(
            recover_interrupted_batches_in(&reopened, |id| try_project_guard_at(&root, id))
                .unwrap(),
            1
        );
        assert_eq!(
            recover_interrupted_batches_in(&reopened, |id| try_project_guard_at(&root, id))
                .unwrap(),
            0
        );
        let state: String = reopened
            .query_row(
                "SELECT state FROM release_batches WHERE id='batch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "unknown");
        let outcome: String = reopened
            .query_row(
                "SELECT outcome FROM release_tasks WHERE id='task'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "unknown");
        let targets = reopened
            .prepare(
                "SELECT state FROM release_batch_targets WHERE batch_id='batch' ORDER BY ordinal",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(targets, ["unknown", "waiting"]);
        drop(reopened);
        std::fs::remove_file(root.join("release-project-91.lock")).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn project_guard_excludes_an_active_release_and_allows_crash_recovery() {
        let root = std::env::temp_dir().join(format!("devingpanel-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let first = try_project_guard_at(&root, 4).unwrap().unwrap();
        assert!(try_project_guard_at(&root, 4).unwrap().is_none());
        assert!(try_project_guard_at(&root, 5).unwrap().is_some());
        drop(first);
        assert!(try_project_guard_at(&root, 4).unwrap().is_some());
        for project in [4, 5] {
            std::fs::remove_file(root.join(format!("release-project-{project}.lock"))).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn private_snapshot_preserves_compose_and_env_without_local_plaintext_files() {
        let files = [DeploymentFile {
            path: "deploy/配置.yaml".into(),
            bytes: b"services: {}\n".to_vec(),
        }];
        let env = b"TOKEN=\"private-value\"\n";
        let prepared = prepare(&files, env).unwrap();
        assert!(prepared.chunks.len() <= MAX_CHUNKS);
        assert!(
            prepared
                .chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= CHUNK_UTF16)
        );
        let json: Value = serde_json::from_str(&prepared.chunks.concat()).unwrap();
        assert_eq!(json["compose"][0]["path"], "deploy/配置.yaml");
        assert_eq!(
            STANDARD
                .decode(json["compose"][0]["bytes"].as_str().unwrap())
                .unwrap(),
            files[0].bytes
        );
        assert_eq!(STANDARD.decode(json["env"].as_str().unwrap()).unwrap(), env);
        assert!(
            prepare(
                &[DeploymentFile {
                    path: "compose.yaml".into(),
                    bytes: vec![b'x'; MAX_MATERIAL_BYTES + 1]
                }],
                b""
            )
            .is_err()
        );
        assert!(
            prepare(
                &[DeploymentFile {
                    path: "compose.yaml".into(),
                    bytes: vec![b'x'; MAX_MATERIAL_BYTES],
                }],
                b""
            )
            .is_ok()
        );
    }

    #[test]
    fn interrupted_prepared_snapshot_remains_locked_until_cleanup_succeeds() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
            host_address, host_port, host_username, host_auth, host_fingerprint,
            image_reference, platform, config_sha256, phase, outcome)
            VALUES (?1, 3, 1, 'smoke', 'host', '127.0.0.1', 22, 'user', 'password', 'SHA256:test',
            'smoke:v1', 'linux/amd64', ?2, 'prepared', 'running')",
            params![id, "a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_materials (task_id, state, chunks) VALUES (?1, 'writing', 0)",
            [&id],
        )
        .unwrap();
        assert_eq!(
            recover_prepared_in(
                &conn,
                |_| Ok::<_, String>(None::<()>),
                |_, _| panic!("active writer")
            )
            .unwrap(),
            0
        );
        assert!(
            recover_prepared_in(
                &conn,
                |_| Ok::<_, String>(Some(())),
                |_, count| {
                    assert_eq!(count, MAX_CHUNKS);
                    Err("simulated cleanup failure".into())
                }
            )
            .is_err()
        );
        let state: (String, String) = conn.query_row(
            "SELECT t.outcome, m.state FROM release_tasks AS t JOIN release_materials AS m ON m.task_id = t.id WHERE t.id = ?1",
            [&id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(state, ("running".into(), "writing".into()));
        let other = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
             host_address, host_port, host_username, host_auth, host_fingerprint,
             image_reference, platform, config_sha256, phase, outcome)
             VALUES (?1, 4, 1, 'other', 'host', '127.0.0.1', 22, 'user', 'password', 'SHA256:test',
             'other:v1', 'linux/amd64', ?2, 'prepared', 'running')",
            params![other, "b".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_materials (task_id, state, chunks) VALUES (?1, 'ready', 2)",
            [&other],
        )
        .unwrap();
        assert!(
            recover_prepared_in(
                &conn,
                |_| Ok::<_, String>(Some(())),
                |task_id, count| {
                    if task_id == id {
                        Err("transient vault failure".into())
                    } else {
                        assert_eq!(count, 2);
                        Ok(())
                    }
                }
            )
            .is_err()
        );
        let other_state: String = conn
            .query_row(
                "SELECT outcome FROM release_tasks WHERE id = ?1",
                [&other],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(other_state, "failed");
        let other_materials: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_materials WHERE task_id = ?1",
                [&other],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(other_materials, 0);
        assert_eq!(
            recover_prepared_in(
                &conn,
                |_| Ok::<_, String>(Some(())),
                |_, count| {
                    assert_eq!(count, MAX_CHUNKS);
                    Ok(())
                }
            )
            .unwrap(),
            1
        );
        let outcome: String = conn
            .query_row(
                "SELECT outcome FROM release_tasks WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "failed");
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_materials WHERE task_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes isolated Windows Credential Manager entries"]
    fn windows_vault_round_trip_and_prepared_recovery() {
        let root = std::env::temp_dir().join(format!(
            "devingpanel-vault-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root).unwrap();
        let previous = std::env::var_os("APPDATA");
        unsafe {
            std::env::set_var("APPDATA", &root);
        }
        let result = (|| -> Result<(), String> {
            let conn = crate::database::connect()?;
            conn.execute("INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'vault-test', 'path', 'Dockerfile', 'app')", [])
                .map_err(|err| err.to_string())?;
            conn.execute("INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id, trusted_fingerprint)
                VALUES (1, 'host', '127.0.0.1', 22, 'user', 'password', 'test', 'unused-ref', 'SHA256:test')", [])
                .map_err(|err| err.to_string())?;
            let id = uuid::Uuid::new_v4().to_string();
            let host = crate::host_store::Host {
                id: 1,
                name: "host".into(),
                address: "127.0.0.1".into(),
                port: 22,
                username: "user".into(),
                group: "test".into(),
                auth: "password".into(),
                trusted_fingerprint: Some("SHA256:test".into()),
            };
            let guard = super::acquire_guard(1)?;
            crate::release_store::create(&crate::release_store::Intent {
                id: id.clone(),
                project_id: 1,
                project_name: "vault-test".into(),
                host,
                docker_endpoint: "unix:///var/run/docker.sock".into(),
                docker_daemon_id: "TEST:ID".into(),
                image_reference: "app:v1-amd64".into(),
                platform: "linux/amd64".into(),
                images: vec![("web".into(), "app:v1-amd64".into(), "linux/amd64".into())],
                config_sha256: "a".repeat(64),
            })?;
            let files = [DeploymentFile {
                path: "compose.yaml".into(),
                bytes: b"services: {}\n".to_vec(),
            }];
            let env = b"MODE=\"isolated-vault-test\"\n";
            super::save(&id, prepare(&files, env)?)?;
            let restored = super::load(&id)?;
            if restored.compose_files[0].bytes != files[0].bytes || restored.env_file != env {
                return Err("系统凭据管理器材料往返不一致。".into());
            }
            super::entry(&format!("{id}:0"))?
                .set_password("{\"version\":1,\"compose\":[{\"path\":\"compose.yaml\",\"bytes\":\"c2VydmljZXM6IHt9Cg==\"}],\"env\":\"\"}")
                .map_err(|_| "无法执行隔离凭据篡改测试。")?;
            if super::load(&id).is_ok() {
                return Err("被替换的凭据快照未被摘要校验阻断。".into());
            }
            drop(guard);
            if super::recover_prepared()? != 1 || super::load(&id).is_ok() {
                return Err("构建前中断材料未完整清理。".into());
            }
            drop(conn);
            let data = root.join("devingPanel");
            for name in [
                "state.sqlite3",
                "release-project-1.lock",
                "credential-lifecycle.lock",
            ] {
                let path = data.join(name);
                if path.exists() {
                    std::fs::remove_file(path).map_err(|_| "无法清理隔离测试文件。")?;
                }
            }
            std::fs::remove_dir(data).map_err(|_| "无法清理隔离测试数据目录。")?;
            Ok(())
        })();
        unsafe {
            match previous {
                Some(path) => std::env::set_var("APPDATA", path),
                None => std::env::remove_var("APPDATA"),
            }
        }
        if result.is_ok() {
            std::fs::remove_dir(root).unwrap();
        }
        result.unwrap();
    }
}
