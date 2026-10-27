use crate::preferences;
use rusqlite::Connection;
use std::fs;

pub fn connect() -> Result<Connection, String> {
    let dir = preferences::data_dir().ok_or("无法确定本机配置目录")?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|err| err.to_string())?;
    }
    let db_path = dir.join("state.sqlite3");
    let conn = Connection::open(&db_path).map_err(|err| err.to_string())?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600))
            .map_err(|err| err.to_string())?;
    }
    initialize(&conn).map_err(|err| err.to_string())?;
    Ok(conn)
}

pub fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => conn.execute_batch(
            "BEGIN;
         CREATE TABLE projects (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             path TEXT NOT NULL,
             dockerfile TEXT NOT NULL,
             image TEXT NOT NULL,
             tag_rule TEXT NOT NULL DEFAULT '{timestamp}',
             platform TEXT NOT NULL DEFAULT 'auto',
             service TEXT NOT NULL DEFAULT '',
             manifest_mode INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE project_compose_files (
             project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             path TEXT NOT NULL,
             PRIMARY KEY (project_id, ordinal)
         );
         CREATE TABLE project_build_args (
             project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             name TEXT NOT NULL,
             value TEXT NOT NULL,
             PRIMARY KEY (project_id, ordinal)
         );
         CREATE TABLE hosts (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             address TEXT NOT NULL,
             port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
             username TEXT NOT NULL,
             auth TEXT NOT NULL CHECK (auth IN ('password', 'key')),
             group_name TEXT NOT NULL DEFAULT '',
             credential_id TEXT NOT NULL UNIQUE,
             trusted_fingerprint TEXT,
             deleting INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE pending_credentials (credential_id TEXT PRIMARY KEY);
         CREATE TABLE project_hosts (
             project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
             host_id INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             PRIMARY KEY (project_id, host_id),
             UNIQUE (project_id, ordinal)
         );
         CREATE TABLE project_env (
             id INTEGER PRIMARY KEY,
             project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
             host_id INTEGER REFERENCES hosts(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             name TEXT NOT NULL,
             value TEXT,
             credential_id TEXT UNIQUE,
             CHECK ((value IS NULL) <> (credential_id IS NULL))
         );
         CREATE UNIQUE INDEX project_env_common_name ON project_env(project_id, name) WHERE host_id IS NULL;
         CREATE UNIQUE INDEX project_env_host_name ON project_env(project_id, host_id, name) WHERE host_id IS NOT NULL;
         CREATE TABLE pending_env_credentials (credential_id TEXT PRIMARY KEY);
         CREATE TABLE release_tasks (
             id TEXT PRIMARY KEY,
             project_id INTEGER NOT NULL,
             host_id INTEGER NOT NULL,
             project_name TEXT NOT NULL,
             host_name TEXT NOT NULL,
             host_address TEXT NOT NULL,
             host_port INTEGER NOT NULL,
             host_username TEXT NOT NULL,
             host_auth TEXT NOT NULL,
             host_fingerprint TEXT NOT NULL,
             image_reference TEXT NOT NULL,
             platform TEXT NOT NULL,
             config_sha256 TEXT NOT NULL,
             phase TEXT NOT NULL,
             outcome TEXT NOT NULL CHECK (outcome IN ('running', 'failed', 'unknown', 'blocked', 'complete')),
             detail TEXT NOT NULL DEFAULT '',
             started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE UNIQUE INDEX release_task_active_project ON release_tasks(project_id) WHERE outcome IN ('running', 'unknown', 'blocked');
         CREATE TABLE release_task_images (
             task_id TEXT NOT NULL REFERENCES release_tasks(id),
             ordinal INTEGER NOT NULL,
             service TEXT NOT NULL,
             reference TEXT NOT NULL,
             platform TEXT NOT NULL,
             PRIMARY KEY (task_id, ordinal),
             UNIQUE (task_id, service)
         );
         PRAGMA user_version = 12;
         COMMIT;",
        ),
        1 => {
            conn.execute_batch(
                "BEGIN;
             ALTER TABLE projects ADD COLUMN image TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN tag_rule TEXT NOT NULL DEFAULT '{timestamp}';
             ALTER TABLE projects ADD COLUMN platform TEXT NOT NULL DEFAULT 'auto';
             ALTER TABLE projects ADD COLUMN service TEXT NOT NULL DEFAULT '';
             UPDATE projects SET image = lower(name) WHERE image = '';
             PRAGMA user_version = 2;
             COMMIT;",
            )?;
            initialize(conn)
        }
        2 => {
            conn.execute_batch(
                "BEGIN;
             CREATE TABLE project_compose_files (
                 project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL,
                 path TEXT NOT NULL,
                 PRIMARY KEY (project_id, ordinal)
             );
             PRAGMA user_version = 3;
             COMMIT;",
            )?;
            initialize(conn)
        }
        3 => {
            conn.execute_batch(
                "BEGIN;
             CREATE TABLE project_build_args (
                 project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 value TEXT NOT NULL,
                 PRIMARY KEY (project_id, ordinal)
             );
             PRAGMA user_version = 4;
             COMMIT;",
            )?;
            initialize(conn)
        }
        4 => {
            conn.execute_batch(
                "BEGIN;
             CREATE TABLE hosts (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 address TEXT NOT NULL,
                 port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
                 username TEXT NOT NULL,
                 auth TEXT NOT NULL CHECK (auth IN ('password', 'key')),
                 group_name TEXT NOT NULL DEFAULT '',
                 credential_id TEXT NOT NULL UNIQUE,
                 trusted_fingerprint TEXT
             );
             PRAGMA user_version = 5;
             COMMIT;",
            )?;
            initialize(conn)
        }
        5 => {
            conn.execute_batch(
                "BEGIN;
             CREATE TABLE pending_credentials (credential_id TEXT PRIMARY KEY);
             PRAGMA user_version = 6;
             COMMIT;",
            )?;
            initialize(conn)
        }
        6 => {
            conn.execute_batch(
                "BEGIN;
             ALTER TABLE hosts ADD COLUMN deleting INTEGER NOT NULL DEFAULT 0;
             PRAGMA user_version = 7;
             COMMIT;",
            )?;
            initialize(conn)
        }
        7 => {
            conn.execute_batch("BEGIN;
             CREATE TABLE project_hosts (
                 project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 host_id INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL,
                 PRIMARY KEY (project_id, host_id),
                 UNIQUE (project_id, ordinal)
             );
             PRAGMA user_version = 8;
             COMMIT;")?;
            initialize(conn)
        }
        8 => {
            conn.execute_batch(
            "BEGIN;
             CREATE TABLE project_env (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 host_id INTEGER REFERENCES hosts(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 value TEXT,
                 credential_id TEXT UNIQUE,
                 CHECK ((value IS NULL) <> (credential_id IS NULL))
             );
             CREATE UNIQUE INDEX project_env_common_name ON project_env(project_id, name) WHERE host_id IS NULL;
             CREATE UNIQUE INDEX project_env_host_name ON project_env(project_id, host_id, name) WHERE host_id IS NOT NULL;
             CREATE TABLE pending_env_credentials (credential_id TEXT PRIMARY KEY);
             PRAGMA user_version = 9;
             COMMIT;",
            )?;
            initialize(conn)
        }
        9 => {
            conn.execute_batch("BEGIN;
             ALTER TABLE projects ADD COLUMN manifest_mode INTEGER NOT NULL DEFAULT 0;
             PRAGMA user_version = 10;
             COMMIT;")?;
            initialize(conn)
        }
        10 => {
            conn.execute_batch(
            "BEGIN;
             CREATE TABLE release_tasks (
                 id TEXT PRIMARY KEY,
                 project_id INTEGER NOT NULL,
                 host_id INTEGER NOT NULL,
                 project_name TEXT NOT NULL,
                 host_name TEXT NOT NULL,
                 host_address TEXT NOT NULL,
                 host_port INTEGER NOT NULL,
                 host_username TEXT NOT NULL,
                 host_auth TEXT NOT NULL,
                 host_fingerprint TEXT NOT NULL,
                 image_reference TEXT NOT NULL,
                 platform TEXT NOT NULL,
                 config_sha256 TEXT NOT NULL,
                 phase TEXT NOT NULL,
                 outcome TEXT NOT NULL CHECK (outcome IN ('running', 'failed', 'unknown', 'blocked', 'complete')),
                 detail TEXT NOT NULL DEFAULT '',
                 started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                 updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE UNIQUE INDEX release_task_active_project ON release_tasks(project_id) WHERE outcome IN ('running', 'unknown', 'blocked');
             PRAGMA user_version = 11;
             COMMIT;",
            )?;
            initialize(conn)
        }
        11 => conn.execute_batch(
            "BEGIN;
             CREATE TABLE release_task_images (
                 task_id TEXT NOT NULL REFERENCES release_tasks(id),
                 ordinal INTEGER NOT NULL,
                 service TEXT NOT NULL,
                 reference TEXT NOT NULL,
                 platform TEXT NOT NULL,
                 PRIMARY KEY (task_id, ordinal),
                 UNIQUE (task_id, service)
             );
             INSERT INTO release_task_images (task_id, ordinal, service, reference, platform)
             SELECT id, 0, '', image_reference, platform FROM release_tasks;
             PRAGMA user_version = 12;
             COMMIT;",
        ),
        12 => Ok(()),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

#[cfg(test)]
mod tests {
    use super::initialize;
    use rusqlite::Connection;

    #[test]
    fn upgrades_version_eleven_with_existing_release_plan() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, image_reference TEXT NOT NULL, platform TEXT NOT NULL);
             INSERT INTO release_tasks VALUES ('task', 'app:v1-amd64', 'linux/amd64');
             PRAGMA user_version = 11;",
        ).unwrap();
        initialize(&conn).unwrap();
        let plan: (String, String, String) = conn.query_row(
            "SELECT service, reference, platform FROM release_task_images WHERE task_id = 'task'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        ).unwrap();
        assert_eq!(
            plan,
            ("".into(), "app:v1-amd64".into(), "linux/amd64".into())
        );
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
    }

    #[test]
    fn upgrades_version_four_without_touching_projects() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO projects (name) VALUES ('app');
             PRAGMA user_version = 4;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
        let name: String = conn
            .query_row("SELECT name FROM projects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "app");
        conn.execute(
            "INSERT INTO hosts (name, address, port, username, auth, group_name, credential_id)
             VALUES ('web-1', '203.0.113.10', 22, 'deploy', 'password', 'prod', 'random-id')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO pending_credentials VALUES ('pending-id')", [])
            .unwrap();
    }

    #[test]
    fn upgrades_version_five_without_touching_hosts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE hosts (id INTEGER PRIMARY KEY, credential_id TEXT NOT NULL);
             INSERT INTO hosts (credential_id) VALUES ('retained-id');
             PRAGMA user_version = 5;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let id: String = conn
            .query_row("SELECT credential_id FROM hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, "retained-id");
        conn.execute("INSERT INTO pending_credentials VALUES ('orphan-id')", [])
            .unwrap();
    }

    #[test]
    fn upgrades_version_six_with_pending_credentials() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE hosts (id INTEGER PRIMARY KEY, credential_id TEXT NOT NULL, trusted_fingerprint TEXT);
             CREATE TABLE pending_credentials (credential_id TEXT PRIMARY KEY);
             INSERT INTO hosts (credential_id, trusted_fingerprint) VALUES ('live-id', 'SHA256:known');
             INSERT INTO pending_credentials VALUES ('orphan-id');
             PRAGMA user_version = 6;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
        let pending: String = conn
            .query_row("SELECT credential_id FROM pending_credentials", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, "orphan-id");
        let fingerprint: String = conn
            .query_row("SELECT trusted_fingerprint FROM hosts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fingerprint, "SHA256:known");
    }

    #[test]
    fn upgrades_version_seven_with_existing_hosts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE hosts (id INTEGER PRIMARY KEY);
             INSERT INTO projects VALUES (3);
             INSERT INTO hosts VALUES (5);
             PRAGMA user_version = 7;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
        conn.execute(
            "INSERT INTO project_hosts (project_id, host_id, ordinal) VALUES (3, 5, 0)",
            [],
        )
        .unwrap();
        initialize(&conn).unwrap();
    }

    #[test]
    fn upgrades_version_eight_without_changing_bindings() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE hosts (id INTEGER PRIMARY KEY);
             CREATE TABLE project_hosts (project_id INTEGER, host_id INTEGER, ordinal INTEGER);
             INSERT INTO projects VALUES (3);
             INSERT INTO hosts VALUES (5);
             INSERT INTO project_hosts VALUES (3, 5, 0);
             PRAGMA user_version = 8;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
        let host_id: i64 = conn
            .query_row("SELECT host_id FROM project_hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(host_id, 5);
        conn.execute("INSERT INTO project_env (project_id, name, value, ordinal) VALUES (3, 'MODE', 'prod', 0)", []).unwrap();
    }

    #[test]
    fn upgrades_version_nine_without_adopting_existing_projects() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY);
             INSERT INTO projects VALUES (7);
             PRAGMA user_version = 9;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let mode: i64 = conn
            .query_row(
                "SELECT manifest_mode FROM projects WHERE id = 7",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mode, 0);
    }

    #[test]
    fn upgrades_version_ten_with_release_intent_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 10;").unwrap();
        initialize(&conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_tasks'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(tables, 1);
    }
}
