//! Local mail index (cargo feature `index`): database location, schema
//! migrations, `doctor`, `sync`, `search` and `status`.

pub mod doctor;
pub mod search;
pub mod sync;
pub mod text;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Schema migrations, applied in order. Version = position + 1. Append only.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    // Mail index. A message (identity: Message-ID or fingerprint) has one
    // location per copy; the NID changes on a move. FTS5 is external-content
    // over `chunk_text`, kept in sync by the chunk triggers: delete a
    // message's chunks before the message.
    "CREATE TABLE folders (nid INTEGER PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL);
     CREATE TABLE messages (
        id INTEGER PRIMARY KEY,
        ident TEXT NOT NULL UNIQUE,
        message_id TEXT,
        received INTEGER NOT NULL,
        sent INTEGER,
        size INTEGER,
        class TEXT,
        has_attachments INTEGER NOT NULL,
        subject TEXT,
        sender TEXT,
        from_name TEXT,
        from_email TEXT,
        display_to TEXT,
        display_cc TEXT);
     CREATE INDEX messages_received ON messages(received);
     CREATE TABLE locations (
        nid INTEGER PRIMARY KEY,
        folder INTEGER NOT NULL,
        message INTEGER NOT NULL REFERENCES messages(id),
        flags INTEGER NOT NULL);
     CREATE INDEX locations_message ON locations(message);
     CREATE TABLE chunks (
        id INTEGER PRIMARY KEY,
        message INTEGER NOT NULL REFERENCES messages(id),
        seq INTEGER NOT NULL,
        text TEXT NOT NULL);
     CREATE INDEX chunks_message ON chunks(message);
     CREATE VIEW chunk_text AS
        SELECT c.id AS id, m.subject AS subject, c.text AS text FROM chunks c JOIN messages m ON m.id = c.message;
     CREATE VIRTUAL TABLE chunks_fts USING fts5(subject, text, content = 'chunk_text', content_rowid = 'id',
        tokenize = 'unicode61 remove_diacritics 2');
     CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
        INSERT INTO chunks_fts(rowid, subject, text)
            SELECT new.id, subject, new.text FROM messages WHERE id = new.message;
     END;
     CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
        INSERT INTO chunks_fts(chunks_fts, rowid, subject, text)
            SELECT 'delete', old.id, subject, old.text FROM messages WHERE id = old.message;
     END;",
    // Char range of a chunk in the quote-stripped body; NULL for older rows.
    "ALTER TABLE chunks ADD COLUMN start_char INTEGER;
     ALTER TABLE chunks ADD COLUMN end_char INTEGER;",
];

/// %LOCALAPPDATA%\outlook-cli\index.sqlite on Windows, the XDG data dir elsewhere.
pub fn default_path() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
    };
    base.map(|b| b.join("outlook-cli").join("index.sqlite"))
        .ok_or_else(|| Error::Usage("no data directory found; pass --index or set OUTLOOK_INDEX_DB".into()))
}

/// A path inside a OneDrive folder ("OneDrive", "OneDrive - Company"): syncing
/// a live SQLite file there is slow and can corrupt it.
pub fn under_onedrive(p: &Path) -> bool {
    p.components().any(|c| c.as_os_str().to_string_lossy().to_lowercase().starts_with("onedrive"))
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Io(format!("index database: {e}"))
}

/// Open (creating if needed) the index and bring its schema up to date.
pub fn open(p: &Path) -> Result<Connection> {
    if let Some(d) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).map_err(|e| Error::Io(format!("cannot create the index directory: {e}")))?;
    }
    let mut c = Connection::open(p).map_err(db_err)?;
    c.busy_timeout(std::time::Duration::from_secs(10)).map_err(db_err)?;
    c.pragma_update(None, "journal_mode", "WAL").map_err(db_err)?;
    c.pragma_update(None, "synchronous", "NORMAL").map_err(db_err)?;
    migrate(&mut c)?;
    Ok(c)
}

pub fn migrate(c: &mut Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .map_err(db_err)?;
    let v = schema_version(c)?;
    if v > MIGRATIONS.len() {
        return Err(Error::Format(format!("index schema {v} is newer than this tool; upgrade outlook-index")));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(v) {
        let tx = c.transaction().map_err(db_err)?;
        tx.execute_batch(sql).map_err(db_err)?;
        tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [i as i64 + 1]).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
    }
    Ok(())
}

pub fn schema_version(c: &Connection) -> Result<usize> {
    let v: i64 =
        c.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0)).map_err(db_err)?;
    Ok(v.max(0) as usize)
}

/// Whether the linked SQLite has FTS5.
pub fn fts5_available() -> bool {
    Connection::open_in_memory().and_then(|c| c.execute_batch("CREATE VIRTUAL TABLE temp.f USING fts5(x)")).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn fts5_create_and_match() {
        let c = mem();
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(body);
             INSERT INTO t(body) VALUES ('quarterly budget review'), ('lunch on friday');",
        )
        .unwrap();
        let hits: Vec<String> = c
            .prepare("SELECT body FROM t WHERE t MATCH 'budget'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(hits, ["quarterly budget review"]);
        assert!(fts5_available());
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut c = mem();
        assert_eq!(schema_version(&c).unwrap(), MIGRATIONS.len());
        migrate(&mut c).unwrap();
        assert_eq!(schema_version(&c).unwrap(), MIGRATIONS.len());
        c.execute("INSERT INTO schema_version(version) VALUES (999)", []).unwrap();
        assert!(migrate(&mut c).is_err(), "a newer schema must be refused");
    }

    #[test]
    fn chunk_ranges_migration_keeps_old_rows() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT)").unwrap();
        for (i, sql) in MIGRATIONS[..2].iter().enumerate() {
            c.execute_batch(sql).unwrap();
            c.execute("INSERT INTO schema_version(version) VALUES (?1)", [i as i64 + 1]).unwrap();
        }
        c.execute_batch(
            "INSERT INTO messages(id, ident, received, has_attachments) VALUES (1, 'mid:x', 0, 0);
             INSERT INTO chunks(message, seq, text) VALUES (1, 0, 'old');",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        assert_eq!(schema_version(&c).unwrap(), MIGRATIONS.len());
        let r: (Option<i64>, Option<i64>, String) = c
            .query_row("SELECT start_char, end_char, text FROM chunks", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(r, (None, None, "old".into()), "existing rows stay, ranges NULL");
    }

    #[test]
    fn open_creates_file_and_parent() {
        let dir = std::env::temp_dir().join(format!("outlook-index-test-{}", std::process::id()));
        let p = dir.join("sub").join("index.sqlite");
        let c = open(&p).unwrap();
        assert_eq!(schema_version(&c).unwrap(), MIGRATIONS.len());
        drop(c);
        assert!(p.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn onedrive_paths() {
        assert!(under_onedrive(Path::new("/home/u/OneDrive/index.sqlite")));
        assert!(under_onedrive(Path::new("/home/u/OneDrive - Contoso/x/index.sqlite")));
        assert!(!under_onedrive(Path::new("/home/u/.local/share/outlook-cli/index.sqlite")));
    }

    #[test]
    fn default_path_shape() {
        let p = default_path().unwrap();
        assert!(p.ends_with(Path::new("outlook-cli").join("index.sqlite")));
    }
}
