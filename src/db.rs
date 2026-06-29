//! SQLite-backed metadata store for the recoverable trash (step 2 scaffold).
//!
//! This module is deliberately self-contained and NOT yet wired into the `rm`
//! command path: step 1 reproduces GNU rm 1:1 with a no-op removal hook, and
//! the trash design is still to be discussed. It is here so the storage layer
//! is ready to plug into `destroy()` once we settle the schema.
//!
//! Like the rest of the crate, nothing here may panic: every fallible call
//! returns a `rusqlite::Result` to the caller.
#![allow(dead_code)]

use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// One row of trash metadata: where a trashed entry lives and where it came
/// from. `id` is the unique on-disk name under the trash's files directory.
#[derive(Debug, Clone, PartialEq)]
pub struct TrashEntry {
    /// Short, stable, never-reused handle shown to the user (displayed in hex).
    /// Ignored on insert (assigned by the database).
    pub seq: i64,
    /// On-disk name under the trash target (a UUIDv7).
    pub id: String,
    /// Absolute path the entry was removed from.
    pub original_path: String,
    /// Deletion time, microseconds since the Unix epoch.
    pub deleted_at: i64,
    /// st_dev of the original location (to pick a restore strategy later).
    pub original_dev: i64,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// Program that invoked rm (parent process `comm`), empty if unknown.
    pub caller: String,
    /// Full command line of the caller, empty if unknown.
    pub caller_cmdline: String,
    /// Whether the on-disk blob is a compressed archive (tar.zst) that must be
    /// extracted on restore rather than moved.
    pub compressed: bool,
}

/// Open (creating if needed) the trash metadata database at `path` and make
/// sure the schema exists.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(path)?;
    // Wait instead of failing immediately when another rm process holds the
    // lock (concurrent invocations share this DB).
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Open an in-memory database (used for tests).
pub fn open_in_memory() -> rusqlite::Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Current on-disk schema version. Bump this and add a `from < N` step in
/// `migrate` for every schema change. Never edit an already-shipped step.
const SCHEMA_VERSION: i64 = 1;

/// Bring a database up to `SCHEMA_VERSION` using `PRAGMA user_version` as the
/// stored version. Each step transforms version N-1 -> N. All steps run inside
/// one transaction so a failed upgrade leaves the DB untouched.
///
/// Rules for future changes:
///   * To evolve the schema, bump `SCHEMA_VERSION` and append a `from < N`
///     block with `ALTER TABLE ...` / `CREATE TABLE ...`. Do NOT modify the v1
///     block, so fresh and existing databases converge on the same schema.
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let from: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if from >= SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn.transaction()?;

    // v0 -> v1: initial schema. `IF NOT EXISTS` so it is also a safe no-op for
    // databases created before versioning was introduced (they already have
    // these tables and just get stamped with user_version = 1).
    if from < 1 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS trash (
                 seq           INTEGER PRIMARY KEY AUTOINCREMENT,
                 id            TEXT NOT NULL UNIQUE,
                 original_path TEXT NOT NULL,
                 deleted_at    INTEGER NOT NULL,
                 original_dev  INTEGER NOT NULL,
                 is_dir        INTEGER NOT NULL,
                 caller        TEXT NOT NULL DEFAULT '',
                 caller_cmdline TEXT NOT NULL DEFAULT '',
                 compressed    INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS blacklist (
                 prog TEXT PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS whitelist (
                 path TEXT PRIMARY KEY
             );",
        )?;
    }

    // Future migrations go here, e.g.:
    // if from < 2 {
    //     tx.execute_batch("ALTER TABLE trash ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0;")?;
    // }

    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// Store a key/value setting, overwriting any existing value.
pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

/// Read a setting, returning None if it has never been set.
pub fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0))
        .optional()
}

/// Record a newly trashed entry.
pub fn insert(conn: &Connection, entry: &TrashEntry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO trash
             (id, original_path, deleted_at, original_dev, is_dir, caller, caller_cmdline, compressed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            &entry.id,
            &entry.original_path,
            entry.deleted_at,
            entry.original_dev,
            entry.is_dir as i64,
            &entry.caller,
            &entry.caller_cmdline,
            entry.compressed as i64,
        ),
    )?;
    Ok(())
}

/// Fetch a single entry by its short handle (seq), if present.
pub fn get(conn: &Connection, seq: i64) -> rusqlite::Result<Option<TrashEntry>> {
    conn.query_row(
        "SELECT seq, id, original_path, deleted_at, original_dev, is_dir, caller, caller_cmdline, compressed
         FROM trash WHERE seq = ?1",
        [seq],
        row_to_entry,
    )
    .optional()
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<TrashEntry> {
    Ok(TrashEntry {
        seq: row.get(0)?,
        id: row.get(1)?,
        original_path: row.get(2)?,
        deleted_at: row.get(3)?,
        original_dev: row.get(4)?,
        is_dir: row.get::<_, i64>(5)? != 0,
        caller: row.get(6)?,
        caller_cmdline: row.get(7)?,
        compressed: row.get::<_, i64>(8)? != 0,
    })
}

/// List all trashed entries, most recently deleted first.
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<TrashEntry>> {
    let mut stmt = conn.prepare(
        "SELECT seq, id, original_path, deleted_at, original_dev, is_dir, caller, caller_cmdline, compressed
         FROM trash ORDER BY deleted_at DESC",
    )?;
    let rows = stmt.query_map([], row_to_entry)?;
    rows.collect()
}

/// Add a program name to the permanent-delete blacklist (idempotent).
pub fn blacklist_add(conn: &Connection, prog: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT OR IGNORE INTO blacklist (prog) VALUES (?1)", [prog])?;
    Ok(())
}

/// Remove a program from the blacklist. Returns whether a row was removed.
pub fn blacklist_remove(conn: &Connection, prog: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM blacklist WHERE prog = ?1", [prog])? > 0)
}

/// List all blacklisted program names, alphabetically.
pub fn blacklist_list(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT prog FROM blacklist ORDER BY prog")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}

/// Whether a program name is on the blacklist.
pub fn is_blacklisted(conn: &Connection, prog: &str) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM blacklist WHERE prog = ?1", [prog], |_| Ok(()))
        .optional()
        .map(|o| o.is_some())
}

/// Add an absolute path prefix to the trash whitelist (idempotent).
pub fn whitelist_add(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT OR IGNORE INTO whitelist (path) VALUES (?1)", [path])?;
    Ok(())
}

/// Remove a path from the whitelist. Returns whether a row was removed.
pub fn whitelist_remove(conn: &Connection, path: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM whitelist WHERE path = ?1", [path])? > 0)
}

/// List all whitelisted path prefixes, alphabetically.
pub fn whitelist_list(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM whitelist ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}

/// Whether a blob id is tracked by a trash row (vs. an orphan blob).
pub fn id_exists(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM trash WHERE id = ?1", [id], |_| Ok(()))
        .optional()
        .map(|o| o.is_some())
}

/// Update the compressed flag of an entry by its UUID id.
pub fn set_compressed(conn: &Connection, id: &str, compressed: bool) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE trash SET compressed = ?1 WHERE id = ?2",
        (compressed as i64, id),
    )?;
    Ok(())
}

/// Remove an entry's metadata by id, returning whether a row was deleted.
pub fn remove(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let affected = conn.execute("DELETE FROM trash WHERE id = ?1", [id])?;
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let conn = open_in_memory().expect("open in-memory db");
        let e = TrashEntry {
            seq: 0, // assigned by the DB on insert
            id: "0190abcd-uuid".into(),
            original_path: "/home/user/file.txt".into(),
            deleted_at: 1_700_000_000_000_000,
            original_dev: 64_768,
            is_dir: false,
            caller: "make".into(),
            caller_cmdline: "make install".into(),
            compressed: false,
        };
        insert(&conn, &e).expect("insert");
        let got = list(&conn).expect("list");
        // The DB assigns the first seq = 1. Everything else round-trips.
        assert_eq!(got, vec![TrashEntry { seq: 1, ..e }]);
        assert!(remove(&conn, "0190abcd-uuid").expect("remove"));
        assert!(list(&conn).expect("list").is_empty());
    }

    #[test]
    fn blacklist_roundtrip() {
        let conn = open_in_memory().expect("open in-memory db");
        assert!(!is_blacklisted(&conn, "make").expect("check"));
        blacklist_add(&conn, "make").expect("add");
        blacklist_add(&conn, "make").expect("add again (idempotent)");
        assert!(is_blacklisted(&conn, "make").expect("check"));
        assert_eq!(blacklist_list(&conn).expect("list"), vec!["make".to_string()]);
        assert!(blacklist_remove(&conn, "make").expect("remove"));
        assert!(!is_blacklisted(&conn, "make").expect("check"));
    }

    #[test]
    fn settings_roundtrip() {
        let conn = open_in_memory().expect("open in-memory db");
        assert_eq!(get_setting(&conn, "target").expect("get"), None);
        set_setting(&conn, "target", "/mnt/storagebox/trash").expect("set");
        assert_eq!(
            get_setting(&conn, "target").expect("get"),
            Some("/mnt/storagebox/trash".to_string())
        );
        // overwrite
        set_setting(&conn, "target", "/other").expect("set");
        assert_eq!(get_setting(&conn, "target").expect("get"), Some("/other".to_string()));
    }
}
