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
             service TEXT NOT NULL DEFAULT ''
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
         PRAGMA user_version = 9;
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
        8 => conn.execute_batch(
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
        ),
        9 => Ok(()),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

#[cfg(test)]
mod tests {
    use super::initialize;
    use rusqlite::Connection;

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
        assert_eq!(version, 9);
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
            "CREATE TABLE hosts (id INTEGER PRIMARY KEY, credential_id TEXT NOT NULL);
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
            "CREATE TABLE hosts (id INTEGER PRIMARY KEY, credential_id TEXT NOT NULL, trusted_fingerprint TEXT);
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
        assert_eq!(version, 9);
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
        assert_eq!(version, 9);
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
        assert_eq!(version, 9);
        let host_id: i64 = conn
            .query_row("SELECT host_id FROM project_hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(host_id, 5);
        conn.execute("INSERT INTO project_env (project_id, name, value, ordinal) VALUES (3, 'MODE', 'prod', 0)", []).unwrap();
    }
}
