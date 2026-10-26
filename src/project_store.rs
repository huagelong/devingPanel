use crate::database::connect;
use crate::env_store;
use crate::host_store::with_credential_lock;
use rusqlite::{Connection, Row, params};
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
    })
}

fn list_in(conn: &Connection) -> rusqlite::Result<Vec<Project>> {
    let mut stmt =
        conn.prepare("SELECT id, name, path, dockerfile, image, tag_rule, platform, service FROM projects ORDER BY id DESC")?;
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
    list_in(&connect()?).map_err(|err| err.to_string())
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
    })
}

fn set_hosts_in(conn: &mut Connection, id: i64, host_ids: Vec<i64>) -> Result<Project, String> {
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
    let transaction = conn
        .unchecked_transaction()
        .map_err(|err| err.to_string())?;
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

fn valid_tag_rule(rule: &str) -> bool {
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
        BuildConfig, NewProject, create_in, delete_in, list_in, set_compose_files_in, set_hosts_in,
        set_service_in, update_build_in, update_in,
    };
    use crate::database::initialize;
    use rusqlite::Connection;
    use std::fs;

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
        reopened.pragma_update(None, "user_version", 10).unwrap();
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
        assert_eq!(version, 9);
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
