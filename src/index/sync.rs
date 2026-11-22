//! `outlook-index sync`: full reconciliation of the index with the mailbox.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value as Json, json};

use super::db_err;
use super::doctor::{Source, StoreSource};
use super::text::{chunk, strip_quotes};
use crate::body::text_of;
use crate::error::{Error, Result};
use crate::ndb::Nid;
use crate::store::{Folder, MSGFLAG_HASATTACH, is_mail_folder, tag};
use crate::timeutil::{format_ft, now_ft};

/// One contents-table row: everything sync needs without opening the message.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub nid: Nid,
    pub folder: Nid,
    pub received: i64,
    pub submitted: i64,
    pub size: i64,
    pub flags: i64,
    pub class: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub message_id: Option<String>,
}

/// What an opened message adds to its row.
#[derive(Clone, Debug, Default)]
pub struct Opened {
    pub text: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
}

/// What sync reads. Implemented over a `Store` and by a fake in tests.
pub trait MailSource {
    /// Changes whenever the file is written.
    fn revision(&mut self) -> Result<String>;
    /// Mail folders only.
    fn folders(&mut self) -> Result<Vec<Folder>>;
    fn entries(&mut self, folder: Nid) -> Result<Vec<Entry>>;
    fn open(&mut self, nid: Nid) -> Result<Opened>;
}

/// The real file, with the doctor's retry/reopen on inconsistent reads.
impl MailSource for StoreSource {
    fn revision(&mut self) -> Result<String> {
        let (nbt, bbt, eof) = self.store().ndb.revision;
        Ok(format!("{nbt:x}-{bbt:x}-{eof:x}"))
    }

    fn folders(&mut self) -> Result<Vec<Folder>> {
        Ok(Source::folders(self)?.into_iter().filter(is_mail_folder).collect())
    }

    fn entries(&mut self, folder: Nid) -> Result<Vec<Entry>> {
        self.retry(|s| {
            let mut v = Vec::new();
            s.scan_folder(folder, &mut |r| {
                v.push(Entry {
                    nid: r.nid,
                    folder: r.folder,
                    received: r.received,
                    submitted: r.submitted,
                    size: r.size,
                    flags: r.flags,
                    class: r.class.get(),
                    subject: r.subject.get(),
                    sender: r.from_name.get(),
                    message_id: r.message_id.get(),
                });
                true
            })?;
            Ok(v)
        })
    }

    fn open(&mut self, nid: Nid) -> Result<Opened> {
        self.retry(|s| {
            let m = s.message(nid)?;
            let from = m.from();
            Ok(Opened {
                text: text_of(&m),
                from_name: from.name,
                from_email: from.email,
                to: m.pc.str(tag::DISPLAY_TO),
                cc: m.pc.str(tag::DISPLAY_CC),
            })
        })
    }
}

/// Exclusive lock for one sync per index, released when the file is
/// dropped or the process dies.
pub fn lock(index: &std::path::Path) -> Result<std::fs::File> {
    let mut p = index.as_os_str().to_owned();
    p.push(".lock");
    let f = std::fs::File::create(&p).map_err(|e| Error::Io(format!("cannot create the sync lock file: {e}")))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Usage("another sync is running on this index".into())),
        Err(std::fs::TryLockError::Error(e)) => Err(Error::Io(format!("cannot lock the index: {e}"))),
    }
}

/// Identity of a message: Internet Message-ID, else a fingerprint of
/// contents-table columns. The NID is only a locator.
pub fn identity(e: &Entry) -> String {
    match e.message_id.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("mid:{m}"),
        None => format!(
            "fp:{}\u{1f}{}\u{1f}{}\u{1f}{}",
            e.sender.as_deref().unwrap_or(""),
            e.submitted,
            e.subject.as_deref().unwrap_or(""),
            e.size
        ),
    }
}

pub struct Options {
    /// New messages per transaction.
    pub batch: usize,
    /// Stop (resumable) after the first batch that ends past this budget.
    pub max_time: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Options { batch: 200, max_time: None }
    }
}

pub fn get_meta(c: &Connection, key: &str) -> Result<Option<String>> {
    c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().map_err(db_err)
}

fn set_meta(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)", [key, value]).map(drop).map_err(db_err)
}

/// Reconcile the index with `src`: new, deleted, moved/copied messages and
/// flag changes. Only new messages are opened. New messages are committed in
/// batches, so an interrupted run resumes where it stopped. Returns a
/// count-only summary (no content, no names).
pub fn sync(c: &mut Connection, src: &mut dyn MailSource, o: &Options) -> Result<Json> {
    let start = Instant::now();
    let revision = src.revision()?;
    let complete_before = get_meta(c, "sync_complete")?.as_deref() == Some("1");
    if complete_before && get_meta(c, "sync_revision")?.as_deref() == Some(revision.as_str()) {
        let at = get_meta(c, "sync_at")?.and_then(|t| t.parse().ok()).and_then(|t| format_ft(t, false));
        return Ok(json!({
            "unchanged": true,
            "complete": true,
            "revision": revision,
            "synced_at": at,
            "totals": totals(c)?,
            "elapsed_ms": start.elapsed().as_millis() as u64,
        }));
    }

    let folders = src.folders()?;
    let mut entries = Vec::new();
    for f in &folders {
        entries.extend(src.entries(f.nid)?);
    }

    // Group rows by identity, in scan order.
    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<(String, Vec<Entry>)> = Vec::new();
    for e in entries {
        let id = identity(&e);
        let i = *idx.entry(id.clone()).or_insert_with(|| {
            groups.push((id, Vec::new()));
            groups.len() - 1
        });
        groups[i].1.push(e);
    }

    // "moved" counts new locations of known messages (moves and copies).
    let (mut moved, mut flags_changed) = (0u64, 0u64);
    let mut fresh = Vec::new();
    let tx = c.transaction().map_err(db_err)?;
    let deleted = {
        tx.execute("DELETE FROM folders", []).map_err(db_err)?;
        let mut ins = tx.prepare("INSERT INTO folders(nid, path, name) VALUES (?1, ?2, ?3)").map_err(db_err)?;
        for f in &folders {
            ins.execute(params![f.nid, f.path, f.name]).map_err(db_err)?;
        }
        let old: HashMap<Nid, (i64, Nid, i64)> = tx
            .prepare("SELECT nid, message, folder, flags FROM locations")
            .map_err(db_err)?
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))
            .map_err(db_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_err)?;
        let mut seen: HashSet<Nid> = HashSet::new();
        let mut find = tx.prepare("SELECT id FROM messages WHERE ident = ?1").map_err(db_err)?;
        let mut put = tx
            .prepare("INSERT OR REPLACE INTO locations(nid, folder, message, flags) VALUES (?1, ?2, ?3, ?4)")
            .map_err(db_err)?;
        for (ident, group) in groups {
            let Some(id) = find.query_row([&ident], |r| r.get::<_, i64>(0)).optional().map_err(db_err)? else {
                fresh.push((ident, group));
                continue;
            };
            for e in &group {
                seen.insert(e.nid);
                match old.get(&e.nid) {
                    Some(&(m, folder, flags)) if m == id && folder == e.folder => {
                        if flags != e.flags {
                            flags_changed += 1;
                        } else {
                            continue;
                        }
                    }
                    _ => moved += 1,
                }
                put.execute(params![e.nid, e.folder, id, e.flags]).map_err(db_err)?;
            }
        }
        let mut drop_loc = tx.prepare("DELETE FROM locations WHERE nid = ?1").map_err(db_err)?;
        for nid in old.keys().filter(|n| !seen.contains(n)) {
            drop_loc.execute([nid]).map_err(db_err)?;
        }
        let orphans = "SELECT id FROM messages WHERE id NOT IN (SELECT message FROM locations)";
        let gone: Vec<i64> = tx
            .prepare(orphans)
            .map_err(db_err)?
            .query_map([], |r| r.get(0))
            .map_err(db_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_err)?;
        for id in &gone {
            tx.execute("DELETE FROM chunks WHERE message = ?1", [id]).map_err(db_err)?;
            tx.execute("DELETE FROM messages WHERE id = ?1", [id]).map_err(db_err)?;
        }
        gone.len() as u64
    };
    set_meta(&tx, "sync_complete", "0")?;
    tx.commit().map_err(db_err)?;

    let (mut new, mut chunks, mut errors) = (0u64, 0u64, BTreeMap::<&'static str, u64>::new());
    let (mut interrupted, mut batches) = (false, 0);
    let mut pending = fresh.into_iter().peekable();
    while pending.peek().is_some() {
        if batches > 0 && o.max_time.is_some_and(|m| start.elapsed() > m) {
            interrupted = true;
            break;
        }
        let tx = c.transaction().map_err(db_err)?;
        for (ident, group) in pending.by_ref().take(o.batch.max(1)) {
            let e = &group[0];
            let m = match src.open(e.nid) {
                Ok(m) => m,
                Err(err) => {
                    *errors.entry(err.kind()).or_default() += 1;
                    continue;
                }
            };
            tx.execute(
                "INSERT INTO messages(ident, message_id, received, sent, size, class, has_attachments, subject, sender,
                                      from_name, from_email, display_to, display_cc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    ident,
                    e.message_id,
                    e.received,
                    (e.submitted > 0).then_some(e.submitted),
                    e.size,
                    e.class,
                    e.flags & MSGFLAG_HASATTACH != 0,
                    e.subject,
                    e.sender,
                    m.from_name,
                    m.from_email,
                    m.to,
                    m.cc
                ],
            )
            .map_err(db_err)?;
            let id = tx.last_insert_rowid();
            for l in &group {
                tx.execute(
                    "INSERT OR REPLACE INTO locations(nid, folder, message, flags) VALUES (?1, ?2, ?3, ?4)",
                    params![l.nid, l.folder, id, l.flags],
                )
                .map_err(db_err)?;
            }
            let body = m.text.as_deref().map(strip_quotes).unwrap_or_default();
            // At least one (maybe empty) chunk, so the subject is searchable.
            let parts = chunk(&body);
            let parts = if parts.is_empty() { vec![String::new()] } else { parts };
            for (seq, t) in parts.iter().enumerate() {
                tx.execute("INSERT INTO chunks(message, seq, text) VALUES (?1, ?2, ?3)", params![id, seq as i64, t])
                    .map_err(db_err)?;
                chunks += 1;
            }
            new += 1;
        }
        tx.commit().map_err(db_err)?;
        batches += 1;
    }

    // Failed opens are retried by the next sync (never skipped: incomplete).
    let failed: u64 = errors.values().sum();
    let complete = !interrupted && failed == 0;
    let now = now_ft();
    set_meta(c, "sync_revision", &revision)?;
    set_meta(c, "sync_complete", if complete { "1" } else { "0" })?;
    set_meta(c, "sync_at", &now.to_string())?;
    let summary = json!({
        "unchanged": false,
        "complete": complete,
        "revision": revision,
        "synced_at": format_ft(now, false),
        "folders": folders.len(),
        "new": new,
        "deleted": deleted,
        "moved": moved,
        "flags_changed": flags_changed,
        "chunks_added": chunks,
        "failed": failed,
        "remaining": pending.count() as u64 + failed,
        "errors": errors,
        "totals": totals(c)?,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    set_meta(c, "sync_last", &summary.to_string())?;
    Ok(summary)
}

/// Index-wide counts.
pub fn totals(c: &Connection) -> Result<Json> {
    let n = |t: &str| c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).map_err(db_err);
    Ok(json!({
        "messages": n("messages")?,
        "locations": n("locations")?,
        "folders": n("folders")?,
        "chunks": n("chunks")?,
    }))
}

#[cfg(test)]
pub mod fake {
    use std::collections::BTreeMap;

    use super::*;

    /// In-memory mailbox. Fictional content only.
    #[derive(Default)]
    pub struct Fake {
        pub revision: u64,
        pub folders: Vec<Folder>,
        pub entries: Vec<Entry>,
        pub bodies: BTreeMap<Nid, String>,
        pub opened: Vec<Nid>,
    }

    pub fn folder(nid: Nid, path: &str) -> Folder {
        Folder {
            nid,
            parent: None,
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            class: Some("IPF.Note".into()),
            count: 0,
            unread: 0,
            depth: path.matches('/').count(),
        }
    }

    impl Fake {
        pub fn add(&mut self, nid: Nid, folder: Nid, mid: Option<&str>, subject: &str, body: &str) -> &mut Entry {
            self.bodies.insert(nid, body.into());
            self.entries.push(Entry {
                nid,
                folder,
                received: 134_000_000_000_000_000 + nid as i64 * 10_000_000,
                submitted: 134_000_000_000_000_000 + nid as i64,
                size: 1000 + body.len() as i64,
                flags: 1,
                class: Some("IPM.Note".into()),
                subject: Some(subject.into()),
                sender: Some("Zed Fict".into()),
                message_id: mid.map(str::to_string),
            });
            self.revision += 1;
            self.entries.last_mut().unwrap()
        }

        /// Move: new NID in another folder, same content.
        pub fn relocate(&mut self, nid: Nid, new_nid: Nid, folder: Nid) {
            let e = self.entries.iter_mut().find(|e| e.nid == nid).unwrap();
            e.nid = new_nid;
            e.folder = folder;
            let b = self.bodies.remove(&nid).unwrap();
            self.bodies.insert(new_nid, b);
            self.revision += 1;
        }

        pub fn remove(&mut self, nid: Nid) {
            self.entries.retain(|e| e.nid != nid);
            self.revision += 1;
        }
    }

    impl MailSource for Fake {
        fn revision(&mut self) -> Result<String> {
            Ok(self.revision.to_string())
        }
        fn folders(&mut self) -> Result<Vec<Folder>> {
            Ok(self.folders.clone())
        }
        fn entries(&mut self, folder: Nid) -> Result<Vec<Entry>> {
            Ok(self.entries.iter().filter(|e| e.folder == folder).cloned().collect())
        }
        fn open(&mut self, nid: Nid) -> Result<Opened> {
            self.opened.push(nid);
            let text = self.bodies.get(&nid).cloned().ok_or_else(|| Error::NotFound("message".into()))?;
            let sender = self.entries.iter().find(|e| e.nid == nid).and_then(|e| e.sender.clone());
            Ok(Opened {
                text: Some(text),
                from_name: sender,
                from_email: Some("zed@fict.invalid".into()),
                to: Some("Ann Fict".into()),
                cc: Some("Cy Fict".into()),
            })
        }
    }

    /// Mailbox with an Inbox (0x22) and an Archive (0x42) and three messages.
    pub fn mailbox() -> Fake {
        let mut f = Fake { folders: vec![folder(0x22, "Inbox"), folder(0x42, "Archive/2026")], ..Fake::default() };
        f.add(0x204, 0x22, Some("<a1@fict.invalid>"), "Budget café", "Le budget du café est validé.");
        f.add(0x224, 0x22, Some("<a2@fict.invalid>"), "Lunch", "Lunch on friday at noon.");
        f.add(0x244, 0x42, None, "Old note", "Archived résumé of the kickoff.");
        f
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use crate::index::migrate;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn run(c: &mut Connection, f: &mut Fake) -> Json {
        sync(c, f, &Options::default()).unwrap()
    }

    #[test]
    fn initial_then_unchanged() {
        let (mut c, mut f) = (db(), mailbox());
        let s = run(&mut c, &mut f);
        assert_eq!((s["new"].as_u64(), s["complete"].as_bool()), (Some(3), Some(true)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations"), 3);
        assert!(count(&c, "SELECT COUNT(*) FROM chunks") >= 3);
        assert_eq!(f.opened.len(), 3);
        let s = run(&mut c, &mut f);
        assert_eq!(s["unchanged"], true, "{s}");
        assert_eq!(s["totals"]["messages"], 3, "{s}");
        assert_eq!(s["revision"], f.revision.to_string());
        assert!(s["synced_at"].as_str().is_some_and(|t| t.ends_with('Z')), "{s}");
        assert_eq!(f.opened.len(), 3, "unchanged revision opens nothing");
        // Revision moved but nothing new: full reconciliation, nothing opened.
        f.revision += 1;
        let s = run(&mut c, &mut f);
        assert_eq!((s["unchanged"].as_bool(), s["new"].as_u64()), (Some(false), Some(0)), "{s}");
        assert_eq!((s["totals"]["messages"].as_u64(), s["remaining"].as_u64()), (Some(3), Some(0)), "{s}");
        assert_eq!(f.opened.len(), 3);
    }

    #[test]
    fn new_deleted_moved_flags() {
        let (mut c, mut f) = (db(), mailbox());
        run(&mut c, &mut f);
        f.add(0x264, 0x22, Some("<a4@fict.invalid>"), "New", "A new message.");
        f.remove(0x224);
        f.relocate(0x204, 0x284, 0x42);
        f.entries.iter_mut().find(|e| e.nid == 0x244).unwrap().flags = 0;
        let s = run(&mut c, &mut f);
        assert_eq!(s["new"], 1, "{s}");
        assert_eq!(s["deleted"], 1, "{s}");
        assert_eq!(s["moved"], 1, "{s}");
        assert_eq!(s["flags_changed"], 1, "{s}");
        assert_eq!(f.opened, [0x204, 0x224, 0x244, 0x264], "only the new message is opened");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations WHERE nid = 0x284 AND folder = 0x42"), 1);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations WHERE nid = 0x204"), 0);
        assert_eq!(count(&c, "SELECT flags FROM locations WHERE nid = 0x244"), 0);
        let fts = "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'lunch'";
        assert_eq!(count(&c, fts), 0, "deleted message gone from FTS");
        assert_eq!(
            count(&c, "SELECT COUNT(*) FROM chunks c LEFT JOIN messages m ON m.id = c.message WHERE m.id IS NULL"),
            0
        );
    }

    #[test]
    fn same_message_id_two_locations() {
        let (mut c, mut f) = (db(), mailbox());
        f.add(0x2a4, 0x42, Some("<a1@fict.invalid>"), "Budget café", "Le budget du café est validé.");
        let s = run(&mut c, &mut f);
        assert_eq!(s["new"], 3, "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations"), 4);
        assert_eq!(f.opened.len(), 3, "a copy is opened once");
        f.remove(0x204);
        run(&mut c, &mut f);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 3, "still in the other folder");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations"), 3);
    }

    #[test]
    fn fallback_fingerprint() {
        let (mut c, mut f) = (db(), mailbox());
        run(&mut c, &mut f);
        // 0x244 has no Message-ID: a move keeps its identity through the fingerprint.
        f.relocate(0x244, 0x2c4, 0x22);
        let s = run(&mut c, &mut f);
        assert_eq!((s["new"].as_u64(), s["moved"].as_u64()), (Some(0), Some(1)), "{s}");
        let fp = f.entries.iter().find(|e| e.nid == 0x2c4).unwrap();
        assert!(identity(fp).starts_with("fp:"));
        let mut other = fp.clone();
        other.size += 1;
        assert_ne!(identity(fp), identity(&other));
        other.message_id = Some("  <x@fict.invalid> ".into());
        assert_eq!(identity(&other), "mid:<x@fict.invalid>");
    }

    #[test]
    fn interrupted_then_resumed() {
        let (mut c, mut f) = (db(), mailbox());
        for i in 0..7u32 {
            f.add(0x1004 + i * 0x20, 0x22, Some(&format!("<b{i}@fict.invalid>")), "Bulk", "Bulk text.");
        }
        let o = Options { batch: 4, max_time: Some(Duration::ZERO) };
        let s = sync(&mut c, &mut f, &o).unwrap();
        assert_eq!((s["new"].as_u64(), s["complete"].as_bool()), (Some(4), Some(false)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 4, "one committed batch");
        let s = sync(&mut c, &mut f, &o).unwrap();
        assert_eq!((s["unchanged"].as_bool(), s["new"].as_u64()), (Some(false), Some(4)), "resumes: {s}");
        assert_eq!(s["remaining"], 2, "{s}");
        let s = run(&mut c, &mut f);
        assert_eq!((s["new"].as_u64(), s["complete"].as_bool()), (Some(2), Some(true)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 10);
        assert_eq!(f.opened.len(), 10, "nothing opened twice");
        assert_eq!(run(&mut c, &mut f)["unchanged"], true);
    }

    #[test]
    fn open_errors_are_counted_and_retried() {
        let (mut c, mut f) = (db(), mailbox());
        f.bodies.remove(&0x224);
        let s = run(&mut c, &mut f);
        assert_eq!((s["new"].as_u64(), s["complete"].as_bool()), (Some(2), Some(false)), "{s}");
        assert_eq!(s["errors"]["not_found"], 1, "{s}");
        assert_eq!((s["failed"].as_u64(), s["remaining"].as_u64()), (Some(1), Some(1)), "{s}");
        assert_eq!(run(&mut c, &mut f)["unchanged"], false, "a failed open is retried, revision unchanged");
        assert_eq!(f.opened.iter().filter(|n| **n == 0x224).count(), 2);
        f.bodies.insert(0x224, "Back again.".into());
        let s = run(&mut c, &mut f);
        assert_eq!((s["new"].as_u64(), s["complete"].as_bool()), (Some(1), Some(true)), "{s}");
    }

    #[test]
    fn concurrent_sync_rejected() {
        let p = std::env::temp_dir().join(format!("outlook-index-lock-{}.sqlite", std::process::id()));
        let held = lock(&p).unwrap();
        assert_eq!(lock(&p).unwrap_err().kind(), "usage");
        drop(held);
        drop(lock(&p).unwrap());
        let mut l = p.into_os_string();
        l.push(".lock");
        std::fs::remove_file(l).unwrap();
    }

    #[test]
    fn summary_has_counts_only() {
        let (mut c, mut f) = (db(), mailbox());
        f.folders[0].path = "SECRET_FOLDER".into();
        f.add(0x2e4, 0x22, Some("<SECRET@fict.invalid>"), "SECRET_SUBJECT", "SECRET_BODY");
        f.bodies.remove(&0x224);
        let s = run(&mut c, &mut f).to_string();
        for m in ["SECRET", "Zed", "fict", "Budget", "Inbox", "Archive"] {
            assert!(!s.contains(m), "sync summary leaks {m:?}: {s}");
        }
    }
}
