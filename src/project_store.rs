use crate::database::connect;
use crate::env_store;
use crate::host_store::with_credential_lock;
use crate::project_config;
use rusqlite::{Connection, Row, Transaction, TransactionBehavior, params};
use std::path::{Component, Path, PathBuf};

#[derive(Clone)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub dockerfile: String,
    pub image: String,
    pub tag_rule: String,
    pub platform: String,
    pub service: String,
    pub compose_files: Vec<String>,
    pub missing_compose_files: Vec<String>,
    pub build_args: Vec<(String, String)>,
    pub host_ids: Vec<i64>,
    pub manifest_mode: bool,
    pub config_error: Option<String>,
}

pub struct BuildConfig {
    pub image: String,
    pub tag_rule: String,
    pub platform: String,
    pub build_args: Vec<(String, String)>,
}

pub struct NewProject {
    pub name: String,
    pub path: String,
    pub dockerfile: String,
}

impl NewProject {
    pub(crate) fn normalize(mut self) -> Result<Self, String> {
        self.name = self.name.trim().to_owned();
        self.path = self.path.trim().to_owned();
        self.dockerfile = self.dockerfile.trim().to_owned();
        if self.name.is_empty() || self.path.is_empty() || self.dockerfile.is_empty() {
            return Err("请填写项目名、本地代码目录和 Dockerfile 路径。".into());
        }
        let directory = Path::new(&self.path);
        if !directory.is_absolute() || !directory.is_dir() {
            return Err("本地代码目录必须是已存在的绝对路径。".into());
        }
        let dockerfile = Path::new(&self.dockerfile);
        if !dockerfile
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err("Dockerfile 路径必须位于本地代码目录内。".into());
        }
        let root = directory.canonicalize().map_err(|err| err.to_string())?;
        let file = directory
            .join(dockerfile)
            .canonicalize()
            .map_err(|_| "Dockerfile 文件不存在。")?;
        if !file.starts_with(root) || !file.is_file() {
            return Err("Dockerfile 必须是项目目录内的文件。".into());
        }
        Ok(self)
    }
}

fn project_from_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        dockerfile: row.get(3)?,
        image: row.get(4)?,
        tag_rule: row.get(5)?,
        platform: row.get(6)?,
        service: row.get(7)?,
        compose_files: Vec::new(),
        missing_compose_files: Vec::new(),
        build_args: Vec::new(),
        host_ids: Vec::new(),
        manifest_mode: row.get::<_, i64>(8)? != 0,
        config_error: None,
    })
}

fn list_in(conn: &Connection) -> rusqlite::Result<Vec<Project>> {
    let mut stmt =
        conn.prepare("SELECT id, name, path, dockerfile, image, tag_rule, platform, service, manifest_mode FROM projects ORDER BY id DESC")?;
    let mut projects: Vec<Project> = stmt
        .query_map([], project_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut files = conn.prepare(
        "SELECT project_id, path FROM project_compose_files ORDER BY project_id, ordinal",
    )?;
    let rows = files.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, path) = row?;
        if let Some(project) = projects.iter_mut().find(|project| project.id == id) {
            project.compose_files.push(path);
        }
    }
    for project in &mut projects {
        let root = Path::new(&project.path).canonicalize().ok();
        for path in &project.compose_files {
            let valid = root.as_ref().is_some_and(|root| {
                root.join(path)
                    .canonicalize()
                    .is_ok_and(|file| file.starts_with(root) && file.is_file())
            });
            if !valid {
                project.missing_compose_files.push(path.clone());
            }
        }
    }
    let mut args = conn.prepare(
        "SELECT project_id, name, value FROM project_build_args ORDER BY project_id, ordinal",
    )?;
    let rows = args.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, name, value) = row?;
        if let Some(project) = projects.iter_mut().find(|project| project.id == id) {
            project.build_args.push((name, value));
        }
    }
    let mut targets =
        conn.prepare("SELECT project_id, host_id FROM project_hosts ORDER BY project_id, ordinal")?;
    let rows = targets.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    for row in rows {
        let (project_id, host_id) = row?;
        if let Some(project) = projects.iter_mut().find(|project| project.id == project_id) {
            project.host_ids.push(host_id);
        }
    }
    Ok(projects)
}

pub fn list() -> Result<Vec<Project>, String> {
    let mut projects = list_in(&connect()?).map_err(|err| err.to_string())?;
    if projects.iter().any(|project| project.manifest_mode) {
        let (hosts, _) = crate::host_store::list()?;
        for project in projects.iter_mut().filter(|project| project.manifest_mode) {
            let config = resolve_manifest_project(project, &hosts);
            match config {
                Ok(effective) => *project = effective,
                Err(error) => {
                    project.dockerfile.clear();
                    project.image.clear();
                    project.tag_rule.clear();
                    project.platform.clear();
                    project.service.clear();
                    project.compose_files.clear();
                    project.build_args.clear();
                    project.host_ids.clear();
                    project.config_error = Some(error);
                }
            }
        }
    }
    Ok(projects)
}

pub fn effective(id: i64) -> Result<Project, String> {
    let project = list()?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。")?;
    if let Some(error) = &project.config_error {
        return Err(format!("devingpanel.yaml 待修复：{error}"));
    }
    Ok(project)
}

fn resolve_manifest_project(
    project: &Project,
    hosts: &[crate::host_store::Host],
) -> Result<Project, String> {
    let manifest = project_config::load(Path::new(&project.path))?;
    let ids = project_config::resolve_targets(&manifest, hosts)?;
    if ids != project.host_ids {
        return Err("YAML 目标主机与本机绑定不同，请管理员确认目标变更。".into());
    }
    project_config::apply(project, &manifest, ids)
}

pub fn adopt_manifest(id: i64) -> Result<(Project, Option<String>), String> {
    let conn = connect()?;
    let project = list_in(&conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。")?;
    if project.manifest_mode {
        return Err("项目已使用 devingpanel.yaml。".into());
    }
    let (hosts, _) = crate::host_store::list()?;
    let path = Path::new(&project.path).join(project_config::FILE_NAME);
    if !path.exists() {
        project_config::export_legacy(&project, &hosts)?;
    }
    let manifest = project_config::load(Path::new(&project.path))?;
    let host_ids = project_config::resolve_targets(&manifest, &hosts)?;
    project_config::apply(&project, &manifest, host_ids.clone())?;
    let warning = if project.host_ids != host_ids {
        with_credential_lock(|| {
            let mut conn = connect()?;
            set_hosts_inner(
                &mut conn,
                id,
                host_ids,
                Some((&project.name, &project.path)),
            )?;
            Ok(())
        })?;
        env_store::cleanup_pending().unwrap_or_else(Some)
    } else {
        let changed = conn.execute(
            "UPDATE projects SET manifest_mode = 1 WHERE id = ?1 AND manifest_mode = 0 AND name = ?2 AND path = ?3",
            params![id, project.name, project.path],
        ).map_err(|err| err.to_string())?;
        if changed != 1 {
            return Err("项目在迁移期间发生变化，请重新核对。".into());
        }
        None
    };
    let adopted = list()?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。")?;
    Ok((adopted, warning))
}

pub fn confirm_manifest_targets(id: i64) -> Result<(Project, Option<String>), String> {
    let changed = with_credential_lock(|| {
        let mut conn = connect()?;
        let project = list_in(&conn)
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|project| project.id == id && project.manifest_mode)
            .ok_or("YAML 项目不存在。")?;
        let manifest = project_config::load(Path::new(&project.path))?;
        let hosts = crate::host_store::list_in(&conn).map_err(|err| err.to_string())?;
        let ids = project_config::resolve_targets(&manifest, &hosts)?;
        project_config::apply(&project, &manifest, ids.clone())?;
        if project.host_ids == ids {
            return Ok(false);
        }
        set_hosts_inner(&mut conn, id, ids, None)?;
        Ok(true)
    })?;
    let warning = if changed {
        env_store::cleanup_pending().unwrap_or_else(Some)
    } else {
        None
    };
    Ok((effective(id)?, warning))
}

fn create_from_manifest_in(
    conn: &Connection,
    directory: &Path,
    hosts: &[crate::host_store::Host],
) -> Result<i64, String> {
    let root = directory.canonicalize().map_err(|_| "项目目录不存在。")?;
    let manifest = project_config::load(&root)?;
    let host_ids = project_config::resolve_targets(&manifest, hosts).ok();
    let path = root.to_string_lossy().into_owned();
    let transaction = conn
        .unchecked_transaction()
        .map_err(|err| err.to_string())?;
    let changed = transaction
        .execute(
            "INSERT INTO projects (name, path, dockerfile, image, tag_rule, platform, service, manifest_mode)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, 1
             WHERE NOT EXISTS (SELECT 1 FROM projects WHERE name = ?1 OR path = ?2)",
            params![
                manifest.name,
                path,
                manifest.dockerfile,
                manifest.image,
                manifest.tag_rule,
                manifest.platform,
                manifest.service
            ],
        )
        .map_err(|err| err.to_string())?;
    if changed == 0 {
        return Err("同名项目或项目目录已存在，请打开现有项目。".into());
    }
    let id = transaction.last_insert_rowid();
    if let Some(host_ids) = host_ids {
        for (ordinal, host_id) in host_ids.into_iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO project_hosts (project_id, host_id, ordinal) VALUES (?1, ?2, ?3)",
                    params![id, host_id, ordinal as i64],
                )
                .map_err(|err| err.to_string())?;
        }
    }
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(id)
}

pub fn create_from_manifest(directory: &Path) -> Result<Project, String> {
    let (hosts, _) = crate::host_store::list()?;
    let id = create_from_manifest_in(&connect()?, directory, &hosts)?;
    list()?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。".into())
}

fn ensure_editable(conn: &Connection, id: i64) -> Result<(), String> {
    let mode: i64 = conn
        .query_row(
            "SELECT manifest_mode FROM projects WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|_| "项目不存在。")?;
    if mode != 0 {
        return Err("项目已使用 devingpanel.yaml；请编辑文件后重新校验。".into());
    }
    Ok(())
}

pub fn missing_compose_files(id: i64) -> Result<(Vec<String>, Vec<String>), String> {
    let project = list()?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。")?;
    Ok((project.compose_files, project.missing_compose_files))
}

fn create_in(conn: &Connection, input: NewProject) -> Result<Project, String> {
    let input = input.normalize()?;
    let candidate = input.name.to_ascii_lowercase();
    let image = if valid_image_name(&candidate) {
        candidate
    } else {
        String::new()
    };
    let changed = conn
        .execute(
            "INSERT INTO projects (name, path, dockerfile, image)
         SELECT ?1, ?2, ?3, ?4 WHERE NOT EXISTS (SELECT 1 FROM projects WHERE name = ?1)",
            params![input.name, input.path, input.dockerfile, image],
        )
        .map_err(|err| err.to_string())?;
    if changed == 0 {
        return Err("项目名已存在。".into());
    }
    Ok(Project {
        id: conn.last_insert_rowid(),
        name: input.name,
        path: input.path,
        dockerfile: input.dockerfile,
        image,
        tag_rule: "{timestamp}".into(),
        platform: "auto".into(),
        service: String::new(),
        compose_files: Vec::new(),
        missing_compose_files: Vec::new(),
        build_args: Vec::new(),
        host_ids: Vec::new(),
        manifest_mode: false,
        config_error: None,
    })
}

fn set_hosts_in(conn: &mut Connection, id: i64, host_ids: Vec<i64>) -> Result<Project, String> {
    ensure_editable(conn, id)?;
    set_hosts_inner(conn, id, host_ids, None)
}

fn set_hosts_inner(
    conn: &mut Connection,
    id: i64,
    host_ids: Vec<i64>,
    adopt: Option<(&str, &str)>,
) -> Result<Project, String> {
    let transaction = conn.transaction().map_err(|err| err.to_string())?;
    if !transaction
        .query_row("SELECT 1 FROM projects WHERE id = ?1", [id], |_| Ok(()))
        .is_ok()
    {
        return Err("项目不存在。".into());
    }
    let mut group: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    for host_id in &host_ids {
        if !seen.insert(*host_id) {
            return Err("目标主机不可重复。".into());
        }
        let host_group: String = transaction
            .query_row(
                "SELECT group_name FROM hosts WHERE id = ?1 AND deleting = 0",
                [host_id],
                |row| row.get(0),
            )
            .map_err(|_| "目标主机不存在或正在删除。")?;
        if group.as_ref().is_some_and(|group| group != &host_group) {
            return Err("目标主机必须属于同一主机组。".into());
        }
        group = Some(host_group);
    }
    transaction
        .execute("DELETE FROM project_hosts WHERE project_id = ?1", [id])
        .map_err(|err| err.to_string())?;
    for (ordinal, host_id) in host_ids.into_iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO project_hosts (project_id, host_id, ordinal) VALUES (?1, ?2, ?3)",
                params![id, host_id, ordinal as i64],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO pending_env_credentials (credential_id)
         SELECT credential_id FROM project_env WHERE project_id = ?1 AND host_id IS NOT NULL
         AND host_id NOT IN (SELECT host_id FROM project_hosts WHERE project_id = ?1)
         AND credential_id IS NOT NULL",
            [id],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "DELETE FROM project_env WHERE project_id = ?1 AND host_id IS NOT NULL
         AND host_id NOT IN (SELECT host_id FROM project_hosts WHERE project_id = ?1)",
            [id],
        )
        .map_err(|err| err.to_string())?;
    if let Some((name, path)) = adopt {
        let changed = transaction.execute(
            "UPDATE projects SET manifest_mode = 1 WHERE id = ?1 AND manifest_mode = 0 AND name = ?2 AND path = ?3",
            params![id, name, path],
        ).map_err(|err| err.to_string())?;
        if changed != 1 {
            return Err("项目在迁移期间发生变化，请重新核对。".into());
        }
    }
    transaction.commit().map_err(|err| err.to_string())?;
    list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or_else(|| "项目不存在。".into())
}

pub fn set_hosts(id: i64, host_ids: Vec<i64>) -> Result<(Project, Option<String>), String> {
    let mut conn = connect()?;
    let project = with_credential_lock(|| set_hosts_in(&mut conn, id, host_ids))?;
    let warning = env_store::cleanup_pending().unwrap_or_else(Some);
    Ok((project, warning))
}

pub fn create(input: NewProject) -> Result<Project, String> {
    create_in(&connect()?, input)
}

fn update_in(conn: &Connection, id: i64, input: NewProject) -> Result<Project, String> {
    ensure_editable(conn, id)?;
    let input = input.normalize()?;
    let transaction = conn
        .unchecked_transaction()
        .map_err(|err| err.to_string())?;
    let old_path: String = transaction
        .query_row("SELECT path FROM projects WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .map_err(|_| "项目不存在。")?;
    if transaction
        .execute(
            "UPDATE projects SET name = ?1, path = ?2, dockerfile = ?3,
             service = CASE WHEN path <> ?2 THEN '' ELSE service END
             WHERE id = ?4 AND NOT EXISTS (SELECT 1 FROM projects WHERE name = ?1 AND id <> ?4)",
            params![input.name, input.path, input.dockerfile, id],
        )
        .map_err(|err| err.to_string())?
        == 0
    {
        return Err("项目名已存在。".into());
    }
    if old_path != input.path {
        transaction
            .execute(
                "DELETE FROM project_compose_files WHERE project_id = ?1",
                [id],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or_else(|| "项目不存在。".into())
}

pub fn update(id: i64, input: NewProject) -> Result<Project, String> {
    update_in(&connect()?, id, input)
}

fn delete_in(conn: &Connection, id: i64) -> Result<(), String> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let batch_history: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM release_batches WHERE project_id = ?1",
            [id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if batch_history != 0 {
        return Err("项目有发布批次历史，不能删除或复用项目 ID。".into());
    }
    let unresolved: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1 AND (
            outcome IN ('running', 'unknown', 'blocked')
            OR (outcome = 'complete' AND health_result = 'healthy'))",
            [id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if unresolved != 0 {
        return Err("项目有未核实任务或已完成的受保护版本，不能删除。".into());
    }
    let rollback_active: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM rollback_tasks WHERE project_id = ?1
         AND outcome IN ('running', 'unknown', 'blocked')",
            [id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if rollback_active != 0 {
        return Err("项目有待核实的回滚任务，不能删除。".into());
    }
    env_store::queue_for_project(&transaction, id).map_err(|err| err.to_string())?;
    if transaction
        .execute("DELETE FROM projects WHERE id = ?1", [id])
        .map_err(|err| err.to_string())?
        == 0
    {
        return Err("项目不存在。".into());
    }
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(())
}

pub fn delete(id: i64) -> Result<Option<String>, String> {
    let conn = connect()?;
    with_credential_lock(|| delete_in(&conn, id))?;
    Ok(env_store::cleanup_pending().unwrap_or_else(Some))
}

pub(crate) fn valid_image_name(image: &str) -> bool {
    if image.is_empty() || image.len() > 255 || image.contains('@') {
        return false;
    }
    let mut parts = image.split('/');
    let first = parts.next().unwrap_or_default();
    let rest: Vec<_> = parts.collect();
    let (host, port) = if !rest.is_empty() {
        match first.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (first, None),
        }
    } else {
        (first, None)
    };
    if port.is_some_and(|port| port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit())) {
        return false;
    }
    let registry_host =
        !rest.is_empty() && (first.contains('.') || first.contains(':') || first == "localhost");
    if registry_host {
        valid_registry_host(host) && rest.into_iter().all(valid_repository_segment)
    } else {
        std::iter::once(host)
            .chain(rest)
            .all(valid_repository_segment)
    }
}

fn valid_registry_host(host: &str) -> bool {
    host.split('.').all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    })
}

fn valid_repository_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len()
        && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
    {
        index += 1;
    }
    if index == 0 {
        return false;
    }
    while index < bytes.len() {
        let separator = bytes[index];
        if separator == b'-' {
            while index < bytes.len() && bytes[index] == b'-' {
                index += 1;
            }
        } else if separator == b'_' {
            index += 1;
            if index < bytes.len() && bytes[index] == b'_' {
                index += 1;
            }
        } else if separator == b'.' {
            index += 1;
        } else {
            return false;
        }
        let start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
        {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    true
}

pub(crate) fn valid_tag_rule(rule: &str) -> bool {
    if rule.is_empty() || rule.len() > 128 {
        return false;
    }
    let expanded = rule
        .replace("{timestamp}", "20260101-000000")
        .replace("{git-sha}", &"a".repeat(64));
    expanded.len() + "-amd64".len() <= 128
        && expanded
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && expanded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn update_build_in(
    conn: &mut Connection,
    id: i64,
    mut config: BuildConfig,
) -> Result<Project, String> {
    ensure_editable(conn, id)?;
    config.image = config.image.trim().to_owned();
    config.tag_rule = config.tag_rule.trim().to_owned();
    if !valid_image_name(&config.image) {
        return Err("镜像名格式无效：请使用小写仓库路径，不要包含 tag。".into());
    }
    if !valid_tag_rule(&config.tag_rule) {
        return Err("Tag 规则须生成合法的 Docker tag，并为架构后缀预留长度。".into());
    }
    if !matches!(
        config.platform.as_str(),
        "auto" | "linux/amd64" | "linux/arm64"
    ) {
        return Err("目标平台无效。".into());
    }
    let mut names = std::collections::HashSet::new();
    for (name, _) in &mut config.build_args {
        *name = name.trim().to_owned();
        let mut chars = name.bytes();
        if !chars
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !chars.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !names.insert(name.clone())
        {
            return Err("Build args 名称无效或重复。".into());
        }
    }
    let transaction = conn.transaction().map_err(|err| err.to_string())?;
    let changed = transaction
        .execute(
            "UPDATE projects SET image = ?1, tag_rule = ?2, platform = ?3 WHERE id = ?4",
            params![config.image, config.tag_rule, config.platform, id],
        )
        .map_err(|err| err.to_string())?;
    if changed == 0 {
        return Err("项目不存在。".into());
    }
    transaction
        .execute("DELETE FROM project_build_args WHERE project_id = ?1", [id])
        .map_err(|err| err.to_string())?;
    for (ordinal, (name, value)) in config.build_args.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO project_build_args (project_id, ordinal, name, value) VALUES (?1, ?2, ?3, ?4)",
                params![id, ordinal as i64, name, value],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or_else(|| "项目不存在。".into())
}

pub fn update_build(id: i64, config: BuildConfig) -> Result<Project, String> {
    update_build_in(&mut connect()?, id, config)
}

fn set_service_in(conn: &Connection, id: i64, service: String) -> Result<Project, String> {
    ensure_editable(conn, id)?;
    if service.is_empty() || service.len() > 256 {
        return Err("部署服务无效。".into());
    }
    if conn
        .execute(
            "UPDATE projects SET service = ?1 WHERE id = ?2",
            params![service, id],
        )
        .map_err(|err| err.to_string())?
        == 0
    {
        return Err("项目不存在。".into());
    }
    list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or_else(|| "项目不存在。".into())
}

pub fn set_service(id: i64, service: String) -> Result<Project, String> {
    set_service_in(&connect()?, id, service)
}

fn validate_compose_files(project: &Project, files: &[String]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for (index, file) in files.iter().enumerate() {
        let relative = Path::new(file);
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            || !seen.insert(file)
        {
            return Err("Compose 文件路径无效或重复。".into());
        }
        if index == 0 && relative.components().count() != 1 {
            return Err("首个 Compose 文件必须位于项目根目录。".into());
        }
        if project.compose_files.contains(file) {
            continue;
        }
        let root = Path::new(&project.path)
            .canonicalize()
            .map_err(|_| "本地代码目录不存在，请先核对项目路径。")?;
        let resolved = root
            .join(relative)
            .canonicalize()
            .map_err(|_| format!("Compose 文件不存在：{file}"))?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            return Err(format!("Compose 文件必须位于项目目录内：{file}"));
        }
    }
    Ok(())
}

fn set_compose_files_in(
    conn: &mut Connection,
    id: i64,
    files: Vec<String>,
) -> Result<Project, String> {
    ensure_editable(conn, id)?;
    let project = list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。")?;
    validate_compose_files(&project, &files)?;
    let transaction = conn.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "DELETE FROM project_compose_files WHERE project_id = ?1",
            [id],
        )
        .map_err(|err| err.to_string())?;
    for (ordinal, path) in files.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO project_compose_files (project_id, ordinal, path) VALUES (?1, ?2, ?3)",
                params![id, ordinal as i64, path],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    list_in(conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or_else(|| "项目不存在。".into())
}

pub fn set_compose_files(id: i64, files: Vec<String>) -> Result<Project, String> {
    set_compose_files_in(&mut connect()?, id, files)
}

pub fn add_compose_files(id: i64, selected: Vec<PathBuf>) -> Result<Project, String> {
    let mut conn = connect()?;
    let project = list_in(&conn)
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|project| project.id == id)
        .ok_or("项目不存在。")?;
    let root = Path::new(&project.path)
        .canonicalize()
        .map_err(|_| "本地代码目录不存在，请先核对项目路径。")?;
    let mut files = project.compose_files;
    for selected_file in selected {
        let resolved = selected_file
            .canonicalize()
            .map_err(|_| "所选 Compose 文件不存在。")?;
        let relative = resolved
            .strip_prefix(&root)
            .map_err(|_| "Compose 文件必须位于项目目录内。")?;
        files.push(relative.to_string_lossy().into_owned());
    }
    set_compose_files_in(&mut conn, id, files)
}

#[cfg(test)]
mod tests {
    use super::{
        BuildConfig, NewProject, create_from_manifest_in, create_in, delete_in, list_in,
        resolve_manifest_project, set_compose_files_in, set_hosts_in, set_hosts_inner,
        set_service_in, update_build_in, update_in,
    };
    use crate::database::initialize;
    use crate::host_store::Host;
    use rusqlite::Connection;
    use std::fs;

    #[test]
    fn active_batch_prevents_project_deletion() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, path, dockerfile, image, manifest_mode)
            VALUES (7, 'app', '.', 'Dockerfile', 'app', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
            VALUES ('batch', 7, ?1, 'prepared')",
            ["a".repeat(64)],
        )
        .unwrap();
        assert!(delete_in(&conn, 7).is_err());
        conn.execute(
            "UPDATE release_batches SET state = 'terminated' WHERE id = 'batch'",
            [],
        )
        .unwrap();
        assert!(delete_in(&conn, 7).is_err());
    }

    #[test]
    fn imports_developer_manifest_and_never_falls_back_to_stale_db_fields() {
        let directory = std::env::temp_dir().join(format!(
            "devingpanel-manifest-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("Dockerfile"), "FROM scratch\n").unwrap();
        fs::write(
            directory.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE}\n",
        )
        .unwrap();
        let manifest_path = directory.join("devingpanel.yaml");
        fs::write(
            &manifest_path,
            "version: 2\nname: myapp\ntag: '{timestamp}'\nbuilds:\n  - service: web\n    dockerfile: Dockerfile\n    image: myapp\n    platform: auto\n    args: {}\ndeploy:\n  compose:\n    files: [compose.yaml]\n  targets:\n    - group: prod\n      host: web-1\n",
        ).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id)
            VALUES (4, 'web-1', '203.0.113.10', 22, 'deploy', 'password', 'prod', 'ref')",
            [],
        )
        .unwrap();
        let host = Host {
            id: 4,
            name: "web-1".into(),
            address: "203.0.113.10".into(),
            port: 22,
            username: "deploy".into(),
            group: "prod".into(),
            auth: "password".into(),
            trusted_fingerprint: None,
        };
        let id = create_from_manifest_in(&conn, &directory, std::slice::from_ref(&host)).unwrap();
        assert!(create_from_manifest_in(&conn, &directory, std::slice::from_ref(&host)).is_err());
        let raw = list_in(&conn).unwrap().remove(0);
        assert_eq!(raw.id, id);
        assert!(raw.manifest_mode);
        assert_eq!(raw.host_ids, [4]);
        let effective = resolve_manifest_project(&raw, std::slice::from_ref(&host)).unwrap();
        assert_eq!(effective.host_ids, [4]);
        assert_eq!(effective.compose_files, ["compose.yaml"]);
        assert!(resolve_manifest_project(&raw, &[]).is_err());
        let changed = fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("host: web-1", "host: web-2");
        fs::write(&manifest_path, changed).unwrap();
        let mut renamed = host.clone();
        renamed.id = 5;
        renamed.name = "web-2".into();
        assert!(resolve_manifest_project(&raw, &[renamed]).is_err());
        fs::write(&manifest_path, "version: 1\ninvalid: true\n").unwrap();
        assert!(resolve_manifest_project(&raw, &[host]).is_err());
        fs::remove_file(manifest_path).unwrap();
        assert!(resolve_manifest_project(&raw, &[]).is_err());
        fs::remove_file(directory.join("Dockerfile")).unwrap();
        fs::remove_file(directory.join("compose.yaml")).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn failed_adoption_rolls_back_bindings_and_host_environment() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'app', 'path', 'Dockerfile', 'app');
             INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id)
             VALUES (1, 'old', '203.0.113.1', 22, 'root', 'password', 'prod', 'c1'),
                    (2, 'new', '203.0.113.2', 22, 'root', 'password', 'prod', 'c2');
             INSERT INTO project_hosts VALUES (1, 1, 0);
             INSERT INTO project_env (project_id, host_id, ordinal, name, value)
             VALUES (1, 1, 0, 'MODE', 'old');",
        ).unwrap();
        assert!(set_hosts_inner(&mut conn, 1, vec![2], Some(("wrong", "path"))).is_err());
        let project = list_in(&conn).unwrap().remove(0);
        assert_eq!(project.host_ids, [1]);
        assert!(!project.manifest_mode);
        let value: String = conn
            .query_row(
                "SELECT value FROM project_env WHERE host_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "old");
    }

    #[test]
    fn project_edits_preserve_identity_and_delete_local_relations() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        initialize(&conn).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "deving-panel-project-edit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("Dockerfile"), "FROM scratch\n").unwrap();
        let draft = |name: &str| NewProject {
            name: name.into(),
            path: directory.to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
        };
        let first = create_in(&conn, draft("app")).unwrap();
        let other = create_in(&conn, draft("other")).unwrap();
        assert!(create_in(&conn, draft("app")).is_err());
        assert!(update_in(&conn, first.id, draft("other")).is_err());
        let edited = update_in(&conn, first.id, draft("renamed")).unwrap();
        assert_eq!(edited.id, first.id);
        assert_eq!(edited.image, "app");
        assert_eq!(list_in(&conn).unwrap().len(), 2);
        conn.execute(
            "UPDATE projects SET service = 'web' WHERE id = ?1",
            [first.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_compose_files VALUES (?1, 0, 'compose.yml')",
            [first.id],
        )
        .unwrap();
        let new_directory = directory.join("moved");
        fs::create_dir(&new_directory).unwrap();
        fs::write(new_directory.join("Dockerfile"), "FROM scratch\n").unwrap();
        fs::write(new_directory.join("compose.yml"), "services: {}\n").unwrap();
        let moved = update_in(
            &conn,
            first.id,
            NewProject {
                name: "renamed".into(),
                path: new_directory.to_string_lossy().into_owned(),
                dockerfile: "Dockerfile".into(),
            },
        )
        .unwrap();
        assert!(moved.compose_files.is_empty());
        assert!(moved.service.is_empty());
        conn.execute(
            "INSERT INTO project_build_args VALUES (?1, 0, 'MODE', 'prod')",
            [first.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hosts (id, name, address, port, username, auth, credential_id)
             VALUES (5, 'host', 'host.example', 22, 'root', 'password', 'random')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO project_hosts VALUES (?1, 5, 0)", [first.id])
            .unwrap();
        assert!(delete_in(&conn, 999).is_err());
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name, host_address,
             host_port, host_username, host_auth, host_fingerprint, image_reference, platform,
             config_sha256, phase, outcome)
             VALUES ('task', ?1, 5, 'app', 'host', 'host.example', 22, 'root', 'password',
             'SHA256:known', 'app:v1', 'linux/amd64', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'loading', 'unknown')",
            [first.id],
        ).unwrap();
        assert!(delete_in(&conn, first.id).is_err());
        conn.execute(
            "UPDATE release_tasks SET outcome = 'failed' WHERE id = 'task'",
            [],
        )
        .unwrap();
        conn.execute("UPDATE release_tasks SET phase = 'checked', outcome = 'complete', health_result = 'healthy' WHERE id = 'task'", []).unwrap();
        assert!(delete_in(&conn, first.id).is_err());
        conn.execute(
            "UPDATE release_tasks SET outcome = 'failed' WHERE id = 'task'",
            [],
        )
        .unwrap();
        delete_in(&conn, first.id).unwrap();
        assert_eq!(list_in(&conn).unwrap()[0].id, other.id);
        let relations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_hosts WHERE project_id = ?1",
                [first.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(relations, 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM project_build_args", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        fs::remove_file(directory.join("Dockerfile")).unwrap();
        fs::remove_file(new_directory.join("Dockerfile")).unwrap();
        fs::remove_file(new_directory.join("compose.yml")).unwrap();
        fs::remove_dir(new_directory).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn targets_are_ordered_and_bound_to_one_group() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (1, 'app', '.', 'Dockerfile', 'app');
             INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id)
             VALUES (1, 'one', 'one.example', 22, 'root', 'password', 'prod', 'one'),
                    (2, 'two', 'two.example', 22, 'root', 'password', 'prod', 'two'),
                    (3, 'three', 'three.example', 22, 'root', 'password', 'stage', 'three');",
        ).unwrap();
        assert_eq!(
            set_hosts_in(&mut conn, 1, vec![2, 1]).unwrap().host_ids,
            [2, 1]
        );
        for ids in [vec![1, 3], vec![1, 1], vec![1, 99]] {
            assert!(set_hosts_in(&mut conn, 1, ids).is_err());
            assert_eq!(list_in(&conn).unwrap()[0].host_ids, [2, 1]);
        }
        assert_eq!(
            set_hosts_in(&mut conn, 1, vec![1, 2]).unwrap().host_ids,
            [1, 2]
        );
        conn.execute(
            "INSERT INTO project_env (project_id, host_id, ordinal, name, credential_id)
            VALUES (1, 2, 0, 'TOKEN', 'env-ref')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM hosts WHERE id = 1", []).unwrap();
        assert_eq!(list_in(&conn).unwrap()[0].host_ids, [2]);
        assert!(set_hosts_in(&mut conn, 99, vec![2]).is_err());
        assert!(
            set_hosts_in(&mut conn, 1, vec![])
                .unwrap()
                .host_ids
                .is_empty()
        );
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_env", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
        let queued: String = conn
            .query_row(
                "SELECT credential_id FROM pending_env_credentials",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, "env-ref");
    }

    #[test]
    fn projects_are_stored_as_non_secret_fields() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "deving-panel-project-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("Dockerfile"), "FROM scratch\n").unwrap();
        fs::write(directory.join("compose.yml"), "services: {}\n").unwrap();
        fs::create_dir(directory.join("deploy")).unwrap();
        fs::write(directory.join("deploy/compose.prod.yml"), "services: {}\n").unwrap();
        let project = create_in(
            &conn,
            NewProject {
                name: " myapp ".into(),
                path: directory.to_string_lossy().into(),
                dockerfile: "Dockerfile".into(),
            },
        )
        .unwrap();
        let projects = list_in(&conn).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, project.id);
        assert_eq!(projects[0].name, "myapp");
        assert_eq!(projects[0].dockerfile, "Dockerfile");
        assert_eq!(projects[0].image, "myapp");
        assert_eq!(
            set_service_in(&conn, project.id, "web".into())
                .unwrap()
                .service,
            "web"
        );
        assert!(set_service_in(&conn, project.id, String::new()).is_err());
        assert_eq!(list_in(&conn).unwrap()[0].service, "web");
        let updated = update_build_in(
            &mut conn,
            project.id,
            BuildConfig {
                image: "registry.example/myapp".into(),
                tag_rule: "{timestamp}".into(),
                platform: "linux/arm64".into(),
                build_args: vec![("NODE_ENV".into(), "production".into())],
            },
        )
        .unwrap();
        assert_eq!(updated.platform, "linux/arm64");
        assert_eq!(
            updated.build_args,
            [("NODE_ENV".into(), "production".into())]
        );
        assert_eq!(list_in(&conn).unwrap()[0].image, "registry.example/myapp");
        for build_args in [
            vec![("NODE-ENV".into(), "production".into())],
            vec![("A".into(), "1".into()), ("A".into(), "2".into())],
        ] {
            assert!(
                update_build_in(
                    &mut conn,
                    project.id,
                    BuildConfig {
                        image: "myapp".into(),
                        tag_rule: "{timestamp}".into(),
                        platform: "auto".into(),
                        build_args,
                    }
                )
                .is_err()
            );
        }
        assert_eq!(
            list_in(&conn).unwrap()[0].build_args,
            [("NODE_ENV".into(), "production".into())]
        );
        let configured = set_compose_files_in(
            &mut conn,
            project.id,
            vec!["compose.yml".into(), "deploy/compose.prod.yml".into()],
        )
        .unwrap();
        assert_eq!(
            configured.compose_files,
            ["compose.yml", "deploy/compose.prod.yml"]
        );
        for invalid in [
            vec!["deploy/compose.prod.yml".into()],
            vec!["compose.yml".into(), "compose.yml".into()],
            vec!["compose.yml".into(), "../outside.yml".into()],
        ] {
            assert!(set_compose_files_in(&mut conn, project.id, invalid).is_err());
        }
        assert_eq!(
            list_in(&conn).unwrap()[0].compose_files,
            configured.compose_files
        );
        let reordered =
            set_compose_files_in(&mut conn, project.id, vec!["compose.yml".into()]).unwrap();
        assert_eq!(reordered.compose_files, ["compose.yml"]);
        for image in [
            "my app",
            "UPPER",
            "myapp:",
            "bad\nname",
            "../app",
            "a..b",
            "a._b",
            "a___b",
            "bad_host:5000/app",
            "bad_host.example/app",
        ] {
            assert!(
                update_build_in(
                    &mut conn,
                    project.id,
                    BuildConfig {
                        image: image.into(),
                        tag_rule: "{timestamp}".into(),
                        platform: "auto".into(),
                        build_args: Vec::new(),
                    }
                )
                .is_err()
            );
        }
        for tag_rule in [
            "../{timestamp}",
            "{unknown}",
            "{timestamp}:bad",
            &"a".repeat(123),
        ] {
            assert!(
                update_build_in(
                    &mut conn,
                    project.id,
                    BuildConfig {
                        image: "myapp".into(),
                        tag_rule: tag_rule.into(),
                        platform: "auto".into(),
                        build_args: Vec::new(),
                    }
                )
                .is_err()
            );
        }
        assert!(
            update_build_in(
                &mut conn,
                999,
                BuildConfig {
                    image: "myapp".into(),
                    tag_rule: "{git-sha}".into(),
                    platform: "auto".into(),
                    build_args: Vec::new(),
                }
            )
            .is_err()
        );
        assert_eq!(list_in(&conn).unwrap()[0].image, "registry.example/myapp");
        assert!(
            update_build_in(
                &mut conn,
                project.id,
                BuildConfig {
                    image: "registry.example:5000/team/a__b".into(),
                    tag_rule: "{timestamp}".into(),
                    platform: "auto".into(),
                    build_args: Vec::new(),
                }
            )
            .is_ok()
        );
        assert!(
            update_build_in(
                &mut conn,
                project.id,
                BuildConfig {
                    image: "myapp".into(),
                    tag_rule: "v1.4.2".into(),
                    platform: "auto".into(),
                    build_args: Vec::new(),
                }
            )
            .is_ok()
        );
        assert!(
            update_build_in(
                &mut conn,
                project.id,
                BuildConfig {
                    image: "myapp".into(),
                    tag_rule: "a".repeat(122),
                    platform: "auto".into(),
                    build_args: Vec::new(),
                }
            )
            .is_ok()
        );
        assert!(
            NewProject {
                name: "".into(),
                path: directory.to_string_lossy().into(),
                dockerfile: "Dockerfile".into(),
            }
            .normalize()
            .is_err()
        );
        for invalid_file in ["../Dockerfile", "missing.Dockerfile"] {
            assert!(
                NewProject {
                    name: "myapp".into(),
                    path: directory.to_string_lossy().into(),
                    dockerfile: invalid_file.into(),
                }
                .normalize()
                .is_err()
            );
        }
        assert!(
            NewProject {
                name: "myapp".into(),
                path: "relative/path".into(),
                dockerfile: "Dockerfile".into(),
            }
            .normalize()
            .is_err()
        );
        let db_path = directory.join("test.sqlite3");
        let mut disk = Connection::open(&db_path).unwrap();
        initialize(&disk).unwrap();
        let disk_project = create_in(
            &disk,
            NewProject {
                name: "disk-project".into(),
                path: directory.to_string_lossy().into(),
                dockerfile: "Dockerfile".into(),
            },
        )
        .unwrap();
        set_compose_files_in(
            &mut disk,
            disk_project.id,
            vec!["compose.yml".into(), "deploy/compose.prod.yml".into()],
        )
        .unwrap();
        set_service_in(&disk, disk_project.id, "web".into()).unwrap();
        update_build_in(
            &mut disk,
            disk_project.id,
            BuildConfig {
                image: "disk-project".into(),
                tag_rule: "{timestamp}".into(),
                platform: "auto".into(),
                build_args: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
            },
        )
        .unwrap();
        drop(disk);
        let reopened = Connection::open(&db_path).unwrap();
        initialize(&reopened).unwrap();
        assert_eq!(list_in(&reopened).unwrap()[0].name, "disk-project");
        assert_eq!(
            list_in(&reopened).unwrap()[0].compose_files,
            ["compose.yml", "deploy/compose.prod.yml"]
        );
        assert_eq!(
            list_in(&reopened).unwrap()[0].build_args,
            [("A".into(), "1".into()), ("B".into(), "2".into())]
        );
        assert_eq!(list_in(&reopened).unwrap()[0].service, "web");
        reopened.pragma_update(None, "user_version", 34).unwrap();
        assert!(initialize(&reopened).is_err());
        drop(reopened);
        set_compose_files_in(
            &mut conn,
            project.id,
            vec!["compose.yml".into(), "deploy/compose.prod.yml".into()],
        )
        .unwrap();
        fs::remove_file(directory.join("deploy/compose.prod.yml")).unwrap();
        fs::remove_file(directory.join("compose.yml")).unwrap();
        assert_eq!(list_in(&conn).unwrap()[0].missing_compose_files.len(), 2);
        let repaired =
            set_compose_files_in(&mut conn, project.id, vec!["compose.yml".into()]).unwrap();
        assert_eq!(repaired.missing_compose_files, ["compose.yml"]);
        assert!(
            set_compose_files_in(&mut conn, project.id, Vec::new())
                .unwrap()
                .compose_files
                .is_empty()
        );
        fs::remove_file(db_path).unwrap();
        fs::remove_dir(directory.join("deploy")).unwrap();
        fs::remove_file(directory.join("Dockerfile")).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn upgrades_version_one_projects() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL, dockerfile TEXT NOT NULL);
             INSERT INTO projects (name, path, dockerfile) VALUES ('old-app', 'path', 'Dockerfile');
             PRAGMA user_version = 1;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let project = &list_in(&conn).unwrap()[0];
        assert_eq!(project.image, "old-app");
        assert_eq!(project.tag_rule, "{timestamp}");
        assert_eq!(project.platform, "auto");
        assert_eq!(project.service, "");
        assert!(project.compose_files.is_empty());
    }

    #[test]
    fn upgrades_version_two_without_changing_project_fields() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
                dockerfile TEXT NOT NULL, image TEXT NOT NULL,
                tag_rule TEXT NOT NULL, platform TEXT NOT NULL, service TEXT NOT NULL
             );
             INSERT INTO projects (name, path, dockerfile, image, tag_rule, platform, service)
             VALUES ('old-app', 'path', 'Dockerfile', 'repo/app', 'v{timestamp}', 'linux/arm64', 'web');
             PRAGMA user_version = 2;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 33);
        let project = &list_in(&conn).unwrap()[0];
        assert_eq!(project.image, "repo/app");
        assert_eq!(project.platform, "linux/arm64");
        assert_eq!(project.service, "web");
        assert!(project.compose_files.is_empty());
        assert!(project.build_args.is_empty());
    }

    #[test]
    fn upgrades_version_three_with_compose_files() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
                dockerfile TEXT NOT NULL, image TEXT NOT NULL, tag_rule TEXT NOT NULL,
                platform TEXT NOT NULL, service TEXT NOT NULL);
             CREATE TABLE project_compose_files (project_id INTEGER NOT NULL, ordinal INTEGER NOT NULL,
                path TEXT NOT NULL, PRIMARY KEY(project_id, ordinal));
             INSERT INTO projects VALUES (1, 'app', 'path', 'Dockerfile', 'app', '{timestamp}', 'auto', '');
             INSERT INTO project_compose_files VALUES (1, 0, 'compose.yml');
             PRAGMA user_version = 3;",
        ).unwrap();
        initialize(&conn).unwrap();
        let project = &list_in(&conn).unwrap()[0];
        assert_eq!(project.compose_files, ["compose.yml"]);
        assert!(project.build_args.is_empty());
    }
}
