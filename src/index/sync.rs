//! `outlook-index sync`: full reconciliation of the index with the mailbox.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::attach;
use super::db_err;
use super::doctor::{ExtractFn, Source, StoreSource};
use super::progress::Progress;
use super::text::{chunk, strip_quotes};
use crate::body::text_of;
use crate::error::{Error, Result};
use crate::ndb::Nid;
use crate::store::{Attachment, Folder, MSGFLAG_HASATTACH, WellKnown, is_mail_folder, tag};
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
    /// PR_LAST_MODIFICATION_TIME, 0 when absent.
    pub modified: i64,
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
    /// Inbox, Deleted Items and Junk E-mail among `folders`.
    fn well_known(&mut self, folders: &[Folder]) -> Result<WellKnown>;
    /// Attachments of message `nid`: properties only, no data.
    fn attachments(&mut self, nid: Nid) -> Result<Vec<Attachment>>;
    /// Data of attachment `a` of message `nid`.
    fn attachment_data(&mut self, nid: Nid, a: &Attachment) -> Result<Vec<u8>>;
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
                    modified: r.modified,
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

    fn well_known(&mut self, folders: &[Folder]) -> Result<WellKnown> {
        self.retry(|s| s.well_known(folders))
    }

    fn attachments(&mut self, nid: Nid) -> Result<Vec<Attachment>> {
        self.retry(|s| s.message(nid)?.attachments())
    }

    fn attachment_data(&mut self, nid: Nid, a: &Attachment) -> Result<Vec<u8>> {
        Source::attachment_data(self, nid, a)
    }
}

/// How long a writer retries a held sync lock: an observer (`held`) holds
/// it shared for an instant only.
pub const LOCK_WAIT: Duration = Duration::from_secs(3);

/// Exclusive lock for one writer per index, released when the file is
/// dropped or the process dies. Retried for LOCK_WAIT.
pub fn lock(index: &std::path::Path) -> Result<std::fs::File> {
    let mut p = index.as_os_str().to_owned();
    p.push(".lock");
    let f = super::private_file()
        .create(true)
        .truncate(true)
        .open(&p)
        .map_err(|e| Error::Io(format!("cannot create the sync lock file: {e}")))?;
    let start = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::Usage("another sync is running on this index".into()));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(Error::Io(format!("cannot lock the index: {e}"))),
        }
    }
}

/// Whether a process (or another handle of this one) holds the sync lock of
/// `index`. Does not create the lock file. Probes with a shared lock,
/// released at once; `lock` waits that out.
pub fn held(index: &std::path::Path) -> bool {
    let mut p = index.as_os_str().to_owned();
    p.push(".lock");
    let Ok(f) = std::fs::File::open(&p) else { return false };
    matches!(f.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock))
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

/// Change signature of a message, from its contents-table row only:
/// modification time, size, attachment flag and subject. An edit, an added,
/// removed or replaced attachment changes it; read/unread, a move or a copy
/// does not.
pub fn signature(e: &Entry) -> String {
    format!(
        "{:x}:{}:{}:{}",
        e.modified,
        e.size,
        u8::from(e.flags & MSGFLAG_HASATTACH != 0),
        e.subject.as_deref().unwrap_or("")
    )
}

/// A message to open: new (identity, rows) or changed (stored id, row).
enum Work {
    New(String, Vec<Entry>),
    Refresh(i64, Entry),
}

pub struct Options {
    /// New and changed messages per transaction.
    pub batch: usize,
    /// Stop (resumable) after the first batch that ends past this budget.
    pub max_time: Option<Duration>,
    /// Allow deleting more than max(100, 20%) of the indexed messages.
    pub allow_mass_delete: bool,
    /// Change the stored choice of well-known folders to index.
    pub include: Option<Include>,
}

impl Default for Options {
    fn default() -> Self {
        Options { batch: 200, max_time: None, allow_mass_delete: false, include: None }
    }
}

/// Well-known folders indexed with their subfolders (meta `include_folders`).
/// Default: neither, Deleted Items and Junk E-mail are skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Include {
    pub deleted: bool,
    pub junk: bool,
}

/// The stored choice; None for an index synced before it existed.
pub fn include(c: &Connection) -> Result<Option<Include>> {
    match get_meta(c, "include_folders")? {
        Some(s) => serde_json::from_str(&s).map(Some).map_err(|e| Error::Format(format!("stored folder choice: {e}"))),
        None => Ok(None),
    }
}

pub fn get_meta(c: &Connection, key: &str) -> Result<Option<String>> {
    c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().map_err(db_err)
}

pub fn set_meta(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)", [key, value]).map(drop).map_err(db_err)
}

/// `sync_with` without the attachment phase (`--no-attachments`).
pub fn sync(c: &mut Connection, src: &mut dyn MailSource, o: &Options) -> Result<Json> {
    sync_with(c, src, None, o)
}

/// `sync_with_progress`, silent.
pub fn sync_with(c: &mut Connection, src: &mut dyn MailSource, x: Option<&mut ExtractFn>, o: &Options) -> Result<Json> {
    sync_with_progress(c, src, x, o, &mut Progress::none())
}

/// Reconcile the index with `src`: new, changed (`signature`), deleted,
/// moved/copied messages and flag changes. Only new and changed messages are
/// opened, committed in batches (a changed one keeps its old content and
/// signature until its batch commits), so an interrupted run resumes where
/// it stopped. Then, with `x`, the attachment phase (`attach::phase`), in
/// batches too. Returns a count-only summary (no content, no names). Phases
/// `scan` (folders, not stored: the mass-deletion guard writes nothing),
/// `index` (new and changed messages) and `attachments` go to `p`, stored in
/// each batch transaction.
pub fn sync_with_progress(
    c: &mut Connection,
    src: &mut dyn MailSource,
    x: Option<&mut ExtractFn>,
    o: &Options,
    p: &mut Progress,
) -> Result<Json> {
    let start = Instant::now();
    let revision = src.revision()?;
    let stored = include(c)?;
    let inc = o.include.or(stored).unwrap_or_default();
    let complete_before = get_meta(c, "sync_complete")?.as_deref() == Some("1");
    if complete_before
        && stored == Some(inc)
        && get_meta(c, "sync_revision")?.as_deref() == Some(revision.as_str())
        && (x.is_none() || attach::pending(c)? == 0)
        // Legacy NULL signatures (pre-signature index): adopted by a full pass.
        && !c
            .query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE signature IS NULL)", [], |r| r.get::<_, bool>(0))
            .map_err(db_err)?
    {
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

    // Deleted Items and Junk E-mail with their subfolders, unless included.
    let all = src.folders()?;
    let wk = src.well_known(&all)?;
    let roots: Vec<String> = [(wk.deleted_items, inc.deleted), (wk.junk, inc.junk)]
        .iter()
        .filter(|(_, included)| !included)
        .filter_map(|(w, _)| all.iter().find(|f| Some(f.nid) == w.nid))
        .map(|f| format!("{}/", f.path))
        .collect();
    let (excluded, folders): (Vec<Folder>, Vec<Folder>) =
        all.into_iter().partition(|f| roots.iter().any(|r| format!("{}/", f.path).starts_with(r.as_str())));
    p.phase(None, "scan", (folders.len() + excluded.len()) as u64)?;
    let mut entries = Vec::new();
    for (i, f) in folders.iter().enumerate() {
        entries.extend(src.entries(f.nid)?);
        p.update(None, i as u64 + 1)?;
    }
    // Identities still in the file, in excluded folders: leaving the index
    // because of the exclusion, not deleted.
    let mut kept_out = HashSet::new();
    for (i, f) in excluded.iter().enumerate() {
        kept_out.extend(src.entries(f.nid)?.iter().map(identity));
        p.update(None, (folders.len() + i) as u64 + 1)?;
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
    let mut work = Vec::new();
    let tx = c.transaction().map_err(db_err)?;
    // Mass-deletion guard, before any write: an indexed message is deleted
    // when its identity is gone from the scan (a vanished folder, a bad read).
    let (mut indexed, mut gone) = (0usize, 0usize);
    {
        let mut st = tx.prepare("SELECT ident FROM messages").map_err(db_err)?;
        let mut rows = st.query([]).map_err(db_err)?;
        while let Some(r) = rows.next().map_err(db_err)? {
            indexed += 1;
            let ident: String = r.get(0).map_err(db_err)?;
            gone += usize::from(!idx.contains_key(&ident) && !kept_out.contains(&ident));
        }
    }
    let limit = (indexed / 5).max(100);
    if gone > limit && !o.allow_mass_delete {
        return Err(Error::Usage(format!(
            "sync would delete {gone} of {indexed} indexed messages (limit {limit}); nothing changed. \
             Pass --allow-mass-delete if this is expected"
        )));
    }
    let (deleted, excluded_removed) = {
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
        let mut find = tx.prepare("SELECT id, signature FROM messages WHERE ident = ?1").map_err(db_err)?;
        let mut adopt = tx.prepare("UPDATE messages SET signature = ?2 WHERE id = ?1").map_err(db_err)?;
        let mut put = tx
            .prepare("INSERT OR REPLACE INTO locations(nid, folder, message, flags) VALUES (?1, ?2, ?3, ?4)")
            .map_err(db_err)?;
        for (ident, group) in groups {
            let found = find
                .query_row([&ident], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))
                .optional()
                .map_err(db_err)?;
            let Some((id, stored)) = found else {
                work.push(Work::New(ident, group));
                continue;
            };
            // Unchanged while one copy still has the stored signature. NULL
            // (indexed before signatures): adopted, not opened.
            match stored {
                None => {
                    adopt.execute(params![id, signature(&group[0])]).map_err(db_err)?;
                }
                Some(s) if group.iter().any(|e| signature(e) == s) => {}
                Some(_) => work.push(Work::Refresh(id, group[0].clone())),
            }
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
        let orphans = "SELECT id, ident FROM messages WHERE id NOT IN (SELECT message FROM locations)";
        let gone: Vec<(i64, String)> = tx
            .prepare(orphans)
            .map_err(db_err)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_err)?;
        for (id, _) in &gone {
            tx.execute("DELETE FROM chunks WHERE message = ?1", [id]).map_err(db_err)?;
            tx.execute("DELETE FROM message_attachments WHERE message = ?1", [id]).map_err(db_err)?;
            tx.execute("DELETE FROM messages WHERE id = ?1", [id]).map_err(db_err)?;
        }
        if !gone.is_empty() {
            attach::gc(&tx)?;
        }
        let out = gone.iter().filter(|(_, ident)| kept_out.contains(ident)).count() as u64;
        (gone.len() as u64 - out, out)
    };
    set_meta(&tx, "include_folders", &serde_json::to_string(&inc).unwrap())?;
    set_meta(&tx, "sync_complete", "0")?;
    p.phase(Some(&tx), "index", work.len() as u64)?;
    tx.commit().map_err(db_err)?;

    let (mut new, mut refreshed, mut chunks, mut errors) = (0u64, 0u64, 0u64, BTreeMap::<&'static str, u64>::new());
    let (mut interrupted, mut batches, mut done) = (false, 0, 0u64);
    let mut pending = work.into_iter().peekable();
    while pending.peek().is_some() {
        if batches > 0 && o.max_time.is_some_and(|m| start.elapsed() > m) {
            interrupted = true;
            break;
        }
        let tx = c.transaction().map_err(db_err)?;
        for w in pending.by_ref().take(o.batch.max(1)) {
            done += 1;
            let e = match &w {
                Work::New(_, group) => &group[0],
                Work::Refresh(_, e) => e,
            };
            let m = match src.open(e.nid) {
                Ok(m) => m,
                Err(err) => {
                    *errors.entry(err.kind()).or_default() += 1;
                    continue;
                }
            };
            chunks += match &w {
                Work::New(ident, group) => {
                    new += 1;
                    insert(&tx, ident, group, &m)?
                }
                Work::Refresh(id, e) => {
                    refreshed += 1;
                    refresh(&tx, *id, e, &m)?
                }
            };
        }
        p.update(Some(&tx), done)?;
        tx.commit().map_err(db_err)?;
        batches += 1;
    }

    let over = || o.max_time.is_some_and(|m| start.elapsed() > m);
    let with_attachments = x.is_some();
    let att = match x {
        Some(x) if !interrupted => Some(attach::phase(c, src, x, o.batch, batches > 0, &over, p)?),
        _ => None,
    };
    let att_remaining = attach::pending(c)?;

    // Failed opens are retried by the next sync (never skipped: incomplete).
    let failed: u64 = errors.values().sum();
    let complete = !interrupted && failed == 0 && (!with_attachments || att_remaining == 0);
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
        "excluded_folders": excluded.len(),
        "include": inc,
        "new": new,
        "refreshed": refreshed,
        "deleted": deleted,
        "excluded_removed": excluded_removed,
        "moved": moved,
        "flags_changed": flags_changed,
        "chunks_added": chunks,
        "failed": failed,
        "remaining": pending.count() as u64 + failed,
        "errors": errors,
        "totals": totals(c)?,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    let mut summary = summary;
    let o = summary.as_object_mut().unwrap();
    match att {
        Some(a) => {
            o.insert("attachments_extracted".into(), json!(a.extracted));
            o.insert("attachments_deduplicated".into(), json!(a.deduplicated));
            o.insert("attachments_failed".into(), json!(a.failed));
            o.insert("attachments_skipped_by_reason".into(), json!(a.skipped));
            o.insert("attachments_errors".into(), json!(a.errors));
        }
        None => {
            o.insert("attachments".into(), json!(if with_attachments { "not_started" } else { "skipped" }));
        }
    }
    // Messages whose attachments are still to read (new, backfill, retries).
    o.insert("attachments_remaining".into(), json!(att_remaining));
    set_meta(c, "sync_last", &summary.to_string())?;
    Ok(summary)
}

/// Store a new message, its locations and its body chunks; returns the chunk count.
fn insert(tx: &Connection, ident: &str, group: &[Entry], m: &Opened) -> Result<u64> {
    let e = &group[0];
    tx.execute(
        "INSERT INTO messages(ident, message_id, received, sent, size, class, has_attachments, subject, sender,
                              from_name, from_email, display_to, display_cc, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            m.cc,
            signature(e)
        ],
    )
    .map_err(db_err)?;
    let id = tx.last_insert_rowid();
    for l in group {
        tx.execute(
            "INSERT OR REPLACE INTO locations(nid, folder, message, flags) VALUES (?1, ?2, ?3, ?4)",
            params![l.nid, l.folder, id, l.flags],
        )
        .map_err(db_err)?;
    }
    add_chunks(tx, id, m, &HashMap::new())
}

/// A stored vector: model, revision, blob.
type Vector = (String, String, Vec<u8>);

/// Re-read message `id` (signature changed): metadata, body chunks and FTS
/// rows replaced; a chunk whose passage (subject and text) is unchanged keeps
/// its vectors, the others wait for `embed`. The attachments are read again
/// by the attachment phase; with the attachment flag gone, their links go
/// now (`attach::gc` collects contents left without a link).
fn refresh(tx: &Connection, id: i64, e: &Entry, m: &Opened) -> Result<u64> {
    let subject: Option<String> =
        tx.query_row("SELECT subject FROM messages WHERE id = ?1", [id], |r| r.get(0)).map_err(db_err)?;
    let mut keep: HashMap<String, Vec<Vector>> = HashMap::new();
    if subject == e.subject {
        let mut st = tx
            .prepare("SELECT c.text, v.model, v.revision, v.vec FROM chunks c JOIN vectors v ON v.chunk = c.id WHERE c.message = ?1")
            .map_err(db_err)?;
        let mut rows = st.query([id]).map_err(db_err)?;
        while let Some(r) = rows.next().map_err(db_err)? {
            keep.entry(r.get(0).map_err(db_err)?).or_default().push((
                r.get(1).map_err(db_err)?,
                r.get(2).map_err(db_err)?,
                r.get(3).map_err(db_err)?,
            ));
        }
    }
    // Before the subject changes: the FTS delete needs the indexed values.
    tx.execute("DELETE FROM chunks WHERE message = ?1", [id]).map_err(db_err)?;
    let has_attachments = e.flags & MSGFLAG_HASATTACH != 0;
    if !has_attachments && tx.execute("DELETE FROM message_attachments WHERE message = ?1", [id]).map_err(db_err)? > 0 {
        attach::gc(tx)?;
    }
    // Stored contents may have changed: unverified until the attachment
    // phase reads them (a link kept out of the scope stays so, `set_scope`).
    tx.execute(
        "UPDATE message_attachments SET skip = ?2 WHERE message = ?1 AND sha256 IS NOT NULL",
        params![id, attach::UNVERIFIED],
    )
    .map_err(db_err)?;
    tx.execute(
        "UPDATE messages SET received = ?2, sent = ?3, size = ?4, class = ?5, has_attachments = ?6, subject = ?7,
                sender = ?8, from_name = ?9, from_email = ?10, display_to = ?11, display_cc = ?12, signature = ?13,
                attachments_done = NULL
         WHERE id = ?1",
        params![
            id,
            e.received,
            (e.submitted > 0).then_some(e.submitted),
            e.size,
            e.class,
            has_attachments,
            e.subject,
            e.sender,
            m.from_name,
            m.from_email,
            m.to,
            m.cc,
            signature(e)
        ],
    )
    .map_err(db_err)?;
    add_chunks(tx, id, m, &keep)
}

/// Chunk the body of message `id` (at least one, maybe empty, chunk: the
/// subject stays searchable); a chunk whose text is in `keep` gets those
/// vectors back. Returns the chunk count.
fn add_chunks(tx: &Connection, id: i64, m: &Opened, keep: &HashMap<String, Vec<Vector>>) -> Result<u64> {
    let body = m.text.as_deref().map(strip_quotes).unwrap_or_default();
    let parts = chunk(&body);
    let parts = if parts.is_empty() { vec![(String::new(), 0..0)] } else { parts };
    for (seq, (t, r)) in parts.iter().enumerate() {
        tx.execute(
            "INSERT INTO chunks(message, seq, text, start_char, end_char) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, seq as i64, t, r.start as i64, r.end as i64],
        )
        .map_err(db_err)?;
        let chunk = tx.last_insert_rowid();
        for (model, revision, vec) in keep.get(t).into_iter().flatten() {
            tx.execute(
                "INSERT OR IGNORE INTO vectors(chunk, model, revision, vec) VALUES (?1, ?2, ?3, ?4)",
                params![chunk, model, revision, vec],
            )
            .map_err(db_err)?;
        }
    }
    Ok(parts.len() as u64)
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
    use crate::store::{DELETED_ITEMS_NAMES, JUNK_NAMES, locate};

    /// In-memory mailbox. Fictional content only.
    #[derive(Default)]
    pub struct Fake {
        pub revision: u64,
        pub folders: Vec<Folder>,
        pub entries: Vec<Entry>,
        pub bodies: BTreeMap<Nid, String>,
        pub opened: Vec<Nid>,
        /// Attachments with their data, by message NID.
        pub files: BTreeMap<Nid, Vec<(Attachment, Vec<u8>)>>,
        /// Attachment lists read, and attachment data read (NID, index).
        pub listed: Vec<Nid>,
        pub reads: Vec<(Nid, usize)>,
        /// Opening this NID panics (a killed sync, mid-batch).
        pub panic_on: Option<Nid>,
    }

    /// Modification time of a message as added.
    pub const MODIFIED0: i64 = 134_100_000_000_000_000;

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
                modified: MODIFIED0,
                class: Some("IPM.Note".into()),
                subject: Some(subject.into()),
                sender: Some("Zed Fict".into()),
                message_id: mid.map(str::to_string),
            });
            self.revision += 1;
            self.entries.last_mut().unwrap()
        }

        /// A by-value attachment of `nid`; sets the message's attachment flag.
        pub fn attach(
            &mut self,
            nid: Nid,
            name: &str,
            mime: Option<&str>,
            inline: bool,
            data: &[u8],
        ) -> &mut Attachment {
            let e = self.entries.iter_mut().find(|e| e.nid == nid).unwrap();
            e.flags |= MSGFLAG_HASATTACH;
            e.size += data.len() as i64;
            e.modified += 1;
            let v = self.files.entry(nid).or_default();
            let a = Attachment {
                index: v.len(),
                name: (!name.is_empty()).then(|| name.into()),
                size: Some(data.len() as i64),
                mime_type: mime.map(str::to_string),
                content_id: None,
                method: "by_value",
                inline,
                nid: 0,
            };
            v.push((a, data.to_vec()));
            self.revision += 1;
            &mut v.last_mut().unwrap().0
        }

        /// Move: new NID in another folder, same content.
        pub fn relocate(&mut self, nid: Nid, new_nid: Nid, folder: Nid) {
            let e = self.entries.iter_mut().find(|e| e.nid == nid).unwrap();
            e.nid = new_nid;
            e.folder = folder;
            let b = self.bodies.remove(&nid).unwrap();
            self.bodies.insert(new_nid, b);
            if let Some(a) = self.files.remove(&nid) {
                self.files.insert(new_nid, a);
            }
            self.revision += 1;
        }

        pub fn remove(&mut self, nid: Nid) {
            self.entries.retain(|e| e.nid != nid);
            self.revision += 1;
        }

        /// The message's row, by NID.
        pub fn entry(&mut self, nid: Nid) -> &mut Entry {
            self.entries.iter_mut().find(|e| e.nid == nid).unwrap()
        }

        /// Edited in place (same NID, same Message-ID): new body, size and
        /// modification time.
        pub fn edit(&mut self, nid: Nid, body: &str) {
            let old = self.bodies.insert(nid, body.into()).unwrap();
            let e = self.entry(nid);
            e.size += body.len() as i64 - old.len() as i64;
            e.modified += 1;
            self.revision += 1;
        }

        /// Attachment `index` of `nid` removed; the next ones shift down.
        pub fn detach(&mut self, nid: Nid, index: usize) {
            let v = self.files.get_mut(&nid).unwrap();
            let (_, data) = v.remove(index);
            for (i, (a, _)) in v.iter_mut().enumerate() {
                a.index = i;
            }
            let empty = v.is_empty();
            let e = self.entry(nid);
            e.size -= data.len() as i64;
            e.modified += 1;
            if empty {
                e.flags &= !MSGFLAG_HASATTACH;
            }
            self.revision += 1;
        }

        /// Attachment `index` of `nid` replaced by another file of the same name.
        pub fn replace(&mut self, nid: Nid, index: usize, data: &[u8]) {
            let (a, d) = &mut self.files.get_mut(&nid).unwrap()[index];
            let delta = data.len() as i64 - d.len() as i64;
            a.size = Some(data.len() as i64);
            *d = data.to_vec();
            let e = self.entry(nid);
            e.size += delta;
            e.modified += 1;
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
            assert_ne!(self.panic_on, Some(nid), "fake: killed while opening");
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
        fn attachments(&mut self, nid: Nid) -> Result<Vec<Attachment>> {
            self.listed.push(nid);
            Ok(self.files.get(&nid).map(|v| v.iter().map(|x| x.0.clone()).collect()).unwrap_or_default())
        }
        fn attachment_data(&mut self, nid: Nid, a: &Attachment) -> Result<Vec<u8>> {
            self.reads.push((nid, a.index));
            let v = self.files.get(&nid).and_then(|v| v.get(a.index));
            v.map(|x| x.1.clone()).ok_or_else(|| Error::NotFound("attachment".into()))
        }
        /// By name, as a file without well-known entry ids.
        fn well_known(&mut self, folders: &[Folder]) -> Result<WellKnown> {
            let mut no_key = |_| Ok(None);
            Ok(WellKnown {
                inbox: Default::default(),
                deleted_items: locate(folders, None, DELETED_ITEMS_NAMES, &mut no_key)?,
                junk: locate(folders, None, JUNK_NAMES, &mut no_key)?,
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
        let o = Options { batch: 4, max_time: Some(Duration::ZERO), ..Options::default() };
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
    fn chunk_ranges_stored() {
        let (mut c, mut f) = (db(), mailbox());
        let body = format!("Reply text.\n\n{}\n\nOn Mon, Zed wrote:\n> old", "word ".repeat(800));
        f.add(0x604, 0x22, Some("<r@fict.invalid>"), "Ranges", &body);
        run(&mut c, &mut f);
        let stripped: Vec<char> = strip_quotes(&body).chars().collect();
        let rows: Vec<(String, i64, i64)> = c
            .prepare(
                "SELECT c.text, c.start_char, c.end_char FROM chunks c JOIN messages m ON m.id = c.message
                 WHERE m.ident = 'mid:<r@fict.invalid>' ORDER BY c.seq",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(rows.len() >= 2, "{}", rows.len());
        for (t, s, e) in rows {
            assert_eq!(stripped[s as usize..e as usize].iter().collect::<String>(), t);
        }
        assert_eq!(count(&c, "SELECT COUNT(*) FROM chunks WHERE start_char IS NULL OR end_char IS NULL"), 0);
    }

    /// The mailbox plus `n` messages in the Archive folder.
    fn bulk(n: u32) -> Fake {
        let mut f = mailbox();
        for i in 0..n {
            f.add(0x10004 + i * 0x20, 0x42, Some(&format!("<m{i}@fict.invalid>")), "Bulk", "Bulk text.");
        }
        f
    }

    fn snapshot(c: &Connection) -> String {
        let q = |sql: &str| c.query_row(sql, [], |r| r.get::<_, Option<String>>(0)).unwrap().unwrap_or_default();
        [
            "SELECT group_concat(id || ident, ',') FROM (SELECT * FROM messages ORDER BY id)",
            "SELECT group_concat(nid || ':' || folder || ':' || message || ':' || flags, ',')
             FROM (SELECT * FROM locations ORDER BY nid)",
            "SELECT group_concat(nid || path, ',') FROM (SELECT * FROM folders ORDER BY nid)",
            "SELECT group_concat(id || ':' || text, ',') FROM (SELECT * FROM chunks ORDER BY id)",
            "SELECT group_concat(key || '=' || value, ',') FROM (SELECT * FROM meta ORDER BY key)",
        ]
        .map(q)
        .join("\n")
    }

    #[test]
    fn mass_delete_guard_trips_db_unchanged() {
        let (mut c, mut f) = (db(), bulk(150));
        run(&mut c, &mut f);
        // The Archive folder vanishes: 151 of 153 messages would go.
        f.folders.retain(|x| x.nid != 0x42);
        f.revision += 1;
        let (before, writes) = (snapshot(&c), c.total_changes());
        let e = sync(&mut c, &mut f, &Options::default()).unwrap_err();
        assert_eq!(e.kind(), "usage");
        let m = e.to_string();
        assert!(m.contains("151") && m.contains("153") && m.contains("--allow-mass-delete"), "{m}");
        for s in ["fict", "Bulk", "Archive", "Inbox", "Zed"] {
            assert!(!m.contains(s), "guard message leaks {s:?}: {m}");
        }
        assert_eq!(c.total_changes(), writes, "no write at all");
        assert_eq!(snapshot(&c), before, "DB unchanged");
        assert_eq!(sync(&mut c, &mut f, &Options::default()).unwrap_err().kind(), "usage", "still guarded");
    }

    #[test]
    fn mass_delete_allowed_with_flag() {
        let (mut c, mut f) = (db(), bulk(150));
        run(&mut c, &mut f);
        f.folders.retain(|x| x.nid != 0x42);
        f.revision += 1;
        let s = sync(&mut c, &mut f, &Options { allow_mass_delete: true, ..Options::default() }).unwrap();
        assert_eq!((s["deleted"].as_u64(), s["complete"].as_bool()), (Some(151), Some(true)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 2);
    }

    #[test]
    fn small_deletions_pass() {
        // 603 indexed: limit max(100, 20%) = 120.
        let (mut c, mut f) = (db(), bulk(600));
        run(&mut c, &mut f);
        for i in 0..120 {
            f.remove(0x10004 + i * 0x20);
        }
        assert_eq!(run(&mut c, &mut f)["deleted"], 120, "at the limit");
        // 483 indexed: limit 100.
        for i in 120..221 {
            f.remove(0x10004 + i * 0x20);
        }
        assert_eq!(sync(&mut c, &mut f, &Options::default()).unwrap_err().kind(), "usage", "101 > 100");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 483);
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

    /// The mailbox plus Deleted Items (0x62) with a subfolder (0x82) and Junk
    /// (0xA2, French name), one message each, and a copy of 0x204 in Deleted Items.
    fn with_well_known() -> Fake {
        let mut f = mailbox();
        f.folders.extend([
            folder(0x62, "Deleted Items"),
            folder(0x82, "Deleted Items/Old"),
            folder(0xA2, "Courrier indésirable"),
        ]);
        f.add(0x404, 0x62, Some("<d1@fict.invalid>"), "Trash", "Deleted text.");
        f.add(0x424, 0x82, Some("<d2@fict.invalid>"), "Old trash", "Older deleted text.");
        f.add(0x444, 0xA2, Some("<j1@fict.invalid>"), "Spam", "Junk text.");
        f.add(0x464, 0x62, Some("<a1@fict.invalid>"), "Budget café", "Le budget du café est validé.");
        f
    }

    fn inc(deleted: bool, junk: bool) -> Options {
        Options { include: Some(Include { deleted, junk }), ..Options::default() }
    }

    #[test]
    fn well_known_folders_excluded_by_default() {
        let (mut c, mut f) = (db(), with_well_known());
        let s = run(&mut c, &mut f);
        assert_eq!((s["new"].as_u64(), s["excluded_folders"].as_u64()), (Some(3), Some(3)), "{s}");
        assert_eq!(s["include"], json!({ "deleted": false, "junk": false }), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations WHERE folder IN (0x62, 0x82, 0xA2)"), 0);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM folders WHERE nid IN (0x62, 0x82, 0xA2)"), 0, "not searchable");
        assert!(!f.opened.iter().any(|n| [0x404, 0x424, 0x444, 0x464].contains(n)), "excluded: never opened");
        assert_eq!(include(&c).unwrap(), Some(Include::default()), "default stored");
        let t = s.to_string();
        assert!(!t.contains("Deleted") && !t.contains("indésirable"), "{t}");
    }

    #[test]
    fn include_choice_persisted_and_changed() {
        let (mut c, mut f) = (db(), with_well_known());
        run(&mut c, &mut f);
        let budget = |c: &Connection| count(c, "SELECT id FROM messages WHERE ident = 'mid:<a1@fict.invalid>'");
        let id = budget(&c);
        // Same revision, new choice: a full reconciliation.
        let s = sync(&mut c, &mut f, &inc(true, false)).unwrap();
        assert_eq!((s["unchanged"].as_bool(), s["new"].as_u64()), (Some(false), Some(2)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 5);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations WHERE folder = 0x82"), 1, "subfolder included");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations WHERE folder = 0xA2"), 0, "junk still excluded");
        assert_eq!(budget(&c), id, "the copy joins the same message");
        // No flag: the stored choice is kept.
        f.revision += 1;
        let s = run(&mut c, &mut f);
        assert_eq!((s["include"]["deleted"].as_bool(), s["excluded_removed"].as_u64()), (Some(true), Some(0)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 5);
        assert_eq!(run(&mut c, &mut f)["unchanged"], true);
        assert_eq!(sync(&mut c, &mut f, &inc(true, true)).unwrap()["new"], 1);
        // Back to the default: removed by the reconciliation, reported apart.
        let s = sync(&mut c, &mut f, &inc(false, false)).unwrap();
        assert_eq!((s["excluded_removed"].as_u64(), s["deleted"].as_u64()), (Some(3), Some(0)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(budget(&c), id, "a copy in a kept folder keeps its identity");
        assert_eq!(
            count(
                &c,
                "SELECT COUNT(*) FROM locations l JOIN messages m ON m.id = l.message WHERE m.id = (SELECT id FROM messages WHERE ident = 'mid:<a1@fict.invalid>')"
            ),
            1
        );
        assert_eq!(
            count(&c, "SELECT COUNT(*) FROM chunks c LEFT JOIN messages m ON m.id = c.message WHERE m.id IS NULL"),
            0
        );
        assert_eq!(count(&c, "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'spam OR trash'"), 0);
    }

    #[test]
    fn index_without_stored_choice_is_reconciled() {
        // An index synced before the choice existed held every folder.
        let (mut c, mut f) = (db(), with_well_known());
        sync(&mut c, &mut f, &inc(true, true)).unwrap();
        c.execute("DELETE FROM meta WHERE key = 'include_folders'", []).unwrap();
        let s = run(&mut c, &mut f);
        assert_eq!((s["unchanged"].as_bool(), s["excluded_removed"].as_u64()), (Some(false), Some(3)), "{s}");
        assert_eq!(include(&c).unwrap(), Some(Include::default()));
    }

    #[test]
    fn exclusion_is_not_a_mass_deletion() {
        let mut f = mailbox();
        f.folders.push(folder(0x62, "Deleted Items"));
        for i in 0..150 {
            f.add(0x10004 + i * 0x20, 0x62, Some(&format!("<t{i}@fict.invalid>")), "Bulk", "Bulk text.");
        }
        let mut c = db();
        sync(&mut c, &mut f, &inc(true, false)).unwrap();
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 153);
        // 150 of 153 leave, plus one real deletion: the guard counts the deletion only.
        f.remove(0x224);
        let s = sync(&mut c, &mut f, &inc(false, false)).unwrap();
        assert_eq!((s["excluded_removed"].as_u64(), s["deleted"].as_u64()), (Some(150), Some(1)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages"), 2);
        // A vanished folder is still guarded.
        sync(&mut c, &mut f, &inc(true, false)).unwrap();
        f.folders.retain(|x| x.nid != 0x62);
        f.revision += 1;
        assert_eq!(sync(&mut c, &mut f, &Options::default()).unwrap_err().kind(), "usage");
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

    // S1: refresh of changed messages.

    /// Four paragraphs, several chunks; paragraph 3 says `last`.
    fn long_body(last: &str) -> String {
        let mut p: Vec<String> =
            (0..3).map(|i| format!("Paragraph {i}. {}", format!("filler{i} ").repeat(110))).collect();
        p.push(format!("Paragraph 3. {last}"));
        p.join("\n\n")
    }

    /// Fake vectors for every body chunk: the blob is the chunk text, so a
    /// reused vector can be traced to its text.
    fn embed_all(c: &Connection) {
        let m = crate::index::model::get("e5-small").unwrap();
        c.execute(
            "INSERT OR IGNORE INTO vectors SELECT id, ?1, ?2, CAST(text AS BLOB) FROM chunks",
            params![m.name, m.revision],
        )
        .unwrap();
        set_meta(c, "embed_model", m.name).unwrap();
        set_meta(c, "embed_revision", m.revision).unwrap();
    }

    /// (chunk text, vector blob) of a message, in order.
    fn chunk_vectors(c: &Connection, ident: &str) -> Vec<(String, Option<Vec<u8>>)> {
        c.prepare(
            "SELECT c.text, v.vec FROM chunks c JOIN messages m ON m.id = c.message
             LEFT JOIN vectors v ON v.chunk = c.id WHERE m.ident = ?1 ORDER BY c.seq",
        )
        .unwrap()
        .query_map([ident], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn fts(c: &Connection, q: &str) -> i64 {
        count(c, &format!("SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH '{q}'"))
    }

    fn fts_consistent(c: &Connection) {
        c.execute("INSERT INTO chunks_fts(chunks_fts, rank) VALUES ('integrity-check', 1)", []).unwrap();
        assert_eq!(
            count(c, "SELECT COUNT(*) FROM chunks c LEFT JOIN messages m ON m.id = c.message WHERE m.id IS NULL"),
            0
        );
    }

    #[test]
    fn edited_body_refreshes_text_and_reuses_unchanged_vectors() {
        let (mut c, mut f) = (db(), mailbox());
        const ID: &str = "mid:<e1@fict.invalid>";
        f.add(0x604, 0x22, Some("<e1@fict.invalid>"), "Report", &long_body("Original Quorvex ending."));
        run(&mut c, &mut f);
        embed_all(&c);
        let before = chunk_vectors(&c, ID);
        assert!(before.len() >= 3 && before.iter().all(|(_, v)| v.is_some()), "{before:?}");
        let id = count(&c, &format!("SELECT id FROM messages WHERE ident = '{ID}'"));
        let opened = f.opened.len();

        // Same Message-ID, same NID: the last paragraph and the sender change.
        f.edit(0x604, &long_body("Edited Zanthorpe ending, much longer than before."));
        f.entry(0x604).sender = Some("Yan Fict".into());
        let s = run(&mut c, &mut f);
        assert_eq!(
            (s["refreshed"].as_u64(), s["new"].as_u64(), s["complete"].as_bool()),
            (Some(1), Some(0), Some(true)),
            "{s}"
        );
        assert_eq!(f.opened[opened..], [0x604], "the edited message is opened again, alone");
        assert_eq!(count(&c, &format!("SELECT id FROM messages WHERE ident = '{ID}'")), id, "same message row");
        let size = f.entries.iter().find(|e| e.nid == 0x604).unwrap().size;
        assert_eq!(count(&c, &format!("SELECT size FROM messages WHERE id = {id}")), size);
        let from: String = c.query_row("SELECT from_name FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(from, "Yan Fict", "metadata refreshed");
        assert_eq!((fts(&c, "zanthorpe"), fts(&c, "quorvex")), (1, 0), "FTS follows the new text");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM locations WHERE nid = 0x604"), 1);
        fts_consistent(&c);

        // Unchanged chunk texts keep their own vector; the changed one is pending.
        let old: HashMap<String, Vec<u8>> = before.into_iter().map(|(t, v)| (t, v.unwrap())).collect();
        let after = chunk_vectors(&c, ID);
        let (mut reused, mut pending) = (0, 0);
        for (t, v) in &after {
            match old.get(t) {
                Some(o) => {
                    assert_eq!(v.as_ref(), Some(o), "vector of {t:.20}… reused");
                    reused += 1;
                }
                None => {
                    assert_eq!(v, &None, "changed chunk must be embedded again");
                    pending += 1;
                }
            }
        }
        assert!(reused >= 2 && pending == 1, "reused {reused}, pending {pending}");
        assert_eq!(crate::index::vectors::counts(&c).unwrap()["pending"], 1);

        // Nothing changed since: no refresh, no open.
        f.revision += 1;
        let s = run(&mut c, &mut f);
        assert_eq!((s["refreshed"].as_u64(), f.opened.len()), (Some(0), opened + 1), "{s}");

        // Same length edit: the modification time alone tells.
        let body = long_body("Edited Zanthorpe ending, much longer than before.").replace("Zanthorpe", "Yelbranx!");
        f.edit(0x604, &body);
        assert_eq!(f.entry(0x604).size, size, "same size");
        assert_eq!(run(&mut c, &mut f)["refreshed"], 1);
        assert_eq!((fts(&c, "yelbranx"), fts(&c, "zanthorpe")), (1, 0));

        // New subject: in every passage, so no vector of the message is kept.
        f.entry(0x604).subject = Some("Renamed Wexplod".into());
        f.revision += 1;
        assert_eq!(run(&mut c, &mut f)["refreshed"], 1);
        assert_eq!((fts(&c, "wexplod"), fts(&c, "report")), (chunk_vectors(&c, ID).len() as i64, 0));
        assert!(chunk_vectors(&c, ID).iter().all(|(_, v)| v.is_none()));
        fts_consistent(&c);
    }

    #[test]
    fn flag_move_and_copy_do_not_reopen() {
        let (mut c, mut f) = (db(), mailbox());
        run(&mut c, &mut f);
        embed_all(&c);
        let vectors = count(&c, "SELECT COUNT(*) FROM vectors");
        f.entry(0x224).flags = 0;
        f.relocate(0x204, 0x284, 0x42);
        f.add(0x2a4, 0x42, Some("<a2@fict.invalid>"), "Lunch", "Lunch on friday at noon.");
        let s = run(&mut c, &mut f);
        assert_eq!(
            (s["refreshed"].as_u64(), s["new"].as_u64(), s["moved"].as_u64(), s["flags_changed"].as_u64()),
            (Some(0), Some(0), Some(2), Some(1)),
            "{s}"
        );
        assert_eq!(f.opened.len(), 3, "nothing opened again");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors"), vectors);
    }

    /// Messages indexed before the signature existed (NULL) adopt the
    /// current one without being opened: an edit made before the upgrade is
    /// not picked up; one made after is.
    #[test]
    fn null_signature_adopted_without_open() {
        let (mut c, mut f) = (db(), mailbox());
        run(&mut c, &mut f);
        c.execute("UPDATE messages SET signature = NULL", []).unwrap();
        f.edit(0x204, "Edited before the upgrade.");
        let s = run(&mut c, &mut f);
        assert_eq!((s["refreshed"].as_u64(), f.opened.len()), (Some(0), 3), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages WHERE signature IS NULL"), 0, "adopted");
        assert_eq!(fts(&c, "upgrade"), 0, "documented limitation");
        f.edit(0x204, "Edited after the upgrade.");
        assert_eq!(run(&mut c, &mut f)["refreshed"], 1);
        assert_eq!(fts(&c, "upgrade"), 1);
    }

    /// An index at the schema before the signature (HEAD 62c4675: 6
    /// migrations) with body and attachment vectors: the migration adds a
    /// NULL signature and keeps every vector; the first sync adopts the
    /// signatures without opening or embedding anything.
    #[test]
    fn signature_migration_keeps_vectors() {
        use crate::index::MIGRATIONS;
        let (mut c, mut f) = (db(), mailbox());
        run(&mut c, &mut f);
        embed_all(&c);
        c.execute_batch(
            "INSERT INTO attachment_contents(sha256, kind, size, outcome, extracted_at) VALUES ('s', 'txt', 3, 'ok', 0);
             INSERT INTO message_attachments(message, idx, name, sha256) VALUES (1, 0, 'a.txt', 's');
             INSERT INTO attachment_chunks(id, sha256, seq, text) VALUES (1, 's', 0, 'one');
             INSERT INTO attachment_vectors VALUES (1, 'e5-small', 'r', x'0102');
             ALTER TABLE messages DROP COLUMN signature;",
        )
        .unwrap();
        c.execute("DELETE FROM schema_version WHERE version > 6", []).unwrap();
        let cols = |c: &Connection| -> Vec<String> {
            c.prepare("SELECT name FROM pragma_table_info('messages')")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let v6 = Connection::open_in_memory().unwrap();
        v6.execute_batch(&MIGRATIONS[..6].join(";\n")).unwrap();
        assert_eq!(cols(&c), cols(&v6), "the schema of HEAD 62c4675");
        let vecs = |c: &Connection| {
            c.query_row(
                "SELECT (SELECT group_concat(chunk || hex(vec)) FROM vectors),
                        (SELECT group_concat(chunk || hex(vec)) FROM attachment_vectors)",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .unwrap()
        };
        let before = vecs(&c);
        migrate(&mut c).unwrap();
        assert_eq!(crate::index::schema_version(&c).unwrap(), MIGRATIONS.len());
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages WHERE signature IS NULL"), 3);
        assert_eq!(vecs(&c), before, "vectors kept by the migration");
        f.revision += 1;
        let s = run(&mut c, &mut f);
        assert_eq!((s["refreshed"].as_u64(), f.opened.len()), (Some(0), 3), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages WHERE signature IS NULL"), 0);
        assert_eq!(vecs(&c), before, "vectors kept by the first sync");
    }

    /// Refreshes go in batches: `--max-time` stops between them, a failed
    /// open or a kill mid-batch leaves the old content and the old
    /// signature, and the next sync resumes.
    #[test]
    fn refresh_interrupted_then_resumed() {
        let (mut c, mut f) = (db(), mailbox());
        let nids: Vec<Nid> = (0..7u32).map(|i| 0x1004 + i * 0x20).collect();
        for (i, n) in nids.iter().enumerate() {
            f.add(*n, 0x22, Some(&format!("<b{i}@fict.invalid>")), "Bulk", &format!("Bulk text {i}."));
        }
        run(&mut c, &mut f);
        for n in &nids {
            f.edit(*n, "Quillbrand edit.");
        }
        let o = Options { batch: 4, max_time: Some(Duration::ZERO), ..Options::default() };
        let s = sync(&mut c, &mut f, &o).unwrap();
        assert_eq!(
            (s["refreshed"].as_u64(), s["complete"].as_bool(), s["remaining"].as_u64()),
            (Some(4), Some(false), Some(3)),
            "{s}"
        );
        assert_eq!(fts(&c, "quillbrand"), 4, "one committed batch");
        fts_consistent(&c);

        // Killed while opening the 2nd message of the next batch: rolled back.
        f.panic_on = Some(nids[5]);
        let snap = snapshot(&c);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sync(&mut c, &mut f, &Options::default())));
        assert!(r.is_err());
        assert_eq!(snapshot(&c).lines().take(4).collect::<Vec<_>>(), snap.lines().take(4).collect::<Vec<_>>());
        assert_eq!(fts(&c, "quillbrand"), 4);
        f.panic_on = None;

        // A failed open: counted, old content kept, retried.
        let body = f.bodies.remove(&nids[6]).unwrap();
        let s = run(&mut c, &mut f);
        assert_eq!(
            (s["refreshed"].as_u64(), s["failed"].as_u64(), s["complete"].as_bool()),
            (Some(2), Some(1), Some(false)),
            "{s}"
        );
        assert_eq!((fts(&c, "quillbrand"), fts(&c, "\"text 6\"")), (6, 1));
        f.bodies.insert(nids[6], body);
        let s = run(&mut c, &mut f);
        assert_eq!((s["refreshed"].as_u64(), s["complete"].as_bool()), (Some(1), Some(true)), "{s}");
        assert_eq!(fts(&c, "quillbrand"), 7);
        assert_eq!(run(&mut c, &mut f)["unchanged"], true);
        fts_consistent(&c);
    }

    #[test]
    fn mass_delete_guard_blocks_refreshes() {
        let (mut c, mut f) = (db(), bulk(150));
        run(&mut c, &mut f);
        f.edit(0x204, "Edited Brumwick text.");
        f.folders.retain(|x| x.nid != 0x42);
        let (before, opened) = (snapshot(&c), f.opened.len());
        assert_eq!(sync(&mut c, &mut f, &Options::default()).unwrap_err().kind(), "usage");
        assert_eq!((snapshot(&c), f.opened.len()), (before, opened), "nothing refreshed or opened");
        let s = sync(&mut c, &mut f, &Options { allow_mass_delete: true, ..Options::default() }).unwrap();
        assert_eq!((s["refreshed"].as_u64(), s["deleted"].as_u64()), (Some(1), Some(151)), "{s}");
        assert_eq!(fts(&c, "brumwick"), 1);
    }

    #[test]
    fn f3_schema6_matching_revision_adopts_before_next_edit() {
        let (mut c, mut f) = (db(), mailbox());
        f.edit(0x204, "Original Quorvex body.");
        run(&mut c, &mut f);
        embed_all(&c);
        c.execute_batch(
            "INSERT INTO attachment_contents(sha256, kind, size, outcome, extracted_at) VALUES ('f3', 'txt', 3, 'ok', 0);
             INSERT INTO message_attachments(message, idx, name, sha256)
                SELECT id, 0, 'a.txt', 'f3' FROM messages WHERE ident = 'mid:<a1@fict.invalid>';
             INSERT INTO attachment_chunks(id, sha256, seq, text) VALUES (1, 'f3', 0, 'one');
             INSERT INTO attachment_vectors VALUES (1, 'e5-small', 'r', x'0102');
             ALTER TABLE messages DROP COLUMN signature;",
        )
        .unwrap();
        c.execute("DELETE FROM schema_version WHERE version > 6", []).unwrap();
        assert_eq!(crate::index::schema_version(&c).unwrap(), 6);
        let columns = |connection: &Connection| -> Vec<String> {
            connection
                .prepare("SELECT name FROM pragma_table_info('messages')")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let schema6 = Connection::open_in_memory().unwrap();
        schema6.execute_batch(&crate::index::MIGRATIONS[..6].join(";\n")).unwrap();
        assert_eq!(columns(&c), columns(&schema6));
        let vectors = |connection: &Connection| {
            connection
                .query_row(
                    "SELECT (SELECT group_concat(chunk || hex(vec)) FROM vectors),
                            (SELECT group_concat(chunk || hex(vec)) FROM attachment_vectors)",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap()
        };
        let before = vectors(&c);
        let revision = f.revision;
        let opened = f.opened.len();
        assert_eq!(get_meta(&c, "sync_complete").unwrap().as_deref(), Some("1"));
        assert_eq!(get_meta(&c, "sync_revision").unwrap(), Some(revision.to_string()));

        migrate(&mut c).unwrap();
        assert_eq!(count(&c, "SELECT COUNT(*) FROM messages WHERE signature IS NULL"), 3);
        assert_eq!(vectors(&c), before, "migration preserves body and attachment vectors");
        let adoption = run(&mut c, &mut f);
        let null_after_adoption = count(&c, "SELECT COUNT(*) FROM messages WHERE signature IS NULL");
        assert_eq!(f.revision, revision, "first sync must use the already stored revision");
        assert_eq!(f.opened.len(), opened, "adoption should not open messages: {adoption}");
        assert_eq!(vectors(&c), before, "adoption preserves body and attachment vectors");
        assert_eq!((fts(&c, "quorvex"), fts(&c, "zanthorpe")), (1, 0));

        f.edit(0x204, "Replacement Zanthorpe body.");
        let edited = run(&mut c, &mut f);
        let text: String = c
            .query_row(
                "SELECT group_concat(ch.text, '\n') FROM chunks ch JOIN messages m ON m.id = ch.message
                 WHERE m.ident = 'mid:<a1@fict.invalid>'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (null_after_adoption, fts(&c, "zanthorpe"), fts(&c, "quorvex")),
            (0, 1, 0),
            "expected all NULL signatures adopted at matching revision, then replacement indexed and old text gone; \
             observed NULLs after adoption={null_after_adoption}, indexed text={text:?}, \
             opened after edit={:?}, adoption={adoption}, edited={edited}",
            &f.opened[opened..]
        );
        assert!(text.contains("Replacement Zanthorpe body."), "observed indexed text: {text:?}");
        fts_consistent(&c);
    }
}
