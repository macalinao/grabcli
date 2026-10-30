//! Opening the cache database: file permissions, pragmas, ordered schema
//! migrations, and cheap housekeeping on open.

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, Result};

/// The database file name inside the cache dir.
pub const DB_FILE: &str = "cache.sqlite";

/// How many expired response rows to delete when the database is opened. Keeps
/// opening cheap; `grab cache prune` removes the rest.
const PRUNE_ON_OPEN_LIMIT: i64 = 200;

/// Ordered schema migrations. Index `i` upgrades version `i` to `i + 1`. Never
/// edit a released migration; append a new one.
pub(crate) const MIGRATIONS: &[&str] = &[SCHEMA_V1];

/// Version 1: raw responses, the normalized catalog with FTS5 indexes, and the
/// image store index.
const SCHEMA_V1: &str = r#"
-- Raw upstream responses for read-only browse calls. `key` is the SHA-256 of
-- `identity` (namespace, kind, endpoint, params incl. the rounded location).
CREATE TABLE entries (
    key        TEXT PRIMARY KEY,
    namespace  TEXT NOT NULL,
    kind       TEXT NOT NULL,
    identity   TEXT NOT NULL,
    body       TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    etag       TEXT
);
CREATE INDEX entries_expires ON entries (expires_at);
CREATE INDEX entries_kind ON entries (namespace, kind);

-- Restaurants and stores seen in any list/search/menu response. `rid` is a
-- stable integer rowid for the external-content FTS tables.
CREATE TABLE restaurants (
    rid                INTEGER PRIMARY KEY,
    id                 TEXT NOT NULL UNIQUE,
    namespace          TEXT NOT NULL,
    name               TEXT NOT NULL,
    chain_name         TEXT,
    branch_name        TEXT,
    business_type      TEXT,
    cuisines           TEXT,
    rating             REAL,
    vote_count         INTEGER,
    price_level        INTEGER,
    lat                REAL,
    lon                REAL,
    delivery_fee_minor INTEGER,
    delivery_eta_min   INTEGER,
    has_promo          INTEGER,
    promo_text         TEXT,
    free_delivery      INTEGER,
    open_now           INTEGER,
    opening_hours      TEXT,
    distance_km        REAL,
    photo_url          TEXT,
    fetched_at         INTEGER NOT NULL,
    raw_json           TEXT
);
CREATE INDEX restaurants_latlon ON restaurants (lat, lon);
CREATE INDEX restaurants_fetched ON restaurants (fetched_at);

-- Where a restaurant was seen from: distance/ETA/fee depend on the delivery
-- location, so they are kept per searched location bucket (~110 m).
CREATE TABLE restaurant_locations (
    restaurant_id      TEXT NOT NULL,
    bucket             TEXT NOT NULL,
    search_lat         REAL NOT NULL,
    search_lon         REAL NOT NULL,
    distance_km        REAL,
    delivery_eta_min   INTEGER,
    delivery_fee_minor INTEGER,
    free_delivery      INTEGER,
    fetched_at         INTEGER NOT NULL,
    PRIMARY KEY (restaurant_id, bucket)
);
CREATE INDEX restaurant_locations_pos ON restaurant_locations (search_lat, search_lon);

CREATE TABLE menu_categories (
    restaurant_id TEXT NOT NULL,
    id            TEXT NOT NULL,
    name          TEXT NOT NULL,
    sort          INTEGER NOT NULL,
    fetched_at    INTEGER NOT NULL,
    raw_json      TEXT,
    PRIMARY KEY (restaurant_id, id)
);

CREATE TABLE menu_items (
    rid                    INTEGER PRIMARY KEY,
    id                     TEXT NOT NULL UNIQUE,
    restaurant_id          TEXT NOT NULL,
    category_id            TEXT,
    name                   TEXT NOT NULL,
    description            TEXT,
    price_minor            INTEGER,
    discounted_price_minor INTEGER,
    available              INTEGER,
    image_url              TEXT,
    image_hash             TEXT,
    fetched_at             INTEGER NOT NULL,
    raw_json               TEXT
);
CREATE INDEX menu_items_restaurant ON menu_items (restaurant_id);
CREATE INDEX menu_items_price ON menu_items (price_minor);
CREATE INDEX menu_items_fetched ON menu_items (fetched_at);

-- Word index (diacritic-insensitive, prefix queries) and trigram index
-- (substring matches such as "isig" in "sisig"), both external-content over
-- the tables above and kept in sync by triggers.
CREATE VIRTUAL TABLE restaurants_fts USING fts5(
    name, cuisines, content='restaurants', content_rowid='rid',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE restaurants_tri USING fts5(
    name, cuisines, content='restaurants', content_rowid='rid',
    tokenize='trigram remove_diacritics 1'
);
CREATE TRIGGER restaurants_ai AFTER INSERT ON restaurants BEGIN
    INSERT INTO restaurants_fts (rowid, name, cuisines) VALUES (new.rid, new.name, new.cuisines);
    INSERT INTO restaurants_tri (rowid, name, cuisines) VALUES (new.rid, new.name, new.cuisines);
END;
CREATE TRIGGER restaurants_ad AFTER DELETE ON restaurants BEGIN
    INSERT INTO restaurants_fts (restaurants_fts, rowid, name, cuisines) VALUES ('delete', old.rid, old.name, old.cuisines);
    INSERT INTO restaurants_tri (restaurants_tri, rowid, name, cuisines) VALUES ('delete', old.rid, old.name, old.cuisines);
END;
CREATE TRIGGER restaurants_au AFTER UPDATE OF name, cuisines ON restaurants BEGIN
    INSERT INTO restaurants_fts (restaurants_fts, rowid, name, cuisines) VALUES ('delete', old.rid, old.name, old.cuisines);
    INSERT INTO restaurants_tri (restaurants_tri, rowid, name, cuisines) VALUES ('delete', old.rid, old.name, old.cuisines);
    INSERT INTO restaurants_fts (rowid, name, cuisines) VALUES (new.rid, new.name, new.cuisines);
    INSERT INTO restaurants_tri (rowid, name, cuisines) VALUES (new.rid, new.name, new.cuisines);
END;

CREATE VIRTUAL TABLE menu_items_fts USING fts5(
    name, description, content='menu_items', content_rowid='rid',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE menu_items_tri USING fts5(
    name, description, content='menu_items', content_rowid='rid',
    tokenize='trigram remove_diacritics 1'
);
CREATE TRIGGER menu_items_ai AFTER INSERT ON menu_items BEGIN
    INSERT INTO menu_items_fts (rowid, name, description) VALUES (new.rid, new.name, new.description);
    INSERT INTO menu_items_tri (rowid, name, description) VALUES (new.rid, new.name, new.description);
END;
CREATE TRIGGER menu_items_ad AFTER DELETE ON menu_items BEGIN
    INSERT INTO menu_items_fts (menu_items_fts, rowid, name, description) VALUES ('delete', old.rid, old.name, old.description);
    INSERT INTO menu_items_tri (menu_items_tri, rowid, name, description) VALUES ('delete', old.rid, old.name, old.description);
END;
CREATE TRIGGER menu_items_au AFTER UPDATE OF name, description ON menu_items BEGIN
    INSERT INTO menu_items_fts (menu_items_fts, rowid, name, description) VALUES ('delete', old.rid, old.name, old.description);
    INSERT INTO menu_items_tri (menu_items_tri, rowid, name, description) VALUES ('delete', old.rid, old.name, old.description);
    INSERT INTO menu_items_fts (rowid, name, description) VALUES (new.rid, new.name, new.description);
    INSERT INTO menu_items_tri (rowid, name, description) VALUES (new.rid, new.name, new.description);
END;

-- Content-addressed image files: <cache>/images/ab/cd/<sha256>.<ext>. Several
-- URLs may share one file (same hash).
CREATE TABLE images (
    url          TEXT PRIMARY KEY,
    hash         TEXT NOT NULL,
    bytes        INTEGER NOT NULL,
    width        INTEGER,
    height       INTEGER,
    content_type TEXT NOT NULL,
    last_access  INTEGER NOT NULL,
    fetched_at   INTEGER NOT NULL
);
CREATE INDEX images_hash ON images (hash);
CREATE INDEX images_access ON images (last_access);
"#;

/// Create `dir` (and parents) with every created directory 0700.
pub(crate) fn create_private_dirs(dir: &Path) -> Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)?;
    Ok(())
}

/// Create an empty file with mode 0600 if it doesn't exist yet, so SQLite (and
/// its -wal/-shm files, which copy the database's mode) never start out
/// world-readable.
fn touch_private(path: &Path) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The current schema version (0 for a new database).
pub(crate) fn schema_version(conn: &Connection) -> Result<usize> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    let v: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(usize::try_from(v.unwrap_or(0)).unwrap_or(0))
}

/// Apply every pending migration, each in its own transaction. Returns
/// `(from, to)` versions. A database written by a newer grabcli is an error
/// (the caller then runs without a cache rather than guessing).
pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(usize, usize)> {
    let from = schema_version(conn)?;
    if from > migrations.len() {
        return Err(Error::invalid(format!(
            "cache database schema v{from} is newer than this grabcli (v{}); \
             run `grab cache clear --all` or upgrade",
            migrations.len()
        )));
    }
    for (i, sql) in migrations.iter().enumerate().skip(from) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("DELETE FROM schema_version", [])?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [i64::try_from(i + 1).unwrap_or(i64::MAX)],
        )?;
        tx.commit()?;
    }
    Ok((from, migrations.len()))
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // journal_mode returns a row, so query it rather than execute.
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// The pre-SQLite cache stored one JSON file per entry under
/// `<root>/<namespace>/<kind>/`. It was never released; remove those
/// directories (only ones whose children are all kind names).
fn remove_legacy_json_dirs(root: &Path) {
    const KINDS: [&str; 4] = ["list", "menu", "details", "categories"];
    let Ok(dirs) = fs::read_dir(root) else {
        return;
    };
    for ns in dirs.flatten() {
        let path = ns.path();
        if !path.is_dir() || ns.file_name() == "images" {
            continue;
        }
        let Ok(children) = fs::read_dir(&path) else {
            continue;
        };
        let names: Vec<String> = children
            .flatten()
            .map(|c| c.file_name().to_string_lossy().into_owned())
            .collect();
        if !names.is_empty() && names.iter().all(|n| KINDS.contains(&n.as_str())) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn open_once(path: &Path, now: i64) -> Result<Connection> {
    touch_private(path)?;
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    let (from, _) = migrate(&mut conn, MIGRATIONS)?;
    if from == 0
        && let Some(root) = path.parent()
    {
        remove_legacy_json_dirs(root);
    }
    conn.execute(
        "DELETE FROM entries WHERE rowid IN \
         (SELECT rowid FROM entries WHERE expires_at <= ?1 LIMIT ?2)",
        rusqlite::params![now, PRUNE_ON_OPEN_LIMIT],
    )?;
    Ok(conn)
}

fn is_corrupt(e: &Error) -> bool {
    let m = e.message.to_ascii_lowercase();
    m.contains("not a database") || m.contains("malformed")
}

/// Open (creating if needed) the cache database at `path`: parent dir 0700,
/// file 0600, WAL, busy timeout, migrations, and a bounded prune of expired
/// responses. A corrupt file is deleted and recreated — it's only a cache.
pub(crate) fn open(path: &Path, now: i64) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        create_private_dirs(dir)?;
    }
    match open_once(path, now) {
        Err(e) if is_corrupt(&e) => {
            remove_db_files(path);
            open_once(path, now)
        }
        other => other,
    }
}

/// Delete the database and its WAL/SHM side files.
pub(crate) fn remove_db_files(path: &Path) {
    let _ = fs::remove_file(path);
    for suffix in ["-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = fs::remove_file(Path::new(&p));
    }
}

/// Size of the database plus its WAL file.
pub(crate) fn db_size(path: &Path) -> u64 {
    let mut total = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    total += fs::metadata(Path::new(&wal)).map(|m| m.len()).unwrap_or(0);
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_in_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(DB_FILE);
        let mut conn = open(&path, 0)?;
        assert_eq!(schema_version(&conn)?, MIGRATIONS.len());
        // Re-running is a no-op.
        assert_eq!(
            migrate(&mut conn, MIGRATIONS)?,
            (MIGRATIONS.len(), MIGRATIONS.len())
        );
        // A later migration applies on top, exactly once.
        let more = [MIGRATIONS[0], "CREATE TABLE extra (x INTEGER);"];
        assert_eq!(migrate(&mut conn, &more)?, (1, 2));
        assert_eq!(schema_version(&conn)?, 2);
        assert_eq!(migrate(&mut conn, &more)?, (2, 2));
        // The database is now "newer" than this binary's migrations.
        assert!(migrate(&mut conn, MIGRATIONS).is_err());
        Ok(())
    }

    #[test]
    fn corrupt_database_is_recreated() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(DB_FILE);
        fs::write(
            &path,
            b"this is not sqlite, just some bytes padding it out......",
        )?;
        let conn = open(&path, 0)?;
        assert_eq!(schema_version(&conn)?, MIGRATIONS.len());
        Ok(())
    }

    #[test]
    fn legacy_json_dirs_are_removed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let legacy = dir.path().join("food").join("list");
        fs::create_dir_all(&legacy)?;
        fs::write(legacy.join("abc.json"), b"{}")?;
        let keep = dir.path().join("other");
        fs::create_dir_all(keep.join("stuff"))?;
        open(&dir.path().join(DB_FILE), 0)?;
        assert!(!dir.path().join("food").exists());
        assert!(keep.exists());
        Ok(())
    }

    #[test]
    fn fts5_trigram_available() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='trigram remove_diacritics 1');
             INSERT INTO t (x) VALUES ('Café Sisig');",
        )?;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM t WHERE t MATCH '\"cafe\"'", [], |r| {
                r.get(0)
            })?;
        assert_eq!(n, 1);
        Ok(())
    }
}
