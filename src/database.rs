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
             docker_endpoint TEXT,
             docker_daemon_id TEXT,
             image_reference TEXT NOT NULL,
             platform TEXT NOT NULL,
             config_sha256 TEXT NOT NULL,
             phase TEXT NOT NULL,
             outcome TEXT NOT NULL CHECK (outcome IN ('running', 'failed', 'unknown', 'blocked', 'complete')),
             health_result TEXT CHECK (health_result IN ('healthy', 'running_unverified', 'failed')),
             detail TEXT NOT NULL DEFAULT '',
             started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             execution_finished_at TEXT
         );
         CREATE UNIQUE INDEX release_task_active_daemon ON release_tasks(project_id, docker_daemon_id)
             WHERE docker_daemon_id IS NOT NULL AND outcome IN ('running', 'unknown', 'blocked');
         CREATE UNIQUE INDEX release_task_active_legacy ON release_tasks(project_id)
             WHERE docker_daemon_id IS NULL AND outcome IN ('running', 'unknown', 'blocked');
         CREATE TABLE release_task_images (
             task_id TEXT NOT NULL REFERENCES release_tasks(id),
             ordinal INTEGER NOT NULL,
             service TEXT NOT NULL,
             reference TEXT NOT NULL,
             platform TEXT NOT NULL,
             image_ids TEXT,
             archive_sha256 TEXT,
             archive_bytes INTEGER,
             PRIMARY KEY (task_id, ordinal),
             UNIQUE (task_id, service)
         );
         CREATE TABLE release_uploaded_archives (
             task_id TEXT NOT NULL REFERENCES release_tasks(id),
             service TEXT NOT NULL,
             directory TEXT NOT NULL UNIQUE,
             bytes INTEGER NOT NULL,
             sha256 TEXT NOT NULL,
             state TEXT NOT NULL CHECK (state IN ('reserved', 'uploaded', 'loaded', 'cleaned')),
             PRIMARY KEY (task_id, service)
         );
         CREATE TABLE release_task_resolutions (
             task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
             decision TEXT NOT NULL,
             observed_containers INTEGER NOT NULL,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE release_task_health_reconciliations (
             task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
             observed_containers INTEGER NOT NULL,
             source_phase TEXT NOT NULL DEFAULT 'checking'
                 CHECK (source_phase IN ('checking', 'deploying')),
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE rollback_tasks (
             id TEXT PRIMARY KEY,
             project_id INTEGER NOT NULL,
             source_task_id TEXT NOT NULL REFERENCES release_tasks(id),
             target_task_id TEXT NOT NULL REFERENCES release_tasks(id),
             docker_daemon_id TEXT,
             phase TEXT NOT NULL,
             outcome TEXT NOT NULL CHECK (outcome IN ('running', 'unknown', 'blocked', 'failed', 'complete')),
             observed_containers INTEGER,
             detail TEXT NOT NULL DEFAULT '',
             started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE UNIQUE INDEX rollback_active_daemon ON rollback_tasks(project_id, docker_daemon_id)
             WHERE docker_daemon_id IS NOT NULL AND outcome IN ('running', 'unknown', 'blocked');
         CREATE UNIQUE INDEX rollback_active_legacy ON rollback_tasks(project_id)
             WHERE docker_daemon_id IS NULL AND outcome IN ('running', 'unknown', 'blocked');
         CREATE TABLE rollback_uploaded_archives (
             rollback_id TEXT NOT NULL REFERENCES rollback_tasks(id),
             service TEXT NOT NULL,
             directory TEXT NOT NULL,
             bytes INTEGER NOT NULL,
             sha256 TEXT NOT NULL,
             state TEXT NOT NULL CHECK (state IN ('reserved', 'uploaded', 'loaded', 'cleaned')),
             PRIMARY KEY (rollback_id, service)
         );
         CREATE TABLE rollback_reconciliations (
             rollback_id TEXT PRIMARY KEY REFERENCES rollback_tasks(id),
             observed_containers INTEGER NOT NULL,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE project_current_version (
             project_id INTEGER NOT NULL,
             task_id TEXT NOT NULL REFERENCES release_tasks(id),
             docker_daemon_id TEXT NOT NULL,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             PRIMARY KEY (project_id, docker_daemon_id)
         );
         CREATE TABLE release_predecessors (
             task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
             predecessor_task_id TEXT REFERENCES release_tasks(id)
         );
         CREATE TABLE release_task_services (
             task_id TEXT NOT NULL REFERENCES release_tasks(id),
             service TEXT NOT NULL,
             reference TEXT NOT NULL,
             image_ids TEXT,
             confirmed_image_id TEXT,
             PRIMARY KEY (task_id, service)
         );
         CREATE TABLE release_materials (
             task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
             state TEXT NOT NULL CHECK (state IN ('writing', 'ready')),
             chunks INTEGER NOT NULL CHECK (chunks BETWEEN 0 AND 128),
             material_sha256 TEXT
         );
         CREATE TABLE release_batches (
             id TEXT PRIMARY KEY,
             project_id INTEGER NOT NULL,
             config_sha256 TEXT NOT NULL,
             state TEXT NOT NULL CHECK (state IN ('prepared', 'building', 'ready', 'running', 'paused', 'complete', 'unknown', 'terminated')),
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE UNIQUE INDEX release_batch_active_project ON release_batches(project_id)
             WHERE state IN ('prepared', 'building', 'ready', 'running', 'paused', 'unknown');
         CREATE TABLE release_batch_images (
             batch_id TEXT NOT NULL REFERENCES release_batches(id),
             service TEXT NOT NULL,
             platform TEXT NOT NULL,
             reference TEXT NOT NULL,
             archive_bytes INTEGER,
             archive_sha256 TEXT,
             image_ids TEXT,
             PRIMARY KEY (batch_id, service, platform)
         );
         CREATE TABLE release_batch_targets (
             batch_id TEXT NOT NULL REFERENCES release_batches(id),
             ordinal INTEGER NOT NULL,
             host_id INTEGER NOT NULL,
             host_name TEXT NOT NULL,
             host_group TEXT NOT NULL,
             host_address TEXT NOT NULL,
             host_port INTEGER NOT NULL,
             host_username TEXT NOT NULL,
             host_auth TEXT NOT NULL,
             host_fingerprint TEXT NOT NULL,
             platform TEXT NOT NULL,
             docker_endpoint TEXT NOT NULL,
             docker_daemon_id TEXT NOT NULL,
             env_sha256 TEXT,
             state TEXT NOT NULL CHECK (state IN ('waiting', 'running', 'healthy', 'failed', 'unknown', 'skipped')),
             release_task_id TEXT REFERENCES release_tasks(id),
             detail TEXT NOT NULL DEFAULT '',
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             PRIMARY KEY (batch_id, ordinal),
             UNIQUE (batch_id, host_id),
             UNIQUE (batch_id, docker_daemon_id),
             UNIQUE (release_task_id)
         );
         PRAGMA user_version = 31;
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
        11 => {
            conn.execute_batch("BEGIN;
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
             COMMIT;")?;
            initialize(conn)
        }
        12 => conn.execute_batch(
            "BEGIN;
             ALTER TABLE release_tasks ADD COLUMN docker_endpoint TEXT;
             CREATE TABLE release_task_resolutions (
                 task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
                 decision TEXT NOT NULL,
                 observed_containers INTEGER NOT NULL,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             PRAGMA user_version = 13;
             COMMIT;",
        ).and_then(|_| initialize(conn)),
        13 => {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('release_tasks') WHERE name = 'docker_daemon_id'",
                [], |row| row.get(0),
            )?;
            if exists == 0 {
                conn.execute_batch("BEGIN;
                    ALTER TABLE release_tasks ADD COLUMN docker_daemon_id TEXT;
                    PRAGMA user_version = 14;
                    COMMIT;")?;
            } else {
                conn.execute_batch("BEGIN; PRAGMA user_version = 14; COMMIT;")?;
            }
            initialize(conn)
        }
        14 => {
            conn.execute_batch(
            "BEGIN;
             CREATE TABLE release_materials (
                 task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
                 state TEXT NOT NULL CHECK (state IN ('writing', 'ready')),
                 chunks INTEGER NOT NULL CHECK (chunks BETWEEN 0 AND 128)
             );
             PRAGMA user_version = 15;
             COMMIT;",
            )?;
            initialize(conn)
        }
        15 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_task_images ADD COLUMN image_ids TEXT;
                PRAGMA user_version = 16;
                COMMIT;")?;
            initialize(conn)
        }
        16 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_task_images ADD COLUMN archive_sha256 TEXT;
                ALTER TABLE release_task_images ADD COLUMN archive_bytes INTEGER;
                PRAGMA user_version = 17;
                COMMIT;")?;
            initialize(conn)
        }
        17 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_tasks ADD COLUMN health_result TEXT CHECK (health_result IN ('healthy', 'running_unverified', 'failed'));
                PRAGMA user_version = 18;
                COMMIT;")?;
            initialize(conn)
        }
        18 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_materials ADD COLUMN material_sha256 TEXT;
                PRAGMA user_version = 19;
                COMMIT;")?;
            initialize(conn)
        }
        19 => {
            conn.execute_batch("BEGIN;
                CREATE TABLE release_task_services (
                    task_id TEXT NOT NULL REFERENCES release_tasks(id),
                    service TEXT NOT NULL,
                    reference TEXT NOT NULL,
                    image_ids TEXT,
                    confirmed_image_id TEXT,
                    PRIMARY KEY (task_id, service)
                );
                PRAGMA user_version = 20;
                COMMIT;")?;
            initialize(conn)
        }
        20 => {
            conn.execute_batch("BEGIN;
                CREATE TABLE release_task_health_reconciliations (
                    task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
                    observed_containers INTEGER NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                PRAGMA user_version = 21;
                COMMIT;")?;
            initialize(conn)
        }
        21 => {
            conn.execute_batch("BEGIN;
                CREATE TABLE rollback_tasks (
                    id TEXT PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    source_task_id TEXT NOT NULL REFERENCES release_tasks(id),
                    target_task_id TEXT NOT NULL REFERENCES release_tasks(id),
                    phase TEXT NOT NULL,
                    outcome TEXT NOT NULL CHECK (outcome IN ('running', 'unknown', 'blocked', 'failed', 'complete')),
                    observed_containers INTEGER,
                    detail TEXT NOT NULL DEFAULT '',
                    started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE UNIQUE INDEX rollback_active_project ON rollback_tasks(project_id)
                    WHERE outcome IN ('running', 'unknown', 'blocked');
                CREATE TABLE rollback_uploaded_archives (
                    rollback_id TEXT NOT NULL REFERENCES rollback_tasks(id),
                    service TEXT NOT NULL,
                    directory TEXT NOT NULL,
                    bytes INTEGER NOT NULL,
                    sha256 TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('reserved', 'uploaded', 'loaded', 'cleaned')),
                    PRIMARY KEY (rollback_id, service)
                );
                CREATE TABLE rollback_reconciliations (
                    rollback_id TEXT PRIMARY KEY REFERENCES rollback_tasks(id),
                    observed_containers INTEGER NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE TABLE project_current_version (
                    project_id INTEGER PRIMARY KEY,
                    task_id TEXT NOT NULL REFERENCES release_tasks(id),
                    docker_daemon_id TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE TABLE release_predecessors (
                    task_id TEXT PRIMARY KEY REFERENCES release_tasks(id),
                    predecessor_task_id TEXT REFERENCES release_tasks(id)
                );
                PRAGMA user_version = 22;
                COMMIT;")?;
            initialize(conn)
        }
        22 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_tasks ADD COLUMN execution_finished_at TEXT;
                PRAGMA user_version = 23;
                COMMIT;")?;
            initialize(conn)
        }
        23 => {
            conn.execute_batch("BEGIN;
                CREATE TABLE project_current_version_next (
                    project_id INTEGER NOT NULL,
                    task_id TEXT NOT NULL REFERENCES release_tasks(id),
                    docker_daemon_id TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (project_id, docker_daemon_id)
                );
                INSERT INTO project_current_version_next (project_id, task_id, docker_daemon_id, updated_at)
                    SELECT project_id, task_id, docker_daemon_id, updated_at FROM project_current_version;
                DROP TABLE project_current_version;
                ALTER TABLE project_current_version_next RENAME TO project_current_version;
                PRAGMA user_version = 24;
                COMMIT;")?;
            initialize(conn)
        }
        24 => {
            conn.execute_batch("BEGIN;
                DROP INDEX IF EXISTS release_task_active_project;
                CREATE UNIQUE INDEX release_task_active_daemon ON release_tasks(project_id, docker_daemon_id)
                    WHERE docker_daemon_id IS NOT NULL AND outcome IN ('running', 'unknown', 'blocked');
                CREATE UNIQUE INDEX release_task_active_legacy ON release_tasks(project_id)
                    WHERE docker_daemon_id IS NULL AND outcome IN ('running', 'unknown', 'blocked');
                PRAGMA user_version = 25;
                COMMIT;")?;
            initialize(conn)
        }
        25 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE rollback_tasks ADD COLUMN docker_daemon_id TEXT;
                UPDATE rollback_tasks SET docker_daemon_id = (
                    SELECT docker_daemon_id FROM release_tasks WHERE id = source_task_id);
                DROP INDEX IF EXISTS rollback_active_project;
                CREATE UNIQUE INDEX rollback_active_daemon ON rollback_tasks(project_id, docker_daemon_id)
                    WHERE docker_daemon_id IS NOT NULL AND outcome IN ('running', 'unknown', 'blocked');
                CREATE UNIQUE INDEX rollback_active_legacy ON rollback_tasks(project_id)
                    WHERE docker_daemon_id IS NULL AND outcome IN ('running', 'unknown', 'blocked');
                PRAGMA user_version = 26;
                COMMIT;")?;
            initialize(conn)
        }
        26 => {
            conn.execute_batch("BEGIN;
                CREATE TABLE release_batches (
                    id TEXT PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    config_sha256 TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('prepared', 'building', 'ready', 'running', 'paused', 'complete', 'unknown', 'terminated')),
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE UNIQUE INDEX release_batch_active_project ON release_batches(project_id)
                    WHERE state IN ('prepared', 'building', 'ready', 'running', 'paused', 'unknown');
                CREATE TABLE release_batch_images (
                    batch_id TEXT NOT NULL REFERENCES release_batches(id),
                    service TEXT NOT NULL, platform TEXT NOT NULL, reference TEXT NOT NULL,
                    archive_bytes INTEGER, archive_sha256 TEXT,
                    PRIMARY KEY (batch_id, service, platform)
                );
                CREATE TABLE release_batch_targets (
                    batch_id TEXT NOT NULL REFERENCES release_batches(id),
                    ordinal INTEGER NOT NULL, host_id INTEGER NOT NULL,
                    host_name TEXT NOT NULL, host_group TEXT NOT NULL, host_address TEXT NOT NULL,
                    host_port INTEGER NOT NULL, host_username TEXT NOT NULL,
                    host_auth TEXT NOT NULL, host_fingerprint TEXT NOT NULL,
                    platform TEXT NOT NULL, docker_endpoint TEXT NOT NULL,
                    docker_daemon_id TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('waiting', 'running', 'healthy', 'failed', 'unknown', 'skipped')),
                    release_task_id TEXT REFERENCES release_tasks(id),
                    detail TEXT NOT NULL DEFAULT '',
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (batch_id, ordinal), UNIQUE (batch_id, host_id),
                    UNIQUE (batch_id, docker_daemon_id), UNIQUE (release_task_id)
                );
                PRAGMA user_version = 27;
                COMMIT;")?;
            initialize(conn)
        }
        27 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_batch_images ADD COLUMN image_ids TEXT;
                PRAGMA user_version = 28;
                COMMIT;")?;
            initialize(conn)
        }
        28 => {
            conn.execute_batch("BEGIN;
                CREATE TABLE release_uploaded_archives (
                    task_id TEXT NOT NULL REFERENCES release_tasks(id),
                    service TEXT NOT NULL,
                    directory TEXT NOT NULL UNIQUE,
                    bytes INTEGER NOT NULL,
                    sha256 TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('reserved', 'uploaded', 'loaded', 'cleaned')),
                    PRIMARY KEY (task_id, service)
                );
                PRAGMA user_version = 29;
                COMMIT;")?;
            initialize(conn)
        }
        29 => {
            conn.execute_batch("BEGIN;
                ALTER TABLE release_batch_targets ADD COLUMN env_sha256 TEXT;
                PRAGMA user_version = 30;
                COMMIT;")?;
            initialize(conn)
        }
        30 => {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE release_task_health_reconciliations
                     ADD COLUMN source_phase TEXT NOT NULL DEFAULT 'checking'
                     CHECK (source_phase IN ('checking', 'deploying'));
                 PRAGMA user_version = 31;
                 COMMIT;",
            )?;
            initialize(conn)
        }
        31 => Ok(()),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

#[cfg(test)]
mod tests {
    use super::initialize;
    use rusqlite::Connection;

    fn existing_health_audit(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE release_task_health_reconciliations (
                task_id TEXT PRIMARY KEY,
                observed_containers INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);",
        )
        .unwrap();
    }

    #[test]
    fn upgrades_version_twenty_six_without_inventing_batch_history() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY);
            INSERT INTO release_tasks VALUES ('old'); PRAGMA user_version = 26;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let batches: i64 = conn
            .query_row("SELECT COUNT(*) FROM release_batches", [], |row| row.get(0))
            .unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!((old, batches, version), (1, 0, 31));
    }

    #[test]
    fn upgrades_version_twenty_seven_without_inventing_archive_image_ids() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_batch_images (
            batch_id TEXT, service TEXT, platform TEXT, reference TEXT,
            archive_bytes INTEGER, archive_sha256 TEXT);
            CREATE TABLE release_batch_targets (batch_id TEXT, ordinal INTEGER);
            INSERT INTO release_batch_images VALUES ('batch', 'web', 'linux/amd64',
                'app:v1-amd64', 123, 'abcdef');
            PRAGMA user_version = 27;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        let ids: Option<String> = conn
            .query_row(
                "SELECT image_ids FROM release_batch_images WHERE batch_id = 'batch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ids, None);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_eight_with_empty_upload_intents() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY);
            CREATE TABLE release_batch_targets (batch_id TEXT, ordinal INTEGER);
            INSERT INTO release_tasks VALUES ('old'); PRAGMA user_version = 28;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_uploaded_archives",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((count, old), (0, 1));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_thirty_preserving_health_reconciliation_origin() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_task_health_reconciliations (
                task_id TEXT PRIMARY KEY,
                observed_containers INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             INSERT INTO release_task_health_reconciliations (task_id, observed_containers)
                VALUES ('old-checking-task', 4);
             PRAGMA user_version = 30;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let (count, phase): (i64, String) = conn
            .query_row(
                "SELECT observed_containers, source_phase
                 FROM release_task_health_reconciliations WHERE task_id = 'old-checking-task'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, phase.as_str()), (4, "checking"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_nine_without_inventing_environment_digests() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_batch_targets (
            batch_id TEXT, ordinal INTEGER, host_id INTEGER, state TEXT);
            INSERT INTO release_batch_targets VALUES ('batch', 0, 7, 'waiting');
            PRAGMA user_version = 29;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        let digest: Option<String> = conn
            .query_row(
                "SELECT env_sha256 FROM release_batch_targets WHERE batch_id = 'batch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(digest, None);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_eleven_with_existing_release_plan() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, image_reference TEXT NOT NULL, platform TEXT NOT NULL);
             INSERT INTO release_tasks (id, image_reference, platform) VALUES ('task', 'app:v1-amd64', 'linux/amd64');
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
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twelve_with_resolution_audit_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT);
             CREATE TABLE release_task_images (task_id TEXT, service TEXT);
             PRAGMA user_version = 12;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_task_resolutions'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(found, 1);
        let column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('release_tasks') WHERE name = 'docker_endpoint'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(column, 1);
        let daemon_column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('release_tasks') WHERE name = 'docker_daemon_id'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(daemon_column, 1);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_both_version_thirteen_daemon_column_shapes() {
        for has_daemon_column in [false, true] {
            let conn = Connection::open_in_memory().unwrap();
            let schema = if has_daemon_column {
                "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_endpoint TEXT, docker_daemon_id TEXT);
                 CREATE TABLE release_task_images (task_id TEXT, service TEXT);
                 PRAGMA user_version = 13;"
            } else {
                "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_endpoint TEXT);
                 CREATE TABLE release_task_images (task_id TEXT, service TEXT);
                 PRAGMA user_version = 13;"
            };
            conn.execute_batch(schema).unwrap();
            initialize(&conn).unwrap();
            initialize(&conn).unwrap();
            let columns: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('release_tasks') WHERE name = 'docker_daemon_id'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(columns, 1);
            let version: i64 = conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(version, 31);
        }
    }

    #[test]
    fn upgrades_version_fourteen_with_private_material_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT);
             CREATE TABLE release_task_images (task_id TEXT, service TEXT);
             PRAGMA user_version = 14;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_materials'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(found, 1);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_fifteen_with_image_id_snapshots() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT); CREATE TABLE release_task_images (task_id TEXT, service TEXT); CREATE TABLE release_materials (task_id TEXT, state TEXT, chunks INTEGER); PRAGMA user_version = 15;").unwrap();
        initialize(&conn).unwrap();
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('release_task_images') WHERE name = 'image_ids'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(found, 1);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_sixteen_with_local_archive_metadata() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT); CREATE TABLE release_task_images (task_id TEXT, service TEXT); CREATE TABLE release_materials (task_id TEXT, state TEXT, chunks INTEGER); PRAGMA user_version = 16;").unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        for name in ["archive_sha256", "archive_bytes"] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('release_task_images') WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1);
        }
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_seventeen_without_inventing_old_health_results() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT, phase TEXT);
            CREATE TABLE release_materials (task_id TEXT, state TEXT, chunks INTEGER);
            INSERT INTO release_tasks (id, phase, outcome) VALUES ('old', 'checked', 'blocked'); PRAGMA user_version = 17;").unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT health_result FROM release_tasks WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(result, None);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_eighteen_without_inventing_old_material_digest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT);
            CREATE TABLE release_materials (task_id TEXT PRIMARY KEY, state TEXT, chunks INTEGER);
            INSERT INTO release_materials VALUES ('old', 'ready', 1); PRAGMA user_version = 18;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let digest: Option<String> = conn
            .query_row(
                "SELECT material_sha256 FROM release_materials WHERE task_id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(digest, None);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_nineteen_with_empty_service_plan() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT); PRAGMA user_version = 19;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('release_task_services') WHERE name = 'confirmed_image_id'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(found, 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM release_task_services", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_with_separate_health_reconciliation_audit() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT); PRAGMA user_version = 20;",
        )
        .unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_task_health_reconciliations'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(found, 1);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_one_without_inventing_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT);
            INSERT INTO release_tasks (id) VALUES ('old'); PRAGMA user_version = 21;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM rollback_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let pointer: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_current_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pointer, 0);
        let predecessors: i64 = conn
            .query_row("SELECT COUNT(*) FROM release_predecessors", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(predecessors, 0);
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 1);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_two_without_inventing_execution_duration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT);
            CREATE TABLE project_current_version (
                project_id INTEGER PRIMARY KEY, task_id TEXT NOT NULL REFERENCES release_tasks(id),
                docker_daemon_id TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            CREATE TABLE rollback_tasks (id TEXT PRIMARY KEY, project_id INTEGER,
                source_task_id TEXT, target_task_id TEXT, phase TEXT, outcome TEXT);
            INSERT INTO release_tasks (id) VALUES ('old'); PRAGMA user_version = 22;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let finished: Option<String> = conn
            .query_row(
                "SELECT execution_finished_at FROM release_tasks WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(finished, None);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_three_without_inventing_other_daemon_pointers() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER, outcome TEXT, docker_daemon_id TEXT);
             INSERT INTO release_tasks (id) VALUES ('old');
             CREATE TABLE project_current_version (
                 project_id INTEGER PRIMARY KEY, task_id TEXT NOT NULL REFERENCES release_tasks(id),
                 docker_daemon_id TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE rollback_tasks (id TEXT PRIMARY KEY, project_id INTEGER,
                 source_task_id TEXT, target_task_id TEXT, phase TEXT, outcome TEXT);
             INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
                 VALUES (7, 'old', 'DAEMON:ONE');
             PRAGMA user_version = 23;",
        ).unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let pointer: (String, String) = conn.query_row(
            "SELECT task_id, docker_daemon_id FROM project_current_version WHERE project_id = 7",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(pointer, ("old".into(), "DAEMON:ONE".into()));
        conn.execute("INSERT INTO release_tasks (id) VALUES ('other')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO project_current_version (project_id, task_id, docker_daemon_id)
            VALUES (7, 'other', 'DAEMON:TWO')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = 7",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_four_to_daemon_scoped_active_release_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, project_id INTEGER NOT NULL,
                 docker_daemon_id TEXT, outcome TEXT NOT NULL);
             CREATE UNIQUE INDEX release_task_active_project ON release_tasks(project_id)
                 WHERE outcome IN ('running', 'unknown', 'blocked');
             CREATE TABLE rollback_tasks (id TEXT PRIMARY KEY, project_id INTEGER,
                 source_task_id TEXT, target_task_id TEXT, phase TEXT, outcome TEXT);
             INSERT INTO release_tasks VALUES ('one', 7, 'DAEMON:ONE', 'blocked');
             PRAGMA user_version = 24;",
        )
        .unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO release_tasks VALUES ('two', 7, 'DAEMON:TWO', 'running')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO release_tasks VALUES ('again', 7, 'DAEMON:ONE', 'running')",
                []
            )
            .is_err()
        );
        conn.execute(
            "INSERT INTO release_tasks VALUES ('legacy', 8, NULL, 'unknown')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO release_tasks VALUES ('legacy-again', 8, NULL, 'running')",
                []
            )
            .is_err()
        );
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
    }

    #[test]
    fn upgrades_version_twenty_five_preserving_rollback_daemon_identity() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE release_tasks (id TEXT PRIMARY KEY, docker_daemon_id TEXT);
             INSERT INTO release_tasks VALUES ('source', 'DAEMON:ONE');
             CREATE TABLE rollback_tasks (id TEXT PRIMARY KEY, project_id INTEGER,
                 source_task_id TEXT, target_task_id TEXT, phase TEXT, outcome TEXT);
             CREATE UNIQUE INDEX rollback_active_project ON rollback_tasks(project_id)
                 WHERE outcome IN ('running', 'unknown', 'blocked');
             INSERT INTO rollback_tasks VALUES ('old', 7, 'source', 'source', 'checking', 'unknown');
             PRAGMA user_version = 25;",
        ).unwrap();
        existing_health_audit(&conn);
        initialize(&conn).unwrap();
        let daemon: String = conn
            .query_row(
                "SELECT docker_daemon_id FROM rollback_tasks WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(daemon, "DAEMON:ONE");
        conn.execute(
            "INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id,
            docker_daemon_id, phase, outcome) VALUES ('other', 7, 'source', 'source',
            'DAEMON:TWO', 'checking', 'running')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO rollback_tasks (id, project_id, source_task_id, target_task_id,
            docker_daemon_id, phase, outcome) VALUES ('duplicate', 7, 'source', 'source',
            'DAEMON:ONE', 'checking', 'running')",
                []
            )
            .is_err()
        );
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 31);
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
        assert_eq!(version, 31);
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
        assert_eq!(version, 31);
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
        assert_eq!(version, 31);
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
        assert_eq!(version, 31);
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
        assert_eq!(version, 31);
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'release_tasks'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(tables, 1);
    }
}
